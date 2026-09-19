use futures::stream::{self, StreamExt};
use serde::Serialize;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use tokio::net::lookup_host;

use crate::net::format_host_port;
use crate::snapshot::Snapshot;

/// One relay line from the snapshot, tied to its pool.
#[derive(Clone, Debug, Serialize)]
pub struct Entry {
    pub pool: usize,
    pub address: String,
    pub port: u16,
}

/// One address actually probed. Entries that resolve to the same socket
/// address share an endpoint and inherit its result.
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub key: String,
    pub entries: Vec<usize>,
    pub dns_error: Option<String>,
}

pub fn entries(snapshot: &Snapshot, default_port: u16) -> Vec<Entry> {
    snapshot
        .pools
        .iter()
        .enumerate()
        .flat_map(|(pool, p)| {
            p.relays.iter().map(move |r| Entry {
                pool,
                address: r.address.clone(),
                port: r.port.unwrap_or(default_port),
            })
        })
        .collect()
}

struct Resolved {
    idx: usize,
    literal: Option<String>,
    addrs: Vec<String>,
    dns_error: Option<String>,
}

pub async fn resolve(entries: &[Entry], parallel: usize) -> Vec<Endpoint> {
    let mut resolved: Vec<Resolved> = Vec::with_capacity(entries.len());
    let mut hosts: Vec<(usize, String, u16)> = Vec::new();

    for (idx, e) in entries.iter().enumerate() {
        let host = e.address.trim_matches(&['[', ']'][..]);
        match host.parse::<IpAddr>() {
            Ok(ip) => resolved.push(Resolved {
                idx,
                literal: Some(SocketAddr::new(ip, e.port).to_string()),
                addrs: Vec::new(),
                dns_error: None,
            }),
            Err(_) => hosts.push((idx, e.address.clone(), e.port)),
        }
    }

    let looked_up: Vec<Resolved> = stream::iter(hosts)
        .map(|(idx, host, port)| async move {
            match lookup_host((host.as_str(), port)).await {
                Ok(addrs) => {
                    let addrs: Vec<String> = addrs.map(|a| a.to_string()).collect();
                    let dns_error = addrs.is_empty().then(|| "no addresses".to_string());
                    Resolved { idx, literal: None, addrs, dns_error }
                }
                Err(e) => Resolved {
                    idx,
                    literal: None,
                    addrs: Vec::new(),
                    dns_error: Some(e.to_string()),
                },
            }
        })
        .buffer_unordered(parallel.max(1))
        .collect()
        .await;

    resolved.extend(looked_up);
    resolved.sort_by_key(|r| r.idx);

    let mut groups: HashMap<String, Endpoint> = HashMap::new();
    let mut ip_to_key: HashMap<String, String> = HashMap::new();

    // IP literals define the first groups.
    for r in resolved.iter().filter(|r| r.literal.is_some()) {
        let key = r.literal.clone().unwrap();
        ip_to_key.entry(key.clone()).or_insert_with(|| key.clone());
        groups
            .entry(key.clone())
            .or_insert_with(|| Endpoint { key, entries: Vec::new(), dns_error: None })
            .entries
            .push(r.idx);
    }

    // Hostnames join the group of any address they resolve to, else start their own.
    for r in resolved.iter().filter(|r| r.literal.is_none()) {
        let e = &entries[r.idx];
        let own_key = format_host_port(&e.address, e.port);
        let key = r
            .addrs
            .iter()
            .find_map(|a| ip_to_key.get(a).cloned())
            .unwrap_or(own_key);
        let ep = groups.entry(key.clone()).or_insert_with(|| Endpoint {
            key: key.clone(),
            entries: Vec::new(),
            dns_error: r.dns_error.clone(),
        });
        ep.entries.push(r.idx);
        // One good lookup for this name is enough to probe it.
        if r.dns_error.is_none() {
            ep.dns_error = None;
        }
        for a in &r.addrs {
            ip_to_key.entry(a.clone()).or_insert_with(|| key.clone());
        }
    }

    let mut out: Vec<Endpoint> = groups.into_values().collect();
    out.sort_by(|a, b| a.key.cmp(&b.key));
    out
}
