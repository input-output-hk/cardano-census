use serde::Serialize;
use std::collections::BTreeMap;
use std::time::Duration;

use crate::probe::{Family, Outcome, Stage};
use crate::resolve::{Endpoint, Entry};
use crate::snapshot::Snapshot;

/// How much of a pool answered: none of its relays, some, or all.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Reach {
    None,
    Partial,
    Full,
}

impl Reach {
    pub const ALL: [Reach; 3] = [Reach::None, Reach::Partial, Reach::Full];

    pub fn label(self) -> &'static str {
        match self {
            Reach::None => "none",
            Reach::Partial => "partial",
            Reach::Full => "full",
        }
    }
}

/// Cumulative buckets in the Prometheus `le` sense.
#[derive(Clone, Debug, Serialize)]
pub struct Histogram {
    pub bounds: Vec<f64>,
    pub counts: Vec<u64>,
    pub sum: f64,
    pub count: u64,
}

impl Histogram {
    pub fn new(bounds: &[f64]) -> Self {
        Self {
            bounds: bounds.to_vec(),
            counts: vec![0; bounds.len()],
            sum: 0.0,
            count: 0,
        }
    }

    pub fn observe(&mut self, v: f64) {
        for (i, b) in self.bounds.iter().enumerate() {
            if v <= *b {
                self.counts[i] += 1;
            }
        }
        self.sum += v;
        self.count += 1;
    }
}

/// Weight accumulated per `le` bound, cumulative like Prometheus buckets.
#[derive(Clone, Debug, Serialize)]
pub struct Cumulative {
    pub bounds: Vec<f64>,
    pub values: Vec<f64>,
    pub total: f64,
}

impl Cumulative {
    pub fn new(bounds: &[f64]) -> Self {
        Self {
            bounds: bounds.to_vec(),
            values: vec![0.0; bounds.len()],
            total: 0.0,
        }
    }

