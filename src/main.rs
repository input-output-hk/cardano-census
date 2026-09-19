mod asn;
mod census;
mod cli;
mod metrics;
mod net;
mod node;
mod output;
mod probe;
mod report;
mod resolve;
mod snapshot;

use anyhow::Result;
use clap::Parser;
use futures::stream::{self, StreamExt};
use rand::seq::SliceRandom;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cli::Args;
use probe::{Failed, Outcome, Stage};

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    if let Err(e) = run(&args).await {
        eprintln!("cardano-census: {e:#}");
        if !args.output_is_stdout() {
            if let Err(w) = output::write(&args.output, &metrics::render_failure(unix_now())) {
                eprintln!("cardano-census: writing failure metrics: {w:#}");
            }
        }
        std::process::exit(1);
    }
}

async fn run(args: &Args) -> Result<()> {
    net::set_happy_eyeballs_config(!args.prefer_ipv4, args.happy_eyeballs_delay_ms);

    let (snap, source) = match (&args.node_socket, &args.snapshot) {
        (Some(socket), _) => {
            let magic = args
                .network_magic
                .ok_or_else(|| anyhow::anyhow!("--node-socket needs --network-magic"))?;
            eprintln!("asking {} for its big ledger peer snapshot", socket.display());
            let snap = node::fetch(socket, magic, Duration::from_secs(30)).await?;
            (snap, socket.display().to_string())
        }
        (None, Some(path)) => (snapshot::load(path)?, path.display().to_string()),
        (None, None) => anyhow::bail!("one of --snapshot or --node-socket is required"),
    };
    let magic = args.network_magic.unwrap_or(snap.network_magic);
    let entries = resolve::entries(&snap);
    eprintln!(
        "snapshot from {source}: {} pools, {} relay entries, magic {}, N2C v{}, slot {}",
        snap.pools.len(),
        entries.len(),
        snap.network_magic,
        snap.node_to_client_version,
        snap.point.block_point_slot,
    );

    let asn_db = args.asn_db.as_ref().and_then(|path| match asn::AsnDb::load(path) {
        Ok(db) => {
            eprintln!("AS database: {} ranges from {}", db.len(), path.display());
            Some(db)
        }
        Err(e) => {
            eprintln!("AS database unavailable, relays will not be placed in networks: {e:#}");
            None
        }
    });

    let started = Instant::now();
    let resolve::Resolved { endpoints, srv_errors } = resolve::resolve(&entries, args.parallel).await;
    let unresolved = endpoints.iter().filter(|e| e.dns_error.is_some()).count();
    let srv_total = entries.iter().filter(|e| e.is_srv()).count();
    let srv_failed = srv_errors.iter().flatten().count();
    eprintln!(
        "resolved to {} endpoints in {:.1}s, {} names did not resolve, {} of {} SRV lookups failed",
        endpoints.len(),
        started.elapsed().as_secs_f64(),
        unresolved,
        srv_failed,
        srv_total,
    );

    let timeout = Duration::from_secs(args.timeout);
    let parallel = args.parallel.max(1);
    let mut order: Vec<usize> = (0..endpoints.len())
        .filter(|&i| endpoints[i].dns_error.is_none())
        .collect();
    order.shuffle(&mut rand::rng());
    let waves = order.len().div_ceil(parallel) as u64;
    eprintln!(
        "probing {} endpoints, {parallel} at once, {}s budget each; finishes within {}s at worst",
        order.len(),
        args.timeout,
        waves * args.timeout,
    );

    let probed: Vec<(usize, Outcome)> = stream::iter(order)
        .map(|i| {
            let addr = endpoints[i].key.clone();
            async move { (i, probe::probe(&addr, magic, timeout).await) }
        })
        .buffer_unordered(parallel)
        .collect()
        .await;

    let mut outcomes: Vec<Option<Outcome>> = vec![None; endpoints.len()];
    for (i, o) in probed {
        outcomes[i] = Some(o);
    }
    for (i, ep) in endpoints.iter().enumerate() {
        if let Some(err) = &ep.dns_error {
            outcomes[i] = Some(Err(Failed {
                stage: Stage::Dns,
                error: err.clone(),
            }));
        }
    }

    let census = census::build(
        &source,
        &snap,
        &entries,
        &endpoints,
        &outcomes,
        &srv_errors,
        args.fork_tolerance,
        asn_db.as_ref(),
        args.asn_min_relays,
        started.elapsed(),
        unix_now(),
    );

    output::write(&args.output, &metrics::render(&census))?;
    if let Some(path) = &args.report {
        output::write(
            path,
            &report::render(&census, &snap, &entries, &endpoints, &outcomes, &srv_errors)?,
        )?;
    }

    let reachable = census.relays_reachable_v4 + census.relays_reachable_v6;
    eprintln!(
        "{} pools, {} relays at {} endpoints; {} relays reachable, {:.1}% of stake reachable, {:.1}s",
        census.blp_total,
        census.relays_total,
        census.endpoints_total,
        reachable,
        census.reachable_stake_ratio * 100.0,
        census.scan_duration_seconds,
    );
    Ok(())
}
