use futures::stream::{self, StreamExt};
use hickory_resolver::proto::rr::RData;
use hickory_resolver::TokioResolver;
use serde::Serialize;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use tokio::net::lookup_host;

use crate::net::format_host_port;
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
    pub dns_error: Option<String>,
}

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

/// Top-priority SRV targets as host and port, or why there were none.
type SrvTargets = Result<Vec<(String, u16)>, String>;

/// A host and port to probe on behalf of an entry.
struct Target {
    entry: usize,
    host: String,
    port: u16,
}

struct Looked {
    target: usize,
    literal: Option<String>,
    addrs: Vec<String>,
    dns_error: Option<String>,
}

pub async fn resolve(entries: &[Entry], parallel: usize) -> Resolved {
    let parallel = parallel.max(1);
    let mut srv_errors: Vec<Option<String>> = vec![None; entries.len()];
    let mut targets: Vec<Target> = Vec::with_capacity(entries.len());

    for (idx, e) in entries.iter().enumerate() {
        if let Some(port) = e.port {
            targets.push(Target {
                entry: idx,
                host: e.address.clone(),
                port,
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
                            for (host, port) in found {
                                targets.push(Target { entry: idx, host, port });
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

    let endpoints = group(&targets, parallel).await;
    Resolved { endpoints, srv_errors }
}

/// The SRV targets a node would choose between: every record at the lowest
/// priority value present, as host and port.
async fn srv_targets(resolver: &TokioResolver, name: &str) -> SrvTargets {
    let lookup = resolver
        .srv_lookup(format!("{}.", name.trim_end_matches('.')).as_str())
        .await
        .map_err(|e| format!("SRV lookup failed for {name}: {e}"))?;
    let records: Vec<(u16, String, u16)> = lookup
        .answers()
        .iter()
        .filter_map(|r| match &r.data {
            RData::SRV(srv) => Some((
                srv.priority,
                srv.target.to_utf8().trim_end_matches('.').to_string(),
                srv.port,
            )),
            _ => None,
        })
        .collect();
    let best = records
        .iter()
        .map(|(p, _, _)| *p)
        .min()
        .ok_or_else(|| format!("no SRV records for {name}"))?;
    Ok(records
        .into_iter()
        .filter(|(p, _, _)| *p == best)
        .map(|(_, host, port)| (host, port))
        .collect())
}

async fn group(targets: &[Target], parallel: usize) -> Vec<Endpoint> {
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

    let mut groups: HashMap<String, Endpoint> = HashMap::new();
    let mut ip_to_key: HashMap<String, String> = HashMap::new();

    let push_entry = |ep: &mut Endpoint, entry: usize| {
        if !ep.entries.contains(&entry) {
            ep.entries.push(entry);
        }
    };

    // IP literals define the first groups.
    for l in looked.iter().filter(|l| l.literal.is_some()) {
        let key = l.literal.clone().unwrap();
        ip_to_key.entry(key.clone()).or_insert_with(|| key.clone());
        let ep = groups
            .entry(key.clone())
            .or_insert_with(|| Endpoint { key, entries: Vec::new(), dns_error: None });
        push_entry(ep, targets[l.target].entry);
    }

    // Hostnames join the group of any address they resolve to, else start their own.
    for l in looked.iter().filter(|l| l.literal.is_none()) {
        let target = &targets[l.target];
        let own_key = format_host_port(&target.host, target.port);
        let key = l
            .addrs
            .iter()
            .find_map(|a| ip_to_key.get(a).cloned())
            .unwrap_or(own_key);
        let ep = groups.entry(key.clone()).or_insert_with(|| Endpoint {
            key: key.clone(),
            entries: Vec::new(),
            dns_error: l.dns_error.clone(),
        });
        push_entry(ep, target.entry);
        // One good lookup for this name is enough to probe it.
        if l.dns_error.is_none() {
            ep.dns_error = None;
        }
        for a in &l.addrs {
            ip_to_key.entry(a.clone()).or_insert_with(|| key.clone());
        }
    }

    let mut out: Vec<Endpoint> = groups.into_values().collect();
    out.sort_by(|a, b| a.key.cmp(&b.key));
    out
}
