use serde::Serialize;
use std::collections::BTreeMap;
use std::time::Duration;

use crate::asn::{special_use, AsInfo, AsnDb};
use crate::churn::{self, Change, Churn, Previous};
use crate::net::format_host_port;
use crate::pools::{operator_domain, PoolIndex, PoolMeta};
use crate::probe::{Failed, Family, Outcome, Stage};
use crate::resolve::{Endpoint, Entry};
use crate::reversed::Shadow;
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
    /// Like `fraction`, but an SRV entry counts by the weight of its answering
    /// targets: the chance a node choosing that record reaches a working one.
    pub weighted_fraction: f64,
    pub fastest_rtt_ms: Option<u64>,
    /// From the pool index, when one was given and a relay matched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<PoolMeta>,
    /// Relays the index matched only through their octet reversal.
    pub relays_reversed: usize,
    /// Why relays failed, distinct, most common first; empty when all answered.
    pub reasons: Vec<&'static str>,
    /// Distinct socket addresses the pool's relays resolved to.
    pub endpoints: Vec<String>,
    /// The pool's relay entries as written in the snapshot.
    #[serde(skip)]
    pub relays: Vec<String>,
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
    /// Weight of the answering targets over the weight of all of them.
    pub reach_probability: f64,
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

/// Relay entries hosted in one autonomous system, with each entry carrying an
/// equal share of its pool's stake.
#[derive(Clone, Debug, Default, Serialize)]
pub struct AsnGroup {
    /// The AS number, or `other`, `unrouted` or `unresolved` for the buckets.
    pub asn: String,
    pub name: String,
    pub relays: u64,
    pub relays_reachable: u64,
    pub stake_ratio: f64,
    pub stake_reachable_ratio: f64,
}

/// Answering relay entries that negotiated one node-to-node protocol version,
/// with each entry carrying an equal share of its pool's stake.
#[derive(Clone, Debug, Default, Serialize)]
pub struct VersionGroup {
    pub relays: u64,
    pub stake_ratio: f64,
}

/// One line of the outreach list. Pools that share an identity, the same
/// pool id from the index or, without one, the same relay set, are one
/// operator and are merged, so every row is a distinct series.
#[derive(Clone, Debug, Serialize)]
pub struct OutreachRow {
    pub pool_id: String,
    pub ticker: String,
    pub name: String,
    pub reach: Reach,
    pub pools: u64,
    pub relays_total: u64,
    pub relays_reachable: u64,
    /// Relays named only through their octet reversal, see reversed.rs.
    pub relays_reversed: u64,
    /// Distinct socket addresses behind all the operator's relays.
    pub endpoints: u64,
    /// Why relays failed, distinct and most common first, comma separated.
    pub reasons: String,
    pub stake_ratio: f64,
    pub relays: Vec<String>,
}

/// Where one relay entry's tip sits relative to the highest tip seen.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct EntryTip {
    pub lag_blocks: u64,
    pub main: bool,
}

/// IPv4 literal entries probed as given and at their octet reversal.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Ipv4Reversal {
    pub relays: u64,
    pub reachable_given: u64,
    pub reachable_reversed: u64,
    /// Pools with no relay answering as given and one answering reversed.
    pub pools_reversed_only: u64,
    pub stake_reversed_only_ratio: f64,
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
    /// Endpoints known only from earlier runs' lookups of the same name.
    pub endpoints_remembered: u64,
    pub relays_reachable_v4: u64,
    pub relays_reachable_v6: u64,
    pub relays_failed: BTreeMap<&'static str, u64>,
    /// Connect-stage failures by what the socket reported.
    pub relays_failed_connect: BTreeMap<&'static str, u64>,
    /// Answering entries by negotiated node-to-node version.
    pub n2n_versions: BTreeMap<String, VersionGroup>,
    pub ipv4: Ipv4Reversal,
    /// Changes since the previous report, when one was readable.
    pub churn: Option<Churn>,

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

    /// Pools the index named, 0 when no index was given.
    pub pools_indexed: u64,
    pub top_pools_limit: u64,

    /// Ranges in the AS database, 0 when none was loaded.
    pub asn_db_ranges: u64,
    pub asn_min_relays: u64,
    /// Every AS seen, most relays first.
    pub asn_table: Vec<AsnGroup>,

    pub scan_duration_seconds: f64,
    pub timestamp_seconds: u64,

    #[serde(skip)]
    pub pools: Vec<PoolStat>,
    /// The operators with the most stake among pools not fully reachable,
    /// for the outreach series; at most `top_pools_limit` of them.
    #[serde(skip)]
    pub top_pools: Vec<OutreachRow>,
    /// Operators running the most pools per distinct relay endpoint, at most
    /// `top_pools_limit` of them, reachable or not.
    #[serde(skip)]
    pub thin_operators: Vec<OutreachRow>,
    #[serde(skip)]
    pub entry_tips: Vec<Option<EntryTip>>,
    /// Per entry, how its state changed since the previous report.
    #[serde(skip)]
    pub entry_change: Vec<Option<Change>>,
    /// The AS table with small ASes folded into `other`, for the metrics.
    #[serde(skip)]
    pub asn_metrics: Vec<AsnGroup>,
    #[serde(skip)]
    pub entry_asn: Vec<Option<AsInfo>>,
    /// Per entry, the chance a node using it reaches a working relay: 0 or 1
    /// for a plain relay, weight-based for an SRV record.
    #[serde(skip)]
    pub entry_probability: Vec<f64>,
}

