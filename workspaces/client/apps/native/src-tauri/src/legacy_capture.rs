//! Fixed, read-only compatibility capture. Page code cannot supply source values.
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tauri::WebviewWindow;

// Private-world state slot. Page-world properties cannot select this state.
const CAPTURE_STATE_SLOT: &str = "__studioNativeSourceCapture";
/// Maximum UTF-8 bytes in a private-world document token.
pub(crate) const DOCUMENT_TOKEN_MAX_BYTES: usize = 128;

#[cfg(not(target_os = "macos"))]
pub(crate) async fn store_selection(
    _window: &WebviewWindow,
    _source_identity: &str,
    _capture: &CapturedSource,
) -> Result<crate::native_origin_inventory::TrustedStoreSelection, String> {
    Err("This device cannot verify its saved browser store identity.".into())
}

#[cfg(target_os = "macos")]
pub(crate) async fn store_selection(
    window: &WebviewWindow,
    source_identity: &str,
    capture: &CapturedSource,
) -> Result<crate::native_origin_inventory::TrustedStoreSelection, String> {
    use crate::native_origin_inventory::{
        HostConfiguration, SourceFence, StoreIdentity, TrustedStoreSelection,
    };
    use objc2::{ClassType, sel};
    use objc2_foundation::{
        NSBundle, NSObjectProtocol, NSSearchPathDirectory, NSSearchPathDomainMask,
        NSSearchPathForDirectoriesInDomains, NSString,
    };
    use objc2_web_kit::WKWebView;
    use tauri::Manager;
    crate::trusted(window)?;
    let profile = window
        .app_handle()
        .try_state::<crate::ProfileLock>()
        .ok_or("The native profile lock is not held.")?
        .directory
        .clone();
    let fence = SourceFence {
        source_identity: source_identity.into(),
        document_token: capture.document_token.clone(),
    };
    let (send, receive) = tokio::sync::oneshot::channel();
    window
        .with_webview(move |view| {
            let result = (|| -> Result<TrustedStoreSelection, String> {
                let web = unsafe { (view.inner() as *mut WKWebView).as_ref() }
                    .ok_or("The source view is unavailable.")?;
                let store = unsafe { web.configuration().websiteDataStore() };
                if !unsafe { store.isPersistent() } {
                    return Err("The source browser store is not persistent.".into());
                }
                if !store.respondsToSelector(sel!(identifier)) {
                    return Err(
                        "The source store identity cannot be verified by this device.".into(),
                    );
                }
                let store = match unsafe { store.identifier() } {
                    None => StoreIdentity::Default,
                    Some(id) => {
                        let hex = id.UUIDString().to_string().replace('-', "");
                        if hex.len() != 32 {
                            return Err("The source store identifier is invalid.".into());
                        }
                        let mut bytes = [0; 16];
                        for (index, value) in bytes.iter_mut().enumerate() {
                            *value = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
                                .map_err(|_| "The source store identifier is invalid.")?;
                        }
                        StoreIdentity::Identified(bytes)
                    }
                };
                let bundle = NSBundle::mainBundle();
                let application = bundle
                    .bundleIdentifier()
                    .ok_or("The source application has no bundle identity.")?
                    .to_string();
                let (signing, sandboxed) = self_security_identity()?;
                if application != signing {
                    return Err(
                        "The source application identity differs from its signature.".into(),
                    );
                }
                let framework = unsafe { NSBundle::bundleForClass(WKWebView::class()) }
                    .objectForInfoDictionaryKey(&NSString::from_str("CFBundleVersion"))
                    .and_then(|value| value.downcast::<NSString>().ok())
                    .ok_or("The source framework build cannot be verified.")?
                    .to_string();
                let directory = |kind| {
                    NSSearchPathForDirectoriesInDomains(
                        kind,
                        NSSearchPathDomainMask::UserDomainMask,
                        true,
                    )
                    .firstObject()
                    .map(|value| std::path::PathBuf::from(value.to_string()))
                    .ok_or("The source storage directory cannot be verified.")
                };
                let configuration = HostConfiguration {
                    library_directory: directory(NSSearchPathDirectory::LibraryDirectory)?,
                    cache_directory: directory(NSSearchPathDirectory::CachesDirectory)?,
                    application_identifier: application,
                    inside_app_container: sandboxed,
                    profile_directory: profile,
                    store,
                    framework_build: framework,
                };
                TrustedStoreSelection::from_host(configuration, fence)
                    .map_err(|error| error.to_string())
            })();
            let _ = send.send(result);
        })
        .map_err(|error| error.to_string())?;
    tokio::time::timeout(
        std::time::Duration::from_millis(
            crate::profile::migration_policy()["captureDeadlineMs"]
                .as_u64()
                .ok_or("Invalid source selection deadline.")?,
        ),
        receive,
    )
    .await
    .map_err(|_| "The source store identity check timed out.")?
    .map_err(|_| "The source store identity returned no result.")?
}

