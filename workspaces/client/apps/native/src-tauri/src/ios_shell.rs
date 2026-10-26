//! iOS shell: one full-window WKWebView that loads the Studio web client from
//! the user's server. The page receives no Tauri commands. The shell pushes
//! UIKit safe-area insets and keyboard animations into the page through
//! `ios_viewport.js`; see README.md "iOS shell".

use crate::ios_insets::{Geometry, Insets, reported};
pub use crate::ios_server::START_SCHEME;
use crate::ios_server::{BUILD_DEFAULT, Connect, Source, choose, connect, save_after_load};
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
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{Manager, WebviewWindow};

/// Script message handler name used by `ios_viewport.js`.
const MESSAGE_HANDLER: &str = "studioNativeViewport";
/// Saved server URL in the app configuration directory.
const SERVER_FILE: &str = "server-url.txt";

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
    // Last UIHingeInteraction update: UIHingeStatus raw value and UIHinge.angle.
    static HINGE: Cell<Option<(isize, f64)>> = const { Cell::new(None) };
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

/// Time, in seconds, for the first server page to finish loading before the
/// shell shows the start page with the saved address and a retry.
const FIRST_LOAD_TIMEOUT: u64 = 12;
/// Set when a server page finished loading in this launch.
static SERVER_LOADED: AtomicBool = AtomicBool::new(false);
/// The main window, for the start page gesture.
static MAIN_WINDOW: OnceLock<WebviewWindow> = OnceLock::new();

/// The address that the launch value or the build default opened. Only its
/// load saves it; see ios_server::save_after_load.
static OPENED: OnceLock<tauri::Url> = OnceLock::new();

/// The page to load at launch; see ios_server::choose for the order. With
/// none, the start page opens.
pub fn server_url(app: &tauri::App) -> Option<tauri::Url> {
    let launch = std::env::var("STUDIO_SERVER_URL").ok();
    let saved = saved_server(app.handle());
    let (url, source) = choose(launch.as_deref(), saved.as_deref(), &[], BUILD_DEFAULT)?;
    if source != Source::Saved {
        let _ = OPENED.set(url.clone());
    }
    Some(url)
}

pub(crate) fn saved_server(app: &tauri::AppHandle) -> Option<String> {
    std::fs::read_to_string(app.path().app_config_dir().ok()?.join(SERVER_FILE)).ok()
}

