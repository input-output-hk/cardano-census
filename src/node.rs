//! Fetch the big ledger peer snapshot from a local cardano-node over the
//! node-to-client socket, with the Shelley `GetBigLedgerPeerSnapshot` query.

use anyhow::{anyhow, bail, Context, Result};
use pallas_codec::minicbor::data::{Tag, Type};
use pallas_codec::minicbor::{Decoder, Encoder};
use pallas_codec::utils::AnyCbor;
use pallas_network::{
    miniprotocols::{
        handshake::{n2c, Client as HandshakeClient, Confirmation},
        localstate::{queries_v16, Client as StateQueryClient},
        Point, PROTOCOL_N2C_HANDSHAKE, PROTOCOL_N2C_STATE_QUERY,
    },
    multiplexer::{Bearer, Plexer},
};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::time::Duration;
use tokio::time::timeout;

use crate::snapshot::{Point as SnapshotPoint, Pool, Relay, Snapshot};

pub const MAINNET_MAGIC: u64 = 764824073;

/// Parse a network magic from the command line, accepting "mainnet" for its magic.
pub fn parse_magic(s: &str) -> Result<u64, String> {
    if s.eq_ignore_ascii_case("mainnet") {
        return Ok(MAINNET_MAGIC);
    }
    s.parse::<u64>()
        .map_err(|_| format!("expected a network magic number or \"mainnet\", got {s:?}"))
}

/// The Shelley query is `[34]` through ouroboros-consensus-cardano 0.26 and
/// `[34, 1]` once the all-ledger-peers variant lands. The node closes the
/// connection on the one it does not know, so both are tried in turn.
#[derive(Clone, Copy, Debug)]
enum QueryShape {
    Plain,
    Kinded,
}

enum Failure {
    /// Connecting, handshake, acquire, or the framing queries failed; not worth a retry.
    Setup(anyhow::Error),
    /// The snapshot query itself failed; the other query shape may work.
    Query(anyhow::Error),
}

/// Longest unix socket path Linux accepts, sun_path less its terminator.
const MAX_SOCKET_PATH: usize = 107;

pub async fn fetch(socket: &Path, magic: u64, budget: Duration) -> Result<Snapshot> {
    let len = socket.as_os_str().len();
    if len > MAX_SOCKET_PATH {
        bail!(
            "socket path is {len} bytes, unix sockets allow {MAX_SOCKET_PATH}: {}",
            socket.display()
        );
    }
    let first = match session(socket, magic, budget, QueryShape::Plain).await {
        Ok(snap) => return Ok(snap),
        Err(Failure::Setup(e)) => return Err(e),
        Err(Failure::Query(e)) => e,
    };
    match session(socket, magic, budget, QueryShape::Kinded).await {
        Ok(snap) => Ok(snap),
        Err(Failure::Setup(e)) | Err(Failure::Query(e)) => Err(anyhow!(
            "snapshot query failed as [34] ({first:#}) and as [34, 1] ({e:#})"
        )),
    }
}

