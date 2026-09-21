use anyhow::Result;
use serde::Serialize;

use crate::census::{best_outcomes, Census, Reach};
use crate::churn::Change;
use crate::probe::{Family, Outcome, Stage, Tip};
use crate::resolve::{Endpoint, Entry};
use crate::reversed::Shadow;
use crate::snapshot::Snapshot;

#[derive(Serialize)]
struct Report<'a> {
    summary: &'a Census,
    pools: Vec<PoolReport>,
}

#[derive(Serialize)]
struct PoolReport {
    index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pool_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ticker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    accumulated_stake: f64,
    relative_stake: f64,
    relays_total: usize,
    relays_reachable: usize,
    /// Relays the pool index matched only through their octet reversal.
    relays_reversed: usize,
    /// Why relays failed, distinct and most common first.
    reasons: Vec<&'static str>,
    /// Distinct socket addresses the relays resolved to.
    endpoints: Vec<String>,
    reach: Reach,
    fraction: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    fastest_rtt_ms: Option<u64>,
    relays: Vec<RelayReport>,
}

#[derive(Serialize)]
struct RelayReport {
    address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    port: Option<u16>,
    srv: bool,
    /// Every endpoint this entry was probed at. One for a plain relay, one per
    /// top-priority SRV target otherwise.
    endpoints: Vec<String>,
    /// Each target's own result for an SRV name or a name with several
    /// addresses; empty for a relay with one address.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    targets: Vec<TargetReport>,
    /// For an SRV record, the weight of answering targets over all of them.
    #[serde(skip_serializing_if = "Option::is_none")]
    reach_probability: Option<f64>,
    /// The endpoint whose result the fields below describe.
    #[serde(skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
    shared_endpoint: bool,
    reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    peer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    family: Option<Family>,
    #[serde(skip_serializing_if = "Option::is_none")]
    n2n_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tip: Option<Tip>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rtt_ms: Option<u64>,
    /// Blocks behind the highest tip seen, and whether on the main chain group.
    #[serde(skip_serializing_if = "Option::is_none")]
    lag_blocks: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    main_chain: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    asn: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    as_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stage: Option<Stage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// For an IPv4 literal, the address with its octets reversed and whether
    /// that answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    reversed_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reversed_reachable: Option<bool>,
    /// How the entry's state differs from the previous report.
    #[serde(skip_serializing_if = "Option::is_none")]
    changed: Option<Change>,
}

#[derive(Serialize)]
struct TargetReport {
    endpoint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    weight: Option<u16>,
    /// When a lookup last returned this address; the next run carries it
    /// forward for a day if the name stops returning it.
    last_seen: u64,
    /// Known from an earlier run only, not from this run's lookup.
    remembered: bool,
    reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    rtt_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stage: Option<Stage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn target_report(endpoint: &Endpoint, entry: usize, outcome: Option<&Outcome>, now: u64) -> TargetReport {
    let mut t = TargetReport {
        endpoint: endpoint.key.clone(),
        weight: endpoint
            .entries
            .iter()
            .position(|&e| e == entry)
            .and_then(|i| endpoint.weights[i]),
        last_seen: endpoint.last_seen.unwrap_or(now),
        remembered: endpoint.remembered,
        reachable: false,
        rtt_ms: None,
        stage: None,
        error: None,
    };
    match outcome {
        Some(Ok(ok)) => {
            t.reachable = true;
            t.rtt_ms = Some(ok.rtt_ms);
        }
        Some(Err(f)) => {
            t.stage = Some(f.stage);
            t.error = Some(f.error.clone());
        }
        None => {}
    }
    t
}

pub fn render(
    census: &Census,
    snap: &Snapshot,
    entries: &[Entry],
    endpoints: &[Endpoint],
    outcomes: &[Option<Outcome>],
    srv_errors: &[Option<String>],
    shadow: &Shadow,
) -> Result<String> {
    let mut entry_endpoints: Vec<Vec<usize>> = vec![Vec::new(); entries.len()];
    for (i, ep) in endpoints.iter().enumerate() {
        for &e in &ep.entries {
            entry_endpoints[e].push(i);
        }
    }
    let best = best_outcomes(entries, endpoints, outcomes, srv_errors);

    let mut pools: Vec<PoolReport> = census
        .pools
        .iter()
        .map(|p| PoolReport {
            index: p.index,
            pool_id: p.meta.as_ref().map(|m| m.pool_id.clone()),
            ticker: p.meta.as_ref().and_then(|m| m.ticker.clone()),
            name: p.meta.as_ref().and_then(|m| m.name.clone()),
            accumulated_stake: snap.pools[p.index].accumulated_stake,
            relative_stake: p.relative_stake,
            relays_total: p.relays_total,
            relays_reachable: p.relays_reachable,
            relays_reversed: p.relays_reversed,
            reasons: p.reasons.clone(),
            endpoints: p.endpoints.clone(),
            reach: p.reach,
            fraction: p.fraction,
            fastest_rtt_ms: p.fastest_rtt_ms,
            relays: Vec::new(),
        })
        .collect();

    for (i, e) in entries.iter().enumerate() {
        let eps = &entry_endpoints[i];
        // The endpoint that produced the entry's best outcome, if any did.
        let chosen = match &best[i] {
            Some(Ok(b)) => eps.iter().copied().find(|&x| {
                matches!(&outcomes[x], Some(Ok(o)) if o.peer == b.peer && o.rtt_ms == b.rtt_ms)
            }),
            Some(Err(_)) | None => eps.first().copied(),
        };
        let mut r = RelayReport {
            address: e.address.clone(),
            port: e.port,
            srv: e.is_srv(),
            endpoints: eps.iter().map(|&x| endpoints[x].key.clone()).collect(),
            // Every target of a name, SRV or plain, so the next run knows which
            // addresses it has seen; an IP literal is its own address.
            targets: if e.address.trim_matches(&['[', ']'][..]).parse::<std::net::IpAddr>().is_err() {
                eps.iter()
                    .map(|&x| target_report(&endpoints[x], i, outcomes[x].as_ref(), census.timestamp_seconds))
                    .collect()
            } else {
                Vec::new()
            },
            reach_probability: e.is_srv().then_some(census.entry_probability[i]),
            endpoint: chosen.map(|x| endpoints[x].key.clone()),
            shared_endpoint: chosen.map(|x| endpoints[x].entries.len() > 1).unwrap_or(false),
            reachable: false,
            peer: None,
            family: None,
            n2n_version: None,
            tip: None,
            rtt_ms: None,
            lag_blocks: census.entry_tips[i].map(|t| t.lag_blocks),
            main_chain: census.entry_tips[i].map(|t| t.main),
            asn: census.entry_asn[i].as_ref().map(|a| a.asn),
            as_name: census.entry_asn[i].as_ref().map(|a| a.name.clone()),
            stage: None,
            error: None,
            reversed_address: shadow.address[i].clone(),
            reversed_reachable: shadow.reachable[i],
            changed: census.entry_change[i],
        };
        match &best[i] {
            Some(Ok(ok)) => {
                r.reachable = true;
                r.peer = Some(ok.peer.to_string());
                r.family = Some(ok.family);
                r.n2n_version = Some(ok.n2n_version.clone());
                r.tip = Some(ok.tip.clone());
                r.rtt_ms = Some(ok.rtt_ms);
            }
            Some(Err(f)) => {
                r.stage = Some(f.stage);
                r.error = Some(f.error.clone());
            }
            None => {}
        }
        pools[e.pool].relays.push(r);
    }

    Ok(serde_json::to_string_pretty(&Report { summary: census, pools })?)
}
