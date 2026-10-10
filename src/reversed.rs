//! Shadow probe of every IPv4 literal relay at its octet-reversed address.
//!
//! cardano-node 11.1.x decodes the ledger's IPv4 relay bytes in the wrong
//! order, so the snapshot names `d.c.b.a` for a relay registered as `a.b.c.d`.
//! Probing the reversal alongside the given spelling measures how much of the
//! network that hides, and reads near zero once the codec is fixed.

use std::net::{Ipv4Addr, SocketAddr};

use crate::resolve::Entry;

/// Per entry, the reversed socket address and whether it answered; None for
/// entries that are not IPv4 literals.
pub struct Shadow {
    pub address: Vec<Option<String>>,
    pub reachable: Vec<Option<bool>>,
}

impl Shadow {
    #[cfg(test)]
    pub fn none(entries: usize) -> Self {
        Self {
            address: vec![None; entries],
            reachable: vec![None; entries],
        }
    }
}

pub fn reverse(ip: Ipv4Addr) -> Ipv4Addr {
    let [a, b, c, d] = ip.octets();
    Ipv4Addr::new(d, c, b, a)
}

/// The reversed `ip:port` for each IPv4 literal entry with a port.
pub fn targets(entries: &[Entry]) -> Vec<Option<String>> {
    entries
        .iter()
        .map(|e| {
            let port = e.port?;
            let ip: Ipv4Addr = e.address.parse().ok()?;
            Some(SocketAddr::new(reverse(ip).into(), port).to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverses_octets() {
        assert_eq!(reverse("20.61.229.103".parse().unwrap()), "103.229.61.20".parse::<Ipv4Addr>().unwrap());
    }

    #[test]
    fn targets_only_ipv4_literals_with_ports() {
        let entries = vec![
            Entry { pool: 0, address: "20.61.229.103".into(), port: Some(3001) },
            Entry { pool: 0, address: "relay.example".into(), port: Some(3001) },
            Entry { pool: 0, address: "2001:db8::1".into(), port: Some(3001) },
            Entry { pool: 1, address: "_cardano._tcp.example".into(), port: None },
        ];
        let t = targets(&entries);
        assert_eq!(t[0].as_deref(), Some("103.229.61.20:3001"));
        assert!(t[1].is_none() && t[2].is_none() && t[3].is_none());
    }
}