/// Read only this process's signing identity and sandbox entitlement. No key store access.
#[cfg(target_os = "macos")]
fn self_security_identity() -> Result<(String, bool), String> {
    use objc2_foundation::NSString;
    use std::ffi::c_void;
    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        fn SecTaskCreateFromSelf(allocator: *const c_void) -> *const c_void;
        fn SecTaskCopySigningIdentifier(
            task: *const c_void,
            error: *mut *const c_void,
        ) -> *const c_void;
        fn SecTaskCopyValueForEntitlement(
            task: *const c_void,
            key: *const c_void,
            error: *mut *const c_void,
        ) -> *const c_void;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(value: *const c_void);
        fn CFGetTypeID(value: *const c_void) -> usize;
        fn CFStringGetTypeID() -> usize;
        fn CFBooleanGetTypeID() -> usize;
        fn CFBooleanGetValue(value: *const c_void) -> u8;
    }
    struct Owned(*const c_void);
    impl Drop for Owned {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CFRelease(self.0) }
            }
        }
    }
    let task = Owned(unsafe { SecTaskCreateFromSelf(std::ptr::null()) });
    if task.0.is_null() {
        return Err("The source process identity cannot be verified.".into());
    }
    let mut error = std::ptr::null();
    let identifier = Owned(unsafe { SecTaskCopySigningIdentifier(task.0, &mut error) });
    let failure = Owned(error);
    if identifier.0.is_null()
        || !failure.0.is_null()
        || unsafe { CFGetTypeID(identifier.0) != CFStringGetTypeID() }
    {
        return Err("The source signing identity cannot be verified.".into());
    }
    let identifier = unsafe { &*(identifier.0 as *const NSString) }.to_string();
    let key = NSString::from_str("com.apple.security.app-sandbox");
    let mut error = std::ptr::null();
    let entitlement = Owned(unsafe {
        SecTaskCopyValueForEntitlement(task.0, (&*key as *const NSString).cast(), &mut error)
    });
    let failure = Owned(error);
    if !failure.0.is_null() {
        return Err("The source container identity cannot be verified.".into());
    }
    let sandboxed = if entitlement.0.is_null() {
        false
    } else {
        if unsafe { CFGetTypeID(entitlement.0) != CFBooleanGetTypeID() } {
            return Err("The source container entitlement is invalid.".into());
        }
        unsafe { CFBooleanGetValue(entitlement.0) != 0 }
    };
    Ok((identifier, sandboxed))
}

