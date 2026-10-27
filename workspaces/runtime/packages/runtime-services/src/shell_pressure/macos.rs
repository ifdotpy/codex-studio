#![allow(unsafe_code)]
//! Isolated adapter for Apple's public dispatch memory-pressure source.
use dispatch2::{
    DispatchObject, DispatchSource, dispatch_source_memorypressure_flags_t as MemoryLevel,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
pub(super) struct Source {
    source: dispatch2::DispatchRetained<DispatchSource>,
}
impl Source {
    pub(super) fn new(critical: Arc<AtomicBool>) -> Self {
        let queue = dispatch2::DispatchQueue::new("studio.shell-memory-pressure", None);
        let mask = (MemoryLevel::DISPATCH_MEMORYPRESSURE_NORMAL.0
            | MemoryLevel::DISPATCH_MEMORYPRESSURE_WARN.0
            | MemoryLevel::DISPATCH_MEMORYPRESSURE_CRITICAL.0) as usize;
        // SAFETY: The source type is Apple's supported memory-pressure
        // source. It takes no handle. Subscribe to all pressure transitions.
        let source = unsafe {
            DispatchSource::new(
                std::ptr::addr_of!(dispatch2::_dispatch_source_type_memorypressure).cast_mut(),
                0,
                mask,
                Some(&queue),
            )
        };
        let context = Box::new(PressureContext {
            source: std::ptr::from_ref(&*source),
            critical: Arc::clone(&critical),
        });
        // SAFETY: The context remains owned by the source until its cancel
        // handler runs, after its last event handler completes.
        unsafe {
            source.set_context(Box::into_raw(context).cast());
        }
        source.set_event_handler_f(pressure_event);
        source.set_cancel_handler_f(pressure_cancel);
        source.activate();
        Self { source }
    }
}
struct PressureContext {
    source: *const dispatch2::DispatchSource,
    critical: Arc<AtomicBool>,
}

extern "C" fn pressure_event(raw: *mut std::ffi::c_void) {
    // SAFETY: Only our source calls this handler, with its live context. The
    // source remains alive for an event; cancellation owns context disposal.
    let context = unsafe { &*raw.cast::<PressureContext>() };
    // SAFETY: Dispatch keeps the source alive while its handler runs.
    let flags = unsafe { &*context.source }.data();
    context.critical.store(
        flags == MemoryLevel::DISPATCH_MEMORYPRESSURE_CRITICAL.0 as usize,
        Ordering::Release,
    );
}

extern "C" fn pressure_cancel(raw: *mut std::ffi::c_void) {
    // SAFETY: This is the source's sole cancel callback, after event callbacks.
    drop(unsafe { Box::from_raw(raw.cast::<PressureContext>()) });
}

impl Drop for Source {
    fn drop(&mut self) {
        self.source.cancel();
    }
}