pub const REACHABILITY_BOUNDS: [f64; 5] = [0.0, 0.25, 0.5, 0.75, 1.0];
pub const RTT_BOUNDS: [f64; 13] =
    [0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 15.0, 20.0, 30.0, 45.0, 60.0];
pub const WITHIN_BOUNDS: [f64; 9] = [1.0, 2.5, 5.0, 10.0, 15.0, 20.0, 30.0, 45.0, 60.0];
pub const LAG_BOUNDS: [f64; 8] = [0.0, 1.0, 2.0, 5.0, 10.0, 50.0, 100.0, 1000.0];

pub const CONNECT_REASONS: [&str; 4] = ["timeout", "refused", "unreachable", "other"];

/// What the socket said when the connect stage failed. Refused means a live
/// host with nothing on the port, unreachable a route that does not exist.
pub fn connect_reason(error: &str) -> &'static str {
    let e = error.to_ascii_lowercase();
    if e.contains("timeout") || e.contains("timed out") {
        "timeout"
    } else if e.contains("refused") {
        "refused"
    } else if e.contains("unreachable") || e.contains("no route") {
        "unreachable"
    } else {
        "other"
    }
}

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
    shadow: &Shadow,
    previous: Option<&Previous>,
    fork_tolerance: u64,
    asn_db: Option<&AsnDb>,
    asn_min_relays: usize,
    pool_index: Option<&PoolIndex>,
    top_pools_limit: usize,
    scan: Duration,
    timestamp_seconds: u64,
) -> Census {
    let entry_outcome = best_outcomes(entries, endpoints, outcomes, srv_errors);
    let entry_probability = reach_probabilities(entries, endpoints, outcomes, &entry_outcome);

    let mut relays_failed: BTreeMap<&'static str, u64> =
        Stage::ALL.iter().map(|s| (s.label(), 0)).collect();
    let mut relays_failed_connect: BTreeMap<&'static str, u64> =
        CONNECT_REASONS.iter().map(|r| (*r, 0)).collect();
    let mut relays_reachable_v4 = 0;
    let mut relays_reachable_v6 = 0;
    for o in entry_outcome.iter().flatten() {
        match o {
            Ok(r) => match r.family {
                Family::V4 => relays_reachable_v4 += 1,
                Family::V6 => relays_reachable_v6 += 1,
            },
            Err(f) => {
                *relays_failed.entry(f.stage.label()).or_default() += 1;
                if f.stage == Stage::Connect {
                    *relays_failed_connect.entry(connect_reason(&f.error)).or_default() += 1;
                }
            }
        }
    }

    let mut per_pool_total = vec![0usize; snap.pools.len()];
    let mut per_pool_ok = vec![0usize; snap.pools.len()];
    let mut per_pool_probability = vec![0f64; snap.pools.len()];
    let mut per_pool_fastest: Vec<Option<u64>> = vec![None; snap.pools.len()];
    let mut relays_reachable_within = Cumulative::new(&WITHIN_BOUNDS);
    for (i, e) in entries.iter().enumerate() {
        per_pool_total[e.pool] += 1;
        per_pool_probability[e.pool] += entry_probability[i];
        if let Some(Ok(r)) = &entry_outcome[i] {
            per_pool_ok[e.pool] += 1;
            relays_reachable_within.observe(r.rtt_ms as f64 / 1000.0, 1.0);
            let fastest = per_pool_fastest[e.pool].get_or_insert(r.rtt_ms);
            *fastest = (*fastest).min(r.rtt_ms);
        }
    }
    let ipv4 = ipv4_reversal(entries, &entry_outcome, shadow, &per_pool_ok, |pool| {
        snap.pools[pool].relative_stake
    });
    let mut entry_endpoints: Vec<Vec<usize>> = vec![Vec::new(); entries.len()];
    for (x, ep) in endpoints.iter().enumerate() {
        for &e in &ep.entries {
            entry_endpoints[e].push(x);
        }
    }
    let entry_reason = entry_reasons(entries, endpoints, &entry_endpoints, &entry_outcome, shadow, asn_db);

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
        let weighted_fraction = if relays_total == 0 {
            0.0
        } else {
            per_pool_probability[index] / relays_total as f64
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
        relay_weighted_stake_ratio += p.relative_stake * weighted_fraction;
        snapshot_stake_ratio += p.relative_stake;
        reachability.observe(fraction);
        let fastest_rtt_ms = per_pool_fastest[index];
        if let Some(ms) = fastest_rtt_ms {
            stake_reachable_within.observe(ms as f64 / 1000.0, p.relative_stake);
        }

        let own: Vec<usize> = (0..entries.len()).filter(|&i| entries[i].pool == index).collect();
        let identity = pool_index
            .and_then(|idx| idx.identify(own.iter().map(|&i| (entries[i].address.as_str(), entries[i].port))));
        let relays_reversed = identity.as_ref().map(|i| i.reversed).unwrap_or(0);
        let meta = identity.map(|i| i.meta);
        let relays = own
            .iter()
            .map(|&i| match entries[i].port {
                Some(port) => format_host_port(&entries[i].address, port),
                None => entries[i].address.clone(),
            })
            .collect();
        let mut pool_endpoints: Vec<String> = Vec::new();
        for &i in &own {
            for &x in &entry_endpoints[i] {
                if !pool_endpoints.contains(&endpoints[x].key) {
                    pool_endpoints.push(endpoints[x].key.clone());
                }
            }
        }
        let reasons = rank_reasons(own.iter().filter_map(|&i| entry_reason[i]).map(|r| (r, 1)));

        pools.push(PoolStat {
            index,
            relative_stake: p.relative_stake,
            relays_total,
            relays_reachable,
            reach,
            fraction,
            weighted_fraction,
            fastest_rtt_ms,
            meta,
            relays_reversed,
            reasons,
            endpoints: pool_endpoints,
            relays,
        });
    }

    let pools_indexed = pools.iter().filter(|p| p.meta.is_some()).count() as u64;
    let operators = operators(&pools);
    let top_pools = outreach(&operators, top_pools_limit);
    let thin_operators = thin(&operators, top_pools_limit);

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
    for (i, e) in entries.iter().enumerate().filter(|(_, e)| e.is_srv()) {
        srv_sets.entry(e.address.clone()).or_default().reach_probability = entry_probability[i];
    }

    let mut n2n_versions: BTreeMap<String, VersionGroup> = BTreeMap::new();
    for (i, e) in entries.iter().enumerate() {
        if let Some(Ok(r)) = &entry_outcome[i] {
            let g = n2n_versions.entry(r.n2n_version.clone()).or_default();
            g.relays += 1;
            g.stake_ratio += snap.pools[e.pool].relative_stake / per_pool_total[e.pool].max(1) as f64;
        }
    }

    let (entry_asn, asn_table, asn_metrics) = asn_groups(
        entries,
        endpoints,
        &entry_outcome,
        &per_pool_total,
        |pool| snap.pools[pool].relative_stake,
        asn_db,
        asn_min_relays,
    );

    let (churn, entry_change) = match previous {
        Some(prev) => {
            let (c, changes) = churn::diff(prev, entries, &entry_outcome, &entry_asn, |i| {
                let pool = entries[i].pool;
                snap.pools[pool].relative_stake / per_pool_total[pool].max(1) as f64
            });
            (Some(c), changes)
        }
        None => (None, vec![None; entries.len()]),
    };

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
        // Resolved and actually dialled: not a DNS failure, not special-use
        // space, not an IPv6 address from a host without an IPv6 route.
        endpoints_probed: outcomes
            .iter()
            .filter(|o| !matches!(o, Some(Err(f)) if matches!(f.stage, Stage::Dns | Stage::Address) || f.error.ends_with("not probed")))
            .count() as u64,
        endpoints_remembered: endpoints.iter().filter(|e| e.remembered).count() as u64,
        relays_reachable_v4,
        relays_reachable_v6,
        relays_failed,
        relays_failed_connect,
        n2n_versions,
        ipv4,
        churn,

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

        pools_indexed,
        top_pools_limit: top_pools_limit as u64,

        asn_db_ranges: asn_db.map(|db| db.len() as u64).unwrap_or(0),
        asn_min_relays: asn_min_relays as u64,
        asn_table,

        scan_duration_seconds: scan.as_secs_f64(),
        timestamp_seconds,

        pools,
        top_pools,
        thin_operators,
        entry_tips,
        entry_change,
        asn_metrics,
        entry_asn,
        entry_probability,
    }
}

