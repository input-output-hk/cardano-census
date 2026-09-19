use clap::Parser;
use std::path::PathBuf;

use crate::node::parse_magic;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Set by build.rs from the flake's `GIT_REV` or from git itself.
pub const GIT_REV: &str = env!("CARDANO_CENSUS_GIT_REV");

/// Probe every relay in a peer snapshot once and report what answered, weighted by stake.
#[derive(Parser, Debug)]
#[command(name = "cardano-census", version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("CARDANO_CENSUS_GIT_REV"), ")"))]
pub struct Args {
    /// peerSnapshotV3 file naming the big ledger pools and their relays
    #[arg(long, value_name = "FILE", required_unless_present = "node_socket", conflicts_with = "node_socket")]
    pub snapshot: Option<PathBuf>,

    /// Query the snapshot from a local cardano-node over this socket instead
    #[arg(long, value_name = "PATH", requires = "network_magic")]
    pub node_socket: Option<PathBuf>,

    /// Network magic, or "mainnet"; taken from the snapshot file when omitted
    #[arg(long, value_name = "MAGIC", value_parser = parse_magic)]
    pub network_magic: Option<u64>,

    /// Metrics file in node exporter textfile format; "-" writes to stdout
    #[arg(long, value_name = "FILE", default_value = "-")]
    pub output: PathBuf,

    /// Label stamped on every series, as KEY=VALUE; repeatable
    #[arg(long = "label", value_name = "KEY=VALUE", value_parser = parse_label)]
    pub labels: Vec<(String, String)>,

    /// Per-relay JSON report
    #[arg(long, value_name = "FILE")]
    pub report: Option<PathBuf>,

    /// Seconds allowed per relay for DNS, connect, handshake and tip together
    #[arg(long, default_value_t = 60)]
    pub timeout: u64,

    /// Relays probed at once
    #[arg(long, default_value_t = 128)]
    pub parallel: usize,

    /// Tips this many blocks apart or closer count as the same chain
    #[arg(long, default_value_t = 10)]
    pub fork_tolerance: u64,

    /// iptoasn.com ip2asn TSV for placing relays in autonomous systems
    #[arg(long, value_name = "FILE")]
    pub asn_db: Option<PathBuf>,

    /// Autonomous systems hosting fewer relays than this are folded into "other"
    #[arg(long, default_value_t = 3)]
    pub asn_min_relays: usize,

    /// Try IPv4 first instead of IPv6
    #[arg(long)]
    pub prefer_ipv4: bool,

    /// Milliseconds to wait for the preferred family before trying the other
    #[arg(long, default_value_t = 500)]
    pub happy_eyeballs_delay_ms: u64,
}

impl Args {
    pub fn output_is_stdout(&self) -> bool {
        self.output.as_os_str() == "-"
    }
}

/// `KEY=VALUE` with a Prometheus label name as the key.
fn parse_label(s: &str) -> Result<(String, String), String> {
    let (key, value) = s
        .split_once('=')
        .ok_or_else(|| format!("expected KEY=VALUE, got {s:?}"))?;
    let valid = !key.is_empty()
        && !key.starts_with("__")
        && key
            .chars()
            .enumerate()
            .all(|(i, c)| c == '_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()));
    if !valid {
        return Err(format!("{key:?} is not a valid label name"));
    }
    Ok((key.to_string(), value.to_string()))
}
