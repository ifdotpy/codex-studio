//! Desktop notifications for new Messages. A click opens the target chat.
// The native paths here run only on macOS; other targets build the module for checks.
#![cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, unused_imports, unused_variables)
)]
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Mutex, OnceLock},
};
use tokio::sync::broadcast;

pub const DENIED: &str = "STUDIO_NOTIFICATIONS_DENIED: Enable Allow notifications for Codex Studio in macOS System Settings > Notifications.";
const TARGETS: usize = 200;

/// Chat targets of shown notifications, oldest first. Clicks after a restart are ignored.
fn targets() -> &'static Mutex<VecDeque<(String, Value)>> {
    static VALUE: OnceLock<Mutex<VecDeque<(String, Value)>>> = OnceLock::new();
    VALUE.get_or_init(Mutex::default)
}
pub fn navigation() -> &'static broadcast::Sender<Value> {
    static VALUE: OnceLock<broadcast::Sender<Value>> = OnceLock::new();
    VALUE.get_or_init(|| broadcast::channel(16).0)
}

fn text(value: &Value, limit: usize) -> Result<String, String> {
    value
        .as_str()
        .filter(|v| !v.is_empty() && v.chars().count() <= limit && !v.contains('\0'))
        .map(str::to_owned)
        .ok_or("Invalid notification.".into())
}
/// Accepts exactly the fields of the desktop bridge notification. `sound` (a sign-in card
/// that waits for the person) plays the default sound; other notifications are silent.
pub fn validate(value: &Value) -> Result<(String, String, Value, bool), String> {
    let sound = match &value["sound"] {
        Value::Null => false,
        Value::Bool(sound) => *sound,
        _ => return Err("Invalid notification.".into()),
    };
    let target = &value["target"];
    let mut clean = json!({"agentId": text(&target["agentId"], 128)?, "section": "messages"});
    if target["section"] != "messages" {
        return Err("Invalid notification target.".into());
    }
    if !target["serverId"].is_null() {
        clean["serverId"] = Value::String(text(&target["serverId"], 128)?);
    }
    if !target["itemId"].is_null() {
        clean["itemId"] = Value::String(text(&target["itemId"], 128)?);
    }
    Ok((
        text(&value["title"], 256)?,
        text(&value["body"], 4096)?,
        clean,
        sound,
    ))
}

fn remember(id: String, target: Value) {
    if let Ok(mut targets) = targets().lock() {
        if targets.len() >= TARGETS {
            targets.pop_front();
        }
        targets.push_back((id, target));
    }
}
fn clicked(id: &str) {
    let target = targets().lock().ok().and_then(|mut targets| {
        let index = targets.iter().position(|(key, _)| key == id)?;
        targets.remove(index).map(|(_, target)| target)
    });
    if let Some(target) = target {
        let _ = navigation().send(target);
    }
}

pub async fn notify(value: &Value) -> Result<Value, String> {
    let (title, body, target, sound) = validate(value)?;
    if crate::platform::hidden_check() {
        return Ok(Value::Bool(false));
    }
    #[cfg(target_os = "macos")]
    {
        mac::show(title, body, target, sound).await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (title, body, target, sound);
        Err("Desktop notifications require macOS.".into())
    }
}

