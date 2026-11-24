use std::net::{IpAddr, Ipv4Addr};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;

/// Error returned when a provider cannot produce an IP address this poll.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// The HTTP request failed, timed out, or the body was not a valid IP.
    #[error("provider request failed: {0}")]
    Request(#[from] reqwest::Error),
    /// The provider returned a body that does not parse as an IP address.
    #[error("provider returned unparseable body: {0:?}")]
    Unparseable(String),
}

/// A source of the machine's external IP address.
///
/// Implementations must be `Send + Sync` (the watcher holds one per
/// configured provider) and must not perform blocking I/O.
#[async_trait]
pub trait IpProvider: Send + Sync + 'static {
    /// Fetch the external IP. Returns Err on any failure (network, timeout,
    /// unparseable body) so the watcher can vote on the remaining providers.
    async fn fetch_ip(&self) -> Result<IpAddr, ProviderError>;
}

/// Provider backed by a plain-text HTTP endpoint (e.g. `https://api.ipify.org`
/// or `https://ifconfig.me/ip`), which return just the IP with no JSON.
#[derive(Clone, Debug)]
pub struct HttpProvider {
    url: String,
    client: reqwest::Client,
}

impl HttpProvider {
    /// Create a provider for `url` with the given per-request timeout.
    pub fn new(url: impl Into<String>, timeout: Duration) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            // Force IPv4 sockets ([::] unbound, 0.0.0.0 bound): the echo
            // services resolve to both A and AAAA records, and on dual-stack
            // hosts the v4 and v6 egress addresses differ, so mixed-family
            // answers would look like provider disagreement forever.
            .local_address(Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED)))
            // Disable connection reuse: a pooled TCP connection stays bound to
            // the interface it was established on, so after a VPN (tunnel)
            // comes up or goes down, reused sockets keep reporting the OLD
            // egress IP and the watcher never sees the route change.
            .pool_max_idle_per_host(0)
            .build()
            .expect("reqwest client builder");
        Self {
            url: url.into(),
            client,
        }
    }
}

#[async_trait]
impl IpProvider for HttpProvider {
    async fn fetch_ip(&self) -> Result<IpAddr, ProviderError> {
        let body = self.client.get(&self.url).send().await?.text().await?;
        let trimmed = body.trim();
        trimmed
            .parse::<IpAddr>()
            .map_err(|_| ProviderError::Unparseable(trimmed.to_string()))
    }
}

/// Test double returning a scripted sequence of results, one per poll.
///
/// When the script is exhausted, the last entry repeats forever.
#[derive(Debug)]
pub struct MockProvider {
    script: Mutex<Vec<Result<IpAddr, ProviderError>>>,
    next: std::sync::atomic::AtomicUsize,
}

impl MockProvider {
    pub fn new(script: Vec<Result<IpAddr, ProviderError>>) -> Self {
        Self {
            script: Mutex::new(script),
            next: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl IpProvider for MockProvider {
    async fn fetch_ip(&self) -> Result<IpAddr, ProviderError> {
        let guard = self.script.lock().unwrap_or_else(|e| e.into_inner());
        let idx = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let copy = |r: &Result<IpAddr, ProviderError>| match r {
            Ok(ip) => Ok(*ip),
            Err(e) => Err(ProviderError::Unparseable(format!("{e}"))),
        };
        match guard.get(idx) {
            Some(res) => copy(res),
            // Script exhausted: repeat the last entry forever.
            None => guard
                .last()
                .map(copy)
                .unwrap_or(Err(ProviderError::Unparseable(String::new()))),
        }
    }
}
