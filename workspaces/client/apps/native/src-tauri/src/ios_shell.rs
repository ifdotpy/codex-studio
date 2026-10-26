//! iOS shell: one full-window WKWebView that loads the Studio web client from
//! the user's server. The page receives no Tauri commands. The shell pushes
//! UIKit safe-area insets and keyboard animations into the page through
//! `ios_viewport.js`; see README.md "iOS shell".

use crate::ios_insets::{Geometry, Insets, reported};
use objc2::rc::{Retained, Weak};
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_core_foundation::{CGRect, CGSize};
use objc2_foundation::{
    NSDictionary, NSNotification, NSNotificationCenter, NSNumber, NSOperationQueue, NSString,
    NSValue,
};
use objc2_ui_kit::{
    NSValueUIGeometryExtensions, UIKeyboardAnimationCurveUserInfoKey,
    UIKeyboardAnimationDurationUserInfoKey, UIKeyboardDidChangeFrameNotification,
    UIKeyboardDidHideNotification, UIKeyboardDidShowNotification, UIKeyboardFrameEndUserInfoKey,
    UIKeyboardWillChangeFrameNotification, UIKeyboardWillHideNotification,
    UIKeyboardWillShowNotification, UIScrollView, UIScrollViewContentInsetAdjustmentBehavior,
    UIView,
};
use objc2_web_kit::{WKScriptMessage, WKScriptMessageHandler, WKUserContentController};
use std::cell::Cell;
use std::ptr::NonNull;
use tauri::{Manager, WebviewWindow};

/// Script message handler name used by `ios_viewport.js`.
const MESSAGE_HANDLER: &str = "studioNativeViewport";
/// Saved server URL in the app configuration directory.
const SERVER_FILE: &str = "server-url.txt";
/// Custom scheme of the bundled first-launch page.
pub const START_SCHEME: &str = "studio-shell";

/// Keyboard state in CSS px and ms. `curve` is the UIViewAnimationCurve value.
#[derive(Clone, Copy)]
struct Keyboard {
    overlap: f64,
    duration: f64,
    curve: isize,
}

thread_local! {
    // Main thread only: UIKit notifications and script messages arrive there.
    static KEYBOARD: Cell<Option<Keyboard>> = const { Cell::new(None) };
    // Diagnostic only: the last UIKit end frame and its conversions.
    static KEYBOARD_FRAMES: std::cell::RefCell<serde_json::Value> = const { std::cell::RefCell::new(serde_json::Value::Null) };
}

fn rect_json(r: CGRect) -> serde_json::Value {
    serde_json::json!([r.origin.x, r.origin.y, r.size.width, r.size.height])
}

/// Per-launch secret of the first-launch page form, as hex.
pub fn start_token() -> String {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes));
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The page origin to load. A launch environment value wins over the saved
/// value, so simulator tests never change the user's choice.
pub fn server_url(app: &tauri::App) -> Option<tauri::Url> {
    if let Some(value) = std::env::var("STUDIO_SERVER_URL")
        .ok()
        .filter(|v| !v.is_empty())
    {
        return valid_server(&value);
    }
    let path = app.path().app_config_dir().ok()?.join(SERVER_FILE);
    valid_server(std::fs::read_to_string(path).ok()?.trim())
}

/// HTTPS servers, or plain HTTP only on this device's loopback (simulator tests).
fn valid_server(value: &str) -> Option<tauri::Url> {
    let url: tauri::Url = value.parse().ok()?;
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost"));
    let scheme_ok = url.scheme() == "https" || (url.scheme() == "http" && loopback);
    // Path and query stay, so a simulator check can pass `?layoutDebug=1`.
    (scheme_ok && url.host_str().is_some() && url.username().is_empty() && url.password().is_none())
        .then_some(url)
}

