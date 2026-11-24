//! Unit tests. All provider behaviour is scripted with MockProvider, so no
//! network access happens. Time is controlled with tokio::time::pause.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use crate::providers::{IpProvider, MockProvider, ProviderError};
use crate::watcher::{IpWatcher, IpWatcherConfig};
use shared::IpEvent;

fn ip(octets: [u8; 4]) -> IpAddr {
    IpAddr::V4(Ipv4Addr::from(octets))
}

fn ok(ip: [u8; 4]) -> Result<IpAddr, ProviderError> {
    Ok(IpAddr::V4(Ipv4Addr::from(ip)))
}

fn err() -> Result<IpAddr, ProviderError> {
    Err(ProviderError::Unparseable("x".to_string()))
}

fn config(confirmations: usize) -> IpWatcherConfig {
    IpWatcherConfig {
        interval: Duration::from_secs(1),
        confirmations,
        failures_for_disconnect: 3,
    }
}

/// Drive the watcher's poll loop `ticks` times with a paused clock, feeding
/// each poll the provider script in order, and collect emitted events.
async fn run_polls(
    config: IpWatcherConfig,
    providers: Vec<Arc<dyn IpProvider>>,
    ticks: usize,
) -> Vec<IpEvent> {
    let (watcher, mut rx) = IpWatcher::new(config, providers).unwrap();
    let handle = tokio::spawn(async move { watcher.run().await });
    tokio::time::pause();
    // Warm-up: let the spawned task reach its immediate first tick so that
    // each subsequent 1 s advance produces exactly one poll.
    for _ in 0..5 {
        tokio::time::advance(Duration::ZERO).await;
        tokio::task::yield_now().await;
    }
    let mut events = Vec::new();
    for _ in 0..ticks {
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        // Two scheduler rounds per poll under the paused clock: advance wakes
        // the interval, a further zero-advance + yield lets the boxed futures
        // in the providers finish.
        tokio::time::advance(Duration::from_secs(1)).await;
        for _ in 0..3 {
            tokio::time::advance(Duration::ZERO).await;
            tokio::task::yield_now().await;
        }
        // The second 1 s advance is required: boxed async-trait futures in
        // join_all complete only on the next timer advance, yield_now does
        // not poll them. One poll per iteration = two advances.
        tokio::time::advance(Duration::from_secs(1)).await;
        for _ in 0..3 {
            tokio::time::advance(Duration::ZERO).await;
            tokio::task::yield_now().await;
        }
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
    }
    handle.abort();
    events
}

/// One poll = one tick, so a script of N entries is consumed by N ticks.
/// Note: MockProvider returns the same last entry forever after exhaustion,
/// so tests must size scripts to the number of ticks.

#[tokio::test]
async fn debounce_sequence_emits_only_stable_changes() {
    // A A A B B B C C with confirmations = 2.
    let providers: Vec<Arc<dyn IpProvider>> = vec![Arc::new(MockProvider::new(vec![
        ok([1, 1, 1, 1]),
        ok([1, 1, 1, 1]),
        ok([1, 1, 1, 1]),
        ok([2, 2, 2, 2]),
        ok([2, 2, 2, 2]),
        ok([2, 2, 2, 2]),
        ok([3, 3, 3, 3]),
        ok([3, 3, 3, 3]),
    ]))];
    let events = run_polls(config(2), providers, 8).await;
    assert_eq!(
        events,
        vec![
            IpEvent::Connected(ip([1, 1, 1, 1])),
            IpEvent::Changed(ip([1, 1, 1, 1]), ip([2, 2, 2, 2])),
            IpEvent::Changed(ip([2, 2, 2, 2]), ip([3, 3, 3, 3])),
        ]
    );
}

#[tokio::test]
async fn flap_produces_no_changed_events() {
    // A B A A: single-observation candidates are discarded.
    let providers: Vec<Arc<dyn IpProvider>> = vec![Arc::new(MockProvider::new(vec![
        ok([1, 1, 1, 1]),
        ok([2, 2, 2, 2]),
        ok([1, 1, 1, 1]),
        ok([1, 1, 1, 1]),
    ]))];
    let events = run_polls(config(2), providers, 4).await;
    assert_eq!(events, vec![IpEvent::Connected(ip([1, 1, 1, 1]))]);
}

#[tokio::test]
async fn disconnect_after_three_failures_then_connected_on_recovery() {
    // A, fail, fail, fail -> Disconnected; then A -> Connected.
    let providers: Vec<Arc<dyn IpProvider>> = vec![Arc::new(MockProvider::new(vec![
        ok([1, 1, 1, 1]),
        err(),
        err(),
        err(),
        ok([1, 1, 1, 1]),
        ok([1, 1, 1, 1]),
    ]))];
    let events = run_polls(config(2), providers, 6).await;
    assert_eq!(
        events,
        vec![
            IpEvent::Connected(ip([1, 1, 1, 1])),
            IpEvent::Disconnected,
            IpEvent::Connected(ip([1, 1, 1, 1])),
        ]
    );
}

#[tokio::test]
async fn provider_disagreement_keeps_previous_ip() {
    // P1 sees the new IP, P2 still sees the old one: no event at all.
    // Poll 1: both agree on A -> Connected.
    // Poll 2: P1 = B, P2 = A -> disagreement, no event.
    // Poll 3: both agree on A again -> still A, no change event.
    let p1 = MockProvider::new(vec![ok([1, 1, 1, 1]), ok([2, 2, 2, 2]), ok([1, 1, 1, 1])]);
    let p2 = MockProvider::new(vec![ok([1, 1, 1, 1]), ok([1, 1, 1, 1]), ok([1, 1, 1, 1])]);
    let providers: Vec<Arc<dyn IpProvider>> = vec![Arc::new(p1), Arc::new(p2)];
    let events = run_polls(config(2), providers, 3).await;
    assert_eq!(events, vec![IpEvent::Connected(ip([1, 1, 1, 1]))]);
}

#[tokio::test]
async fn single_responder_majority_is_accepted() {
    // P1 healthy, P2 erroring from poll 2 on: the single live provider is a
    // majority among responders, so after 2 confirmations the new IP wins.
    let p1 = MockProvider::new(vec![ok([1, 1, 1, 1]), ok([2, 2, 2, 2]), ok([2, 2, 2, 2])]);
    let p2 = MockProvider::new(vec![ok([1, 1, 1, 1]), err(), err()]);
    let providers: Vec<Arc<dyn IpProvider>> = vec![Arc::new(p1), Arc::new(p2)];
    let events = run_polls(config(2), providers, 3).await;
    assert_eq!(
        events,
        vec![
            IpEvent::Connected(ip([1, 1, 1, 1])),
            IpEvent::Changed(ip([1, 1, 1, 1]), ip([2, 2, 2, 2])),
        ]
    );
}

#[tokio::test]
async fn two_agreeing_one_failing_is_consensus() {
    // 2 providers agree, 1 fails: strict majority among responders -> Connected.
    let p1 = MockProvider::new(vec![ok([7, 7, 7, 7])]);
    let p2 = MockProvider::new(vec![ok([7, 7, 7, 7])]);
    let p3 = MockProvider::new(vec![err()]);
    let providers: Vec<Arc<dyn IpProvider>> = vec![Arc::new(p1), Arc::new(p2), Arc::new(p3)];
    let events = run_polls(config(2), providers, 1).await;
    assert_eq!(events, vec![IpEvent::Connected(ip([7, 7, 7, 7]))]);
}

#[tokio::test]
async fn empty_provider_list_is_rejected() {
    let result = IpWatcher::new(config(2), vec![]);
    assert!(result.is_err());
}
