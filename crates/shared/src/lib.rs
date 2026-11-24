use std::net::IpAddr;

/// Events emitted by the IP watcher as the observed external IP changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpEvent {
    /// An external IP appeared (initial detection or recovery after being offline).
    Connected(IpAddr),
    /// The external IP changed from one address to another.
    Changed(IpAddr, IpAddr),
    /// The external IP is no longer determinable (all providers unreachable).
    Disconnected,
}
