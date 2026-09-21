use pallas_network::multiplexer::Bearer;
use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::time::timeout;

/// Why a connect attempt produced no bearer.
#[derive(Debug)]
pub enum ConnectError {
    Connect(String),
    Timeout,
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectError::Connect(m) => f.write_str(m),
            ConnectError::Timeout => f.write_str("timeout"),
        }
    }
}

/// Whether this host has a route to the public IPv6 internet. A UDP connect
/// only selects the route, nothing is sent.
pub fn has_ipv6_route() -> bool {
    std::net::UdpSocket::bind("[::]:0")
        .and_then(|s| s.connect("[2001:4860:4860::8888]:53"))
        .is_ok()
}

/// host:port for display and keys, bracketing IPv6 literals.
pub fn format_host_port(host: &str, port: u16) -> String {
    if host.starts_with('[') {
        format!("{}:{}", host, port)
    } else if host.contains(':') {
        format!("[{}]:{}", host, port)
    } else {
        format!("{}:{}", host, port)
    }
}

pub struct ConnectResult {
    pub bearer: Bearer,
    pub addr: SocketAddr,
}

/// Connect to one socket address within `budget`. Names are resolved before
/// probing, one endpoint per address, so there is nothing to look up here.
pub async fn connect(addr: &str, budget: Duration) -> Result<ConnectResult, ConnectError> {
    let sock: SocketAddr = addr
        .parse()
        .map_err(|_| ConnectError::Connect(format!("Invalid address: {addr}")))?;
    match timeout(budget, Bearer::connect_tcp(sock)).await {
        Ok(Ok(bearer)) => Ok(ConnectResult { bearer, addr: sock }),
        Ok(Err(e)) => Err(ConnectError::Connect(format!("Connect error to {sock}: {e}"))),
        Err(_) => Err(ConnectError::Timeout),
    }
}
