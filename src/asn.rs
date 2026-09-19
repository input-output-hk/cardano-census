//! IP to autonomous system lookup from an iptoasn.com `ip2asn` TSV:
//! `range_start range_end asn country name`, one range per line.

use anyhow::{Context, Result};
use serde::Serialize;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AsInfo {
    pub asn: u32,
    /// The registry handle, the first word of the description.
    pub name: String,
}

struct Range<A> {
    start: A,
    end: A,
    info: AsInfo,
}

pub struct AsnDb {
    v4: Vec<Range<u32>>,
    v6: Vec<Range<u128>>,
}

impl AsnDb {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        Ok(Self::parse(&text))
    }

    /// Unrouted ranges, AS 0, are dropped so they read as no match.
    pub fn parse(text: &str) -> Self {
        let mut v4 = Vec::new();
        let mut v6 = Vec::new();
        for line in text.lines() {
            let mut cols = line.split('\t');
            let (Some(start), Some(end), Some(asn)) = (cols.next(), cols.next(), cols.next()) else {
                continue;
            };
            let _country = cols.next();
            let name = cols.next().unwrap_or("");
            let Ok(asn) = asn.parse::<u32>() else { continue };
            if asn == 0 {
                continue;
            }
            let handle = name.split_whitespace().next().unwrap_or("");
            let info = AsInfo {
                asn,
                name: if handle.is_empty() { format!("AS{asn}") } else { handle.to_string() },
            };
            match (start.parse::<Ipv4Addr>(), end.parse::<Ipv4Addr>()) {
                (Ok(s), Ok(e)) => v4.push(Range { start: u32::from(s), end: u32::from(e), info }),
                _ => {
                    if let (Ok(s), Ok(e)) = (start.parse::<Ipv6Addr>(), end.parse::<Ipv6Addr>()) {
                        v6.push(Range { start: u128::from(s), end: u128::from(e), info });
                    }
                }
            }
        }
        v4.sort_by_key(|r| r.start);
        v6.sort_by_key(|r| r.start);
        Self { v4, v6 }
    }

    pub fn len(&self) -> usize {
        self.v4.len() + self.v6.len()
    }

    pub fn lookup(&self, ip: IpAddr) -> Option<&AsInfo> {
        match ip {
            IpAddr::V4(a) => find(&self.v4, u32::from(a)),
            IpAddr::V6(a) => find(&self.v6, u128::from(a)),
        }
    }
}

fn find<A: Ord + Copy>(ranges: &[Range<A>], ip: A) -> Option<&AsInfo> {
    let i = ranges.partition_point(|r| r.start <= ip);
    let r = ranges.get(i.checked_sub(1)?)?;
    (ip <= r.end).then_some(&r.info)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> AsnDb {
        AsnDb::parse(
            "1.0.0.0\t1.0.0.255\t13335\tUS\tCLOUDFLARENET\n\
             1.0.1.0\t1.0.3.255\t0\tNone\tNot routed\n\
             10.0.0.0\t10.255.255.255\t64512\tDE\tHETZNER-AS Hetzner Online GmbH\n\
             2001:db8::\t2001:db8:ffff:ffff:ffff:ffff:ffff:ffff\t64513\tNL\tEXAMPLE-V6\n",
        )
    }

    #[test]
    fn finds_ranges_and_takes_the_handle() {
        let db = db();
        assert_eq!(db.len(), 3);
        let h = db.lookup("10.20.30.40".parse().unwrap()).unwrap();
        assert_eq!((h.asn, h.name.as_str()), (64512, "HETZNER-AS"));
        assert_eq!(db.lookup("1.0.0.7".parse().unwrap()).unwrap().name, "CLOUDFLARENET");
        assert_eq!(db.lookup("2001:db8::1".parse().unwrap()).unwrap().asn, 64513);
    }

    #[test]
    fn misses_unrouted_and_gaps() {
        let db = db();
        assert!(db.lookup("1.0.2.1".parse().unwrap()).is_none());
        assert!(db.lookup("9.9.9.9".parse().unwrap()).is_none());
        assert!(db.lookup("2001:db9::1".parse().unwrap()).is_none());
    }
}