/// Serve the first-launch page and save the chosen server. The page holds a
/// per-launch token so another document cannot change the saved server.
pub fn start_page(
    ctx: tauri::UriSchemeContext<'_, tauri::Wry>,
    request: tauri::http::Request<Vec<u8>>,
    token: &str,
) -> tauri::http::Response<Vec<u8>> {
    let html = |status: u16, body: String| {
        tauri::http::Response::builder()
            .status(status)
            .header("Content-Type", "text/html; charset=utf-8")
            .header("Content-Security-Policy", "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; form-action studio-shell:")
            .body(body.into_bytes())
            .unwrap()
    };
    if request.uri().path() != "/connect" {
        let saved = server_url_text(ctx.app_handle()).unwrap_or_default();
        return html(
            200,
            include_str!("ios_start.html")
                .replace("{{token}}", token)
                .replace("{{saved}}", &escape(&saved)),
        );
    }
    let query = tauri::Url::parse(&format!(
        "{START_SCHEME}://localhost/?{}",
        request.uri().query().unwrap_or_default()
    ))
    .ok();
    let field = |name: &str| {
        query
            .as_ref()?
            .query_pairs()
            .find_map(|(key, value)| (key == name).then(|| value.into_owned()))
    };
    if field("token").as_deref() != Some(token) {
        return html(403, "Open this page from the app start screen.".into());
    }
    let Some(url) = field("url").and_then(|v| valid_server(v.trim())) else {
        return html(
            400,
            "Use an https:// server address. <a href=\"/\">Back</a>".into(),
        );
    };
    let saved = ctx
        .app_handle()
        .path()
        .app_config_dir()
        .map_err(|e| e.to_string())
        .and_then(|dir| {
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            std::fs::write(dir.join(SERVER_FILE), url.as_str()).map_err(|e| e.to_string())
        });
    if let Err(error) = saved {
        return html(
            500,
            format!("Could not save the server: {}", escape(&error)),
        );
    }
    let target = serde_json::to_string(url.as_str()).unwrap_or_default();
    html(
        200,
        format!("<!doctype html><script>location.replace({target})</script>"),
    )
}

fn server_url_text(app: &tauri::AppHandle) -> Option<String> {
    std::fs::read_to_string(app.path().app_config_dir().ok()?.join(SERVER_FILE)).ok()
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "StudioNativeViewportMessages"]
    #[ivars = Weak<UIView>]
    struct ViewportMessages;

    unsafe impl NSObjectProtocol for ViewportMessages {}

    unsafe impl WKScriptMessageHandler for ViewportMessages {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn did_receive(&self, _controller: &WKUserContentController, _message: &WKScriptMessage) {
            if let Some(webview) = self.ivars().load() {
                push(&webview, KEYBOARD.get());
            }
        }
    }
);

impl ViewportMessages {
    fn new(mtm: MainThreadMarker, webview: &UIView) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Weak::from(webview));
        unsafe { msg_send![super(this), init] }
    }
}

