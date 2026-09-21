use pallas_network::{
    miniprotocols::{
        chainsync::{Client as ChainSyncClient, HeaderContent},
        handshake::{n2n, Client as HandshakeClient, Confirmation},
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
}

pub type Outcome = Result<Reached, Failed>;

/// Connect, complete the N2N handshake, and ask for the tip with one FindIntersect.
/// `budget` covers all three together, measured from the first DNS lookup.
pub async fn probe(addr: &str, magic: u64, budget: Duration) -> Outcome {
    let started = Instant::now();
    let deadline = started + budget;

    let conn = match connect(addr, budget).await {
        Ok(c) => c,
        Err(e @ (ConnectError::Connect(_) | ConnectError::Timeout)) => {
            return Err(Failed { stage: Stage::Connect, error: e.to_string() });
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
                return Err(Failed { stage: Stage::Handshake, error: format!("rejected: {:?}", reason) })
            }
            Ok(Confirmation::QueryReply(_)) => {
                return Err(Failed { stage: Stage::Handshake, error: "peer answered with a version query".into() })
            }
            Err(e) => return Err(Failed { stage: Stage::Handshake, error: e.to_string() }),
        };

        *stage.lock().unwrap() = Stage::Chainsync;
        let mut cs = ChainSyncClient::<HeaderContent>::new(cs_channel);
        let (_intersect, tip) = cs
            .find_intersect(vec![Point::Origin])
            .await
            .map_err(|e| Failed { stage: Stage::Chainsync, error: e.to_string() })?;

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
        }),
    }
}
