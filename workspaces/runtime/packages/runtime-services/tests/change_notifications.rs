use studio_runtime_services::change_notifications::ChangeNotificationHub;

#[test]
fn committed_change_advances_every_watch_receiver() {
    let hub = ChangeNotificationHub::new();
    let mut first = hub.subscribe();
    let mut second = hub.subscribe();

    assert_eq!(first.borrow().revision, 0);
    hub.notify_one();

    assert_eq!(hub.snapshot().revision, 1);
    assert_eq!(first.borrow_and_update().revision, 1);
    assert_eq!(second.borrow_and_update().revision, 1);
}

#[test]
fn closed_hub_updates_watch_receivers_and_ignores_later_changes() {
    let hub = ChangeNotificationHub::new();
    let mut receiver = hub.subscribe();

    hub.close();

    assert!(receiver.borrow_and_update().closed);
    assert!(hub.snapshot().closed);
    hub.notify_one();
    assert_eq!(hub.snapshot().revision, 0);
}
