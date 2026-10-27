//! iOS shell: one full-window WKWebView that loads the Studio web client from
//! the user's server. The page receives no Tauri commands. The shell pushes
//! UIKit safe-area insets and keyboard animations into the page through
//! `ios_viewport.js`; see README.md "iOS shell".

use crate::ios_insets::{Geometry, Insets, Rect, reported, status_column};
use crate::ios_server::{BUILD_DEFAULT, choose, saved_value, valid_server};
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
    // Last UIHingeInteraction update: UIHingeStatus raw value and UIHinge.angle.
    static HINGE: Cell<Option<(isize, f64)>> = const { Cell::new(None) };
    // Last status column and hinge band sent to the page; checks push only
    // on change.
    static STATUS_COLUMN: Cell<Option<Rect>> = const { Cell::new(None) };
    static HINGE_BAND: Cell<Option<(Rect, bool)>> = const { Cell::new(None) };
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

/// The page to load; see ios_server::choose for the order.
pub fn server_url(app: &tauri::App) -> Option<tauri::Url> {
    let launch = std::env::var("STUDIO_SERVER_URL").ok();
    let saved = saved_server(app.handle());
    choose(launch.as_deref(), saved.as_deref(), BUILD_DEFAULT)
}

fn saved_server(app: &tauri::AppHandle) -> Option<String> {
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

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

define_class!(
    // SAFETY: NSObject superclass; the class adds only a weak web view ivar.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "StudioNativeViewportMessages"]
    #[ivars = Weak<UIView>]
    struct ViewportMessages;

    // SAFETY: NSObjectProtocol has no extra requirements for an NSObject subclass.
    unsafe impl NSObjectProtocol for ViewportMessages {}

    // SAFETY: the method below has the WKScriptMessageHandler signature.
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
        // SAFETY: NSObject init on a freshly allocated instance with its ivars set.
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
            // SAFETY: UIWindowScene.interfaceOrientation returns a UIInterfaceOrientation (NSInteger).
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
    let (geometry, insets) = reported_insets(webview);
    let column = current_status_column(webview, &insets, geometry.window.0);
    let band = current_hinge_band(webview);
    STATUS_COLUMN.set(column);
    HINGE_BAND.set(band);
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
    // Always present (null without a region), so the page drops an old value.
    state["statusColumn"] = column.map_or(
        serde_json::Value::Null,
        |r| serde_json::json!({ "x": r.x, "y": r.y, "width": r.width, "height": r.height }),
    );
    state["hingeBand"] = band.map_or(serde_json::Value::Null, |(r, active)| {
        serde_json::json!({ "x": r.x, "y": r.y, "width": r.width, "height": r.height, "active": active })
    });
    if let Some((status, angle)) = HINGE.get() {
        state["hinge"] = serde_json::json!({ "status": hinge_status(status), "angle": angle });
    }
    let script = NSString::from_str(&format!(
        "window.__studioNativeViewport&&window.__studioNativeViewport.receive({state})"
    ));
    // Older served clients ignore the event; a page without the receiver is a no-op.
    // SAFETY: evaluateJavaScript:completionHandler: with an NSString and a null handler, on the main thread.
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
    // SAFETY: WKWebView.scrollView returns its non-null UIScrollView.
    let scroll: Retained<UIScrollView> = unsafe { msg_send![webview, scrollView] };
    let frame = webview.frame();
    let (raw, insets) = reported_insets(webview);
    let offset = scroll.contentOffset();
    // Cost of one status column and hinge band check, for the poll interval.
    let start = std::time::Instant::now();
    let _ = reserved_regions(webview, "occlusion");
    let _ = reserved_regions(webview, "division");
    let region_micros = start.elapsed().as_micros() as u64;
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
        "reservedRegions": {
            "occlusion": reserved_regions(webview, "occlusion").iter().map(Region::json).collect::<Vec<_>>(),
            "division": reserved_regions(webview, "division").iter().map(Region::json).collect::<Vec<_>>(),
        },
        "statusColumn": STATUS_COLUMN.get().map(|r| [r.x, r.y, r.width, r.height]),
        "hingeBand": HINGE_BAND.get().map(|(r, active)| serde_json::json!({ "frame": [r.x, r.y, r.width, r.height], "active": active })),
        "regionQueryMicros": region_micros,
    });
    let script = NSString::from_str(include_str!("ios_diag.js"));
    let path = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join("Documents/shell-diag");
    let tag = tag.to_owned();
    let block = block2::RcBlock::new(
        move |value: *mut AnyObject, _error: *mut objc2_foundation::NSError| {
            // SAFETY: WebKit passes the script result or null; ios_diag.js returns a JSON string.
            let page = unsafe { value.cast::<NSString>().as_ref() }
                .and_then(|v| serde_json::from_str::<serde_json::Value>(&v.to_string()).ok())
                .unwrap_or(serde_json::Value::Null);
            let sample = serde_json::json!({ "tag": tag, "native": native, "page": page });
            let _ = std::fs::create_dir_all(&path);
            let _ = std::fs::write(path.join(format!("{tag}.json")), sample.to_string());
        },
    );
    // SAFETY: evaluateJavaScript:completionHandler: with an NSString and a block of the WebKit signature.
    unsafe {
        let _: () = msg_send![webview, evaluateJavaScript: &*script, completionHandler: &*block];
    }
}