async fn session(
    socket: &Path,
    magic: u64,
    budget: Duration,
    shape: QueryShape,
) -> Result<Snapshot, Failure> {
    let bearer = Bearer::connect_unix(socket)
        .await
        .with_context(|| format!("connecting to {}", socket.display()))
        .map_err(Failure::Setup)?;

    let mut plexer = Plexer::new(bearer);
    let hs_channel = plexer.subscribe_client(PROTOCOL_N2C_HANDSHAKE);
    let sq_channel = plexer.subscribe_client(PROTOCOL_N2C_STATE_QUERY);
    let running = plexer.spawn();

    let result = timeout(budget, async {
        let setup = |e: anyhow::Error| Failure::Setup(e);

        let mut hs = HandshakeClient::new(hs_channel);
        let version = match hs.handshake(n2c::VersionTable::v10_and_above(magic)).await {
            Ok(Confirmation::Accepted(v, _)) => v,
            Ok(Confirmation::Rejected(reason)) => {
                return Err(setup(anyhow!("node-to-client handshake rejected: {reason:?}")))
            }
            Ok(Confirmation::QueryReply(_)) => {
                return Err(setup(anyhow!("node answered the handshake with a version query")))
            }
            Err(e) => return Err(setup(anyhow!("node-to-client handshake: {e}"))),
        };

        let mut client = StateQueryClient::new(sq_channel);
        client
            .acquire(None)
            .await
            .map_err(|e| setup(anyhow!("acquiring the node's tip: {e}")))?;
        let era = queries_v16::get_current_era(&mut client)
            .await
            .map_err(|e| setup(anyhow!("querying the current era: {e}")))?;
        let point = queries_v16::get_chain_point(&mut client)
            .await
            .map_err(|e| setup(anyhow!("querying the chain point: {e}")))?;

        let request = AnyCbor::from_raw_bytes(encode_query(era, shape));
        let response = client
            .query_any(request)
            .await
            .map_err(|e| Failure::Query(anyhow!("GetBigLedgerPeerSnapshot as {shape:?}: {e}")))?;
        let decoded = decode_response(response.raw_bytes())
            .map_err(|e| Failure::Query(anyhow!("decoding the snapshot ({shape:?}): {e}")))?;

        let _ = client.send_release().await;
        let _ = client.send_done().await;

        let (block_point_slot, block_point_hash) = match decoded.point.or(match &point {
            Point::Origin => None,
            Point::Specific(slot, hash) => Some((*slot, hex::encode(hash))),
        }) {
            Some(p) => p,
            None => (0, "origin".to_string()),
        };

        Ok(Snapshot {
            network_magic: decoded.magic.unwrap_or(magic),
            // N2C versions travel with bit 15 set to tell them from N2N.
            node_to_client_version: version & 0x7fff,
            point: SnapshotPoint {
                block_point_hash,
                block_point_slot,
            },
            pools: decoded.pools,
        })
    })
    .await;

    running.abort().await;

    match result {
        Ok(r) => r,
        Err(_) => Err(Failure::Setup(anyhow!(
            "no snapshot from {} within {}s",
            socket.display(),
            budget.as_secs()
        ))),
    }
}

/// `Request::LedgerQuery(BlockQuery(era, q))` is `[0, [0, [era, q]]]`.
fn encode_query(era: u16, shape: QueryShape) -> Vec<u8> {
    let mut e = Encoder::new(Vec::new());
    let r: Result<(), pallas_codec::minicbor::encode::Error<std::convert::Infallible>> = (|| {
        e.array(2)?.u8(0)?.array(2)?.u8(0)?.array(2)?.u16(era)?;
        match shape {
            QueryShape::Plain => e.array(1)?.u8(34)?,
            QueryShape::Kinded => e.array(2)?.u8(34)?.u8(1)?,
        };
        Ok(())
    })();
    r.expect("encoding into a Vec cannot fail");
    e.into_writer()
}

struct Decoded {
    pools: Vec<Pool>,
    /// Present in the V23 layout only.
    point: Option<(u64, String)>,
    magic: Option<u64>,
}

/// The result arrives inside the hard fork combinator's `Either` wrapper:
/// `[result]` on success, `[era, era]` on an era mismatch.
fn decode_response(bytes: &[u8]) -> Result<Decoded> {
    let mut d = Decoder::new(bytes);
    match d.array()? {
        Some(1) => {}
        Some(2) => bail!("era mismatch reported by the node"),
        other => bail!("unexpected result wrapper {other:?}"),
    }
    decode_snapshot(&mut d)
}

/// `[1, [withOrigin, pools]]` through consensus 0.26; `[2, [point, magic, pools]]`
/// for the big-ledger-peer V23 layout after it.
fn decode_snapshot(d: &mut Decoder) -> Result<Decoded> {
    expect_array(d, 2, "snapshot")?;
    let version = d.u8()?;
    match version {
        1 => {
            expect_array(d, 2, "snapshot body")?;
            let _slot = decode_with_origin(d)?;
            let pools = decode_pools(d)?;
            Ok(Decoded {
                pools,
                point: None,
                magic: None,
            })
        }
        2 => {
            expect_array(d, 3, "snapshot body")?;
            let point = decode_point(d)?;
            let magic = u64::from(d.u32()?);
            let pools = decode_pools_flat(d)?;
            Ok(Decoded {
                pools,
                point,
                magic: Some(magic),
            })
        }
        3 => bail!("node returned an all-ledger-peers snapshot, not big ledger peers"),
        v => bail!("unknown snapshot version {v}"),
    }
}

