use anyhow::Result;
use serde::Serialize;

use crate::census::{Census, Reach};
use crate::probe::{Family, Outcome, Stage, Tip};
use crate::resolve::{Endpoint, Entry};
use crate::snapshot::Snapshot;

#[derive(Serialize)]
struct Report<'a> {
    summary: &'a Census,
    pools: Vec<PoolReport>,
}

#[derive(Serialize)]
struct PoolReport {
    index: usize,
    accumulated_stake: f64,
    relative_stake: f64,
    relays_total: usize,
    relays_reachable: usize,
    reach: Reach,
    fraction: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    fastest_rtt_ms: Option<u64>,
    relays: Vec<RelayReport>,
}

#[derive(Serialize)]
struct RelayReport {
    address: String,
    port: u16,
    endpoint: String,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    stage: Option<Stage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub fn render(
    census: &Census,
    snap: &Snapshot,
    entries: &[Entry],
    endpoints: &[Endpoint],
    outcomes: &[Option<Outcome>],
) -> Result<String> {
    let mut entry_endpoint = vec![usize::MAX; entries.len()];
    for (i, ep) in endpoints.iter().enumerate() {
        for &e in &ep.entries {
            entry_endpoint[e] = i;
        }
    }

    let mut pools: Vec<PoolReport> = census
        .pools
        .iter()
        .map(|p| PoolReport {
            index: p.index,
            accumulated_stake: snap.pools[p.index].accumulated_stake,
            relative_stake: p.relative_stake,
            relays_total: p.relays_total,
            relays_reachable: p.relays_reachable,
            reach: p.reach,
            fraction: p.fraction,
            fastest_rtt_ms: p.fastest_rtt_ms,
            relays: Vec::new(),
        })
        .collect();

    for (i, e) in entries.iter().enumerate() {
        let ep = &endpoints[entry_endpoint[i]];
        let outcome = outcomes[entry_endpoint[i]].as_ref();
        let mut r = RelayReport {
            address: e.address.clone(),
            port: e.port,
            endpoint: ep.key.clone(),
            shared_endpoint: ep.entries.len() > 1,
            reachable: false,
            peer: None,
            family: None,
            n2n_version: None,
            tip: None,
            rtt_ms: None,
            stage: None,
            error: None,
        };
        match outcome {
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
