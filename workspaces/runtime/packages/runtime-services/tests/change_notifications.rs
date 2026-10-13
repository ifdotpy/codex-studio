use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use studio_runtime_services::change_notifications::ChangeNotificationHub;

#[test]
fn committed_change_wakes_every_subscriber_once_and_advances_revision() {
    let hub = ChangeNotificationHub::new();
    let first_calls = Arc::new(AtomicUsize::new(0));
    let second_calls = Arc::new(AtomicUsize::new(0));
    let first_counter = Arc::clone(&first_calls);
    let second_counter = Arc::clone(&second_calls);
    let (first_snapshot, _first) = hub.subscribe(move || {
        first_counter.fetch_add(1, Ordering::SeqCst);
    });
    let (_, _second) = hub.subscribe(move || {
        second_counter.fetch_add(1, Ordering::SeqCst);
    });

    assert_eq!(first_snapshot.revision, 0);
    hub.notify_one();

    assert_eq!(hub.snapshot().revision, 1);
    assert_eq!(first_calls.load(Ordering::SeqCst), 1);
    assert_eq!(second_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn dropping_a_subscription_unregisters_it_and_close_wakes_remaining_clients() {
    let hub = ChangeNotificationHub::new();
    let detached_calls = Arc::new(AtomicUsize::new(0));
    let detached_counter = Arc::clone(&detached_calls);
    let (_, detached) = hub.subscribe(move || {
        detached_counter.fetch_add(1, Ordering::SeqCst);
    });
    drop(detached);

    let remaining_calls = Arc::new(AtomicUsize::new(0));
    let remaining_counter = Arc::clone(&remaining_calls);
    let (_, _remaining) = hub.subscribe(move || {
        remaining_counter.fetch_add(1, Ordering::SeqCst);
    });

    hub.close();

    assert_eq!(detached_calls.load(Ordering::SeqCst), 0);
    assert_eq!(remaining_calls.load(Ordering::SeqCst), 1);
    assert!(hub.snapshot().closed);
    hub.notify_one();
    assert_eq!(hub.snapshot().revision, 0);
    assert_eq!(remaining_calls.load(Ordering::SeqCst), 1);
}
