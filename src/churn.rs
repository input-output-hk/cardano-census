//! Run-to-run churn: which relays changed state or network since the last
//! report, read from the previous `--report` file before it is overwritten.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

use crate::asn::AsInfo;
use crate::pools::relay_key;
use crate::probe::Outcome;
use crate::resolve::Entry;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrevState {
    pub reachable: bool,
    pub asn: Option<u32>,
}

/// Relay states from the previous report, keyed like the pool index keys relays.
pub struct Previous {
    pub by_key: HashMap<String, PrevState>,
    pub timestamp_seconds: u64,
}

#[derive(Deserialize)]
struct PrevReport {
    summary: PrevSummary,
    pools: Vec<PrevPool>,
}

#[derive(Deserialize)]
struct PrevSummary {
    timestamp_seconds: u64,
}

#[derive(Deserialize)]
struct PrevPool {
    relays: Vec<PrevRelay>,
}

#[derive(Deserialize)]
struct PrevRelay {
    address: String,
    port: Option<u16>,
    reachable: bool,
    asn: Option<u32>,
}

impl Previous {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let report: PrevReport =
            serde_json::from_str(&text).with_context(|| format!("parsing {} as a census report", path.display()))?;
        let mut by_key = HashMap::new();
        for r in report.pools.iter().flat_map(|p| &p.relays) {
            // Every pool listing the same relay saw the same result.
            by_key
                .entry(relay_key(&r.address, r.port))
                .or_insert(PrevState { reachable: r.reachable, asn: r.asn });
        }
        Ok(Self { by_key, timestamp_seconds: report.summary.timestamp_seconds })
    }
}

/// What one relay did since the last run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    BecameReachable,
    BecameUnreachable,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Churn {
    pub previous_timestamp_seconds: u64,
    /// Distinct relays that changed state, and the stake behind them with each
    /// pool split evenly over its relays.
    pub to_reachable: u64,
    pub to_unreachable: u64,
    pub stake_to_reachable: f64,
    pub stake_to_unreachable: f64,
    /// Distinct relays that resolved into a different autonomous system.
    pub moved_network: u64,
}

/// Diff this run against the previous one. Relays absent from the previous
/// report are new to the snapshot and count as neither.
pub fn diff(
    prev: &Previous,
    entries: &[Entry],
    entry_outcome: &[Option<Outcome>],
    entry_asn: &[Option<AsInfo>],
    share_of: impl Fn(usize) -> f64,
) -> (Churn, Vec<Option<Change>>) {
    let mut churn = Churn { previous_timestamp_seconds: prev.timestamp_seconds, ..Default::default() };
    let mut seen: HashMap<String, Option<Change>> = HashMap::new();
    let mut changes = vec![None; entries.len()];
    for (i, e) in entries.iter().enumerate() {
        let key = relay_key(&e.address, e.port);
        let Some(p) = prev.by_key.get(&key) else { continue };
        let reachable = matches!(entry_outcome[i], Some(Ok(_)));
        let change = match (p.reachable, reachable) {
            (false, true) => Some(Change::BecameReachable),
            (true, false) => Some(Change::BecameUnreachable),
            _ => None,
        };
        changes[i] = change;
        // Stake counts per entry, the relay itself once.
        match change {
            Some(Change::BecameReachable) => churn.stake_to_reachable += share_of(i),
            Some(Change::BecameUnreachable) => churn.stake_to_unreachable += share_of(i),
            None => {}
        }
        if seen.insert(key, change).is_some() {
            continue;
        }
        match change {
            Some(Change::BecameReachable) => churn.to_reachable += 1,
            Some(Change::BecameUnreachable) => churn.to_unreachable += 1,
            None => {}
        }
        let now = entry_asn[i].as_ref().map(|a| a.asn);
        if let (Some(a), Some(b)) = (p.asn, now) {
            if a != b {
                churn.moved_network += 1;
            }
        }
    }
    churn.stake_to_reachable += 0.0;
    churn.stake_to_unreachable += 0.0;
    (churn, changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{Failed, Family, Reached, Stage, Tip};

    fn ok() -> Outcome {
        Ok(Reached {
            peer: "192.0.2.1:3001".parse().unwrap(),
            family: Family::V4,
            n2n_version: "14".into(),
            tip: Tip { slot: 1, hash: "aa".into(), block: 1 },
            rtt_ms: 10,
        })
    }

    fn down() -> Outcome {
        Err(Failed { stage: Stage::Connect, error: "timeout".into() })
    }

    fn asn(n: u32) -> Option<AsInfo> {
        Some(AsInfo { asn: n, name: format!("AS{n}") })
    }

    #[test]
    fn diff_counts_relays_once_and_stake_per_entry() {
        let prev = Previous {
            timestamp_seconds: 100,
            by_key: HashMap::from([
                ("shared.example:3001".to_string(), PrevState { reachable: true, asn: Some(1) }),
                ("up.example:3001".to_string(), PrevState { reachable: false, asn: Some(2) }),
                ("same.example:3001".to_string(), PrevState { reachable: true, asn: Some(3) }),
            ]),
        };
        let entries = vec![
            Entry { pool: 0, address: "shared.example".into(), port: Some(3001) },
            Entry { pool: 1, address: "shared.example".into(), port: Some(3001) },
            Entry { pool: 2, address: "up.example".into(), port: Some(3001) },
            Entry { pool: 3, address: "same.example".into(), port: Some(3001) },
            Entry { pool: 4, address: "new.example".into(), port: Some(3001) },
        ];
        let outcomes = vec![Some(down()), Some(down()), Some(ok()), Some(ok()), Some(down())];
        let asns = vec![asn(1), asn(1), asn(9), asn(3), asn(4)];
        let (c, changes) = diff(&prev, &entries, &outcomes, &asns, |i| [0.1, 0.2, 0.3, 0.4, 0.5][i]);
        assert_eq!(c.to_unreachable, 1, "the shared relay went down once");
        assert!((c.stake_to_unreachable - 0.3).abs() < 1e-12, "but both pools behind it lost stake");
        assert_eq!((c.to_reachable, c.stake_to_reachable), (1, 0.3));
        assert_eq!(c.moved_network, 1, "up.example moved from AS2 to AS9");
        assert_eq!(c.previous_timestamp_seconds, 100);
        assert_eq!(changes, vec![Some(Change::BecameUnreachable), Some(Change::BecameUnreachable), Some(Change::BecameReachable), None, None]);
    }
}
