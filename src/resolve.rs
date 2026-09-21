use futures::stream::{self, StreamExt};
use hickory_resolver::proto::rr::RData;
use hickory_resolver::TokioResolver;
use serde::Serialize;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use tokio::net::lookup_host;

use crate::net::format_host_port;
use crate::pools::relay_key;
use crate::snapshot::Snapshot;

/// One relay line from the snapshot, tied to its pool. No port means the
/// address is an SRV record name, as the ledger and the snapshot both write it.
#[derive(Clone, Debug, Serialize)]
pub struct Entry {
    pub pool: usize,
    pub address: String,
    pub port: Option<u16>,
}

impl Entry {
    pub fn is_srv(&self) -> bool {
        self.port.is_none()
    }
}

/// One address actually probed. Entries that resolve to the same socket
/// address share an endpoint and inherit its result. An SRV entry appears in
/// one endpoint per target.
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub key: String,
    pub entries: Vec<usize>,
    /// Per entry, the SRV weight that led it here; None for a plain relay.
    pub weights: Vec<Option<u16>>,
    /// Every address the key resolved to, kept so unreachable endpoints can
    /// still be placed in a network.
    pub addrs: Vec<IpAddr>,
    pub dns_error: Option<String>,
    /// Known only from an earlier run's report, not from this run's lookups.
    pub remembered: bool,
    /// When a remembered address was last returned by a lookup.
    pub last_seen: Option<u64>,
}

/// Addresses seen behind relay names in earlier runs, from the previous
/// report. A name that hands out a subset of its records per lookup, as
/// Route 53 multivalue answers do with at most eight, is completed across
/// runs this way; an address not seen for `REMEMBER_SECS` is forgotten.
pub struct Memory<'a> {
    pub targets: &'a HashMap<String, Vec<(String, u64)>>,
    pub now: u64,
}

pub const REMEMBER_SECS: u64 = 24 * 3600;

pub struct Resolved {
    pub endpoints: Vec<Endpoint>,
    /// Per entry, why its SRV lookup produced no targets. None for plain entries
    /// and for SRV entries that resolved.
    pub srv_errors: Vec<Option<String>>,
}

pub fn entries(snapshot: &Snapshot) -> Vec<Entry> {
    snapshot
        .pools
        .iter()
        .enumerate()
        .flat_map(|(pool, p)| {
            p.relays.iter().map(move |r| Entry {
                pool,
                address: r.address.clone(),
                port: r.port,
            })
        })
        .collect()
}

/// Top-priority SRV targets as host, port and weight, or why there were none.
type SrvTargets = Result<Vec<(String, u16, u16)>, String>;

/// A host and port to probe on behalf of an entry.
struct Target {
    entry: usize,
    host: String,
    port: u16,
    /// SRV weight; None for a plain relay.
    weight: Option<u16>,
}

struct Looked {
    target: usize,
    literal: Option<String>,
    addrs: Vec<String>,
    dns_error: Option<String>,
}

pub async fn resolve(entries: &[Entry], parallel: usize, memory: Option<&Memory<'_>>) -> Resolved {
    let parallel = parallel.max(1);
    let mut srv_errors: Vec<Option<String>> = vec![None; entries.len()];
    let mut targets: Vec<Target> = Vec::with_capacity(entries.len());

    for (idx, e) in entries.iter().enumerate() {
        if let Some(port) = e.port {
            targets.push(Target {
                entry: idx,
                host: e.address.clone(),
                port,
                weight: None,
            });
        }
    }

    let srv_entries: Vec<usize> = (0..entries.len()).filter(|&i| entries[i].is_srv()).collect();
    if !srv_entries.is_empty() {
        match TokioResolver::builder_tokio().map(|b| b.build()) {
            Ok(Ok(resolver)) => {
                let looked: Vec<(usize, SrvTargets)> = stream::iter(srv_entries)
                    .map(|idx| {
                        let resolver = resolver.clone();
                        let name = entries[idx].address.clone();
                        async move { (idx, srv_targets(&resolver, &name).await) }
                    })
                    .buffer_unordered(parallel)
                    .collect()
                    .await;
                for (idx, result) in looked {
                    match result {
                        Ok(found) => {
                            for (host, port, weight) in found {
                                targets.push(Target { entry: idx, host, port, weight: Some(weight) });
                            }
                        }
                        Err(e) => srv_errors[idx] = Some(e),
                    }
                }
            }
            Ok(Err(e)) | Err(e) => {
                let msg = format!("SRV resolver unavailable: {e}");
                for idx in srv_entries {
                    srv_errors[idx] = Some(msg.clone());
                }
            }
        }
    }

    let endpoints = group(&targets, parallel, memory).await;
    Resolved { endpoints, srv_errors }
}

