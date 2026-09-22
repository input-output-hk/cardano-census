use pallas_codec::minicbor::{decode, encode, Decode, Decoder, Encode, Encoder};
use pallas_network::{
    miniprotocols::{
        chainsync::{Client as ChainSyncClient, HeaderContent},
        handshake::{n2n, Client as HandshakeClient, Confirmation, RefuseReason, VersionTable},
        Point, PROTOCOL_N2N_CHAIN_SYNC, PROTOCOL_N2N_HANDSHAKE,
    },
    multiplexer::Plexer,
};
use serde::Serialize;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::time::timeout;

use crate::net::{connect, ConnectError};

/// How far a failed probe got.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    Srv,
    Dns,
    /// Resolved only to private or reserved space, so it was never dialled;
    /// loopback and 0.0.0.0 would reach the census host's own node.
    Address,
    Connect,
    Handshake,
    Chainsync,
}

impl Stage {
    pub const ALL: [Stage; 6] =
        [Stage::Srv, Stage::Dns, Stage::Address, Stage::Connect, Stage::Handshake, Stage::Chainsync];

    pub fn label(self) -> &'static str {
        match self {
            Stage::Srv => "srv",
            Stage::Dns => "dns",
            Stage::Address => "address",
            Stage::Connect => "connect",
            Stage::Handshake => "handshake",
            Stage::Chainsync => "chainsync",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Family {
    V4,
    V6,
}

#[derive(Clone, Debug, Serialize)]
pub struct Tip {
    pub slot: u64,
    pub hash: String,
    pub block: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Reached {
    pub peer: SocketAddr,
    pub family: Family,
    pub n2n_version: String,
    pub tip: Tip,
    pub rtt_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Failed {
    pub stage: Stage,
    pub error: String,
    /// The versions a relay said it supports when it refused ours, so a
    /// relay the census can no longer talk to still reports what it speaks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub versions: Option<Vec<u64>>,
}

pub type Outcome = Result<Reached, Failed>;

/// The node-to-node versions this census proposes, lowest first. The
/// negotiated version can never exceed the last of these.
pub fn offered_versions(magic: u64) -> Vec<u64> {
    let mut v: Vec<u64> = n2n::VersionTable::v11_and_above(magic).values.keys().copied().collect();
    v.sort_unstable();
    v
}

/// Version data for the survey handshake. Encodes the four-field node-to-node
/// data with the query flag set; decodes by skipping whatever a version's
/// data holds, so a reply listing versions whose data has grown a field, as
/// V16 did, still decodes as a whole.
#[derive(Clone, Debug)]
struct SurveyData {
    magic: u64,
}

impl Encode<()> for SurveyData {
    fn encode<W: encode::Write>(&self, e: &mut Encoder<W>, _: &mut ()) -> Result<(), encode::Error<W::Error>> {
        // magic, initiator only, peer sharing disabled, query
        e.array(4)?.u64(self.magic)?.bool(true)?.u8(0)?.bool(true)?;
        Ok(())
    }
}

impl<'b> Decode<'b, ()> for SurveyData {
    fn decode(d: &mut Decoder<'b>, _: &mut ()) -> Result<Self, decode::Error> {
        d.skip()?;
        Ok(SurveyData { magic: 0 })
    }
}

/// Ask a relay which node-to-node versions it supports, lowest first. The
/// handshake's query flag makes the responder answer with its whole version
/// table instead of picking one, and the connection ends there. A relay that
/// shares no version with the census refuses before it reads the flag, so
/// such relays never get here; `probe` records their versions from the
/// refusal instead.
pub async fn query_versions(addr: &str, magic: u64, budget: Duration) -> Result<Vec<u64>, String> {
    let started = Instant::now();
    let conn = connect(addr, budget).await.map_err(|e| e.to_string())?;
    let mut plexer = Plexer::new(conn.bearer);
    let hs_channel = plexer.subscribe_client(PROTOCOL_N2N_HANDSHAKE);
    let running = plexer.spawn();
    let remaining = budget.saturating_sub(started.elapsed());
    let result = timeout(remaining, async {
        let mut hs: HandshakeClient<SurveyData> = HandshakeClient::new(hs_channel);
        let table = VersionTable {
            values: offered_versions(magic).into_iter().map(|v| (v, SurveyData { magic })).collect(),
        };
        match hs.handshake(table).await {
            Ok(Confirmation::QueryReply(table)) => {
                let mut v: Vec<u64> = table.values.keys().copied().collect();
                v.sort_unstable();
                Ok(v)
            }
            Ok(Confirmation::Accepted(v, _)) => Err(format!("accepted version {v} instead of answering the query")),
            Ok(Confirmation::Rejected(reason)) => Err(format!("rejected: {reason:?}")),
            Err(e) => Err(e.to_string()),
        }
    })
    .await;
    running.abort().await;
    match result {
        Ok(r) => r,
        Err(_) => Err("timeout".into()),
    }
}

/// Connect, complete the N2N handshake, and ask for the tip with one FindIntersect.
/// `budget` covers all three together, measured from the first DNS lookup.
pub async fn probe(addr: &str, magic: u64, budget: Duration) -> Outcome {
    let started = Instant::now();
    let deadline = started + budget;

    let conn = match connect(addr, budget).await {
        Ok(c) => c,
        Err(e @ (ConnectError::Connect(_) | ConnectError::Timeout)) => {
            return Err(Failed { stage: Stage::Connect, error: e.to_string(), versions: None });
        }
    };
    let peer = conn.addr;
    let family = if peer.is_ipv6() { Family::V6 } else { Family::V4 };

    let mut plexer = Plexer::new(conn.bearer);
    let hs_channel = plexer.subscribe_client(PROTOCOL_N2N_HANDSHAKE);
    let cs_channel = plexer.subscribe_client(PROTOCOL_N2N_CHAIN_SYNC);
    let running = plexer.spawn();

    let stage = Mutex::new(Stage::Handshake);
    let remaining = deadline.saturating_duration_since(Instant::now());
    let result = timeout(remaining, async {
        let mut hs = HandshakeClient::new(hs_channel);
        let versions = n2n::VersionTable::v11_and_above(magic);
        let n2n_version = match hs.handshake(versions).await {
            Ok(Confirmation::Accepted(v, _)) => v.to_string(),
            Ok(Confirmation::Rejected(reason)) => {
                // A relay with no version in common names the ones it has.
                let versions = match &reason {
                    RefuseReason::VersionMismatch(vs) => {
                        let mut vs = vs.clone();
                        vs.sort_unstable();
                        Some(vs)
                    }
                    _ => None,
                };
                return Err(Failed { stage: Stage::Handshake, error: format!("rejected: {:?}", reason), versions });
            }
            Ok(Confirmation::QueryReply(_)) => {
                return Err(Failed { stage: Stage::Handshake, error: "peer answered with a version query".into(), versions: None })
            }
            Err(e) => return Err(Failed { stage: Stage::Handshake, error: e.to_string(), versions: None }),
        };

        *stage.lock().unwrap() = Stage::Chainsync;
        let mut cs = ChainSyncClient::<HeaderContent>::new(cs_channel);
        let (_intersect, tip) = cs
            .find_intersect(vec![Point::Origin])
            .await
            .map_err(|e| Failed { stage: Stage::Chainsync, error: e.to_string(), versions: None })?;

        let (slot, hash) = match &tip.0 {
            Point::Origin => (0, "origin".to_string()),
            Point::Specific(s, h) => (*s, hex::encode(h)),
        };
        Ok((n2n_version, Tip { slot, hash, block: tip.1 }))
    })
    .await;

    running.abort().await;

    match result {
        Ok(Ok((n2n_version, tip))) => Ok(Reached {
            peer,
            family,
            n2n_version,
            tip,
            rtt_ms: started.elapsed().as_millis() as u64,
        }),
        Ok(Err(f)) => Err(f),
        Err(_) => Err(Failed {
            stage: *stage.lock().unwrap(),
            error: "timeout".into(),
            versions: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A query reply with a four-field v14 entry, the five-field v16 entry
    /// node 11.1 sends, and a four-field v17 after it.
    fn reply_with_v16_and_v17() -> Vec<u8> {
        let mut e = Encoder::new(Vec::new());
        e.map(3).unwrap();
        e.u64(14).unwrap();
        e.array(4).unwrap().u64(764824073).unwrap().bool(false).unwrap().u8(0).unwrap().bool(false).unwrap();
        e.u64(16).unwrap();
        e.array(5).unwrap().u64(764824073).unwrap().bool(false).unwrap().u8(0).unwrap().bool(false).unwrap().u8(0).unwrap();
        e.u64(17).unwrap();
        e.array(4).unwrap().u64(764824073).unwrap().bool(false).unwrap().u8(0).unwrap().bool(false).unwrap();
        e.into_writer()
    }

    #[test]
    fn survey_decoder_reads_every_version_whatever_its_data_holds() {
        let bytes = reply_with_v16_and_v17();
        let table: VersionTable<SurveyData> = pallas_codec::minicbor::decode(&bytes).unwrap();
        let mut keys: Vec<u64> = table.values.keys().copied().collect();
        keys.sort_unstable();
        assert_eq!(keys, vec![14, 16, 17]);
        // The reason SurveyData exists: pallas's own payload reads four fields
        // of the five and takes the leftovers for the next entry.
        let theirs: Result<VersionTable<n2n::VersionData>, _> = pallas_codec::minicbor::decode(&bytes);
        assert!(theirs.is_err() || theirs.unwrap().values.keys().copied().max() != Some(17));
    }

    #[test]
    fn survey_proposal_carries_the_query_flag() {
        let table = VersionTable { values: [(14u64, SurveyData { magic: 2 })].into_iter().collect() };
        let mut e = Encoder::new(Vec::new());
        e.encode(&table).unwrap();
        // {14: [2, true, 0, true]}
        assert_eq!(e.into_writer(), vec![0xa1, 0x0e, 0x84, 0x02, 0xf5, 0x00, 0xf5]);
        assert_eq!(offered_versions(2), vec![11, 12, 13, 14]);
    }
}