fn decode_with_origin(d: &mut Decoder) -> Result<Option<u64>> {
    let len = d.array()?;
    let tag = d.u8()?;
    match (len, tag) {
        (Some(1), 0) => Ok(None),
        (Some(2), 1) => Ok(Some(d.u64()?)),
        _ => bail!("unexpected WithOrigin encoding"),
    }
}

fn decode_point(d: &mut Decoder) -> Result<Option<(u64, String)>> {
    let len = d.array()?;
    let tag = d.u8()?;
    match (len, tag) {
        (Some(1), 0) => Ok(None),
        (Some(3), 1) => {
            let slot = d.u64()?;
            let hash = hex::encode(d.bytes()?);
            Ok(Some((slot, hash)))
        }
        _ => bail!("unexpected Point encoding"),
    }
}

/// `[(accumulatedStake, (relativeStake, [relay]))]`, stakes as tag 30 rationals.
fn decode_pools(d: &mut Decoder) -> Result<Vec<Pool>> {
    let mut pools = Vec::new();
    for_each(d, |d| {
        expect_array(d, 2, "pool")?;
        let accumulated_stake = decode_rational(d)?;
        expect_array(d, 2, "pool stake and relays")?;
        let relative_stake = decode_rational(d)?;
        let mut relays = Vec::new();
        for_each(d, |d| {
            relays.push(decode_relay(d)?);
            Ok(())
        })?;
        pools.push(Pool {
            accumulated_stake,
            relative_stake,
            relays,
        });
        Ok(())
    })?;
    Ok(pools)
}

/// `[(accumulatedStake, relativeStake, [relay])]`, the version-2 body from
/// ouroboros-network 1.2: one flat 3-element array per pool.
fn decode_pools_flat(d: &mut Decoder) -> Result<Vec<Pool>> {
    let mut pools = Vec::new();
    for_each(d, |d| {
        expect_array(d, 3, "pool")?;
        let accumulated_stake = decode_rational(d)?;
        let relative_stake = decode_rational(d)?;
        let mut relays = Vec::new();
        for_each(d, |d| {
            relays.push(decode_relay(d)?);
            Ok(())
        })?;
        pools.push(Pool {
            accumulated_stake,
            relative_stake,
            relays,
        });
        Ok(())
    })?;
    Ok(pools)
}

/// `[0, port, domain]`, `[1, port, [a, b, c, d]]`, `[2, port, [8 x u16]]`, or `[3, domain]` for SRV.
fn decode_relay(d: &mut Decoder) -> Result<Relay> {
    let len = d.array()?;
    let tag = d.u8()?;
    match (len, tag) {
        (Some(3), 0) => {
            let port = decode_port(d)?;
            let address = decode_domain(d)?;
            Ok(Relay {
                address,
                port: Some(port),
            })
        }
        (Some(3), 1) => {
            let port = decode_port(d)?;
            let octets = decode_u64_array(d, 4, "IPv4")?;
            let ip = Ipv4Addr::new(octets[0] as u8, octets[1] as u8, octets[2] as u8, octets[3] as u8);
            Ok(Relay {
                address: ip.to_string(),
                port: Some(port),
            })
        }
        (Some(3), 2) => {
            let port = decode_port(d)?;
            let w = decode_u64_array(d, 8, "IPv6")?;
            let ip = Ipv6Addr::new(
                w[0] as u16, w[1] as u16, w[2] as u16, w[3] as u16,
                w[4] as u16, w[5] as u16, w[6] as u16, w[7] as u16,
            );
            Ok(Relay {
                address: ip.to_string(),
                port: Some(port),
            })
        }
        (Some(2), 3) => Ok(Relay {
            address: decode_domain(d)?,
            port: None,
        }),
        _ => bail!("unexpected relay access point encoding ({len:?}, {tag})"),
    }
}

fn decode_port(d: &mut Decoder) -> Result<u16> {
    let v: i128 = d.int()?.into();
    u16::try_from(v).map_err(|_| anyhow!("port {v} out of range"))
}

fn decode_domain(d: &mut Decoder) -> Result<String> {
    let raw = match d.datatype()? {
        Type::String | Type::StringIndef => d.str()?.to_string(),
        _ => String::from_utf8_lossy(d.bytes()?).into_owned(),
    };
    Ok(raw.trim_end_matches('.').to_string())
}

