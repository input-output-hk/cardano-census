use serde::Serialize;
use std::collections::BTreeMap;
use std::time::Duration;

use crate::probe::{Failed, Family, Outcome, Stage};
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

/// How many of one SRV name's top-priority targets answered.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SrvSet {
    pub targets: u64,
    pub reachable: u64,
}

/// Tips grouped by hash, with groups whose blocks lie within the fork
/// tolerance of each other merged. The group holding the most stake is main.
#[derive(Clone, Debug, Serialize)]
pub struct ChainGroup {
    pub main: bool,
    pub block_max: u64,
    pub block_min: u64,
    /// Hash of the highest tip in the group.
    pub hash: String,
    pub relays: u64,
    pub stake_ratio: f64,
}

/// Where one relay entry's tip sits relative to the highest tip seen.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct EntryTip {
    pub lag_blocks: u64,
    pub main: bool,
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
    pub relays_srv: u64,
    /// Per SRV name, its target count and how many answered.
    pub srv_sets: BTreeMap<String, SrvSet>,
    /// Distinct endpoints that are SRV targets, and how many answered.
    pub srv_endpoints_total: u64,
    pub srv_endpoints_reachable: u64,
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
    /// Distinct hashes reported at the highest block.
    pub tips_at_max_block: u64,
    pub fork_tolerance: u64,
    pub chains: Vec<ChainGroup>,
    pub relays_within_blocks_of_tip: Cumulative,
    pub stake_within_blocks_of_tip: Cumulative,

    pub scan_duration_seconds: f64,
    pub timestamp_seconds: u64,

    #[serde(skip)]
    pub pools: Vec<PoolStat>,
    #[serde(skip)]
    pub entry_tips: Vec<Option<EntryTip>>,
}

pub const REACHABILITY_BOUNDS: [f64; 5] = [0.0, 0.25, 0.5, 0.75, 1.0];
pub const RTT_BOUNDS: [f64; 13] =
    [0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 15.0, 20.0, 30.0, 45.0, 60.0];
pub const WITHIN_BOUNDS: [f64; 9] = [1.0, 2.5, 5.0, 10.0, 15.0, 20.0, 30.0, 45.0, 60.0];
pub const LAG_BOUNDS: [f64; 8] = [0.0, 1.0, 2.0, 5.0, 10.0, 50.0, 100.0, 1000.0];

/// Does `candidate` say more about an entry than `current`? A tip beats any
/// failure, and a faster tip beats a slower one.
fn better(current: Option<&Outcome>, candidate: &Outcome) -> bool {
    match (current, candidate) {
        (None, _) => true,
        (Some(Err(_)), Ok(_)) => true,
        (Some(Ok(a)), Ok(b)) => b.rtt_ms < a.rtt_ms,
        _ => false,
    }
}

/// One outcome per relay entry. An entry probed at several endpoints, as an
/// SRV record is, takes its best one; an SRV entry with no targets fails at
/// the `srv` stage.
pub fn best_outcomes(
    entries: &[Entry],
    endpoints: &[Endpoint],
    outcomes: &[Option<Outcome>],
    srv_errors: &[Option<String>],
) -> Vec<Option<Outcome>> {
    let mut best: Vec<Option<Outcome>> = vec![None; entries.len()];
    for (i, ep) in endpoints.iter().enumerate() {
        if let Some(o) = &outcomes[i] {
            for &e in &ep.entries {
                if better(best[e].as_ref(), o) {
                    best[e] = Some(o.clone());
                }
            }
        }
    }
    for (e, err) in srv_errors.iter().enumerate() {
        if let (Some(err), None) = (err, &best[e]) {
            best[e] = Some(Err(Failed {
                stage: Stage::Srv,
                error: err.clone(),
            }));
        }
    }
    best
}