/// The system status bar frame in web view coordinates.
fn status_bar_frame(webview: &UIView) -> Option<CGRect> {
    let scene = webview.window()?.windowScene()?;
    // SAFETY: UIWindowScene.statusBarManager and UIStatusBarManager.statusBarFrame are UIKit getters.
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
    // SAFETY: a UIKit constant user info key.
    let duration = get(unsafe { UIKeyboardAnimationDurationUserInfoKey })
        .and_then(|v| v.downcast::<NSNumber>().ok())
        .map_or(0.0, |v| v.doubleValue() * 1000.0);
    // SAFETY: a UIKit constant user info key.
    let curve = get(unsafe { UIKeyboardAnimationCurveUserInfoKey })
        .and_then(|v| v.downcast::<NSNumber>().ok())
        .map_or(7, |v| v.integerValue());
    let bounds = webview.bounds();
    let overlap = if hiding {
        0.0
    } else {
        // SAFETY: UIKeyboardFrameEndUserInfoKey is a UIKit constant; its value is an NSValue of a CGRect.
        let frame: CGRect = unsafe {
            get(UIKeyboardFrameEndUserInfoKey)?
                .downcast::<NSValue>()
                .ok()?
                .CGRectValue()
        };
        let window = webview.window()?;
        // SAFETY: UIScreen.coordinateSpace returns its non-null coordinate space.
        let space: Retained<AnyObject> = unsafe { msg_send![&*window.screen(), coordinateSpace] };
        let local: CGRect =
            // SAFETY: UIView convertRect:fromCoordinateSpace: takes a CGRect and a UICoordinateSpace.
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
    let _ = window.with_webview(move |platform| {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        // SAFETY: Tauri passes its live WKWebView (a UIView) on the main thread.
        let webview: Retained<UIView> =
            unsafe { Retained::retain(platform.inner() as *mut UIView) }.expect("WKWebView");
        // SAFETY: WKWebView.scrollView returns its non-null UIScrollView.
        let scroll: Retained<UIScrollView> = unsafe { msg_send![&*webview, scrollView] };
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
            // SAFETY: UIKit's constant notification names; removing the web
            // view as an observer only stops those deliveries.
            unsafe {
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
        }
        // SAFETY: Tauri passes the web view's live WKUserContentController.
        let controller: Retained<WKUserContentController> =
            unsafe { Retained::retain(platform.controller() as *mut WKUserContentController) }
                .expect("controller");
        let handler = ViewportMessages::new(mtm, &webview);
        // SAFETY: the handler implements WKScriptMessageHandler; WebKit
        // retains it for the controller lifetime.
        unsafe {
            controller.addScriptMessageHandler_name(
                ProtocolObject::from_ref(&*handler),
                &NSString::from_str(MESSAGE_HANDLER),
            );
        }
        let main = NSOperationQueue::mainQueue();
        // SAFETY: UIKit's constant notification names.
        let names = unsafe {
            [
                (UIKeyboardWillChangeFrameNotification, false),
                (UIKeyboardWillHideNotification, true),
            ]
        };
        for (name, hiding) in names {
            let target = Weak::from(&*webview);
            let block = block2::RcBlock::new(move |notification: NonNull<NSNotification>| {
                let Some(webview) = target.load() else { return };
                // SAFETY: NSNotificationCenter passes a valid notification
                // for the duration of the block call.
                let notification = unsafe { notification.as_ref() };
                if let Some(keyboard) = keyboard_from(notification, &webview, hiding) {
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
            // SAFETY: the block only reads the notification and runs on the
            // main queue, where the web view lives.
            std::mem::forget(unsafe {
                center.addObserverForName_object_queue_usingBlock(
                    Some(name),
                    None,
                    Some(&main),
                    &block,
                )
            });
        }
        push(&webview, KEYBOARD.get());
        install_server_switch(mtm, &webview, &main_window);
        install_hinge(&webview);
        watch_status_column(&webview);
        if diagnostics() {
            let target = Weak::from(&*webview);
            let block = block2::RcBlock::new(move |_timer: NonNull<objc2_foundation::NSTimer>| {
                if let Some(webview) = target.load() {
                    diagnose(&webview, "latest");
                }
            });
            // SAFETY: a main run loop timer; the block holds a weak web view.
            let _ = unsafe {
                objc2_foundation::NSTimer::scheduledTimerWithTimeInterval_repeats_block(
                    2.0, true, &block,
                )
            };
        }
    });
}

/// Resend the geometry after each page load; the receiver is new per document.
pub fn page_loaded(window: &tauri::Webview, url: &tauri::Url) {
    // Remember the server after each successful load, so the home screen
    // icon opens it next time.
    if let Some(value) = saved_value(url) {
        SERVER_LOADED.store(true, Ordering::SeqCst);
        let app = window.app_handle();
        if saved_server(app).as_deref().map(str::trim) != Some(value.as_str()) {
            if let Ok(dir) = app.path().app_config_dir() {
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::fs::write(dir.join(SERVER_FILE), &value);
            }
        }
    }
    let _ = window.with_webview(|platform| {
        // SAFETY: Tauri passes its live WKWebView (a UIView) on the main thread.
        if let Some(webview) = unsafe { Retained::retain(platform.inner() as *mut UIView) } {
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
    // SAFETY: openURL:options:completionHandler: with an NSURL, an empty dictionary and a null handler.
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
    // SAFETY: NSObject superclass; the class has no ivars.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "StudioServerSwitch"]
    struct ServerSwitch;

    impl ServerSwitch {
        /// A two-finger long press opens the start page to change the server.
        // SAFETY: the method below has the UIGestureRecognizer action signature.
        #[unsafe(method(open:))]
        fn open(&self, recognizer: &AnyObject) {
            // SAFETY: UIGestureRecognizer.state returns a UIGestureRecognizerState (NSInteger).
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
    // SAFETY: NSObject init on a freshly allocated instance.
    let target: Retained<ServerSwitch> = unsafe { msg_send![ServerSwitch::alloc(mtm), init] };
    // SAFETY: UILongPressGestureRecognizer initializer and setters, and UIView addGestureRecognizer:.
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
            // SAFETY: UIKit passes a valid UIHingeInteractionUpdate or null.
            let Some(update) = (unsafe { update.as_ref() }) else {
                return;
            };
            // SAFETY: UIHingeInteractionUpdate.hinge returns a UIHinge or null.
            let hinge: *mut AnyObject = unsafe { msg_send![update, hinge] };
            // SAFETY: hinge is a valid UIHinge or null.
            let value = unsafe { hinge.as_ref() }.map(|hinge| {
                // SAFETY: UIHinge.status returns a UIHingeStatus (NSInteger).
                let status: isize = unsafe { msg_send![hinge, status] };
                // SAFETY: UIHinge.angle returns a CGFloat.
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
    // SAFETY: UIHingeInteraction initWithUpdateHandler: takes a block of the UIKit signature; UIView addInteraction:.
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

/// One iOS 27.1 reserved region, in web view coordinates (CSS px).
struct Region {
    frame: Rect,
    /// UIEdgeInsets: top, right, bottom, left.
    margins: [f64; 4],
    active: bool,
    id: Option<String>,
}

impl Region {
    fn json(&self) -> serde_json::Value {
        let Rect {
            x,
            y,
            width,
            height,
        } = self.frame;
        serde_json::json!({
            "frame": [x, y, width, height],
            "margins": self.margins,
            "active": self.active,
            "id": self.id,
        })
    }
}

/// iOS 27.1 reserved regions of one kind ("occlusion" or "division"),
/// inactive ones included. Looked up at run time, so older systems return none.
fn reserved_regions(webview: &UIView, kind: &str) -> Vec<Region> {
    let Some(class) = objc2::runtime::AnyClass::get(c"UIViewReservedRegionKind") else {
        return Vec::new();
    };
    // SAFETY: respondsToSelector: is an NSObject method.
    let responds: bool = unsafe {
        msg_send![webview, respondsToSelector: objc2::sel!(reservedRegionsOfKind:options:)]
    };
    if !responds {
        return Vec::new();
    }
    // SAFETY: guarded by respondsToSelector:; selectors and types match UIViewReservedRegion (iOS 27.1).
    unsafe {
        let kind_value: Option<Retained<AnyObject>> = match kind {
            "division" => msg_send![class, divisionRegionKind],
            _ => msg_send![class, occlusionRegionKind],
        };
        let Some(kind_value) = kind_value else {
            return Vec::new();
        };
        // UIViewReservedRegionQueryOptionsIncludeInactive
        let regions: Retained<AnyObject> =
            msg_send![webview, reservedRegionsOfKind: &*kind_value, options: 1usize];
        let count: usize = msg_send![&*regions, count];
        (0..count)
            .map(|index| {
                let region: Retained<AnyObject> = msg_send![&*regions, objectAtIndex: index];
                let frame: CGRect = msg_send![&*region, frame];
                let margins: objc2_ui_kit::UIEdgeInsets = msg_send![&*region, margins];
                let active: bool = msg_send![&*region, isActive];
                let identifier: Option<Retained<AnyObject>> = msg_send![&*region, identifier];
                Region {
                    frame: Rect {
                        x: frame.origin.x,
                        y: frame.origin.y,
                        width: frame.size.width,
                        height: frame.size.height,
                    },
                    margins: [margins.top, margins.right, margins.bottom, margins.left],
                    active,
                    id: identifier.map(|id| {
                        let text: Retained<NSString> = msg_send![&*id, description];
                        text.to_string()
                    }),
                }
            })
            .collect()
    }
}

/// The status column in the reported strip, from the active occlusion regions.
fn current_status_column(webview: &UIView, insets: &Insets, width: f64) -> Option<Rect> {
    let active: Vec<Rect> = reserved_regions(webview, "occlusion")
        .into_iter()
        .filter(|region| region.active)
        .map(|region| region.frame)
        .collect();
    status_column(insets, width, &active)
}

/// The hinge band: the first division region (iOS 27.1), with its active
/// flag. The frame includes the region margins.
fn current_hinge_band(webview: &UIView) -> Option<(Rect, bool)> {
    reserved_regions(webview, "division")
        .into_iter()
        .next()
        .map(|region| (region.frame, region.active))
}

/// Push when the status column or the hinge band changed since the last push.
fn check_regions(webview: &UIView, trigger: &str) {
    let (geometry, insets) = reported_insets(webview);
    let column = current_status_column(webview, &insets, geometry.window.0);
    let band = current_hinge_band(webview);
    if column != STATUS_COLUMN.get() || band != HINGE_BAND.get() {
        eprintln!("studio-shell regions ({trigger}): status column {column:?} hinge band {band:?}");
        push(webview, KEYBOARD.get());
        diagnose(webview, "regions");
    }
}

/// UIKit has no change notification for reserved regions. The status bar
/// frame and app activation notifications trigger a check at once; a 0.25 s
/// poll is the backstop (a call, screen recording or
/// hotspot pill moves the clock and Wi-Fi without a safe area change).
fn watch_status_column(webview: &UIView) {
    let center = NSNotificationCenter::defaultCenter();
    let main = NSOperationQueue::mainQueue();
    for name in [
        "UIApplicationDidChangeStatusBarFrameNotification",
        "UIApplicationDidBecomeActiveNotification",
    ] {
        let target = Weak::from(webview);
        let block = block2::RcBlock::new(move |_notification: NonNull<NSNotification>| {
            if let Some(webview) = target.load() {
                check_regions(&webview, name);
            }
        });
        // The observer token lives for the app lifetime.
        // SAFETY: the block runs on the main queue and holds a weak web view.
        std::mem::forget(unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(&NSString::from_str(name)),
                None,
                Some(&main),
                &block,
            )
        });
    }
    let target = Weak::from(webview);
    let block = block2::RcBlock::new(move |_timer: NonNull<objc2_foundation::NSTimer>| {
        if let Some(webview) = target.load() {
            check_regions(&webview, "poll");
        }
    });
    // SAFETY: a main run loop timer; the block holds a weak web view.
    let _ = unsafe {
        objc2_foundation::NSTimer::scheduledTimerWithTimeInterval_repeats_block(
            STATUS_POLL_SECONDS,
            true,
            &block,
        )
    };
}

/// Backstop interval of the reserved region check.
const STATUS_POLL_SECONDS: f64 = 0.25;
