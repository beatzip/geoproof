//! IpWatcher: async external-IP polling with provider consensus and debounce.
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use crate::providers::{IpProvider, ProviderError};
use shared::IpEvent;
use tokio::sync::broadcast;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

/// Default seconds between polls of the external IP.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(15);
/// Default number of consecutive agreeing polls required to accept a new IP.
pub const DEFAULT_CONFIRMATIONS: usize = 2;
/// Default number of consecutive all-failed polls before Disconnected.
pub const DEFAULT_FAILURES_FOR_DISCONNECT: usize = 3;

/// Static configuration, with sane defaults.
#[derive(Debug, Clone)]
pub struct IpWatcherConfig {
    /// How often to poll the providers.
    pub interval: Duration,
    /// Consecutive agreeing polls needed before a new IP is accepted.
    pub confirmations: usize,
    /// Consecutive all-provider-failure polls before emitting Disconnected.
    pub failures_for_disconnect: usize,
}

impl Default for IpWatcherConfig {
    fn default() -> Self {
        Self {
            interval: DEFAULT_INTERVAL,
            confirmations: DEFAULT_CONFIRMATIONS,
            failures_for_disconnect: DEFAULT_FAILURES_FOR_DISCONNECT,
        }
    }
}

/// Errors surfaced when constructing an IpWatcher.
#[derive(Debug, thiserror::Error)]
pub enum WatcherError {
    #[error("invalid watcher config: {0}")]
    InvalidConfig(String),
}

/// Asynchronous external-IP watcher.
///
/// Polls all configured IpProvider values every config.interval. The current
/// IP is decided by strict majority among the providers that ANSWERED (so one
/// permanently dead provider cannot blind the watcher); with no majority the
/// previous value is kept. A new IP must
/// be seen config.confirmations times in a row before it is accepted
/// (debounce against VPN reconnect flapping). If every provider fails for
/// config.failures_for_disconnect polls in a row, Disconnected is emitted;
/// the next accepted IP emits Connected.
pub struct IpWatcher {
    config: IpWatcherConfig,
    providers: Vec<Arc<dyn IpProvider>>,
    tx: broadcast::Sender<IpEvent>,
}

impl IpWatcher {
    /// Max queue depth for subscribers; slow subscribers drop old events.
    const EVENT_CHANNEL_CAPACITY: usize = 64;

    /// Create a watcher; errors if providers is empty or confirmations is 0.
    pub fn new(
        config: IpWatcherConfig,
        providers: Vec<Arc<dyn IpProvider>>,
    ) -> Result<(Self, broadcast::Receiver<IpEvent>), WatcherError> {
        if providers.is_empty() {
            return Err(WatcherError::InvalidConfig(
                "at least one provider is required".to_string(),
            ));
        }
        if config.confirmations == 0 {
            return Err(WatcherError::InvalidConfig(
                "confirmations must be at least 1".to_string(),
            ));
        }
        let (tx, rx) = broadcast::channel(Self::EVENT_CHANNEL_CAPACITY);
        Ok((
            Self {
                config,
                providers,
                tx,
            },
            rx,
        ))
    }

    /// Subscribe to IP events. Multiple subscribers are supported.
    pub fn subscribe(&self) -> broadcast::Receiver<IpEvent> {
        self.tx.subscribe()
    }

    /// Run the poll loop forever.
    pub async fn run(&self) {
        let mut ticker = interval(self.config.interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut state = WatchState::new(&self.config);

        loop {
            ticker.tick().await;
            let results = fetch_all(&self.providers).await;
            // Per-poll outcome at debug level; events stay at info.
            let ok: Vec<&IpAddr> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
            debug!(
                ok = ok.len(),
                total = results.len(),
                ips = ?ok,
                "ip poll"
            );
            if let Some(event) = state.apply(&results) {
                info!(?event, "ip event");
                // Subscribers may lag or vanish; that must not stop the loop.
                let _ = self.tx.send(event);
            }
        }
    }
}

/// Fetch every provider concurrently, one result per provider.
async fn fetch_all(providers: &[Arc<dyn IpProvider>]) -> Vec<Result<IpAddr, ProviderError>> {
    let futs: Vec<_> = providers.iter().map(|p| p.fetch_ip()).collect();
    futures::future::join_all(futs).await
}

/// Debounce + consensus state machine between polls.
struct WatchState {
    confirmations: usize,
    failures_for_disconnect: usize,
    current: Option<IpAddr>,
    candidate: Option<(IpAddr, usize)>,
    consecutive_failures: usize,
}

impl WatchState {
    fn new(config: &IpWatcherConfig) -> Self {
        Self {
            confirmations: config.confirmations,
            failures_for_disconnect: config.failures_for_disconnect,
            current: None,
            candidate: None,
            consecutive_failures: 0,
        }
    }

    /// Apply one poll result set; returns the event to emit (or none).
    fn apply(&mut self, results: &[Result<IpAddr, ProviderError>]) -> Option<IpEvent> {
        let total = results.len();
        let successes: Vec<IpAddr> = results
            .iter()
            .filter_map(|r| r.as_ref().ok().copied())
            .collect();
        let failures = total - successes.len();

        if successes.is_empty() {
            self.consecutive_failures += 1;
            debug!(
                consecutive_failures = self.consecutive_failures,
                "all providers failed this poll"
            );
            if self.consecutive_failures >= self.failures_for_disconnect
                && self.current.take().is_some()
            {
                self.candidate = None;
                return Some(IpEvent::Disconnected);
            }
            return None;
        }

        self.consecutive_failures = 0;

        // Strict majority among the providers that answered. A provider that
        // is permanently blocked on this network (common for public echo
        // services) must not blind the watcher: 2 agreeing answers out of 3
        // providers, or the single answer of a lone provider, still counts.
        // Disconnection is decided separately by "nobody answered".
        let observed = majority_of(&successes);

        let Some(observed) = observed else {
            warn!(
                successes = successes.len(),
                failures, "providers disagree; keeping previous IP"
            );
            self.candidate = None;
            return None;
        };

        match self.current {
            Some(cur) if cur == observed => {
                if self.candidate.take().is_some() {
                    debug!("candidate reset by return to current IP");
                }
                None
            }
            Some(cur) => {
                let count = match self.candidate {
                    Some((cand, n)) if cand == observed => n + 1,
                    _ => 1,
                };
                debug!(observed = %observed, confirmations = count, "candidate seen");
                if count >= self.confirmations {
                    self.current = Some(observed);
                    self.candidate = None;
                    Some(IpEvent::Changed(cur, observed))
                } else {
                    self.candidate = Some((observed, count));
                    None
                }
            }
            None => {
                // Initial detection reports Connected immediately; the
                // confirmations debounce applies to changes only.
                self.current = Some(observed);
                Some(IpEvent::Connected(observed))
            }
        }
    }
}

/// The IP seen by a strict majority of the providers that answered this poll,
/// or None when the answers split with no majority.
fn majority_of(successes: &[IpAddr]) -> Option<IpAddr> {
    if successes.is_empty() {
        return None;
    }
    let mut unique: Vec<IpAddr> = successes.to_vec();
    unique.sort();
    unique.dedup();
    unique
        .into_iter()
        .find(|ip| successes.iter().filter(|a| a == &ip).count() * 2 > successes.len())
}