fn start_url(failed: bool) -> tauri::Url {
    format!(
        "{START_SCHEME}://localhost/{}",
        if failed { "?failed=1" } else { "" }
    )
    .parse()
    .expect("start page URL")
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
        let saved = saved_server(ctx.app_handle())
            .or_else(|| BUILD_DEFAULT.map(str::to_owned))
            .unwrap_or_default();
        let failed = request
            .uri()
            .query()
            .is_some_and(|q| q.contains("failed=1"));
        let message = if failed {
            "Studio did not open from this address. Check that Tailscale is on, then tap Connect."
        } else {
            ""
        };
        // The list of found servers comes from ios_discovery::push after the
        // page loads.
        return html(
            200,
            include_str!("ios_start.html")
                .replace("{{token}}", token)
                .replace("{{message}}", message)
                .replace("{{saved}}", &escape(saved.trim())),
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
    let saved = saved_server(ctx.app_handle());
    let url = match connect(
        &field("url").unwrap_or_default(),
        field("found").as_deref() == Some("1"),
        field("confirm").as_deref() == Some("1"),
        saved.as_deref().map(str::trim),
    ) {
        Connect::Open(url) => url,
        Connect::Confirm(url) => return html(200, confirm_page(&url, token)),
        Connect::Invalid => {
            return html(
                400,
                "Use an https:// server address. <a href=\"/\">Back</a>".into(),
            );
        }
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

/// The confirm step for a found server on another tailnet than the saved
/// address. It shows the full host.
fn confirm_page(url: &tauri::Url, token: &str) -> String {
    let mut open: tauri::Url = format!("{START_SCHEME}://localhost/connect")
        .parse()
        .expect("connect URL");
    open.query_pairs_mut()
        .append_pair("url", url.as_str())
        .append_pair("token", token)
        .append_pair("found", "1")
        .append_pair("confirm", "1");
    let host = escape(url.host_str().unwrap_or_default());
    format!(
        "<!doctype html><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <style>body{{font:17px -apple-system,system-ui,sans-serif;margin:24px;display:grid;gap:16px}}\
         a{{padding:10px 12px;border-radius:10px;border:1px solid currentColor;text-align:center}}</style>\
         <h1>Other network</h1><p><strong>{host}</strong> is not on the tailnet of your saved server. \
         Open it only if you know this server.</p><a href=\"{}\">Open {host}</a><a href=\"/\">Back</a>",
        escape(open.as_str())
    )
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
    if let Some((status, angle)) = HINGE.get() {
        state["hinge"] = serde_json::json!({ "status": hinge_status(status), "angle": angle });
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
        "hinge": HINGE.get().map(|(status, angle)| serde_json::json!({ "status": hinge_status(status), "raw": status, "angle": angle })),
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
    let main_window = window.clone();
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
        // The shell page never zooms: no pinch here, and ios_viewport.js
        // limits the viewport scale (double tap, focus auto-zoom).
        scroll.setMinimumZoomScale(1.0);
        scroll.setMaximumZoomScale(1.0);
        if let Some(pinch) = scroll.pinchGestureRecognizer() {
            pinch.setEnabled(false);
        }
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
        install_server_switch(mtm, &webview, &main_window);
        install_hinge(&webview);
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
pub fn page_loaded(window: &tauri::Webview, url: &tauri::Url) {
    // The search runs only while the start page is shown; its receiver is
    // new per document.
    crate::ios_discovery::page_loaded(window, url);
    if crate::ios_server::saved_value(url).is_some() {
        SERVER_LOADED.store(true, Ordering::SeqCst);
    }
    // Remember the server that the launch value or the build default opened,
    // so the home screen icon opens it next time. Other origins do not
    // change the saved address.
    if let Some(value) = save_after_load(url, OPENED.get()) {
        let app = window.app_handle();
        if saved_server(app).as_deref().map(str::trim) != Some(value.as_str()) {
            if let Ok(dir) = app.path().app_config_dir() {
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::fs::write(dir.join(SERVER_FILE), &value);
            }
        }
    }
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

/// Show the start page with the saved address when the server page does not
/// finish loading in time (for example, Tailscale is off).
pub fn watch_first_load(window: &WebviewWindow) {
    let window = window.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(FIRST_LOAD_TIMEOUT));
        if !SERVER_LOADED.load(Ordering::SeqCst) {
            let _ = window.navigate(start_url(true));
        }
    });
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "StudioServerSwitch"]
    struct ServerSwitch;

    impl ServerSwitch {
        /// A two-finger long press opens the start page to change the server.
        #[unsafe(method(open:))]
        fn open(&self, recognizer: &AnyObject) {
            let state: isize = unsafe { msg_send![recognizer, state] };
            // UIGestureRecognizerStateBegan
            if state == 1 {
                if let Some(window) = MAIN_WINDOW.get() {
                    let _ = window.navigate(start_url(false));
                }
            }
        }
    }
);

fn install_server_switch(mtm: MainThreadMarker, webview: &UIView, window: &WebviewWindow) {
    let _ = MAIN_WINDOW.set(window.clone());
    let target: Retained<ServerSwitch> = unsafe { msg_send![ServerSwitch::alloc(mtm), init] };
    unsafe {
        let class = objc2::runtime::AnyClass::get(c"UILongPressGestureRecognizer").expect("UIKit");
        let recognizer: Retained<AnyObject> = msg_send![msg_send![class, alloc], initWithTarget: &*target, action: objc2::sel!(open:)];
        let _: () = msg_send![&*recognizer, setNumberOfTouchesRequired: 2usize];
        let _: () = msg_send![&*recognizer, setMinimumPressDuration: 1.5f64];
        let _: () = msg_send![&*recognizer, setCancelsTouchesInView: false];
        let _: () = msg_send![webview, addGestureRecognizer: &*recognizer];
    }
    // The recognizer keeps a weak target; keep it for the app lifetime.
    std::mem::forget(target);
}

/// UIHingeStatus raw value as the page API word.
fn hinge_status(status: isize) -> &'static str {
    match status {
        1 => "closed",
        2 => "partial",
        3 => "open",
        _ => "unknown",
    }
}

/// iOS 27.1 UIHingeInteraction (looked up at run time, so older systems skip
/// it): each hinge update goes to the page with the viewport state.
fn install_hinge(webview: &UIView) {
    let Some(class) = objc2::runtime::AnyClass::get(c"UIHingeInteraction") else {
        eprintln!("studio-shell hinge: UIHingeInteraction unavailable");
        return;
    };
    let target = Weak::from(webview);
    let handler = block2::RcBlock::new(
        move |_interaction: *mut AnyObject, update: *mut AnyObject| {
            let Some(update) = (unsafe { update.as_ref() }) else {
                return;
            };
            let hinge: *mut AnyObject = unsafe { msg_send![update, hinge] };
            let value = unsafe { hinge.as_ref() }.map(|hinge| {
                let status: isize = unsafe { msg_send![hinge, status] };
                let angle: f64 = unsafe { msg_send![hinge, angle] };
                (status, angle)
            });
            let value = value.unwrap_or((0, f64::NAN));
            if HINGE.get().map(|(s, a)| (s, a.to_bits())) != Some((value.0, value.1.to_bits())) {
                eprintln!("studio-shell hinge: status {} angle {}", value.0, value.1);
            }
            HINGE.set(Some(value));
            if let Some(webview) = target.load() {
                push(&webview, KEYBOARD.get());
                diagnose(&webview, "hinge");
            }
        },
    );
    unsafe {
        let interaction: Option<Retained<AnyObject>> =
            msg_send![msg_send![class, alloc], initWithUpdateHandler: &*handler];
        let Some(interaction) = interaction else {
            return;
        };
        // The web view's host view carries the interaction.
        let host = webview.superview().unwrap_or_else(|| {
            Retained::retain(webview as *const UIView as *mut UIView).expect("web view")
        });
        let _: () = msg_send![&*host, addInteraction: &*interaction];
    }
}
