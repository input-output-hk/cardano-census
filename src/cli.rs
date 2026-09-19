use clap::Parser;
use std::path::PathBuf;

/// Probe every relay in a peer snapshot once and report what answered, weighted by stake.
#[derive(Parser, Debug)]
#[command(name = "cardano-census", version)]
pub struct Args {
    /// peerSnapshotV3 file naming the big ledger pools and their relays
    #[arg(long, value_name = "FILE")]
    pub snapshot: PathBuf,

    /// Metrics file in node exporter textfile format; "-" writes to stdout
    #[arg(long, value_name = "FILE", default_value = "-")]
    pub output: PathBuf,

    /// Per-relay JSON report
    #[arg(long, value_name = "FILE")]
    pub report: Option<PathBuf>,

    /// Network magic; defaults to the snapshot's NetworkMagic
    #[arg(long, value_name = "MAGIC")]
    pub network_magic: Option<u64>,

    /// Port for relays that list none
    #[arg(long, default_value_t = 3001)]
    pub port: u16,

    /// Seconds allowed per relay for DNS, connect, handshake and tip together
    #[arg(long, default_value_t = 60)]
    pub timeout: u64,

    /// Relays probed at once
    #[arg(long, default_value_t = 128)]
    pub parallel: usize,

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