    pub fn observe(&mut self, v: f64, weight: f64) {
        for (i, b) in self.bounds.iter().enumerate() {
            if v <= *b {
                self.values[i] += weight;
            }
        }
        self.total += weight;
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct PoolStat {
    pub index: usize,
    pub relative_stake: f64,
    pub relays_total: usize,
    pub relays_reachable: usize,
    pub reach: Reach,
    pub fraction: f64,
    pub fastest_rtt_ms: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ReachGroup {
    pub pools: u64,
    pub stake_ratio: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Census {
    pub snapshot_source: String,
    pub network_magic: u64,
    pub node_to_client_version: u64,
    pub snapshot_slot: u64,
    pub snapshot_hash: String,

    pub blp_total: u64,
    pub snapshot_stake_ratio: f64,

    pub relays_total: u64,
    pub endpoints_total: u64,
    pub endpoints_probed: u64,
    pub relays_reachable_v4: u64,
    pub relays_reachable_v6: u64,
    pub relays_failed: BTreeMap<&'static str, u64>,

    pub blp_by_reach: BTreeMap<&'static str, ReachGroup>,
    pub reachable_stake_ratio: f64,
    pub relay_weighted_stake_ratio: f64,
    pub reachability: Histogram,
    pub rtt_seconds: Histogram,
    pub relays_reachable_within: Cumulative,
    pub stake_reachable_within: Cumulative,

    pub tip_block_max: u64,
    pub tip_slot_max: u64,

    pub scan_duration_seconds: f64,
    pub timestamp_seconds: u64,

    #[serde(skip)]
    pub pools: Vec<PoolStat>,
}

pub const REACHABILITY_BOUNDS: [f64; 5] = [0.0, 0.25, 0.5, 0.75, 1.0];
pub const RTT_BOUNDS: [f64; 13] =
    [0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 15.0, 20.0, 30.0, 45.0, 60.0];
pub const WITHIN_BOUNDS: [f64; 9] = [1.0, 2.5, 5.0, 10.0, 15.0, 20.0, 30.0, 45.0, 60.0];

pub fn build(
    snapshot_source: &str,
    snap: &Snapshot,
    entries: &[Entry],
    endpoints: &[Endpoint],
    outcomes: &[Option<Outcome>],
    scan: Duration,
    timestamp_seconds: u64,
) -> Census {
    let mut entry_outcome: Vec<Option<&Outcome>> = vec![None; entries.len()];
    for (i, ep) in endpoints.iter().enumerate() {
        if let Some(o) = &outcomes[i] {
            for &e in &ep.entries {
                entry_outcome[e] = Some(o);
            }
        }
    }

    let mut relays_failed: BTreeMap<&'static str, u64> =
        Stage::ALL.iter().map(|s| (s.label(), 0)).collect();
    let mut relays_reachable_v4 = 0;
    let mut relays_reachable_v6 = 0;
    for o in entry_outcome.iter().flatten() {
        match o {
            Ok(r) => match r.family {
                Family::V4 => relays_reachable_v4 += 1,
                Family::V6 => relays_reachable_v6 += 1,
            },
            Err(f) => *relays_failed.entry(f.stage.label()).or_default() += 1,
        }
    }

    let mut per_pool_total = vec![0usize; snap.pools.len()];
    let mut per_pool_ok = vec![0usize; snap.pools.len()];
    let mut per_pool_fastest: Vec<Option<u64>> = vec![None; snap.pools.len()];
    let mut relays_reachable_within = Cumulative::new(&WITHIN_BOUNDS);
    for (i, e) in entries.iter().enumerate() {
        per_pool_total[e.pool] += 1;
        if let Some(Ok(r)) = entry_outcome[i] {
            per_pool_ok[e.pool] += 1;
            relays_reachable_within.observe(r.rtt_ms as f64 / 1000.0, 1.0);
            let fastest = per_pool_fastest[e.pool].get_or_insert(r.rtt_ms);
            *fastest = (*fastest).min(r.rtt_ms);
        }
    }

    let mut blp_by_reach: BTreeMap<&'static str, ReachGroup> =
        Reach::ALL.iter().map(|r| (r.label(), ReachGroup::default())).collect();
    let mut reachability = Histogram::new(&REACHABILITY_BOUNDS);
    let mut stake_reachable_within = Cumulative::new(&WITHIN_BOUNDS);
    let mut relay_weighted_stake_ratio = 0.0;
    let mut snapshot_stake_ratio = 0.0;
    let mut pools = Vec::with_capacity(snap.pools.len());

    for (index, p) in snap.pools.iter().enumerate() {
        let relays_total = per_pool_total[index];
        let relays_reachable = per_pool_ok[index];
        let fraction = if relays_total == 0 {
            0.0
        } else {
            relays_reachable as f64 / relays_total as f64
        };
        let reach = if relays_reachable == 0 {
            Reach::None
        } else if relays_reachable == relays_total {
            Reach::Full
        } else {
            Reach::Partial
        };

        let group = blp_by_reach.get_mut(reach.label()).unwrap();
        group.pools += 1;
        group.stake_ratio += p.relative_stake;
        relay_weighted_stake_ratio += p.relative_stake * fraction;
        snapshot_stake_ratio += p.relative_stake;
        reachability.observe(fraction);
        let fastest_rtt_ms = per_pool_fastest[index];
        if let Some(ms) = fastest_rtt_ms {
            stake_reachable_within.observe(ms as f64 / 1000.0, p.relative_stake);
        }

        pools.push(PoolStat {
            index,
            relative_stake: p.relative_stake,
            relays_total,
            relays_reachable,
            reach,
            fraction,
            fastest_rtt_ms,
        });
    }

    let reachable_stake_ratio =
        blp_by_reach["partial"].stake_ratio + blp_by_reach["full"].stake_ratio;

    let mut rtt_seconds = Histogram::new(&RTT_BOUNDS);
    let mut tip_block_max = 0;
    let mut tip_slot_max = 0;
    for r in outcomes.iter().flatten().filter_map(|o| o.as_ref().ok()) {
        rtt_seconds.observe(r.rtt_ms as f64 / 1000.0);
        tip_block_max = tip_block_max.max(r.tip.block);
        tip_slot_max = tip_slot_max.max(r.tip.slot);
    }

    Census {
        snapshot_source: snapshot_source.to_string(),
        network_magic: snap.network_magic,
        node_to_client_version: snap.node_to_client_version,
        snapshot_slot: snap.point.block_point_slot,
        snapshot_hash: snap.point.block_point_hash.clone(),

        blp_total: snap.pools.len() as u64,
        snapshot_stake_ratio,

        relays_total: entries.len() as u64,
        endpoints_total: endpoints.len() as u64,
        endpoints_probed: endpoints.iter().filter(|e| e.dns_error.is_none()).count() as u64,
        relays_reachable_v4,
        relays_reachable_v6,
        relays_failed,

        blp_by_reach,
        reachable_stake_ratio,
        relay_weighted_stake_ratio,
        reachability,
        rtt_seconds,
        relays_reachable_within,
        stake_reachable_within,

        tip_block_max,
        tip_slot_max,

        scan_duration_seconds: scan.as_secs_f64(),
        timestamp_seconds,

        pools,
    }
}