/// Count IPv4 literal entries answering as given and reversed, and the pools
/// that only the reversal reaches.
fn ipv4_reversal(
    entries: &[Entry],
    entry_outcome: &[Option<Outcome>],
    shadow: &Shadow,
    per_pool_ok: &[usize],
    stake_of: impl Fn(usize) -> f64,
) -> Ipv4Reversal {
    let mut r = Ipv4Reversal::default();
    let mut reversed_only_pools: Vec<usize> = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        let Some(reversed) = shadow.reachable[i] else { continue };
        r.relays += 1;
        r.reachable_given += matches!(entry_outcome[i], Some(Ok(_))) as u64;
        r.reachable_reversed += reversed as u64;
        if reversed && per_pool_ok[e.pool] == 0 && !reversed_only_pools.contains(&e.pool) {
            reversed_only_pools.push(e.pool);
        }
    }
    r.pools_reversed_only = reversed_only_pools.len() as u64;
    r.stake_reversed_only_ratio = reversed_only_pools.iter().map(|&p| stake_of(p)).sum::<f64>() + 0.0;
    r
}

/// Why one relay entry returned no tip. The reversed spelling answering
/// comes first because it explains the failure on its own; then the kind of
/// address the entry resolved to; then what the socket or protocol said.
fn entry_reasons(
    entries: &[Entry],
    endpoints: &[Endpoint],
    entry_endpoints: &[Vec<usize>],
    entry_outcome: &[Option<Outcome>],
    shadow: &Shadow,
    asn_db: Option<&AsnDb>,
) -> Vec<Option<&'static str>> {
    (0..entries.len())
        .map(|i| {
            let f = match &entry_outcome[i] {
                Some(Err(f)) => f,
                _ => return None,
            };
            if shadow.reachable[i] == Some(true) {
                return Some("reversed ipv4");
            }
            let addr = entry_endpoints[i].iter().find_map(|&x| endpoints[x].addrs.first().copied());
            Some(match f.stage {
                Stage::Srv => "srv",
                Stage::Dns => "dns",
                Stage::Handshake => "handshake",
                Stage::Chainsync => "chainsync",
                Stage::Address | Stage::Connect => match addr {
                    Some(ip) => match special_use(ip) {
                        Some("private") => "private address",
                        Some(_) => "reserved address",
                        None if asn_db.is_some_and(|db| db.lookup(ip).is_none()) => "unrouted address",
                        None => connect_reason(&f.error),
                    },
                    None => connect_reason(&f.error),
                },
            })
        })
        .collect()
}