fn decode_u64_array(d: &mut Decoder, n: usize, what: &str) -> Result<Vec<u64>> {
    let mut out = Vec::with_capacity(n);
    for_each(d, |d| {
        let v: i128 = d.int()?.into();
        out.push(u64::try_from(v).map_err(|_| anyhow!("{what} component {v} out of range"))?);
        Ok(())
    })?;
    if out.len() != n {
        bail!("{what} has {} components, expected {n}", out.len());
    }
    Ok(out)
}

/// cardano-binary writes a Rational as tag 30 over `[numerator, denominator]`.
fn decode_rational(d: &mut Decoder) -> Result<f64> {
    if d.datatype()? == Type::Tag {
        let tag = d.tag()?;
        if tag != Tag::new(30) {
            bail!("expected rational tag 30, got {}", u64::from(tag));
        }
    }
    expect_array(d, 2, "rational")?;
    let num = decode_integer_f64(d)?;
    let den = decode_integer_f64(d)?;
    if den == 0.0 {
        bail!("rational with zero denominator");
    }
    Ok(num / den)
}

/// An integer that may exceed 64 bits, as a bignum (tag 2 or 3 over bytes).
fn decode_integer_f64(d: &mut Decoder) -> Result<f64> {
    if d.datatype()? == Type::Tag {
        let tag = u64::from(d.tag()?);
        let negative = match tag {
            2 => false,
            3 => true,
            t => bail!("unexpected tag {t} where an integer was expected"),
        };
        let mut v = 0f64;
        for b in d.bytes()? {
            v = v * 256.0 + f64::from(*b);
        }
        return Ok(if negative { -1.0 - v } else { v });
    }
    let v: i128 = d.int()?.into();
    Ok(v as f64)
}

fn expect_array(d: &mut Decoder, n: u64, what: &str) -> Result<()> {
    match d.array()? {
        Some(len) if len == n => Ok(()),
        other => bail!("{what}: expected a {n}-element array, got {other:?}"),
    }
}