/// The SRV targets a node would choose between: every record at the lowest
/// priority value present, as host and port.
async fn srv_targets(resolver: &TokioResolver, name: &str) -> SrvTargets {
    let lookup = resolver
        .srv_lookup(format!("{}.", name.trim_end_matches('.')).as_str())
        .await
        .map_err(|e| format!("SRV lookup failed for {name}: {e}"))?;
    let records: Vec<(u16, String, u16, u16)> = lookup
        .answers()
        .iter()
        .filter_map(|r| match &r.data {
            RData::SRV(srv) => Some((
                srv.priority,
                srv.target.to_utf8().trim_end_matches('.').to_string(),
                srv.port,
                srv.weight,
            )),
            _ => None,
        })
        .collect();
    let best = records
        .iter()
        .map(|(p, _, _, _)| *p)
        .min()
        .ok_or_else(|| format!("no SRV records for {name}"))?;
    Ok(records
        .into_iter()
        .filter(|(p, _, _, _)| *p == best)
        .map(|(_, host, port, weight)| (host, port, weight))
        .collect())
}

async fn group(targets: &[Target], parallel: usize, memory: Option<&Memory<'_>>) -> Vec<Endpoint> {
    let mut looked: Vec<Looked> = Vec::with_capacity(targets.len());
    let mut hosts: Vec<(usize, String, u16)> = Vec::new();

    for (t, target) in targets.iter().enumerate() {
        let host = target.host.trim_matches(&['[', ']'][..]);
        match host.parse::<IpAddr>() {
            Ok(ip) => looked.push(Looked {
                target: t,
                literal: Some(SocketAddr::new(ip, target.port).to_string()),
                addrs: Vec::new(),
                dns_error: None,
            }),
            Err(_) => hosts.push((t, target.host.clone(), target.port)),
        }
    }

    let looked_up: Vec<Looked> = stream::iter(hosts)
        .map(|(t, host, port)| async move {
            match lookup_host((host.as_str(), port)).await {
                Ok(addrs) => {
                    let addrs: Vec<String> = addrs.map(|a| a.to_string()).collect();
                    let dns_error = addrs.is_empty().then(|| "no addresses".to_string());
                    Looked { target: t, literal: None, addrs, dns_error }
                }
                Err(e) => Looked {
                    target: t,
                    literal: None,
                    addrs: Vec::new(),
                    dns_error: Some(e.to_string()),
                },
            }
        })
        .buffer_unordered(parallel)
        .collect()
        .await;

    looked.extend(looked_up);
    looked.sort_by_key(|l| l.target);
    assemble(targets, &looked, memory)
}

