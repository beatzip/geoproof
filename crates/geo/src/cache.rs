//! Success-only TTL cache for geo lookups.
//!
//! Uses tokio::time::Instant deliberately: it follows tokio::time::pause, so
//! TTL tests under a paused clock are deterministic (std::time::Instant is
//! not).
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Duration;

use super::provider::GeoInfo;

/// Cache entry: the info plus the instant it was stored at.
type Entry = (GeoInfo, tokio::time::Instant);

/// TTL cache keyed by IP; only successes are ever inserted by the client.
pub struct LookupCache {
    ttl: Duration,
    map: HashMap<IpAddr, Entry>,
}

impl LookupCache {
    /// New cache with the given TTL.
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            map: HashMap::new(),
        }
    }

    /// Return the cached info for `ip` when present and not expired.
    pub fn get(&mut self, ip: &IpAddr) -> Option<GeoInfo> {
        let ttl = self.ttl;
        match self.map.get(ip) {
            Some((info, at)) if at.elapsed() < ttl => Some(info.clone()),
            _ => None,
        }
    }

    /// Store a successful lookup.
    pub fn insert(&mut self, ip: IpAddr, info: GeoInfo) {
        self.map.insert(ip, (info, tokio::time::Instant::now()));
    }
}
