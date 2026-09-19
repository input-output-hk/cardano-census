//! Optional pool identities, matched to snapshot entries by relay address.
//!
//! The peer snapshot names no pools, only stake shares and relays. A pool
//! index built from the same ledger registrations, db-sync's `pool_relay`
//! rows for instance, gives each entry back its pool id, ticker and name.
//! The file is a JSON array of `{pool_id, ticker, name, relays}` records,
//! relays as `host:port`, `[v6]:port`, or a bare SRV name.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PoolMeta {
    pub pool_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticker: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Deserialize)]
struct Record {
    pool_id: String,
    #[serde(default)]
    ticker: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    relays: Vec<String>,
}

pub struct PoolIndex {
    /// A relay can be registered by several pools, one operator's fleet.
    by_relay: HashMap<String, Vec<PoolMeta>>,
    pub pools: usize,
}

/// One spelling for a relay however it was written: lower case, no trailing
/// dot, IP literals in canonical form, the port appended when there is one.
pub fn relay_key(address: &str, port: Option<u16>) -> String {
    let host = address
        .trim()
        .trim_end_matches('.')
        .trim_matches(&['[', ']'][..])
        .to_ascii_lowercase();
    match (host.parse::<IpAddr>(), port) {
        (Ok(ip), Some(p)) => SocketAddr::new(ip, p).to_string(),
        (Ok(ip), None) => ip.to_string(),
        (Err(_), Some(p)) => format!("{host}:{p}"),
        (Err(_), None) => host,
    }
}

/// Split an index relay string into host and optional port.
fn split(relay: &str) -> (String, Option<u16>) {
    let s = relay.trim();
    if let Some(rest) = s.strip_prefix('[') {
        if let Some((host, tail)) = rest.split_once(']') {
            let port = tail.strip_prefix(':').and_then(|p| p.parse().ok());
            return (host.to_string(), port);
        }
    }
    match s.rsplit_once(':') {
        // A single colon with a numeric tail is host:port; more colons is a bare IPv6.
        Some((host, port)) if !host.contains(':') => match port.parse::<u16>() {
            Ok(p) => (host.to_string(), Some(p)),
            Err(_) => (s.to_string(), None),
        },
        _ => (s.to_string(), None),
    }
}

impl PoolIndex {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let records: Vec<Record> = serde_json::from_str(&text)
            .with_context(|| format!("parsing {} as a pool index", path.display()))?;
        Ok(Self::from_records(records))
    }

    fn from_records(records: Vec<Record>) -> Self {
        let pools = records.len();
        let mut by_relay = HashMap::new();
        for r in records {
            let meta = PoolMeta {
                pool_id: r.pool_id,
                ticker: r.ticker.filter(|t| !t.is_empty()),
                name: r.name.filter(|n| !n.is_empty()),
            };
            for relay in r.relays {
                let (host, port) = split(&relay);
                let pools: &mut Vec<PoolMeta> = by_relay.entry(relay_key(&host, port)).or_default();
                if !pools.iter().any(|p| p.pool_id == meta.pool_id) {
                    pools.push(meta.clone());
                }
            }
        }
        Self { by_relay, pools }
    }

    /// The pool most of these relays are registered to; ties go to the
    /// lexically smallest pool id so the answer is stable between runs.
    pub fn identify<'a>(&self, relays: impl IntoIterator<Item = (&'a str, Option<u16>)>) -> Option<PoolMeta> {
        let mut votes: std::collections::BTreeMap<&str, (usize, &PoolMeta)> = std::collections::BTreeMap::new();
        for (address, port) in relays {
            for meta in self.by_relay.get(&relay_key(address, port)).into_iter().flatten() {
                votes.entry(meta.pool_id.as_str()).or_insert((0, meta)).0 += 1;
            }
        }
        votes
            .into_iter()
            .max_by(|a, b| a.1 .0.cmp(&b.1 .0).then(b.0.cmp(a.0)))
            .map(|(_, (_, meta))| meta.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_normalise_case_dots_and_ip_forms() {
        assert_eq!(relay_key("Relay.Example.ORG.", Some(3001)), "relay.example.org:3001");
        assert_eq!(relay_key("192.0.2.1", Some(6000)), "192.0.2.1:6000");
        assert_eq!(relay_key("[2001:DB8::1]", Some(3001)), "[2001:db8::1]:3001");
        assert_eq!(relay_key("2001:db8:0:0::1", Some(3001)), "[2001:db8::1]:3001");
        assert_eq!(relay_key("_cardano._tcp.example.org", None), "_cardano._tcp.example.org");
    }

    fn record(pool_id: &str, ticker: Option<&str>, relays: &[&str]) -> Record {
        Record {
            pool_id: pool_id.into(),
            ticker: ticker.map(Into::into),
            name: None,
            relays: relays.iter().map(|r| r.to_string()).collect(),
        }
    }

    #[test]
    fn index_matches_by_any_spelling() {
        let idx = PoolIndex::from_records(vec![record(
            "pool1abc",
            Some("TST"),
            &["Relay.Example.org:3001", "[2001:db8::1]:3001", "srv.example.org"],
        )]);
        assert_eq!(idx.pools, 1);
        let one = |a: &str, p: Option<u16>| idx.identify([(a, p)]);
        assert_eq!(one("relay.example.org.", Some(3001)).unwrap().ticker.as_deref(), Some("TST"));
        assert_eq!(one("2001:DB8:0::1", Some(3001)).unwrap().pool_id, "pool1abc");
        assert!(one("srv.example.org", None).is_some());
        assert!(one("relay.example.org", Some(3002)).is_none());
    }

    #[test]
    fn identity_goes_to_the_pool_most_relays_point_at() {
        let idx = PoolIndex::from_records(vec![
            record("pool1zzz", Some("ZZZ"), &["shared.example.org:3001", "zzz.example.org:3001"]),
            record("pool1aaa", Some("AAA"), &["shared.example.org:3001"]),
        ]);
        // Two relays vote zzz, one votes both: zzz wins on count.
        let zzz = idx.identify([("shared.example.org", Some(3001)), ("zzz.example.org", Some(3001))]);
        assert_eq!(zzz.unwrap().pool_id, "pool1zzz");
        // Only the shared relay: a tie, broken towards the smaller id.
        let tie = idx.identify([("shared.example.org", Some(3001))]);
        assert_eq!(tie.unwrap().pool_id, "pool1aaa");
    }

    #[test]
    fn empty_ticker_reads_as_unknown() {
        let idx = PoolIndex::from_records(vec![record("pool1xyz", Some(""), &["r.example.org:3001"])]);
        assert_eq!(idx.identify([("r.example.org", Some(3001))]).unwrap().ticker, None);
    }
}
