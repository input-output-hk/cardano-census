use pallas_network::multiplexer::Bearer;
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::net::lookup_host;
use tokio::time::{sleep, timeout};

static HAPPY_EYEBALLS_PREFER_V6: AtomicBool = AtomicBool::new(true);
static HAPPY_EYEBALLS_DELAY_MS: AtomicU64 = AtomicU64::new(500);

/// Set the address family tried first and how long it gets before the other starts.
pub fn set_happy_eyeballs_config(prefer_v6: bool, delay_ms: u64) {
    HAPPY_EYEBALLS_PREFER_V6.store(prefer_v6, Ordering::SeqCst);
    HAPPY_EYEBALLS_DELAY_MS.store(delay_ms, Ordering::SeqCst);
}

fn happy_eyeballs_config() -> (bool, u64) {
    (
        HAPPY_EYEBALLS_PREFER_V6.load(Ordering::SeqCst),
        HAPPY_EYEBALLS_DELAY_MS.load(Ordering::SeqCst),
    )
}

/// Why a connect attempt produced no bearer.
#[derive(Debug)]
pub enum ConnectError {
    Dns(String),
    Connect(String),
    Timeout,
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectError::Dns(m) | ConnectError::Connect(m) => f.write_str(m),
            ConnectError::Timeout => f.write_str("timeout"),
        }
    }
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

fn split_host_port(addr: &str) -> Result<(String, u16), ConnectError> {
    let invalid = || ConnectError::Connect(format!("Invalid address: {}", addr));
    if let Some(rest) = addr.strip_prefix('[') {
        let end = rest.find(']').ok_or_else(invalid)?;
        let host = &rest[..end];
        let port = rest[end + 1..]
            .strip_prefix(':')
            .and_then(|p| p.parse::<u16>().ok())
            .ok_or_else(invalid)?;
        return Ok((host.to_string(), port));
    }

    let (host, port_str) = addr.rsplit_once(':').ok_or_else(invalid)?;
    let port = port_str.parse::<u16>().map_err(|_| invalid())?;
    Ok((host.to_string(), port))
}

type ConnectFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Result<ConnectResult, String>> + Send>>;

pub struct ConnectResult {
    pub bearer: Bearer,
    pub addr: SocketAddr,
}

async fn connect_list(addrs: Vec<SocketAddr>) -> Result<ConnectResult, String> {
    let mut last_err: Option<String> = None;
    for addr in addrs {
        match Bearer::connect_tcp(addr).await {
            Ok(b) => return Ok(ConnectResult { bearer: b, addr }),
            Err(e) => last_err = Some(format!("{}: {}", addr, e)),
        }
    }
    Err(last_err.unwrap_or_else(|| "No addresses to connect to".to_string()))
}

async fn connect_happy_eyeballs_inner(addr: &str) -> Result<ConnectResult, ConnectError> {
    let (prefer_v6, delay_ms) = happy_eyeballs_config();
    let (host, port) = split_host_port(addr)?;
    let addrs: Vec<SocketAddr> = lookup_host((host.as_str(), port))
        .await
        .map_err(|e| ConnectError::Dns(format!("DNS lookup failed for {}: {}", addr, e)))?
        .collect();

    if addrs.is_empty() {
        return Err(ConnectError::Dns(format!("DNS lookup returned no addresses for {}", addr)));
    }

    let mut v6_addrs = Vec::new();
    let mut v4_addrs = Vec::new();
    for addr in addrs {
        match addr {
            SocketAddr::V6(_) => v6_addrs.push(addr),
            SocketAddr::V4(_) => v4_addrs.push(addr),
        }
    }

    let connect_err = |e: String| ConnectError::Connect(format!("Connect error to {}", e));

    if v6_addrs.is_empty() {
        return connect_list(v4_addrs).await.map_err(connect_err);
    }
    if v4_addrs.is_empty() {
        return connect_list(v6_addrs).await.map_err(connect_err);
    }

    let mut v6_err: Option<String> = None;
    let mut v4_err: Option<String> = None;
    let mut v6_done = false;
    let mut v4_done = false;

    let (primary_addrs, secondary_addrs, primary_is_v6) = if prefer_v6 {
        (v6_addrs, v4_addrs, true)
    } else {
        (v4_addrs, v6_addrs, false)
    };

    let mut primary_fut = Box::pin(connect_list(primary_addrs));
    let mut secondary_fut: Option<ConnectFuture> = None;

    if delay_ms == 0 {
        secondary_fut = Some(Box::pin(connect_list(secondary_addrs.clone())));
    }

    // Wait for the preferred family, or start the other after the delay.
    tokio::select! {
        res = &mut primary_fut => {
            if primary_is_v6 {
                v6_done = true;
            } else {
                v4_done = true;
            }
            match res {
                Ok(b) => return Ok(b),
                Err(e) => {
                    if primary_is_v6 {
                        v6_err = Some(e);
                    } else {
                        v4_err = Some(e);
                    }
                }
            }
        }
        _ = sleep(Duration::from_millis(delay_ms)), if secondary_fut.is_none() => {
            secondary_fut = Some(Box::pin(connect_list(secondary_addrs.clone())));
        }
    };

    if secondary_fut.is_none() {
        secondary_fut = Some(Box::pin(connect_list(secondary_addrs)));
    }
    let mut secondary_fut = secondary_fut.unwrap();

    loop {
        tokio::select! {
            res = &mut primary_fut, if !(if primary_is_v6 { v6_done } else { v4_done }) => {
                if primary_is_v6 {
                    v6_done = true;
                } else {
                    v4_done = true;
                }
                match res {
                    Ok(b) => return Ok(b),
                    Err(e) => {
                        if primary_is_v6 {
                            v6_err = Some(e);
                        } else {
                            v4_err = Some(e);
                        }
                    }
                }
            }
            res = &mut secondary_fut, if !(if primary_is_v6 { v4_done } else { v6_done }) => {
                if primary_is_v6 {
                    v4_done = true;
                } else {
                    v6_done = true;
                }
                match res {
                    Ok(b) => return Ok(b),
                    Err(e) => {
                        if primary_is_v6 {
                            v4_err = Some(e);
                        } else {
                            v6_err = Some(e);
                        }
                    }
                }
            }
        }

        if v6_done && v4_done {
            return Err(ConnectError::Connect(format!(
                "Connect error to {} (v6): {}; (v4): {}",
                addr,
                v6_err.unwrap_or_else(|| "unknown".to_string()),
                v4_err.unwrap_or_else(|| "unknown".to_string()),
            )));
        }
    }
}

/// Connect with Happy Eyeballs, returning the bearer and the address that answered.
/// The whole connect, DNS included, must finish within `timeout_duration`.
pub async fn connect_happy_eyeballs_with_addr(
    addr: &str,
    timeout_duration: Duration,
) -> Result<ConnectResult, ConnectError> {
    match timeout(timeout_duration, connect_happy_eyeballs_inner(addr)).await {
        Ok(res) => res,
        Err(_) => Err(ConnectError::Timeout),
    }
}