/// One endpoint per socket address. A name with several addresses becomes
/// one endpoint per address, as a node treats each as its own peer; an
/// address shared by names and literals is one endpoint with every entry on
/// it. A name that did not resolve keeps one endpoint under its own name.
/// Addresses a plain name resolved to in earlier runs, but not this one, are
/// added as remembered endpoints while they are within `REMEMBER_SECS`.
fn assemble(targets: &[Target], looked: &[Looked], memory: Option<&Memory<'_>>) -> Vec<Endpoint> {
    let mut groups: HashMap<String, Endpoint> = HashMap::new();

    // Two SRV records of one name landing on the same endpoint add their weights.
    let push_entry = |ep: &mut Endpoint, entry: usize, weight: Option<u16>| {
        match ep.entries.iter().position(|&e| e == entry) {
            Some(i) => {
                if let (Some(w), Some(have)) = (weight, ep.weights[i]) {
                    ep.weights[i] = Some(have.saturating_add(w));
                }
            }
            None => {
                ep.entries.push(entry);
                ep.weights.push(weight);
            }
        }
    };

    for l in looked {
        let target = &targets[l.target];
        let keys: Vec<String> = match &l.literal {
            Some(k) => vec![k.clone()],
            None if l.addrs.is_empty() => vec![format_host_port(&target.host, target.port)],
            None => l.addrs.clone(),
        };
        for key in keys {
            let ip = key.parse::<SocketAddr>().ok().map(|s| s.ip());
            let ep = groups.entry(key.clone()).or_insert_with(|| Endpoint {
                key: key.clone(),
                entries: Vec::new(),
                weights: Vec::new(),
                addrs: ip.into_iter().collect(),
                dns_error: l.dns_error.clone(),
                remembered: false,
                last_seen: None,
            });
            push_entry(ep, target.entry, target.weight);
        }
    }

    // Plain names only: an SRV lookup returns every target, and a literal is
    // its own address.
    if let Some(memory) = memory {
        for (t, target) in targets.iter().enumerate() {
            let literal = looked.iter().any(|l| l.target == t && l.literal.is_some());
            if target.weight.is_some() || literal {
                continue;
            }
            let Some(seen) = memory.targets.get(&relay_key(&target.host, Some(target.port))) else { continue };
            for (key, last_seen) in seen {
                if memory.now.saturating_sub(*last_seen) > REMEMBER_SECS {
                    continue;
                }
                let Some(sock) = key.parse::<SocketAddr>().ok() else { continue };
                let ep = groups.entry(key.clone()).or_insert_with(|| Endpoint {
                    key: key.clone(),
                    entries: Vec::new(),
                    weights: Vec::new(),
                    addrs: vec![sock.ip()],
                    dns_error: None,
                    remembered: true,
                    last_seen: Some(*last_seen),
                });
                push_entry(ep, target.entry, None);
            }
        }
    }

    let mut out: Vec<Endpoint> = groups.into_values().collect();
    out.sort_by(|a, b| a.key.cmp(&b.key));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(entry: usize, host: &str, port: u16, weight: Option<u16>) -> Target {
        Target { entry, host: host.into(), port, weight }
    }

    #[test]
    fn names_expand_to_one_endpoint_per_address_and_share_with_literals() {
        let targets = vec![
            target(0, "relays.example", 3001, None),
            target(1, "relays.example", 3001, None),
            target(2, "192.0.2.1", 3001, None),
            target(3, "dead.example", 3001, None),
            target(4, "srv-a.example", 6000, Some(10)),
        ];
        let looked = vec![
            Looked { target: 0, literal: None, addrs: vec!["192.0.2.1:3001".into(), "192.0.2.2:3001".into(), "[2001:db8::1]:3001".into()], dns_error: None },
            Looked { target: 1, literal: None, addrs: vec!["192.0.2.1:3001".into(), "192.0.2.2:3001".into(), "[2001:db8::1]:3001".into()], dns_error: None },
            Looked { target: 2, literal: Some("192.0.2.1:3001".into()), addrs: vec![], dns_error: None },
            Looked { target: 3, literal: None, addrs: vec![], dns_error: Some("no such host".into()) },
            Looked { target: 4, literal: None, addrs: vec!["192.0.2.9:6000".into()], dns_error: None },
        ];
        let eps = assemble(&targets, &looked, None);
        let keys: Vec<&str> = eps.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["192.0.2.1:3001", "192.0.2.2:3001", "192.0.2.9:6000", "[2001:db8::1]:3001", "dead.example:3001"]);
        let shared = eps.iter().find(|e| e.key == "192.0.2.1:3001").unwrap();
        assert_eq!(shared.entries, vec![0, 1, 2], "two pools' name and one literal on one address");
        assert_eq!(shared.addrs, vec!["192.0.2.1".parse::<IpAddr>().unwrap()]);
        let dead = eps.iter().find(|e| e.key == "dead.example:3001").unwrap();
        assert_eq!(dead.dns_error.as_deref(), Some("no such host"));
        assert!(dead.addrs.is_empty());
        let srv = eps.iter().find(|e| e.key == "192.0.2.9:6000").unwrap();
        assert_eq!(srv.weights, vec![Some(10)]);
        assert!(eps.iter().all(|e| !e.remembered));
    }

    #[test]
    fn remembered_addresses_join_within_retention_only() {
        let targets = vec![target(0, "relays.example", 3001, None), target(1, "1.2.3.4", 3001, None), target(2, "srv.example", 6000, Some(5))];
        let looked = vec![
            Looked { target: 0, literal: None, addrs: vec!["192.0.2.1:3001".into()], dns_error: None },
            Looked { target: 1, literal: Some("1.2.3.4:3001".into()), addrs: vec![], dns_error: None },
            Looked { target: 2, literal: None, addrs: vec!["192.0.2.9:6000".into()], dns_error: None },
        ];
        let now = 1_000_000;
        let mem = HashMap::from([
            ("relays.example:3001".to_string(), vec![
                ("192.0.2.1:3001".to_string(), now - 60),
                ("192.0.2.2:3001".to_string(), now - 3600),
                ("192.0.2.3:3001".to_string(), now - REMEMBER_SECS - 1),
            ]),
            ("1.2.3.4:3001".to_string(), vec![("9.9.9.9:3001".to_string(), now)]),
            ("srv.example".to_string(), vec![("192.0.2.8:6000".to_string(), now)]),
        ]);
        let eps = assemble(&targets, &looked, Some(&Memory { targets: &mem, now }));
        let keys: Vec<&str> = eps.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["1.2.3.4:3001", "192.0.2.1:3001", "192.0.2.2:3001", "192.0.2.9:6000"], "one remembered address added, the stale one dropped, literals and SRV untouched");
        let fresh = eps.iter().find(|e| e.key == "192.0.2.1:3001").unwrap();
        assert!(!fresh.remembered, "returned this run, so not remembered");
        let old = eps.iter().find(|e| e.key == "192.0.2.2:3001").unwrap();
        assert_eq!((old.remembered, old.last_seen, old.entries.as_slice(), old.weights.as_slice()), (true, Some(now - 3600), &[0][..], &[None][..]));
    }
}
