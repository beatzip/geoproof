//! Demo binary: watches the external IP and prints every event to stdout.
//!
//! Usage: cargo run -p watcher --bin watcher-demo

use std::sync::Arc;
use std::time::Duration;

use watcher::{HttpProvider, IpProvider, IpWatcher, IpWatcherConfig};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = IpWatcherConfig::default();
    let timeout = Duration::from_secs(5);
    let providers: Vec<Arc<dyn IpProvider>> = vec![
        Arc::new(HttpProvider::new("https://api.ipify.org", timeout)),
        Arc::new(HttpProvider::new("https://ifconfig.me/ip", timeout)),
    ];

    let (watcher, mut rx) = match IpWatcher::new(config, providers) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("failed to start watcher: {e}");
            return;
        }
    };

    tokio::spawn(async move { watcher.run().await });

    println!("watcher-demo: observing external IP (Ctrl+C to stop)");
    while let Ok(event) = rx.recv().await {
        println!("{event:?}");
    }
}