/// Run `f` once per element of a definite or indefinite length array.
fn for_each(d: &mut Decoder, mut f: impl FnMut(&mut Decoder) -> Result<()>) -> Result<()> {
    match d.array()? {
        Some(n) => {
            for _ in 0..n {
                f(d)?;
            }
        }
        None => {
            while d.datatype()? != Type::Break {
                f(d)?;
            }
            d.skip()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rational(e: &mut Encoder<Vec<u8>>, num: u64, den: u64) {
        e.tag(Tag::new(30)).unwrap().array(2).unwrap().u64(num).unwrap().u64(den).unwrap();
    }

    #[test]
    fn plain_query_shape() {
        // [0, [0, [6, [34]]]]
        assert_eq!(encode_query(6, QueryShape::Plain), vec![0x82, 0x00, 0x82, 0x00, 0x82, 0x06, 0x81, 0x18, 0x22]);
        assert_eq!(encode_query(6, QueryShape::Kinded), vec![0x82, 0x00, 0x82, 0x00, 0x82, 0x06, 0x82, 0x18, 0x22, 0x01]);
    }

    #[test]
    fn decodes_v2_snapshot_with_every_relay_kind() {
        let mut e = Encoder::new(Vec::new());
        e.array(1).unwrap(); // Either Right
        e.array(2).unwrap().u8(1).unwrap(); // version 1
        e.array(2).unwrap();
        e.array(2).unwrap().u8(1).unwrap().u64(194344226).unwrap(); // At slot
        e.array(2).unwrap(); // two pools
        // pool 1: domain + ipv4
        e.array(2).unwrap();
        rational(&mut e, 1, 100);
        e.array(2).unwrap();
        rational(&mut e, 1, 100);
        e.array(2).unwrap();
        e.array(3).unwrap().u8(0).unwrap().u16(3001).unwrap().bytes(b"relay.example.org.").unwrap();
        e.array(3).unwrap().u8(1).unwrap().u16(6000).unwrap();
        e.array(4).unwrap().u8(10).unwrap().u8(0).unwrap().u8(0).unwrap().u8(1).unwrap();
        // pool 2: ipv6 + srv, indefinite relay list
        e.array(2).unwrap();
        rational(&mut e, 3, 100);
        e.array(2).unwrap();
        rational(&mut e, 2, 100);
        e.begin_array().unwrap();
        e.array(3).unwrap().u8(2).unwrap().u16(3001).unwrap();
        e.array(8).unwrap();
        for w in [0x2001u16, 0xdb8, 0, 0, 0, 0, 0, 1] {
            e.u16(w).unwrap();
        }
        e.array(2).unwrap().u8(3).unwrap().bytes(b"_cardano._tcp.example.org").unwrap();
        e.end().unwrap();

        let decoded = decode_response(&e.into_writer()).unwrap();
        assert_eq!(decoded.pools.len(), 2);
        assert!(decoded.point.is_none());
        let p1 = &decoded.pools[0];
        assert!((p1.relative_stake - 0.01).abs() < 1e-12);
        assert_eq!(p1.relays[0].address, "relay.example.org");
        assert_eq!(p1.relays[0].port, Some(3001));
        assert_eq!(p1.relays[1].address, "10.0.0.1");
        assert_eq!(p1.relays[1].port, Some(6000));
        let p2 = &decoded.pools[1];
        assert!((p2.accumulated_stake - 0.03).abs() < 1e-12);
        assert_eq!(p2.relays[0].address, "2001:db8::1");
        assert_eq!(p2.relays[1].address, "_cardano._tcp.example.org");
        assert_eq!(p2.relays[1].port, None);
    }

    #[test]
    fn decodes_v23_snapshot_point_and_magic() {
        let mut e = Encoder::new(Vec::new());
        e.array(1).unwrap();
        e.array(2).unwrap().u8(2).unwrap();
        e.array(3).unwrap();
        e.array(3).unwrap().u8(1).unwrap().u64(42).unwrap().bytes(&[0xab; 32]).unwrap();
        e.u32(764824073).unwrap();
        // Version 2 pools are flat: [acc, rel, relays].
        e.begin_array().unwrap();
        e.array(3).unwrap();
        rational(&mut e, 1, 100);
        rational(&mut e, 1, 100);
        e.array(2).unwrap();
        e.array(3).unwrap().u8(0).unwrap().u16(3001).unwrap().bytes(b"relay.example.org").unwrap();
        e.array(3).unwrap().u8(1).unwrap().u16(6000).unwrap();
        e.array(4).unwrap().u8(10).unwrap().u8(0).unwrap().u8(0).unwrap().u8(1).unwrap();
        e.array(3).unwrap();
        rational(&mut e, 3, 100);
        rational(&mut e, 2, 100);
        e.array(1).unwrap();
        e.array(2).unwrap().u8(3).unwrap().bytes(b"_cardano._tcp.example.org").unwrap();
        e.end().unwrap();
        let decoded = decode_response(&e.into_writer()).unwrap();
        assert_eq!(decoded.magic, Some(764824073));
        assert_eq!(decoded.point.as_ref().unwrap().0, 42);
        assert_eq!(decoded.point.as_ref().unwrap().1.len(), 64);
        assert_eq!(decoded.pools.len(), 2);
        assert!((decoded.pools[0].accumulated_stake - 0.01).abs() < 1e-12);
        assert_eq!(decoded.pools[0].relays[1].address, "10.0.0.1");
        assert!((decoded.pools[1].relative_stake - 0.02).abs() < 1e-12);
        assert_eq!(decoded.pools[1].relays[0].port, None);
    }

    #[test]
    fn v23_snapshot_rejects_the_nested_pool_shape() {
        let mut e = Encoder::new(Vec::new());
        e.array(1).unwrap();
        e.array(2).unwrap().u8(2).unwrap();
        e.array(3).unwrap();
        e.array(1).unwrap().u8(0).unwrap();
        e.u32(1).unwrap();
        e.array(1).unwrap();
        e.array(2).unwrap();
        rational(&mut e, 1, 100);
        e.array(2).unwrap();
        rational(&mut e, 1, 100);
        e.array(0).unwrap();
        assert!(decode_response(&e.into_writer()).is_err());
    }

    #[test]
    fn era_mismatch_is_an_error() {
        let mut e = Encoder::new(Vec::new());
        e.array(2).unwrap().u8(6).unwrap().u8(5).unwrap();
        assert!(decode_response(&e.into_writer()).is_err());
    }
}
