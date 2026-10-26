//! Bonjour search of the iOS shell: a Network.framework browser for the
//! Studio service type. The start page lists the servers it finds; on the
//! first launch, one server opens directly. See README.md "iOS shell" and
//! `ios_server::choose` for the order.

use crate::ios_server::{
    BROWSER_FAILED, BROWSER_READY, BROWSER_WAITING, BUILD_DEFAULT, DNS_POLICY_DENIED, Discovered,
    ERROR_DOMAIN_DNS, bonjour, choose, discovered, permission_denied,
};
use block2::{DynBlock, RcBlock};
use std::collections::BTreeMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::Mutex;
use std::time::Duration;
use tauri::WebviewWindow;

/// Time, after the search starts, before an empty search counts as settled
/// and the build default can open.
const SEARCH_WINDOW: Duration = Duration::from_secs(3);
/// Quiet time after the last result change before the results count as
/// settled, so that one batch of results is complete.
const SETTLE: Duration = Duration::from_millis(800);
/// Test hook: `denied` reports Local Network access as off without a browser
/// (the simulator does not ask for that permission); `off` does not search.
const TEST_ENV: &str = "STUDIO_SHELL_DISCOVERY";

type NwObject = *mut c_void;
/// C `bool` in a block signature (block2 does not encode Rust `bool`).
type CBool = u8;

#[repr(C)]
struct DispatchQueue {
    _private: [u8; 0],
}

#[link(name = "Network", kind = "framework")]
unsafe extern "C" {
    fn nw_browse_descriptor_create_bonjour_service(
        service_type: *const c_char,
        domain: *const c_char,
    ) -> NwObject;
    fn nw_browse_descriptor_set_include_txt_record(descriptor: NwObject, include: bool);
    fn nw_browser_create(descriptor: NwObject, parameters: NwObject) -> NwObject;
    fn nw_browser_set_queue(browser: NwObject, queue: *const DispatchQueue);
    fn nw_browser_set_browse_results_changed_handler(
        browser: NwObject,
        handler: &DynBlock<dyn Fn(NwObject, NwObject, CBool)>,
    );
    fn nw_browser_set_state_changed_handler(
        browser: NwObject,
        handler: &DynBlock<dyn Fn(u32, NwObject)>,
    );
    fn nw_browser_start(browser: NwObject);
    fn nw_browse_result_copy_endpoint(result: NwObject) -> NwObject;
    fn nw_browse_result_copy_txt_record_object(result: NwObject) -> NwObject;
    fn nw_endpoint_get_bonjour_service_name(endpoint: NwObject) -> *const c_char;
    fn nw_txt_record_apply(
        txt: NwObject,
        applier: &DynBlock<dyn Fn(*const c_char, u32, *const u8, usize) -> CBool>,
    ) -> bool;
    fn nw_error_get_error_domain(error: NwObject) -> u32;
    fn nw_error_get_error_code(error: NwObject) -> i32;
    fn nw_release(object: NwObject);
}

unsafe extern "C" {
    /// `dispatch_get_main_queue()` returns the address of this object.
    static _dispatch_main_q: DispatchQueue;
}

struct Search {
    /// Results by Bonjour service instance name.
    servers: BTreeMap<String, Discovered>,
    denied: bool,
    settled: bool,
    /// The first-launch rule (open one found server, else the build
    /// default) still applies: no address was known at launch, and the
    /// person has not chosen one.
    auto_open: bool,
    /// Changes with each result batch, so an older settle timer does nothing.
    generation: u64,
    window: Option<WebviewWindow>,
}

static SEARCH: Mutex<Search> = Mutex::new(Search {
    servers: BTreeMap::new(),
    denied: false,
    settled: false,
    auto_open: false,
    generation: 0,
    window: None,
});

fn search() -> std::sync::MutexGuard<'static, Search> {
    SEARCH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The servers found now, by name.