pub(crate) struct CapturedSource {
    pub(crate) top_origin: String,
    pub(crate) origin: String,
    pub(crate) document_token: String,
    pub(crate) snapshot: Value,
    pub(crate) digest: String,
}
fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn reader_program() -> Result<String, String> {
    let reader = include_str!("../../../../compat/legacyProfileReader.mjs");
    let declaration = "export async function exportBrowserProfile";
    if reader.matches(declaration).count() != 1 {
        return Err("The fixed legacy reader contract changed.".into());
    }
    Ok(format!(
        r#"
{}
const options=JSON.parse(capture);
const requireMainSource=()=>{{
  if(globalThis.top!==globalThis||globalThis.frames.length!==0) throw new Error('The source frame coverage is incomplete.');
  return true;
}};
const before=requireMainSource();
const state=globalThis[{state_slot:?}]??={{documentToken:options.newDocumentToken,cancelled:new Set(),active:new Set()}};
if(options.documentToken!==null&&state.documentToken!==options.documentToken) throw new Error('The source document changed.');
if(state.cancelled.has(options.lease)) throw new Error('The source capture was canceled.');
state.active.add(options.lease);
try {{
  const snapshot=await exportBrowserProfile(null,{{readOnly:true,expectedOrigin:options.origin,maxRecordBytes:options.maxRecordBytes,isCancelled:()=>state.cancelled.has(options.lease)}});
  const after=requireMainSource();
  return JSON.stringify({{documentToken:state.documentToken,frameCoverage:{{before,after}},snapshot}});
}} finally {{state.active.delete(options.lease);state.cancelled.delete(options.lease);}}
"#,
        reader.replacen(declaration, "async function exportBrowserProfile", 1),
        state_slot = CAPTURE_STATE_SLOT,
    ))
}
fn decode_capture(
    text: &str,
    origin: &str,
    expected_document: Option<&str>,
) -> Result<CapturedSource, String> {
    let policy = crate::profile::migration_policy();
    let limit = policy["captureResponseBytes"]
        .as_u64()
        .ok_or("Invalid capture response bound.")?;
    if text.len() as u64 > limit {
        return Err(
            "The source capture exceeds its response bound. Its source remains held.".into(),
        );
    }
    let packet: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if packet["frameCoverage"] != json!({"before":true,"after":true}) {
        return Err(
            "The private source frame coverage is incomplete. Its source remains held.".into(),
        );
    }
    let document = packet["documentToken"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= DOCUMENT_TOKEN_MAX_BYTES)
        .ok_or("The source capture has no native document token.")?;
    if expected_document.is_some_and(|expected| expected != document) {
        return Err("The source document changed.".into());
    }
    let mut snapshot = packet["snapshot"].clone();
    if snapshot["version"] != 1
        || !snapshot["preferences"].is_object()
        || !snapshot["databases"].is_array()
    {
        return Err("The native source capture is incomplete.".into());
    }
    snapshot["origin"] = json!(origin);
    let databases = snapshot["databases"].as_array_mut().unwrap();
    databases.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    for db in databases {
        if !db["name"].is_string() {
            return Err("The captured database name is invalid.".into());
        }
        let stores = db["stores"]
            .as_array_mut()
            .ok_or("The captured database schema is incomplete.")?;
        stores.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    }
    let bytes =
        serde_json::to_vec(&json!({"topOrigin":origin,"origin":origin,"snapshot":snapshot}))
            .map_err(|e| e.to_string())?;
    Ok(CapturedSource {
        top_origin: origin.into(),
        origin: origin.into(),
        document_token: document.into(),
        snapshot,
        digest: digest(&bytes),
    })
}

#[cfg(not(target_os = "macos"))]
pub(crate) async fn main_source(
    _window: &WebviewWindow,
    _document: Option<&str>,
) -> Result<CapturedSource, String> {
    Err("This device cannot verify its saved browser storage.".into())
}

#[cfg(target_os = "macos")]
pub(crate) async fn main_source(
    window: &WebviewWindow,
    document: Option<&str>,
) -> Result<CapturedSource, String> {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };
    crate::trusted(window)?;
    let url = window.url().map_err(|e| e.to_string())?;
    let origin = if matches!(url.scheme(), "tauri" | "studio-view") {
        format!(
            "{}://{}",
            url.scheme(),
            url.host_str().ok_or("The source origin has no host.")?
        )
    } else {
        url.origin().ascii_serialization()
    };
    let lease = studio_client_core::server_keys::random_id()?;
    let policy = crate::profile::migration_policy();
    let args=json!({"origin":origin,"lease":lease,"documentToken":document,"newDocumentToken":studio_client_core::server_keys::random_id()?,"maxRecordBytes":policy["captureRecordBytes"]}).to_string();
    let (send, receive) = tokio::sync::oneshot::channel();
    let reply = Arc::new(Mutex::new(Some(send)));
    evaluate(window, &reader_program()?, &args, Some(reply))?;
    let result = tokio::time::timeout(
        Duration::from_millis(
            policy["captureDeadlineMs"]
                .as_u64()
                .ok_or("Invalid source capture deadline.")?,
        ),
        receive,
    )
    .await;
    let text = match result {
        Ok(result) => result.map_err(|_| "The native source capture returned no callback.")??,
        Err(_) => {
            // Cancel only this private read, never a source transaction or user data.
            let cancel = format!(
                "const options=JSON.parse(capture);const state=globalThis[{CAPTURE_STATE_SLOT:?}];if(state?.active.has(options.lease))state.cancelled.add(options.lease);"
            );
            let _ = evaluate(window, &cancel, &json!({"lease":lease}).to_string(), None);
            return Err("The native source capture timed out. Its source remains held.".into());
        }
    };
    crate::trusted(window)?;
    if window.url().map_err(|e| e.to_string())? != url {
        return Err("The source view changed during capture.".into());
    }
    decode_capture(&text.0, &origin, document)
}

// Only evaluate's fixed main-frame WKContentWorld callback constructs this.
#[cfg(target_os = "macos")]
struct NativeMainFrameCapture(String);

#[cfg(target_os = "macos")]
type CaptureReply = std::sync::Arc<
    std::sync::Mutex<Option<tokio::sync::oneshot::Sender<Result<NativeMainFrameCapture, String>>>>,
