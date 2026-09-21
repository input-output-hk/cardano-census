mod asn;
mod census;
mod churn;
mod cli;
mod metrics;
mod net;
mod node;
mod output;
mod pools;
mod probe;
mod report;
mod resolve;
mod reversed;
mod snapshot;

use anyhow::Result;
use clap::Parser;
use futures::stream::{self, StreamExt};
use rand::seq::SliceRandom;
use std::collections::HashMap;
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
            if let Err(w) = output::write(&args.output, &metrics::render_failure(unix_now(), &args.labels)) {
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

    let pool_index = args.pool_index.as_ref().and_then(|path| match pools::PoolIndex::load(path) {
        Ok(idx) => {
            eprintln!("pool index: {} pools from {}", idx.pools, path.display());
            Some(idx)
        }
        Err(e) => {
            eprintln!("pool index unavailable, pools will be named by their first relay: {e:#}");
            None
        }
    });

    // The previous run's report, read before this run overwrites it.
    let previous = args.report.as_ref().filter(|p| p.exists()).and_then(|path| match churn::Previous::load(path) {
        Ok(prev) => {
            eprintln!("previous report: {} relays from {}", prev.by_key.len(), path.display());
            Some(prev)
        }
        Err(e) => {
            eprintln!("previous report unusable, no churn this run: {e:#}");
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
    // An endpoint that resolved only to private or reserved space is never
    // dialled: nothing public can live there, and loopback or 0.0.0.0 would
    // answer from this host's own node.
    let ipv6 = net::has_ipv6_route();
    if !ipv6 {
        eprintln!("no IPv6 route from this host, IPv6 addresses are not probed");
    }
    let mut outcomes: Vec<Option<Outcome>> = vec![None; endpoints.len()];
    for (i, ep) in endpoints.iter().enumerate() {
        if let Some(err) = &ep.dns_error {
            outcomes[i] = Some(Err(Failed { stage: Stage::Dns, error: err.clone() }));
        } else if !ep.addrs.is_empty() && ep.addrs.iter().all(|ip| asn::special_use(*ip).is_some()) {
            let ip = ep.addrs[0];
            let class = asn::special_use(ip).unwrap_or("reserved");
            outcomes[i] = Some(Err(Failed { stage: Stage::Address, error: format!("{class} address {ip}, not probed") }));
        } else if !ipv6 && !ep.addrs.is_empty() && ep.addrs.iter().all(|ip| ip.is_ipv6()) {
            let ip = ep.addrs[0];
            outcomes[i] = Some(Err(Failed {
                stage: Stage::Connect,
                error: format!("IPv6 unreachable from the census host, {ip} not probed"),
            }));
        }
    }
    let mut order: Vec<usize> = (0..endpoints.len()).filter(|&i| outcomes[i].is_none()).collect();
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

    for (i, o) in probed {
        outcomes[i] = Some(o);
    }

    // Shadow probe: every IPv4 literal at its octet reversal, see reversed.rs.
    let shadow_address = reversed::targets(&entries);
    let mut shadow_keys: Vec<String> = shadow_address.iter().flatten().cloned().collect();
    shadow_keys.sort();
    shadow_keys.dedup();
    shadow_keys.shuffle(&mut rand::rng());
    eprintln!(
        "probing the octet reversal of {} IPv4 literal relays at {} distinct addresses",
        shadow_address.iter().flatten().count(),
        shadow_keys.len(),
    );
    let shadow_ok: HashMap<String, bool> = stream::iter(shadow_keys)
        .map(|addr| async move {
            // A reversal landing in private or reserved space is not dialled either.
            let special = addr
                .parse::<std::net::SocketAddr>()
                .is_ok_and(|s| asn::special_use(s.ip()).is_some());
            let ok = !special && probe::probe(&addr, magic, timeout).await.is_ok();
            (addr, ok)
        })
        .buffer_unordered(parallel)
        .collect()
        .await;
    let shadow = reversed::Shadow {
        reachable: shadow_address
            .iter()
            .map(|a| a.as_ref().map(|k| shadow_ok.get(k).copied().unwrap_or(false)))
            .collect(),
        address: shadow_address,
    };

    let census = census::build(
        &source,
        &snap,
        &entries,
        &endpoints,
        &outcomes,
        &srv_errors,
        &shadow,
        previous.as_ref(),
        args.fork_tolerance,
        asn_db.as_ref(),
        args.asn_min_relays,
        pool_index.as_ref(),
        args.top_pools,
        started.elapsed(),
        unix_now(),
    );
    if pool_index.is_some() {
        eprintln!("pool index named {} of {} snapshot pools", census.pools_indexed, census.blp_total);
    }

    output::write(&args.output, &metrics::render(&census, &args.labels))?;
    if let Some(path) = &args.report {
        output::write(
            path,
            &report::render(&census, &snap, &entries, &endpoints, &outcomes, &srv_errors, &shadow)?,
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
