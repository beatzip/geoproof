//! watcher: background external-IP observation.
//!
//! Public API: [IpWatcher] polls [IpProvider]s and emits [IpEvent]s over a
//! tokio broadcast channel.
mod providers;
mod watcher;

pub use providers::{HttpProvider, IpProvider, MockProvider, ProviderError};
pub use shared::IpEvent;
pub use watcher::{IpWatcher, IpWatcherConfig, WatcherError};

#[cfg(test)]
mod tests;
