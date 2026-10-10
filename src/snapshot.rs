use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

/// A cardano-node peer snapshot, peerSnapshotV3 layout.
#[derive(Debug, Deserialize)]
pub struct Snapshot {
    #[serde(rename = "NetworkMagic")]
    pub network_magic: u64,
    #[serde(rename = "NodeToClientVersion")]
    pub node_to_client_version: u64,
    #[serde(rename = "Point")]
    pub point: Point,
    #[serde(rename = "bigLedgerPools")]
    pub pools: Vec<Pool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Point {
    pub block_point_hash: String,
    pub block_point_slot: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pool {
    pub accumulated_stake: f64,
    pub relative_stake: f64,
    pub relays: Vec<Relay>,
}

#[derive(Debug, Deserialize)]
pub struct Relay {
    pub address: String,
    pub port: Option<u16>,
}

pub fn load(path: &Path) -> Result<Snapshot> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text)
        .with_context(|| format!("parsing {} as peerSnapshotV3", path.display()))
}