#[cfg(target_os = "macos")]
pub mod mac {
    use super::*;
    use block2::RcBlock;
    use objc2::{
        AnyThread, define_class, msg_send,
        rc::Retained,
        runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject},
    };
    use objc2_foundation::{NSBundle, NSError, NSString};
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotification,
        UNNotificationDefaultActionIdentifier, UNNotificationPresentationOptions,
        UNNotificationRequest, UNNotificationResponse, UNNotificationSound,
        UNUserNotificationCenter, UNUserNotificationCenterDelegate,
    };

    define_class!(
        #[unsafe(super(NSObject))]
        #[name = "StudioNotificationDelegate"]
        struct Delegate;

        unsafe impl NSObjectProtocol for Delegate {}

        unsafe impl UNUserNotificationCenterDelegate for Delegate {
            #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
            fn will_present(
                &self,
                _center: &UNUserNotificationCenter,
                _notification: &UNNotification,
                handler: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
            ) {
                handler.call((UNNotificationPresentationOptions::Banner
                    | UNNotificationPresentationOptions::List
                    | UNNotificationPresentationOptions::Sound,));
            }

            #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
            fn did_receive(
                &self,
                _center: &UNUserNotificationCenter,
                response: &UNNotificationResponse,
                handler: &block2::DynBlock<dyn Fn()>,
            ) {
                if response
                    .actionIdentifier()
                    .isEqualToString(unsafe { UNNotificationDefaultActionIdentifier })
                {
                    clicked(&response.notification().request().identifier().to_string());
                    dispatch2::DispatchQueue::main().exec_async(|| {
                        if let Some(mtm) = objc2::MainThreadMarker::new() {
                            objc2_app_kit::NSApplication::sharedApplication(mtm).activate();
                        }
                    });
                }
                handler.call(());
            }
        }
    );

    fn bundled() -> bool {
        NSBundle::mainBundle().bundleIdentifier().is_some()
    }

    /// The center keeps a weak delegate. This one lives for the process.
    pub fn install() {
        static INSTALLED: OnceLock<()> = OnceLock::new();
        if !bundled() {
            return;
        }
        INSTALLED.get_or_init(|| {
            let delegate: Retained<Delegate> = unsafe { msg_send![Delegate::alloc(), init] };
            UNUserNotificationCenter::currentNotificationCenter()
                .setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            std::mem::forget(delegate);
        });
    }

    pub async fn show(
        title: String,
        body: String,
        target: Value,
        sound: bool,
    ) -> Result<Value, String> {
        if !bundled() {
            return Err("Notifications require the Codex Studio app bundle.".into());
        }
        install();
        // Objective-C objects and blocks stay out of the awaiting future.
        let receive = {
            let (reply, receive) = tokio::sync::oneshot::channel();
            let reply = Mutex::new(Some(reply));
            let granted = RcBlock::new(move |granted: Bool, _error: *mut NSError| {
                if let Some(reply) = reply.lock().ok().and_then(|mut r| r.take()) {
                    let _ = reply.send(granted.as_bool());
                }
            });
            UNUserNotificationCenter::currentNotificationCenter()
                .requestAuthorizationWithOptions_completionHandler(
                    UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
                    &granted,
                );
            receive
        };
        if !receive.await.unwrap_or(false) {
            return Err(DENIED.into());
        }
        let id = crate::file_save::uuid()?;
        remember(id.clone(), target);
        let receive = {
            let content = UNMutableNotificationContent::new();
            content.setTitle(&NSString::from_str(&title));
            content.setBody(&NSString::from_str(&body));
            if sound {
                content.setSound(Some(&UNNotificationSound::defaultSound()));
            }
            let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
                &NSString::from_str(&id),
                &content,
                None,
            );
            let (reply, receive) = tokio::sync::oneshot::channel();
            let reply = Mutex::new(Some(reply));
            let added = RcBlock::new(move |error: *mut NSError| {
                let result = if error.is_null() {
                    Ok(Value::Bool(true))
                } else {
                    Err(unsafe { &*error }.localizedDescription().to_string())
                };
                if let Some(reply) = reply.lock().ok().and_then(|mut r| r.take()) {
                    let _ = reply.send(result);
                }
            });
            UNUserNotificationCenter::currentNotificationCenter()
                .addNotificationRequest_withCompletionHandler(&request, Some(&added));
            receive
        };
        receive
            .await
            .map_err(|_| "macOS did not confirm the notification. Check Studio in System Settings > Notifications.".to_owned())?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notification_fields_are_bounded_and_clicks_deliver_one_target() {
        let (title, body, target, sound) = validate(&json!({
            "title":"Ответ 🦀","body":"Готово","target":{"agentId":"a","section":"messages","itemId":"m","serverId":"remote"},"extra":1
        }))
        .unwrap();
        assert_eq!((title.as_str(), body.as_str()), ("Ответ 🦀", "Готово"));
        assert!(
            !sound,
            "a notification is silent unless it asks for the sound"
        );
        let (_, _, _, sound) = validate(&json!({
            "title":"Sign-in needed","body":"Sign in at id.example.com.","sound":true,
            "target":{"agentId":"a","section":"messages"}
        }))
        .unwrap();
        assert!(sound, "an open sign-in card plays the sound");
        assert_eq!(
            target,
            json!({"agentId":"a","section":"messages","itemId":"m","serverId":"remote"})
        );
        for bad in [
            json!({"title":"","body":"b","target":{"agentId":"a","section":"messages"}}),
            json!({"title":"t","body":"b","target":{"agentId":"a","section":"files"}}),
            json!({"title":"t","body":"b","target":{"agentId":"a","section":"messages","itemId":5}}),
            json!({"title":"t".repeat(257),"body":"b","target":{"agentId":"a","section":"messages"}}),
            json!({"title":"t","body":"b","sound":"yes","target":{"agentId":"a","section":"messages"}}),
        ] {
            assert!(validate(&bad).is_err(), "{bad}");
        }
        let mut events = navigation().subscribe();
        remember("n1".into(), target.clone());
        clicked("unknown");
        clicked("n1");
        clicked("n1");
        assert_eq!(events.try_recv().unwrap(), target);
        assert!(events.try_recv().is_err());
        for index in 0..TARGETS + 1 {
            remember(format!("bulk{index}"), json!(index));
        }
        clicked("bulk0");
        assert!(events.try_recv().is_err(), "oldest target was dropped");
        clicked(&format!("bulk{TARGETS}"));
        assert_eq!(events.try_recv().unwrap(), json!(TARGETS));
    }
}
