//! Desktop notifications for new Messages. A click opens the target chat.
// The native paths here run only on macOS; other targets build the module for checks.
#![cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, unused_imports, unused_variables)
)]
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Mutex, OnceLock},
};
use studio_client_domain::json_object::JsonObject;
use tokio::sync::broadcast;

pub const DENIED: &str = "STUDIO_NOTIFICATIONS_DENIED: Enable Allow notifications for Codex Studio in macOS System Settings > Notifications.";
const TARGETS: usize = 200;

/// Chat targets of shown notifications, oldest first. Clicks after a restart are ignored.
fn targets() -> &'static Mutex<VecDeque<(String, NotificationTarget)>> {
    static VALUE: OnceLock<Mutex<VecDeque<(String, NotificationTarget)>>> = OnceLock::new();
    VALUE.get_or_init(Mutex::default)
}
pub fn navigation() -> &'static broadcast::Sender<Value> {
    static VALUE: OnceLock<broadcast::Sender<Value>> = OnceLock::new();
    VALUE.get_or_init(|| broadcast::channel(16).0)
}

// Native notification text limits count Unicode scalar values.
#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
struct Text<const LIMIT: usize>(String);
impl<'de, const LIMIT: usize> Deserialize<'de> for Text<LIMIT> {
    fn deserialize<D: serde::Deserializer<'de>>(decoder: D) -> Result<Self, D::Error> {
        let value = String::deserialize(decoder)?;
        if value.is_empty() || value.chars().count() > LIMIT || value.contains('\0') {
            return Err(serde::de::Error::custom("Invalid notification."));
        }
        Ok(Self(value))
    }
}

#[derive(Clone, Debug, Serialize)]
enum Section {
    #[serde(rename = "messages")]
    Messages,
}
impl<'de> Deserialize<'de> for Section {
    fn deserialize<D: serde::Deserializer<'de>>(decoder: D) -> Result<Self, D::Error> {
        match String::deserialize(decoder)?.as_str() {
            "messages" => Ok(Self::Messages),
            _ => Err(serde::de::Error::custom("Invalid notification target.")),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct NotificationTarget {
    agent_id: Text<128>,
    section: Section,
    #[serde(skip_serializing_if = "Option::is_none")]
    server_id: Option<Text<128>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    item_id: Option<Text<128>>,
}

#[derive(Deserialize)]
struct Notification {
    title: Text<256>,
    body: Text<4096>,
    target: JsonObject<NotificationTarget>,
    #[serde(default, deserialize_with = "sound")]
    sound: bool,
}

fn sound<'de, D: serde::Deserializer<'de>>(decoder: D) -> Result<bool, D::Error> {
    Ok(Option::<bool>::deserialize(decoder)?.unwrap_or(false))
}

fn validate(value: &Value) -> Result<Notification, String> {
    JsonObject::<Notification>::deserialize(value)
        .map(|value| value.0)
        .map_err(|error| error.to_string())
}

fn remember(id: String, target: NotificationTarget) {
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
    if let Some(target) = target
        && let Ok(value) = serde_json::to_value(target)
    {
        drop(navigation().send(value));
    }
}

pub async fn notify(value: &Value) -> Result<Value, String> {
    let Notification {
        title,
        body,
        target: JsonObject(target),
        sound,
    } = validate(value)?;
    if crate::platform::hidden_check() {
        return Ok(Value::Bool(false));
    }
    #[cfg(target_os = "macos")]
    {
        mac::show(title.0, body.0, target, sound).await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (title, body, target, sound);
        Err("Desktop notifications require macOS.".into())
    }
}

#[cfg(target_os = "macos")]
// SAFETY: Objective-C delegates and notification bindings stay inside this adapter.
#[allow(unsafe_code)]
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

    pub(super) async fn show(
        title: String,
        body: String,
        target: NotificationTarget,
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
                    // The authorization requester may have canceled while the prompt was open.
                    let _send_result = reply.send(granted.as_bool());
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
                    // Completion can outlive the requester.
                    drop(reply.send(result));
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
    fn notification_wire_matches_desktop_owner_and_rejects_sequence_shapes() {
        // DesktopBridge owns the wire fields; native limits intentionally differ
        // from the Electron adapter's display limits.
        let owner = include_str!("../../../apps/desktop/api.ts");
        let declaration = owner
            .split("notify(value: {")
            .nth(1)
            .unwrap()
            .split("}): Promise<boolean>")
            .next()
            .unwrap();
        let fields: Vec<_> = declaration
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with("/**"))
            .filter_map(|line| line.split_once(':').map(|(name, _)| name))
            .collect();
        assert_eq!(
            fields,
            [
                "title",
                "body",
                "sound?",
                "target",
                "serverId?",
                "agentId",
                "section",
                "itemId?"
            ]
        );
        assert!(declaration.contains("section: \"messages\""));
        for wire in [
            r#"["t","b",{"agentId":"a","section":"messages"},false]"#,
            r#"{"title":"t","body":"b","target":["a","messages"]}"#,
            r#"{"title":"t","body":"b","target":{"agentId":"a","section":{"messages":null}}}"#,
        ] {
            let value: Value = serde_json::from_str(wire).unwrap();
            assert!(validate(&value).is_err());
        }
        let notification = validate(&json!({"title":"t","body":"b","sound":null,
            "target":{"agentId":"a","section":"messages","serverId":null,"itemId":null,"extension":1}})).unwrap();
        assert!(!notification.sound);
        assert_eq!(
            serde_json::to_value(notification.target).unwrap(),
            json!({"agentId":"a","section":"messages"})
        );
    }

    #[test]
    fn notification_fields_are_bounded_and_clicks_deliver_one_target() {
        let Notification { title, body, target: JsonObject(target), sound } = validate(&json!({
            "title":"Ответ 🦀","body":"Готово","target":{"agentId":"a","section":"messages","itemId":"m","serverId":"remote"},"extra":1
        }))
        .unwrap();
        assert_eq!((title.0.as_str(), body.0.as_str()), ("Ответ 🦀", "Готово"));
        assert!(
            !sound,
            "a notification is silent unless it asks for the sound"
        );
        let Notification { sound, .. } = validate(&json!({
            "title":"Sign-in needed","body":"Sign in at id.example.com.","sound":true,
            "target":{"agentId":"a","section":"messages"}
        }))
        .unwrap();
        assert!(sound, "an open sign-in card plays the sound");
        assert_eq!(
            serde_json::to_value(&target).unwrap(),
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
        assert_eq!(
            events.try_recv().unwrap(),
            serde_json::to_value(&target).unwrap()
        );
        assert!(events.try_recv().is_err());
        for index in 0..TARGETS + 1 {
            let mut item = target.clone();
            item.agent_id = Text(index.to_string());
            remember(format!("bulk{index}"), item);
        }
        clicked("bulk0");
        assert!(events.try_recv().is_err(), "oldest target was dropped");
        clicked(&format!("bulk{TARGETS}"));
        assert_eq!(events.try_recv().unwrap()["agentId"], TARGETS.to_string());
    }
}