#[allow(clippy::too_many_arguments)]
pub fn build(
    snapshot_source: &str,
    snap: &Snapshot,
    entries: &[Entry],
    endpoints: &[Endpoint],
    outcomes: &[Option<Outcome>],
    srv_errors: &[Option<String>],
    fork_tolerance: u64,
    scan: Duration,
    timestamp_seconds: u64,
) -> Census {
    let entry_outcome = best_outcomes(entries, endpoints, outcomes, srv_errors);

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
        if let Some(Ok(r)) = &entry_outcome[i] {
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

    // SRV set health, invisible to the pool weighting above since an entry
    // counts as reachable when any one target answers.
    let mut srv_sets: BTreeMap<String, SrvSet> = BTreeMap::new();
    let mut srv_endpoints_total = 0;
    let mut srv_endpoints_reachable = 0;
    for (i, ep) in endpoints.iter().enumerate() {
        let reached = matches!(outcomes[i], Some(Ok(_)));
        let mut is_srv_target = false;
        for &e in &ep.entries {
            if entries[e].is_srv() {
                is_srv_target = true;
                let set = srv_sets.entry(entries[e].address.clone()).or_default();
                set.targets += 1;
                set.reachable += reached as u64;
            }
        }
        if is_srv_target {
            srv_endpoints_total += 1;
            srv_endpoints_reachable += reached as u64;
        }
    }
    // Two pools listing the same SRV name share its endpoints and were counted
    // once per name above; names whose lookup failed still get a row.
    for e in entries.iter().filter(|e| e.is_srv()) {
        srv_sets.entry(e.address.clone()).or_default();
    }

    let mut rtt_seconds = Histogram::new(&RTT_BOUNDS);
    let mut tip_block_max = 0;
    let mut tip_slot_max = 0;
    for r in outcomes.iter().flatten().filter_map(|o| o.as_ref().ok()) {
        rtt_seconds.observe(r.rtt_ms as f64 / 1000.0);
        tip_block_max = tip_block_max.max(r.tip.block);
        tip_slot_max = tip_slot_max.max(r.tip.slot);
    }

    let (chains, entry_tips) = chain_groups(
        entries,
        &entry_outcome,
        &per_pool_ok,
        |pool| snap.pools[pool].relative_stake,
        tip_block_max,
        fork_tolerance,
    );
    let tips_at_max_block = {
        let mut hashes: Vec<&str> = entry_outcome
            .iter()
            .filter_map(|o| o.as_ref().and_then(|o| o.as_ref().ok()))
            .filter(|r| r.tip.block == tip_block_max)
            .map(|r| r.tip.hash.as_str())
            .collect();
        hashes.sort_unstable();
        hashes.dedup();
        hashes.len() as u64
    };
    let mut relays_within_blocks_of_tip = Cumulative::new(&LAG_BOUNDS);
    let mut stake_within_blocks_of_tip = Cumulative::new(&LAG_BOUNDS);
    let mut per_pool_least_lag: Vec<Option<u64>> = vec![None; snap.pools.len()];
    for (i, t) in entry_tips.iter().enumerate() {
        if let Some(t) = t {
            relays_within_blocks_of_tip.observe(t.lag_blocks as f64, 1.0);
            let least = per_pool_least_lag[entries[i].pool].get_or_insert(t.lag_blocks);
            *least = (*least).min(t.lag_blocks);
        }
    }
    for (pool, lag) in per_pool_least_lag.iter().enumerate() {
        if let Some(lag) = lag {
            stake_within_blocks_of_tip.observe(*lag as f64, snap.pools[pool].relative_stake);
        }
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
        relays_srv: entries.iter().filter(|e| e.is_srv()).count() as u64,
        srv_sets,
        srv_endpoints_total,
        srv_endpoints_reachable,
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
        tips_at_max_block,
        fork_tolerance,
        chains,
        relays_within_blocks_of_tip,
        stake_within_blocks_of_tip,

        scan_duration_seconds: scan.as_secs_f64(),
        timestamp_seconds,

        pools,
        entry_tips,
    }
}

/// Group reachable entries by tip hash, merge groups whose blocks lie within
/// `tolerance` of each other, and call the group with the most stake main.
/// A pool's stake is split evenly over its answering entries, as scope does,
/// so the groups' stake sums to the reachable stake.
fn chain_groups(
    entries: &[Entry],
    entry_outcome: &[Option<Outcome>],
    per_pool_ok: &[usize],
    pool_stake: impl Fn(usize) -> f64,
    tip_block_max: u64,
    tolerance: u64,
) -> (Vec<ChainGroup>, Vec<Option<EntryTip>>) {
    // Distinct hashes, each with its block and the entries reporting it.
    let mut by_hash: BTreeMap<&str, (u64, Vec<usize>)> = BTreeMap::new();
    for (i, o) in entry_outcome.iter().enumerate() {
        if let Some(Ok(r)) = o {
            let slot = by_hash.entry(r.tip.hash.as_str()).or_insert((r.tip.block, Vec::new()));
            slot.0 = slot.0.max(r.tip.block);
            slot.1.push(i);
        }
    }
    let hashes: Vec<(&str, u64, Vec<usize>)> =
        by_hash.into_iter().map(|(h, (b, es))| (h, b, es)).collect();

    // Union-find over hashes by block distance.
    let mut parent: Vec<usize> = (0..hashes.len()).collect();
    fn root(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    for a in 0..hashes.len() {
        for b in (a + 1)..hashes.len() {
            if hashes[a].1.abs_diff(hashes[b].1) <= tolerance {
                let (ra, rb) = (root(&mut parent, a), root(&mut parent, b));
                if ra != rb {
                    parent[rb] = ra;
                }
            }
        }
    }

    let mut groups: BTreeMap<usize, ChainGroup> = BTreeMap::new();
    let mut entry_group: Vec<Option<usize>> = vec![None; entries.len()];
    for (h, (hash, block, es)) in hashes.iter().enumerate() {
        let r = root(&mut parent, h);
        let g = groups.entry(r).or_insert(ChainGroup {
            main: false,
            block_max: *block,
            block_min: *block,
            hash: hash.to_string(),
            relays: 0,
            stake_ratio: 0.0,
        });
        if *block > g.block_max {
            g.block_max = *block;
            g.hash = hash.to_string();
        }
        g.block_min = g.block_min.min(*block);
        for &e in es {
            let pool = entries[e].pool;
            g.relays += 1;
            g.stake_ratio += pool_stake(pool) / per_pool_ok[pool].max(1) as f64;
            entry_group[e] = Some(r);
        }
    }

    let main_root = groups
        .iter()
        .max_by(|a, b| a.1.stake_ratio.partial_cmp(&b.1.stake_ratio).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(r, _)| *r);
    if let Some(r) = main_root {
        groups.get_mut(&r).unwrap().main = true;
    }

    let entry_tips = entry_outcome
        .iter()
        .enumerate()
        .map(|(i, o)| match (o, entry_group[i]) {
            (Some(Ok(r)), Some(g)) => Some(EntryTip {
                lag_blocks: tip_block_max.saturating_sub(r.tip.block),
                main: Some(g) == main_root,
            }),
            _ => None,
        })
        .collect();

    let mut chains: Vec<ChainGroup> = groups.into_values().collect();
    chains.sort_by(|a, b| {
        b.main
            .cmp(&a.main)
            .then(b.stake_ratio.partial_cmp(&a.stake_ratio).unwrap_or(std::cmp::Ordering::Equal))
    });
    (chains, entry_tips)
}