/// Distinct reasons, most common first, ties alphabetical.
fn rank_reasons(counts: impl IntoIterator<Item = (&'static str, u64)>) -> Vec<&'static str> {
    let mut tally: BTreeMap<&'static str, u64> = BTreeMap::new();
    for (r, n) in counts {
        *tally.entry(r).or_default() += n;
    }
    let mut v: Vec<(&'static str, u64)> = tally.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    v.into_iter().map(|(r, _)| r).collect()
}

/// One row per operator. Pools are merged by the identity the index gave
/// them; pools whose registration carries no ticker or name are merged by
/// the domain their relays share, which is the only handle there is on them.
fn operators(pools: &[PoolStat]) -> Vec<OutreachRow> {
    /// Row under construction with what the merge still needs to know.
    type Acc = (OutreachRow, Vec<String>, BTreeMap<&'static str, u64>, usize, usize);
    let mut rows: BTreeMap<String, Acc> = BTreeMap::new();
    for p in pools {
        let anonymous = p.meta.as_ref().is_none_or(|m| m.ticker.is_none() && m.name.is_none());
        let domain = if anonymous { operator_domain(&p.relays) } else { None };
        let key = match (&domain, &p.meta) {
            (Some(d), _) => format!("domain:{d}"),
            (None, Some(m)) => m.pool_id.clone(),
            (None, None) => p.relays.join(", "),
        };
        let slot = rows.entry(key).or_insert_with(|| {
            (
                OutreachRow {
                    pool_id: match &p.meta {
                        Some(m) => m.pool_id.clone(),
                        None => p.relays.first().cloned().unwrap_or_default(),
                    },
                    ticker: p.meta.as_ref().and_then(|m| m.ticker.clone()).unwrap_or_default(),
                    name: p
                        .meta
                        .as_ref()
                        .and_then(|m| m.name.clone())
                        .or(domain.clone())
                        .unwrap_or_default(),
                    reach: Reach::Full,
                    pools: 0,
                    relays_total: 0,
                    relays_reachable: 0,
                    relays_reversed: 0,
                    endpoints: 0,
                    reasons: String::new(),
                    stake_ratio: 0.0,
                    relays: Vec::new(),
                },
                Vec::new(),
                BTreeMap::new(),
                0,
                0,
            )
        });
        let (row, eps, reasons, full, none) = slot;
        row.pools += 1;
        row.relays_total += p.relays_total as u64;
        row.relays_reachable += p.relays_reachable as u64;
        row.relays_reversed += p.relays_reversed as u64;
        row.stake_ratio += p.relative_stake;
        match p.reach {
            Reach::Full => *full += 1,
            Reach::None => *none += 1,
            Reach::Partial => {}
        }
        for r in &p.relays {
            if !row.relays.contains(r) {
                row.relays.push(r.clone());
            }
        }
        for e in &p.endpoints {
            if !eps.contains(e) {
                eps.push(e.clone());
            }
        }
        for (i, r) in p.reasons.iter().enumerate() {
            // The pool's ranking is all we kept; weight it so the first stays first.
            *reasons.entry(r).or_default() += (p.reasons.len() - i) as u64;
        }
    }
    rows.into_values()
        .map(|(mut row, eps, reasons, full, none)| {
            row.endpoints = distinct_endpoints(&eps);
            row.reasons = rank_reasons(reasons).join(", ");
            row.reach = if full as u64 == row.pools {
                Reach::Full
            } else if none as u64 == row.pools {
                Reach::None
            } else {
                Reach::Partial
            };
            row
        })
        .collect()
}

/// How many relay hosts stand behind these endpoint keys. A dual-stacked host
/// is one relay, so the larger address family counts and the smaller is
/// assumed to be the same hosts; a name that never resolved counts as one.
fn distinct_endpoints(keys: &[String]) -> u64 {
    let (mut v4, mut v6, mut names) = (0u64, 0u64, 0u64);
    for k in keys {
        match k.parse::<std::net::SocketAddr>() {
            Ok(s) if s.is_ipv4() => v4 += 1,
            Ok(_) => v6 += 1,
            Err(_) => names += 1,
        }
    }
    v4.max(v6) + names
}

/// Operators not fully reachable, largest stake first, at most `limit`.
fn outreach(operators: &[OutreachRow], limit: usize) -> Vec<OutreachRow> {
    let mut rows: Vec<OutreachRow> = operators.iter().filter(|r| r.reach != Reach::Full).cloned().collect();
    rows.sort_by(|a, b| b.stake_ratio.partial_cmp(&a.stake_ratio).unwrap_or(std::cmp::Ordering::Equal));
    rows.truncate(limit);
    rows
}

/// Operators with the most pools per distinct relay endpoint, at most
/// `limit`. Only operators with at least as many pools as endpoints qualify,
/// so a single pool behind one relay is listed, one behind two is not.
fn thin(operators: &[OutreachRow], limit: usize) -> Vec<OutreachRow> {
    let density = |r: &OutreachRow| r.pools as f64 / r.endpoints.max(1) as f64;
    let mut rows: Vec<OutreachRow> = operators.iter().filter(|r| r.pools >= r.endpoints.max(1)).cloned().collect();
    rows.sort_by(|a, b| {
        density(b)
            .partial_cmp(&density(a))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.stake_ratio.partial_cmp(&a.stake_ratio).unwrap_or(std::cmp::Ordering::Equal))
    });
    rows.truncate(limit);
    rows
}

/// The chance a node using each entry reaches a working relay. A plain relay
/// is 1 or 0. An SRV record is the weight of its answering targets over the
/// weight of all its targets, as a node draws one target by weight; when
/// every weight is 0 the draw is uniform, so the count ratio is used.
fn reach_probabilities(
    entries: &[Entry],
    endpoints: &[Endpoint],
    outcomes: &[Option<Outcome>],
    entry_outcome: &[Option<Outcome>],
) -> Vec<f64> {
    let mut weight_total = vec![0u64; entries.len()];
    let mut weight_reached = vec![0u64; entries.len()];
    let mut targets = vec![0u64; entries.len()];
    let mut targets_reached = vec![0u64; entries.len()];
    for (i, ep) in endpoints.iter().enumerate() {
        let reached = matches!(outcomes[i], Some(Ok(_)));
        for (&e, w) in ep.entries.iter().zip(&ep.weights) {
            let w = u64::from(w.unwrap_or(0));
            weight_total[e] += w;
            targets[e] += 1;
            if reached {
                weight_reached[e] += w;
                targets_reached[e] += 1;
            }
        }
    }
    entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            if !e.is_srv() {
                return if matches!(entry_outcome[i], Some(Ok(_))) { 1.0 } else { 0.0 };
            }
            if weight_total[i] > 0 {
                weight_reached[i] as f64 / weight_total[i] as f64
            } else if targets[i] > 0 {
                targets_reached[i] as f64 / targets[i] as f64
            } else {
                0.0
            }
        })
        .collect()
}

/// Place every entry in an AS by the address that answered, or the first
/// address it resolved to, and total relays and stake per AS. The metrics
/// view folds ASes hosting fewer than `min_relays` entries into `other`;
/// entries with no address land in `unresolved`, those whose address no AS
/// announces in `unrouted`.
#[allow(clippy::type_complexity)]
fn asn_groups(
    entries: &[Entry],
    endpoints: &[Endpoint],
    entry_outcome: &[Option<Outcome>],
    per_pool_total: &[usize],
    pool_stake: impl Fn(usize) -> f64,
    asn_db: Option<&AsnDb>,
    min_relays: usize,
) -> (Vec<Option<AsInfo>>, Vec<AsnGroup>, Vec<AsnGroup>) {
    let Some(db) = asn_db else {
        return (vec![None; entries.len()], Vec::new(), Vec::new());
    };

    let mut entry_addr: Vec<Option<std::net::IpAddr>> = vec![None; entries.len()];
    for ep in endpoints {
        for &e in &ep.entries {
            if entry_addr[e].is_none() {
                entry_addr[e] = ep.addrs.first().copied();
            }
        }
    }
    let entry_ip: Vec<Option<std::net::IpAddr>> = (0..entries.len())
        .map(|i| match &entry_outcome[i] {
            Some(Ok(r)) => Some(r.peer.ip()),
            _ => entry_addr[i],
        })
        .collect();
    let entry_asn: Vec<Option<AsInfo>> = entry_ip
        .iter()
        .map(|ip| ip.and_then(|ip| db.lookup(ip)).cloned())
        .collect();

    // Keyed by AS number, or by a bucket name for entries without one:
    // `unresolved` never got an address, `private` and `reserved` have one in
    // special-use space, `unrouted` has a public one no AS announces.
    let mut by_asn: BTreeMap<String, AsnGroup> = BTreeMap::new();
    for (i, e) in entries.iter().enumerate() {
        let share = pool_stake(e.pool) / per_pool_total[e.pool].max(1) as f64;
        let reached = matches!(entry_outcome[i], Some(Ok(_)));
        let bucket = |b: &str| (b.to_string(), b.to_string());
        let (key, name) = match (&entry_asn[i], entry_ip[i]) {
            (Some(a), _) => (a.asn.to_string(), a.name.clone()),
            (None, Some(ip)) => bucket(special_use(ip).unwrap_or("unrouted")),
            (None, None) => bucket("unresolved"),
        };
        let g = by_asn
            .entry(key.clone())
            .or_insert_with(|| AsnGroup { asn: key, name, ..Default::default() });
        g.relays += 1;
        g.stake_ratio += share;
        if reached {
            g.relays_reachable += 1;
            g.stake_reachable_ratio += share;
        }
    }

    let mut table: Vec<AsnGroup> = by_asn.into_values().collect();
    table.sort_by(|a, b| b.relays.cmp(&a.relays).then(a.asn.cmp(&b.asn)));

    let mut metrics: Vec<AsnGroup> = Vec::new();
    let mut other = AsnGroup { asn: "other".into(), name: "other".into(), ..Default::default() };
    for g in &table {
        if matches!(g.asn.as_str(), "private" | "reserved" | "unrouted" | "unresolved") || g.relays as usize >= min_relays {
            metrics.push(g.clone());
        } else {
            other.relays += g.relays;
            other.relays_reachable += g.relays_reachable;
            other.stake_ratio += g.stake_ratio;
            other.stake_reachable_ratio += g.stake_reachable_ratio;
        }
    }
    if other.relays > 0 {
        metrics.push(other);
    }
    (entry_asn, table, metrics)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{Family, Reached, Tip};

    fn reached() -> Outcome {
        Ok(Reached {
            peer: "192.0.2.1:3001".parse().unwrap(),
            family: Family::V4,
            n2n_version: "14".into(),
            tip: Tip { slot: 1, hash: "aa".into(), block: 1 },
            rtt_ms: 10,
        })
    }

    fn failed() -> Outcome {
        Err(Failed { stage: Stage::Connect, error: "timeout".into() })
    }

    fn endpoint(key: &str, entries: &[(usize, Option<u16>)]) -> Endpoint {
        Endpoint {
            key: key.into(),
            entries: entries.iter().map(|e| e.0).collect(),
            weights: entries.iter().map(|e| e.1).collect(),
            addrs: Vec::new(),
            dns_error: None,
            remembered: false,
            last_seen: None,
        }
    }

    #[test]
    fn srv_probability_follows_weights_then_counts() {
        let entries = vec![
            Entry { pool: 0, address: "srv.example".into(), port: None },
            Entry { pool: 0, address: "zero.example".into(), port: None },
            Entry { pool: 0, address: "plain.example".into(), port: Some(3001) },
        ];
        let endpoints = vec![
            endpoint("a:6000", &[(0, Some(30)), (1, Some(0))]),
            endpoint("b:6000", &[(0, Some(10)), (1, Some(0))]),
            endpoint("c:6000", &[(1, Some(0))]),
            endpoint("plain.example:3001", &[(2, None)]),
        ];
        let outcomes = vec![Some(reached()), Some(failed()), Some(failed()), Some(failed())];
        let best = best_outcomes(&entries, &endpoints, &outcomes, &[None, None, None]);
        let p = reach_probabilities(&entries, &endpoints, &outcomes, &best);
        assert!((p[0] - 0.75).abs() < 1e-12, "30 of 40 weight answered");
        assert!((p[1] - 1.0 / 3.0).abs() < 1e-12, "all weights zero falls back to 1 of 3 targets");
        assert_eq!(p[2], 0.0, "a plain relay that failed");
    }

    #[test]
    fn ipv4_reversal_counts_entries_and_pools_only_the_reversal_reaches() {
        let entries = vec![
            Entry { pool: 0, address: "20.61.229.103".into(), port: Some(3001) },
            Entry { pool: 0, address: "relay.example".into(), port: Some(3001) },
            Entry { pool: 1, address: "170.23.181.50".into(), port: Some(6001) },
            Entry { pool: 2, address: "198.51.100.7".into(), port: Some(3001) },
            Entry { pool: 2, address: "198.51.100.8".into(), port: Some(3001) },
        ];
        let outcomes = vec![Some(failed()), Some(failed()), Some(reached()), Some(failed()), Some(failed())];
        let shadow = Shadow {
            address: vec![Some("103.229.61.20:3001".into()), None, Some("50.181.23.170:6001".into()), Some("7.100.51.198:3001".into()), Some("8.100.51.198:3001".into())],
            reachable: vec![Some(true), None, Some(false), Some(true), Some(true)],
        };
        let r = ipv4_reversal(&entries, &outcomes, &shadow, &[0, 1, 0], |p| [0.1, 0.2, 0.3][p]);
        assert_eq!(
            r,
            Ipv4Reversal {
                relays: 4,
                reachable_given: 1,
                reachable_reversed: 3,
                pools_reversed_only: 2,
                stake_reversed_only_ratio: 0.4,
            }
        );
        let none = ipv4_reversal(&entries, &outcomes, &Shadow::none(entries.len()), &[0, 1, 0], |_| 1.0);
        assert_eq!(none, Ipv4Reversal::default());
    }

    #[test]
    fn distinct_endpoints_counts_hosts_not_addresses() {
        let s = |v: &[&str]| v.iter().map(|k| k.to_string()).collect::<Vec<_>>();
        assert_eq!(distinct_endpoints(&s(&["192.0.2.1:3001", "192.0.2.2:3001", "192.0.2.3:3001"])), 3);
        assert_eq!(distinct_endpoints(&s(&["192.0.2.1:3001", "[2001:db8::1]:3001"])), 1, "dual stack is one host");
        assert_eq!(distinct_endpoints(&s(&["[2001:db8::1]:3001", "[2001:db8::2]:3001", "192.0.2.1:3001"])), 2);
        assert_eq!(distinct_endpoints(&s(&["dead.example:3001", "192.0.2.1:3001"])), 2, "an unresolved name is one relay");
        assert_eq!(distinct_endpoints(&[]), 0);
    }

    #[test]
    fn connect_reasons_come_from_the_socket_error() {
        assert_eq!(connect_reason("timeout"), "timeout");
        assert_eq!(connect_reason("Connect error to 1.2.3.4:3001: Connection refused (os error 111)"), "refused");
        assert_eq!(connect_reason("Connect error to 10.0.0.1:3001: Network unreachable (os error 101)"), "unreachable");
        assert_eq!(connect_reason("Connect error to 1.2.3.4:3001: Host is unreachable (os error 113)"), "unreachable");
        assert_eq!(connect_reason("Connect error to 1.2.3.4:3001: No route to host (os error 113)"), "unreachable");
        assert_eq!(connect_reason("Invalid address: x"), "other");
    }

    fn pool_stat(index: usize, stake: f64, meta: Option<PoolMeta>, relays: &[&str]) -> PoolStat {
        PoolStat {
            index,
            relative_stake: stake,
            relays_total: relays.len(),
            relays_reachable: 0,
            reach: Reach::None,
            fraction: 0.0,
            weighted_fraction: 0.0,
            fastest_rtt_ms: None,
            meta,
            relays_reversed: 0,
            reasons: vec!["timeout"],
            endpoints: relays.iter().map(|r| r.to_string()).collect(),
            relays: relays.iter().map(|r| r.to_string()).collect(),
        }
    }

    #[test]
    fn anonymous_pools_merge_by_relay_domain() {
        let bare = |id: &str| Some(PoolMeta { pool_id: id.into(), ticker: None, name: None });
        let named = Some(PoolMeta { pool_id: "pool1named".into(), ticker: Some("NMD".into()), name: None });
        let pools = vec![
            pool_stat(0, 0.03, bare("pool1a"), &["112.cardano.staked.cloud:3001"]),
            pool_stat(1, 0.03, bare("pool1b"), &["113.cardano.staked.cloud:3001"]),
            pool_stat(2, 0.02, named, &["r.staked.cloud:3001"]),
            pool_stat(3, 0.01, bare("pool1c"), &["203.0.113.9:3001"]),
        ];
        let rows = outreach(&operators(&pools), 10);
        let by_id: BTreeMap<&str, &OutreachRow> = rows.iter().map(|r| (r.pool_id.as_str(), r)).collect();
        let staked = by_id["pool1a"];
        assert_eq!((staked.pools, staked.name.as_str(), staked.stake_ratio), (2, "staked.cloud", 0.06));
        assert_eq!((staked.endpoints, staked.reasons.as_str()), (2, "timeout"));
        assert_eq!(by_id["pool1named"].pools, 1, "a pool with a ticker keeps its own row");
        assert_eq!(by_id["pool1c"].name, "", "an IP-only anonymous pool has no domain to merge on");
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn operators_merge_reach_and_reasons_and_thin_ranks_by_pools_per_endpoint() {
        let meta = |id: &str| Some(PoolMeta { pool_id: id.into(), ticker: Some(id.to_uppercase()), name: None });
        // An exchange: twenty pools, the same three endpoints, half reachable.
        let mut pools: Vec<PoolStat> = (0..20)
            .map(|i| {
                let mut p = pool_stat(i, 0.001, meta("pool1exch"), &["a:1", "b:1", "c:1"]);
                p.reach = if i % 2 == 0 { Reach::Full } else { Reach::None };
                p.reasons = if i % 2 == 0 { vec![] } else { vec!["private address", "timeout"] };
                p
            })
            .collect();
        // A healthy pool with two relays of its own, and a lone pool behind one relay.
        let mut healthy = pool_stat(20, 0.05, meta("pool1two"), &["x:1", "y:1"]);
        healthy.reach = Reach::Full;
        healthy.reasons = vec![];
        pools.push(healthy);
        pools.push(pool_stat(21, 0.002, meta("pool1one"), &["z:1"]));

        let ops = operators(&pools);
        let exch = ops.iter().find(|r| r.pool_id == "pool1exch").unwrap();
        assert_eq!((exch.pools, exch.endpoints, exch.reach), (20, 3, Reach::Partial));
        assert_eq!(exch.reasons, "private address, timeout", "first reason of each pool weighs most");

        let thin_rows = thin(&ops, 10);
        let ids: Vec<&str> = thin_rows.iter().map(|r| r.pool_id.as_str()).collect();
        assert_eq!(ids, vec!["pool1exch", "pool1one"], "20 pools on 3 endpoints, then 1 on 1; 1 on 2 is not thin");

        let out = outreach(&ops, 10);
        assert!(out.iter().all(|r| r.reach != Reach::Full));
        assert_eq!(out[0].pool_id, "pool1exch", "largest stake among the not fully reachable");
    }
}