pub fn servers() -> Vec<Discovered> {
    let mut servers: Vec<_> = search().servers.values().cloned().collect();
    servers.sort_by(|a, b| a.name.cmp(&b.name).then(a.url.as_str().cmp(b.url.as_str())));
    servers
}

/// Stop the first-launch rule: the person chose a server or a page loaded.
pub fn stop_auto_open() {
    search().auto_open = false;
}

/// Start the search. `auto_open` is true when no address was known at launch.
pub fn start(window: &WebviewWindow, auto_open: bool) {
    {
        let mut search = search();
        search.auto_open = auto_open;
        search.window = Some(window.clone());
    }
    match std::env::var(TEST_ENV).as_deref() {
        Ok("off") => return,
        Ok("denied") => {
            eprintln!("studio-shell discovery: test hook reports Local Network access off");
            state_changed(BROWSER_WAITING, Some((ERROR_DOMAIN_DNS, DNS_POLICY_DENIED)));
        }
        _ => browse(),
    }
    std::thread::spawn(|| {
        std::thread::sleep(SEARCH_WINDOW);
        settle(None);
    });
}

fn browse() {
    let service_type = CString::new(bonjour::SERVICE_TYPE).expect("service type");
    let domain = CString::new("local.").expect("domain");
    let results = RcBlock::new(|old: NwObject, new: NwObject, batch_complete: CBool| {
        // SAFETY: the browser passes valid results (or null) for this call.
        unsafe { result_changed(old, new) };
        if batch_complete != 0 {
            changed();
        }
    });
    let state = RcBlock::new(|state: u32, error: NwObject| {
        let error = (!error.is_null()).then(|| unsafe {
            (
                nw_error_get_error_domain(error),
                nw_error_get_error_code(error),
            )
        });
        state_changed(state, error);
    });
    // SAFETY: Network.framework C API on the main queue. The browser copies
    // the blocks and lives for the app lifetime (it is never released).
    unsafe {
        let descriptor =
            nw_browse_descriptor_create_bonjour_service(service_type.as_ptr(), domain.as_ptr());
        nw_browse_descriptor_set_include_txt_record(descriptor, true);
        let browser = nw_browser_create(descriptor, std::ptr::null_mut());
        nw_release(descriptor);
        nw_browser_set_queue(browser, &raw const _dispatch_main_q);
        nw_browser_set_browse_results_changed_handler(browser, &results);
        nw_browser_set_state_changed_handler(browser, &state);
        nw_browser_start(browser);
    }
    eprintln!("studio-shell discovery: browsing {}", bonjour::SERVICE_TYPE);
}

/// The service name and TXT record of one result.
///
/// # Safety
/// `result` is a valid `nw_browse_result_t`.
unsafe fn read_result(result: NwObject) -> Option<(String, BTreeMap<String, String>)> {
    unsafe {
        let endpoint = nw_browse_result_copy_endpoint(result);
        if endpoint.is_null() {
            return None;
        }
        let name = nw_endpoint_get_bonjour_service_name(endpoint);
        let name = (!name.is_null()).then(|| CStr::from_ptr(name).to_string_lossy().into_owned());
        nw_release(endpoint);
        let mut pairs = BTreeMap::new();
        let txt = nw_browse_result_copy_txt_record_object(result);
        if !txt.is_null() {
            let collected = std::rc::Rc::new(std::cell::RefCell::new(BTreeMap::new()));
            let sink = std::rc::Rc::clone(&collected);
            let applier = RcBlock::new(
                move |key: *const c_char, _found: u32, value: *const u8, length: usize| -> CBool {
                    // 1 continues the iteration.
                    if key.is_null() {
                        return 1;
                    }
                    let key = CStr::from_ptr(key).to_string_lossy().into_owned();
                    let value = if value.is_null() {
                        String::new()
                    } else {
                        String::from_utf8_lossy(std::slice::from_raw_parts(value, length))
                            .into_owned()
                    };
                    sink.borrow_mut().insert(key, value);
                    1
                },
            );
            nw_txt_record_apply(txt, &applier);
            nw_release(txt);
            drop(applier);
            pairs = collected.take();
        }
        Some((name?, pairs))
    }
}