thread_local! {
    // Last logged geometry, so the log has one line per change.
    static LOGGED: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// UIKit geometry of the web view; see ios_insets::reported.
fn geometry(webview: &UIView) -> Geometry {
    let safe = webview.safeAreaInsets();
    let window = webview.window();
    Geometry {
        safe: Insets {
            top: safe.top,
            right: safe.right,
            bottom: safe.bottom,
            left: safe.left,
        },
        window: window.as_ref().map_or((0.0, 0.0), |w| {
            (w.bounds().size.width, w.bounds().size.height)
        }),
        status_bar_height: status_bar_frame(webview).map(|r| r.size.height),
        orientation: window
            .and_then(|w| w.windowScene())
            .map_or(0, |scene| unsafe {
                msg_send![&*scene, interfaceOrientation]
            }),
    }
}

/// The reported insets, logged with their UIKit source when they change, so
/// a device console confirms the iOS 27.2 strip remap.
fn reported_insets(webview: &UIView) -> (Geometry, Insets) {
    let geometry = geometry(webview);
    let insets = reported(&geometry);
    let line = format!("studio-shell insets: uikit {geometry:?} reported {insets:?}");
    LOGGED.with_borrow_mut(|logged| {
        if *logged != line {
            eprintln!("{line}");
            *logged = line;
        }
    });
    (geometry, insets)
}

/// Send the current insets, and a keyboard state when one is known.
fn push(webview: &UIView, keyboard: Option<Keyboard>) {
    let (_, insets) = reported_insets(webview);
    let mut state = serde_json::json!({
        "insets": {
            "top": insets.top, "right": insets.right,
            "bottom": insets.bottom, "left": insets.left,
        }
    });
    if let Some(keyboard) = keyboard {
        state["keyboard"] = serde_json::json!({
            "overlap": keyboard.overlap,
            "duration": keyboard.duration,
            "curve": keyboard.curve,
        });
    }
    let script = NSString::from_str(&format!(
        "window.__studioNativeViewport&&window.__studioNativeViewport.receive({state})"
    ));
    // Older served clients ignore the event; a page without the receiver is a no-op.
    unsafe {
        let _: () = msg_send![
            webview,
            evaluateJavaScript: &*script,
            completionHandler: Option::<&block2::DynBlock<dyn Fn(*mut AnyObject, *mut objc2_foundation::NSError)>>::None
        ];
    }
}

/// Diagnostic mode (`STUDIO_SHELL_DIAG=1` at launch): every 2 s, and after
/// each keyboard event, save page and UIKit geometry to
/// `Documents/shell-diag/<tag>.json` for simulator checks.
fn diagnostics() -> bool {
    std::env::var("STUDIO_SHELL_DIAG").as_deref() == Ok("1")
}

fn diagnose(webview: &UIView, tag: &str) {
    if !diagnostics() {
        return;
    }
    let scroll: Retained<UIScrollView> = unsafe { msg_send![webview, scrollView] };
    let frame = webview.frame();
    let (raw, insets) = reported_insets(webview);
    let offset = scroll.contentOffset();
    let native = serde_json::json!({
        "frame": [frame.origin.x, frame.origin.y, frame.size.width, frame.size.height],
        "uikitSafe": [raw.safe.top, raw.safe.right, raw.safe.bottom, raw.safe.left],
        "reportedSafe": [insets.top, insets.right, insets.bottom, insets.left],
        "contentOffset": [offset.x, offset.y],
        "zoomScale": scroll.zoomScale(),
        "keyboard": KEYBOARD.get().map(|k| [k.overlap, k.duration, k.curve as f64]),
        "keyboardFrames": KEYBOARD_FRAMES.with_borrow(|v| v.clone()),
        "statusBar": status_bar_frame(webview).map(|r| [r.origin.x, r.origin.y, r.size.width, r.size.height]),
        "windowBounds": [raw.window.0, raw.window.1],
        "interfaceOrientation": raw.orientation,
    });
    let script = NSString::from_str(include_str!("ios_diag.js"));
    let path = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join("Documents/shell-diag");
    let tag = tag.to_owned();
    let block = block2::RcBlock::new(
        move |value: *mut AnyObject, _error: *mut objc2_foundation::NSError| {
            let page = unsafe { value.cast::<NSString>().as_ref() }
                .and_then(|v| serde_json::from_str::<serde_json::Value>(&v.to_string()).ok())
                .unwrap_or(serde_json::Value::Null);
            let sample = serde_json::json!({ "tag": tag, "native": native, "page": page });
            let _ = std::fs::create_dir_all(&path);
            let _ = std::fs::write(path.join(format!("{tag}.json")), sample.to_string());
        },
    );
    unsafe {
        let _: () = msg_send![webview, evaluateJavaScript: &*script, completionHandler: &*block];
    }
}

/// The system status bar frame in web view coordinates.
fn status_bar_frame(webview: &UIView) -> Option<CGRect> {
    let scene = webview.window()?.windowScene()?;
    unsafe {
        let manager: Option<Retained<AnyObject>> = msg_send![&*scene, statusBarManager];
        let frame: CGRect = msg_send![&*manager?, statusBarFrame];
        let window = webview.window()?;
        Some(webview.convertRect_fromView(frame, Some(&window)))
    }
}

/// Keyboard overlap with the web view bottom, from a UIKit notification.
fn keyboard_from(
    notification: &NSNotification,
    webview: &UIView,
    hiding: bool,
) -> Option<Keyboard> {
    let info: Retained<NSDictionary<AnyObject, AnyObject>> = notification.userInfo()?;
    let get = |key: &NSString| info.objectForKey(key.as_ref());
    let duration = get(unsafe { UIKeyboardAnimationDurationUserInfoKey })
        .and_then(|v| v.downcast::<NSNumber>().ok())
        .map_or(0.0, |v| v.doubleValue() * 1000.0);
    let curve = get(unsafe { UIKeyboardAnimationCurveUserInfoKey })
        .and_then(|v| v.downcast::<NSNumber>().ok())
        .map_or(7, |v| v.integerValue());
    let bounds = webview.bounds();
    let overlap = if hiding {
        0.0
    } else {
        let frame: CGRect = unsafe {
            get(UIKeyboardFrameEndUserInfoKey)?
                .downcast::<NSValue>()
                .ok()?
                .CGRectValue()
        };
        let window = webview.window()?;
        let space: Retained<AnyObject> = unsafe { msg_send![&*window.screen(), coordinateSpace] };
        let local: CGRect =
            unsafe { msg_send![webview, convertRect: frame, fromCoordinateSpace: &*space] };
        if diagnostics() {
            let from_window = webview.convertRect_fromView(frame, None);
            KEYBOARD_FRAMES.set(serde_json::json!({
                "end": rect_json(frame),
                "fromScreenSpace": rect_json(local),
                "fromWindow": rect_json(from_window),
                "screenBounds": rect_json(window.screen().bounds()),
                "webviewBounds": rect_json(bounds),
            }));
        }
        let horizontal =
            local.origin.x < bounds.size.width && local.origin.x + local.size.width > 0.0;
        if horizontal && local.size != CGSize::ZERO {
            (bounds.size.height - local.origin.y).clamp(0.0, bounds.size.height)
        } else {
            0.0
        }
    };
    Some(Keyboard {
        overlap,
        duration,
        curve,
    })
}

/// Fill the window, keep UIKit and WebKit from moving the page for the
/// keyboard, and connect the viewport receiver.
pub fn configure(window: &WebviewWindow) {
    let webkit_keyboard = std::env::var("STUDIO_SHELL_WEBKIT_KEYBOARD").as_deref() == Ok("1");
    let _ = window.with_webview(move |platform| unsafe {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let webview: Retained<UIView> =
            Retained::retain(platform.inner() as *mut UIView).expect("WKWebView");
        let scroll: Retained<UIScrollView> = msg_send![&*webview, scrollView];
        scroll.setContentInsetAdjustmentBehavior(UIScrollViewContentInsetAdjustmentBehavior::Never);
        // The page root is fixed; inner scrollers are separate child views.
        scroll.setScrollEnabled(false);
        if let Some(parent) = webview.superview() {
            webview.setFrame(parent.bounds());
        }
        let center = NSNotificationCenter::defaultCenter();
        if !webkit_keyboard {
            // WebKit resizes the visual viewport and pans the page for the
            // keyboard. The page moves only its composer, from our events.
            for name in [
                UIKeyboardWillChangeFrameNotification,
                UIKeyboardDidChangeFrameNotification,
                UIKeyboardWillShowNotification,
                UIKeyboardDidShowNotification,
                UIKeyboardWillHideNotification,
                UIKeyboardDidHideNotification,
            ] {
                center.removeObserver_name_object(&webview, Some(name), None);
            }
        }
        let controller: Retained<WKUserContentController> =
            Retained::retain(platform.controller() as *mut WKUserContentController)
                .expect("controller");
        let handler = ViewportMessages::new(mtm, &webview);
        controller.addScriptMessageHandler_name(
            ProtocolObject::from_ref(&*handler),
            &NSString::from_str(MESSAGE_HANDLER),
        );
        let main = NSOperationQueue::mainQueue();
        for (name, hiding) in [
            (UIKeyboardWillChangeFrameNotification, false),
            (UIKeyboardWillHideNotification, true),
        ] {
            let target = Weak::from(&*webview);
            let block = block2::RcBlock::new(move |notification: NonNull<NSNotification>| {
                let Some(webview) = target.load() else { return };
                if let Some(keyboard) = keyboard_from(notification.as_ref(), &webview, hiding) {
                    KEYBOARD.set(Some(keyboard));
                    push(&webview, Some(keyboard));
                    diagnose(
                        &webview,
                        if hiding {
                            "keyboard-hide"
                        } else {
                            "keyboard-change"
                        },
                    );
                }
            });
            // The observer token lives for the app lifetime.
            std::mem::forget(center.addObserverForName_object_queue_usingBlock(
                Some(name),
                None,
                Some(&main),
                &block,
            ));
        }
        push(&webview, KEYBOARD.get());
        if diagnostics() {
            let target = Weak::from(&*webview);
            let block = block2::RcBlock::new(move |_timer: NonNull<objc2_foundation::NSTimer>| {
                if let Some(webview) = target.load() {
                    diagnose(&webview, "latest");
                }
            });
            let _ = objc2_foundation::NSTimer::scheduledTimerWithTimeInterval_repeats_block(
                2.0, true, &block,
            );
        }
    });
}

/// Resend the geometry after each page load; the receiver is new per document.
pub fn page_loaded(window: &tauri::Webview) {
    let _ = window.with_webview(|platform| unsafe {
        if let Some(webview) = Retained::retain(platform.inner() as *mut UIView) {
            push(&webview, KEYBOARD.get());
        }
    });
}

/// Open links that request a new window in the system browser.
pub fn open_external(url: &tauri::Url) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Some(target) = objc2_foundation::NSURL::URLWithString(&NSString::from_str(url.as_str()))
    else {
        return;
    };
    let app = objc2_ui_kit::UIApplication::sharedApplication(mtm);
    unsafe {
        let options = NSDictionary::<NSString, AnyObject>::new();
        let _: () = msg_send![&*app, openURL: &*target, options: &*options, completionHandler: Option::<&block2::DynBlock<dyn Fn(objc2::runtime::Bool)>>::None];
    }
}
