//! Static HTML presentation outside the Attar UI runtime.
//! The system renderer has no Studio bridge, cookies, file grant or network grant.
// The native paths here run only on macOS; other targets build the module for checks.
#![cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, unused_imports, unused_variables)
)]
use crate::protocol::{Request, Response};
use serde_json::Value;

const SOURCE_LIMIT: usize = 2 * 1024 * 1024;
const WIDTH: usize = 960;
const HEIGHT: usize = 600;
const MAX_PAGES: usize = 80;

// This policy is parsed before all document-controlled bytes. A later policy
// cannot relax it. Native navigation policy also denies all subsequent loads.
const PREFIX: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; script-src 'none'; style-src 'unsafe-inline'; img-src data:; font-src data:; connect-src 'none'; frame-src 'none'; object-src 'none'; media-src 'none'; form-action 'none'; base-uri 'none'\"><style>html{color-scheme:light;background:white;color:#17202a;font-family:system-ui,sans-serif}body{margin:16px;overflow-wrap:anywhere}img,svg{max-width:100%}*{box-sizing:border-box}</style></head><body>";
fn document(source: &str) -> Result<String, String> {
    if source.len() > SOURCE_LIMIT {
        return Err("The HTML preview exceeds 2 MiB. Use Source or Save a copy.".into());
    }
    Ok(format!("{PREFIX}{source}</body></html>"))
}
pub async fn dispatch(request: Request) -> Response {
    let result: Result<Value, String> = async {
        if !matches!(
            request.method.as_str(),
            "document.render" | "document.diagram"
        ) {
            return Err("Unknown document command.".into());
        }
        let source = request.params["source"]
            .as_str()
            .ok_or("HTML source is required.")?;
        if request.method == "document.diagram" {
            if source.encode_utf16().count() > 50000 {
                return Err("The diagram exceeds 50,000 characters.".into());
            }
            #[cfg(target_os = "macos")]
            {
                return mac::render(document("")?, 0, Some(source.to_owned())).await;
            }
            #[cfg(not(target_os = "macos"))]
            {
                return Err("Diagram preview requires macOS.".into());
            }
        }
        let page = request.params["page"]
            .as_u64()
            .filter(|p| *p < MAX_PAGES as u64)
            .ok_or("Invalid document page.")? as usize;
        let html = document(source)?;
        #[cfg(target_os = "macos")]
        {
            mac::render(html, page, None).await
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (html, page);
            Err("HTML preview requires macOS.".into())
        }
    }
    .await;
    match result {
        Ok(value) => Response::success(request.id, value),
        Err(error) => Response::failure(request.id, "document_preview", error),
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};
    use objc2::{
        AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
        rc::{Retained, autoreleasepool},
        runtime::{AnyObject, ProtocolObject},
    };
    use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSImage};
    use objc2_foundation::{
        NSDictionary, NSError, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
    };
    use objc2_web_kit::{
        WKContentWorld, WKNavigation, WKNavigationAction, WKNavigationActionPolicy,
        WKNavigationDelegate, WKSnapshotConfiguration, WKWebView, WKWebViewConfiguration,
        WKWebpagePreferences, WKWebsiteDataStore,
    };
    use std::{
        cell::{Cell, RefCell},
        collections::HashMap,
        sync::{
            Arc, OnceLock,
            atomic::{AtomicU64, Ordering},
        },
        time::Duration,
    };
    use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};
    type Reply = oneshot::Sender<Result<Value, String>>;
    struct Job {
        view: Retained<WKWebView>,
        _delegate: Retained<Delegate>,
        reply: Reply,
        page: usize,
        diagram_source: Option<String>,
        _permit: OwnedSemaphorePermit,
    }
    thread_local! { static JOBS: RefCell<HashMap<u64, Job>> = RefCell::default(); }
    struct Ivars {
        id: u64,
        navigated: Cell<bool>,
        finished: Cell<bool>,
    }
    define_class!(
        #[unsafe(super = NSObject)]
        #[name = "StudioStaticDocumentDelegate"]
        #[thread_kind = MainThreadOnly]
        #[ivars = Ivars]
        struct Delegate;
        unsafe impl NSObjectProtocol for Delegate {}
        unsafe impl WKNavigationDelegate for Delegate {
            #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
            fn navigation(
                &self,
                _view: &WKWebView,
                action: &WKNavigationAction,
                decision: &block2::DynBlock<dyn Fn(WKNavigationActionPolicy)>,
            ) {
                // Only the explicit loadHTMLString call may create a document.
                let initial = unsafe {
                    action.targetFrame().is_some_and(|f| f.isMainFrame())
                        && action
                            .request()
                            .URL()
                            .and_then(|u| u.absoluteString())
                            .is_some_and(|u| u.to_string() == "about:blank")
                };
                let allow = initial && !self.ivars().navigated.replace(true);
                decision.call((if allow {
                    WKNavigationActionPolicy::Allow
                } else {
                    WKNavigationActionPolicy::Cancel
                },));
            }
            #[unsafe(method(webView:didFinishNavigation:))]
            fn loaded(&self, view: &WKWebView, _navigation: Option<&WKNavigation>) {
                if self.ivars().finished.replace(true) {
                    return;
                }
                let id = self.ivars().id;
                let diagram = JOBS.with(|jobs| {
                    jobs.borrow()
                        .get(&id)
                        .and_then(|job| job.diagram_source.clone())
                });
                if let Some(source) = diagram {
                    render_diagram(id, view, source);
                } else {
                    measure(id, view);
                }
            }
            #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
            fn failed_provisional(
                &self,
                _view: &WKWebView,
                _navigation: Option<&WKNavigation>,
                error: &NSError,
            ) {
                finish(
                    self.ivars().id,
                    Err(error.localizedDescription().to_string()),
                );
            }
            #[unsafe(method(webView:didFailNavigation:withError:))]
            fn failed(
                &self,
                _view: &WKWebView,
                _navigation: Option<&WKNavigation>,
                error: &NSError,
            ) {
                finish(
                    self.ivars().id,
                    Err(error.localizedDescription().to_string()),
                );
            }
            #[unsafe(method(webViewWebContentProcessDidTerminate:))]
            fn terminated(&self, _view: &WKWebView) {
                finish(
                    self.ivars().id,
                    Err("The document renderer stopped.".into()),
                );
            }
        }
    );
    fn finish(id: u64, result: Result<Value, String>) {
        let job = JOBS.with(|jobs| jobs.borrow_mut().remove(&id));
        if let Some(job) = job {
            unsafe {
                job.view.setNavigationDelegate(None);
                job.view.stopLoading();
            }
            let _ = job.reply.send(result);
        }
    }
    struct Cancel(u64);
    impl Drop for Cancel {
        fn drop(&mut self) {
            let id = self.0;
            dispatch2::DispatchQueue::main()
                .exec_async(move || finish(id, Err("The document preview was cancelled.".into())));
        }
    }
    pub async fn render(
        html: String,
        page: usize,
        diagram_source: Option<String>,
    ) -> Result<Value, String> {
        static SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let permit = SLOTS
            .get_or_init(|| Arc::new(Semaphore::new(2)))
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| e.to_string())?;
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let (reply, receive) = oneshot::channel();
        let _cancel = Cancel(id);
        dispatch2::DispatchQueue::main().exec_async(move || {
            if reply.is_closed() {
                return;
            }
            autoreleasepool(|_| unsafe {
                let mtm = MainThreadMarker::new().expect("main queue");
                let config = WKWebViewConfiguration::new(mtm);
                config.setWebsiteDataStore(&WKWebsiteDataStore::nonPersistentDataStore(mtm));
                let preferences = WKWebpagePreferences::new(mtm);
                preferences.setAllowsContentJavaScript(false);
                config.setDefaultWebpagePreferences(Some(&preferences));
                let view = WKWebView::initWithFrame_configuration(
                    WKWebView::alloc(mtm),
                    NSRect::new(
                        NSPoint::new(0., 0.),
                        NSSize::new(WIDTH as f64, HEIGHT as f64),
                    ),
                    &config,
                );
                let delegate = Delegate::alloc(mtm).set_ivars(Ivars {
                    id,
                    navigated: Cell::new(false),
                    finished: Cell::new(false),
                });
                let delegate: Retained<Delegate> = msg_send![super(delegate), init];
                view.setNavigationDelegate(Some(ProtocolObject::from_ref(&*delegate)));
                JOBS.with(|jobs| {
                    jobs.borrow_mut().insert(
                        id,
                        Job {
                            view: view.clone(),
                            _delegate: delegate,
                            reply,
                            page,
                            diagram_source,
                            _permit: permit,
                        },
                    )
                });
                view.loadHTMLString_baseURL(&NSString::from_str(&html), None);
            });
        });
        tokio::time::timeout(Duration::from_secs(15), receive)
            .await
            .map_err(|_| "The document preview timed out.".to_owned())?
            .map_err(|_| "The document renderer returned no result.".to_owned())?
    }
    fn view(id: u64) -> Option<Retained<WKWebView>> {
        JOBS.with(|jobs| jobs.borrow().get(&id).map(|j| j.view.clone()))
    }
    fn evaluate(
        view: &WKWebView,
        script: &str,
        callback: block2::RcBlock<dyn Fn(*mut AnyObject, *mut NSError)>,
    ) {
        unsafe {
            view.evaluateJavaScript_inFrame_inContentWorld_completionHandler(
                &NSString::from_str(script),
                None,
                &WKContentWorld::defaultClientWorld(MainThreadMarker::new().unwrap()),
                Some(&callback),
            );
        }
    }
    // This trusted rendering library is pinned by web/package-lock.json and
    // embedded in the Rust library. Document source is never executable code.
    const MERMAID: &str =
        include_str!("../../../apps/web/node_modules/mermaid/dist/mermaid.min.js");
    fn render_diagram(id: u64, web: &WKWebView, source: String) {
        evaluate(
            web,
            MERMAID,
            block2::RcBlock::new(move |_: *mut AnyObject, error: *mut NSError| {
                if !error.is_null() {
                    finish(
                        id,
                        Err(unsafe { &*error }.localizedDescription().to_string()),
                    );
                    return;
                }
                let Some(web) = view(id) else {
                    return;
                };
                let script = r#"
                mermaid.initialize({startOnLoad:false,securityLevel:'strict',suppressErrorRendering:true,
                    theme:'neutral',maxTextSize:50000,maxEdges:500});
                const result=await mermaid.render('studio-diagram',source);
                const svg=new DOMParser().parseFromString(result.svg,'image/svg+xml').documentElement;
                const height=Number(svg.getAttribute('viewBox')?.split(/\s+/)[3]);
                return JSON.stringify({svg:result.svg,height:Number.isFinite(height)?Math.max(120,Math.min(560,height+40)):310});
            "#;
                let source = NSString::from_str(&source);
                let key = NSString::from_str("source");
                let arguments =
                    NSDictionary::<NSString, AnyObject>::from_slices(&[&*key], &[source.as_ref()]);
                let callback =
                    block2::RcBlock::new(move |value: *mut AnyObject, error: *mut NSError| {
                        let result = (|| {
                            if !error.is_null() {
                                return Err(unsafe { &*error }.localizedDescription().to_string());
                            }
                            let text = (unsafe { value.as_ref() })
                                .and_then(|v| v.downcast_ref::<NSString>())
                                .ok_or("The diagram renderer returned no result.")?
                                .to_string();
                            if text.len() > 8 * 1024 * 1024 {
                                return Err("The diagram exceeds 8 MiB.".into());
                            }
                            let result: Value =
                                serde_json::from_str(&text).map_err(|e| e.to_string())?;
                            if !result["svg"]
                                .as_str()
                                .is_some_and(|s| s.starts_with("<svg"))
                                || !result["height"]
                                    .as_f64()
                                    .is_some_and(|h| (120.0..=560.0).contains(&h))
                            {
                                return Err("The diagram renderer returned invalid data.".into());
                            }
                            Ok(result)
                        })();
                        finish(id, result);
                    });
                unsafe {
                    web.callAsyncJavaScript_arguments_inFrame_inContentWorld_completionHandler(
                        &NSString::from_str(script),
                        Some(&arguments),
                        None,
                        &WKContentWorld::defaultClientWorld(MainThreadMarker::new().unwrap()),
                        Some(&callback),
                    );
                }
            }),
        );
    }
    fn measure(id: u64, web: &WKWebView) {
        evaluate(
            web,
            "String(Math.max(document.documentElement.scrollHeight,document.body?.scrollHeight||0))",
            block2::RcBlock::new(move |value: *mut AnyObject, error: *mut NSError| {
                if !error.is_null() {
                    finish(
                        id,
                        Err(unsafe { &*error }.localizedDescription().to_string()),
                    );
                    return;
                }
                let Some(height) = (unsafe { value.as_ref() })
                    .and_then(|v| v.downcast_ref::<NSString>())
                    .and_then(|s| s.to_string().parse::<f64>().ok())
                else {
                    finish(id, Err("The document has no valid size.".into()));
                    return;
                };
                if !height.is_finite() || height < 0. || height > (HEIGHT * MAX_PAGES) as f64 {
                    finish(id,Err("This document exceeds the 80-page preview limit. Use Source or Save a copy.".into()));
                    return;
                }
                let pages = (height / HEIGHT as f64).ceil().max(1.) as usize;
                let Some(page) = JOBS.with(|jobs| jobs.borrow().get(&id).map(|j| j.page)) else {
                    return;
                };
                if page >= pages {
                    finish(id, Err("This document page does not exist.".into()));
                    return;
                }
                let Some(web) = view(id) else {
                    return;
                };
                let page_height =
                    ((height - (page * HEIGHT) as f64).ceil() as usize).clamp(1, HEIGHT);
                let script = format!(
                    "window.scrollTo(0,{}); String(window.scrollY)",
                    page * HEIGHT
                );
                evaluate(
                    &web,
                    &script,
                    block2::RcBlock::new(move |value: *mut AnyObject, error: *mut NSError| {
                        if !error.is_null() {
                            finish(
                                id,
                                Err(unsafe { &*error }.localizedDescription().to_string()),
                            );
                            return;
                        }
                        let scroll = unsafe { value.as_ref() }
                            .and_then(|v| v.downcast_ref::<NSString>())
                            .and_then(|s| s.to_string().parse::<f64>().ok());
                        let Some(scroll) = scroll.filter(|v| v.is_finite()) else {
                            finish(id, Err("Cannot locate the document page.".into()));
                            return;
                        };
                        // A short final page clamps scrolling. Crop its own
                        // content instead of repeating the previous page.
                        let top = (page * HEIGHT) as f64 - scroll;
                        if top < -0.5 || top + page_height as f64 > HEIGHT as f64 + 0.5 {
                            finish(id, Err("This document prevents page scrolling. Use Source or Save a copy.".into()));
                            return;
                        }
                        snapshot(id, page, pages, top.max(0.0), page_height);
                    }),
                );
            }),
        );
    }
    fn snapshot(id: u64, page: usize, pages: usize, top: f64, height: usize) {
        let Some(web) = view(id) else {
            return;
        };
        let config = unsafe { WKSnapshotConfiguration::new(MainThreadMarker::new().unwrap()) };
        unsafe {
            config.setAfterScreenUpdates(true);
            config.setRect(NSRect::new(
                NSPoint::new(0.0, top),
                NSSize::new(WIDTH as f64, height as f64),
            ));
        }
        let callback = block2::RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
            let result = (|| {
                if !error.is_null() {
                    return Err(unsafe { &*error }.localizedDescription().to_string());
                }
                let image = unsafe { image.as_ref() }.ok_or("The document snapshot is empty.")?;
                let data = image
                    .TIFFRepresentation()
                    .ok_or("The document snapshot has no pixels.")?;
                let bitmap = NSBitmapImageRep::initWithData(NSBitmapImageRep::alloc(), &data)
                    .ok_or("Invalid document pixels.")?;
                let png = unsafe {
                    bitmap.representationUsingType_properties(
                        NSBitmapImageFileType::PNG,
                        &NSDictionary::new(),
                    )
                }
                .ok_or("Cannot encode the document snapshot.")?;
                if png.len() > 8 * 1024 * 1024 {
                    return Err("The document snapshot exceeds 8 MiB.".into());
                }
                Ok(
                    serde_json::json!({"url":format!("data:image/png;base64,{}",STANDARD.encode(png.to_vec())),
                    "width":WIDTH,"height":height,"page":page,"pages":pages}),
                )
            })();
            finish(id, result);
        });
        unsafe {
            web.takeSnapshotWithConfiguration_completionHandler(Some(&config), &callback);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn invalid_requests_fail_before_starting_the_native_renderer() {
        for (method, params, expected) in [
            (
                "document.unknown",
                serde_json::json!({"source":"ok","page":0}),
                "Unknown document",
            ),
            (
                "document.render",
                serde_json::json!({"source":"ok","page":80}),
                "Invalid document page",
            ),
            (
                "document.render",
                serde_json::json!({"source":"ok","page":-1}),
                "Invalid document page",
            ),
            (
                "document.render",
                serde_json::json!({"source":"x".repeat(SOURCE_LIMIT+1),"page":0}),
                "exceeds 2 MiB",
            ),
        ] {
            let response = dispatch(Request {
                id: "rejected".into(),
                method: method.into(),
                params,
            })
            .await;
            assert!(response.error.unwrap().message.contains(expected));
            assert!(response.result.is_none());
        }
    }
}
