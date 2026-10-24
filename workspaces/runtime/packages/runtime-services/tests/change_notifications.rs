use studio_runtime_services::change_notifications::ChangeNotificationHub;

#[test]
fn closed_hub_updates_watch_receivers_and_ignores_later_changes() {
    let hub = ChangeNotificationHub::new();
    let mut receiver = hub.subscribe();

    assert_eq!(receiver.borrow().revision, 0);
    hub.close();

    assert!(receiver.borrow_and_update().closed);
    assert!(hub.snapshot().closed);
    hub.notify_one();
    assert_eq!(hub.snapshot().revision, 0);
}