>;
#[cfg(target_os = "macos")]
fn evaluate(
    window: &WebviewWindow,
    body: &str,
    args: &str,
    reply: Option<CaptureReply>,
) -> Result<(), String> {
    use objc2::{MainThreadMarker, rc::Retained, runtime::AnyObject};
    use objc2_foundation::{NSDictionary, NSError, NSString};
    use objc2_web_kit::{WKContentWorld, WKWebView};
    thread_local! { static WORLD:std::cell::RefCell<Option<Retained<WKContentWorld>>>=const {std::cell::RefCell::new(None)}; }
    let body = body.to_owned();
    let args = args.to_owned();
    window
        .with_webview(move |view| {
            let result = (|| -> Result<(), String> {
                let mtm = MainThreadMarker::new()
                    .ok_or("The source capture must run on the UI thread.")?;
                let world = WORLD.with(|slot| {
                    let mut slot = slot.borrow_mut();
                    slot.get_or_insert_with(|| unsafe {
                        WKContentWorld::worldWithName(
                            &NSString::from_str(
                                crate::profile::migration_policy()["captureWorld"]
                                    .as_str()
                                    .expect("capture world owner"),
                            ),
                            mtm,
                        )
                    })
                    .clone()
                });
                let web = unsafe { (view.inner() as *mut WKWebView).as_ref() }
                    .ok_or("The source view is unavailable.")?;
                let key = NSString::from_str("capture");
                let args = NSString::from_str(&args);
                let arguments =
                    NSDictionary::<NSString, AnyObject>::from_slices(&[&*key], &[args.as_ref()]);
                let callback_reply = reply.clone();
                let callback =
                    block2::RcBlock::new(move |value: *mut AnyObject, error: *mut NSError| {
                        let result = if !error.is_null() {
                            Err("The native source reader failed. Its source remains held.".into())
                        } else {
                            unsafe { value.as_ref() }
                                .and_then(|value| value.downcast_ref::<NSString>())
                                .map(|value| NativeMainFrameCapture(value.to_string()))
                                .ok_or(
                                    "The native source reader returned an invalid result.".into(),
                                )
                        };
                        if let Some(reply) = &callback_reply
                            && let Ok(mut reply) = reply.lock()
                            && let Some(reply) = reply.take()
                        {
                            let _ = reply.send(result);
                        }
                    });
                unsafe {
                    web.callAsyncJavaScript_arguments_inFrame_inContentWorld_completionHandler(
                        &NSString::from_str(&body),
                        Some(&arguments),
                        // WKWebView.h defines nil as the main frame. No page
                        // frame identifier or renderer content world is used.
                        None,
                        &world,
                        Some(&callback),
                    );
                }
                Ok(())
            })();
            if let Err(error) = result
                && let Some(reply) = reply
                && let Ok(mut reply) = reply.lock()
                && let Some(reply) = reply.take()
            {
                let _ = reply.send(Err(error));
            }
        })
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_capture_refuses_partial_or_changed_document_results() {
        let packet=json!({"documentToken":"native-token","frameCoverage":{"before":true,"after":true},"snapshot":{"version":1,"preferences":{"request-id:old":"original"},"databases":[]}}).to_string();
        let captured = decode_capture(&packet, "tauri://localhost", None).unwrap();
        assert_eq!(captured.document_token, "native-token");
        assert_eq!(captured.origin, "tauri://localhost");
        assert_eq!(
            captured.snapshot["preferences"]["request-id:old"],
            "original"
        );
        assert_eq!(captured.digest.len(), 64);
        assert!(decode_capture(&packet, "tauri://localhost", Some("other-document")).is_err());
        assert!(decode_capture("{}", "tauri://localhost", None).is_err());
        assert!(reader_program().unwrap().contains("isCancelled"));
    }

    #[test]
    fn native_capture_requires_both_fixed_private_frame_checks() {
        let packet = json!({"documentToken":"native-token","frameCoverage":{"before":true,"after":true},"snapshot":{"version":1,"preferences":{},"databases":[]}});
        for field in ["before", "after"] {
            let mut changed = packet.clone();
            changed["frameCoverage"][field] = json!(false);
            assert!(decode_capture(&changed.to_string(), "tauri://localhost", None).is_err());
        }
        let mut missing = packet;
        missing.as_object_mut().unwrap().remove("frameCoverage");
        assert!(decode_capture(&missing.to_string(), "tauri://localhost", None).is_err());
    }
}