/// # Safety
/// `old` and `new` are valid browse results or null.
unsafe fn result_changed(old: NwObject, new: NwObject) {
    let removed = (!old.is_null())
        .then(|| unsafe { read_result(old) })
        .flatten();
    let added = (!new.is_null())
        .then(|| unsafe { read_result(new) })
        .flatten();
    let mut search = search();
    if let Some((name, _)) = &removed {
        search.servers.remove(name);
    }
    if let Some((name, txt)) = added {
        match discovered(&name, &txt) {
            Some(server) => {
                eprintln!("studio-shell discovery: found \"{name}\" {}", server.url);
                search.servers.insert(name, server);
            }
            None => {
                eprintln!("studio-shell discovery: ignored \"{name}\" (TXT {txt:?})");
                search.servers.remove(&name);
            }
        }
    } else if let Some((name, _)) = removed {
        eprintln!("studio-shell discovery: lost \"{name}\"");
    }
}

fn state_changed(state: u32, error: Option<(u32, i32)>) {
    let denied = error.is_some_and(|(domain, code)| permission_denied(domain, code));
    eprintln!("studio-shell discovery: state {state} error {error:?}");
    let failed = {
        let mut search = search();
        match state {
            BROWSER_READY => search.denied = false,
            BROWSER_WAITING | BROWSER_FAILED => search.denied = denied,
            _ => {}
        }
        search.denied || state == BROWSER_FAILED
    };
    // Nothing comes while access is off; do not wait for the search window.
    if failed {
        settle(None);
    } else {
        push();
    }
}

/// A result batch is complete: show it, then settle after a quiet time.
fn changed() {
    let generation = {
        let mut search = search();
        search.generation += 1;
        search.generation
    };
    push();
    std::thread::spawn(move || {
        std::thread::sleep(SETTLE);
        settle(Some(generation));
    });
}

/// Mark the search settled and apply the first-launch rule. `generation`
/// is the result batch of a settle timer; a later batch cancels that timer.
fn settle(generation: Option<u64>) {
    let (window, target) = {
        let mut search = search();
        if generation.is_some_and(|generation| generation != search.generation) {
            return;
        }
        search.settled = true;
        let servers: Vec<_> = search.servers.values().cloned().collect();
        let target = if search.auto_open {
            choose(None, None, &servers, true, BUILD_DEFAULT)
        } else {
            None
        };
        // After a choice is shown, the person picks; a later change does not
        // open a server without them.
        if target.is_some() || servers.len() > 1 {
            search.auto_open = false;
        }
        (search.window.clone(), target)
    };
    push();
    if let (Some(window), Some(url)) = (window, target) {
        eprintln!("studio-shell discovery: opening {url}");
        let _ = window.navigate(url);
        crate::ios_shell::watch_first_load(&window);
    }
}

/// The state that the start page shows, as JSON.
pub fn page_state() -> String {
    let (denied, settled) = {
        let search = search();
        (search.denied, search.settled)
    };
    let servers: Vec<_> = servers()
        .into_iter()
        .map(|server| serde_json::json!({ "name": server.name, "url": server.url.as_str() }))
        .collect();
    serde_json::json!({ "servers": servers, "denied": denied, "settled": settled }).to_string()
}

/// Send the state to the start page. Other pages do not define the receiver,
/// and the script checks the scheme.
pub fn push() {
    let Some(window) = search().window.clone() else {
        return;
    };
    let script = format!(
        "location.protocol==='{}:'&&window.__studioShellDiscovery&&window.__studioShellDiscovery({})",
        crate::ios_shell::START_SCHEME,
        page_state()
    );
    let _ = window.eval(script);
}
