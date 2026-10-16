#![allow(unsafe_code)]

use objc2_core_foundation::{CFArray, CFRetained, CFString};
use objc2_core_services::{
    ConstFSEventStreamRef, FSEventStreamContext, FSEventStreamCreate, FSEventStreamCreateFlags,
    FSEventStreamEventFlags, FSEventStreamEventId, FSEventStreamInvalidate, FSEventStreamRef,
    FSEventStreamRelease, FSEventStreamSetDispatchQueue, FSEventStreamStart, FSEventStreamStop,
    FSEventsGetCurrentEventId, kFSEventStreamCreateFlagFileEvents, kFSEventStreamCreateFlagNoDefer,
    kFSEventStreamCreateFlagWatchRoot, kFSEventStreamEventFlagEventIdsWrapped,
    kFSEventStreamEventFlagHistoryDone, kFSEventStreamEventFlagKernelDropped,
    kFSEventStreamEventFlagRootChanged, kFSEventStreamEventFlagUserDropped,
};
use std::ffi::{CStr, CString, c_char, c_void};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::slice;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

const HISTORY_TIMEOUT: Duration = Duration::from_secs(120);
const FSEVENT_LATENCY_SECONDS: f64 = 0.1;
const FSEVENT_FLAGS: FSEventStreamCreateFlags = kFSEventStreamCreateFlagFileEvents
    | kFSEventStreamCreateFlagNoDefer
    | kFSEventStreamCreateFlagWatchRoot;
const HISTORY_LOST_FLAGS: FSEventStreamEventFlags = kFSEventStreamEventFlagUserDropped
    | kFSEventStreamEventFlagKernelDropped
    | kFSEventStreamEventFlagEventIdsWrapped;
pub(super) const ROOT_CHANGED_FLAG: u32 = kFSEventStreamEventFlagRootChanged;

#[derive(Clone, Debug)]
pub(super) struct FSEvent {
    pub(super) path: PathBuf,
    pub(super) flags: u32,
    pub(super) id: u64,
}

#[derive(Debug)]
pub(super) enum FSEventError {
    HistoryLost,
    Other(String),
}

#[derive(Default)]
struct EventCollector {
    events: Mutex<Vec<FSEvent>>,
    history_done: Mutex<bool>,
    ready: Condvar,
}

struct Stream {
    raw: FSEventStreamRef,
    started: bool,
    queue: dispatch2::DispatchRetained<dispatch2::DispatchQueue>,
}

impl Stream {
    fn raw(&self) -> FSEventStreamRef {
        self.raw
    }

    fn stop(&mut self) {
        if self.started {
            // SAFETY: `raw` is the live stream returned by FSEventStreamCreate.
            unsafe { FSEventStreamStop(self.raw) };
            self.started = false;
        }
        // SAFETY: The stream remains valid until the release call below.
        unsafe { FSEventStreamInvalidate(self.raw) };
        // SAFETY: The serial queue barrier drains callbacks queued before invalidation; the
        // callback context stays alive until after this barrier and stream release.
        self.queue.barrier_sync(|| {});
        // SAFETY: This releases the single owned reference returned by FSEventStreamCreate.
        unsafe { FSEventStreamRelease(self.raw) };
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(super) fn current_event_id() -> u64 {
    // SAFETY: This zero-argument API returns the process-wide FSEvents event counter.
    unsafe { FSEventsGetCurrentEventId() }
}

pub(super) fn read_events(root: &Path, since: u64) -> Result<(Vec<FSEvent>, u64), FSEventError> {
    let current = current_event_id();
    if since >= current {
        return Ok((Vec::new(), current));
    }
    let root_bytes = root.as_os_str().as_encoded_bytes();
    let root_c =
        CString::new(root_bytes).map_err(|error| FSEventError::Other(error.to_string()))?;
    // SAFETY: `root_c` is NUL terminated, stays alive through stream creation, and encodes the
    // filesystem path representation expected by CoreFoundation.
    let root_string =
        unsafe { CFString::with_file_system_representation(None, root_c.as_ptr()) }
            .ok_or_else(|| FSEventError::Other("Could not create FSEvents root path".into()))?;
    let watched_paths: CFRetained<CFArray<CFString>> = CFArray::from_objects(&[&root_string]);
    let collector = EventCollector::default();
    let mut context = FSEventStreamContext {
        version: 0,
        info: (&collector as *const EventCollector).cast_mut().cast(),
        retain: None,
        release: None,
        copyDescription: None,
    };
    // SAFETY: `context` points to `collector`, which remains alive until the stream is stopped,
    // invalidated, drained, and released. `watched_paths` is retained for that same duration.
    let raw = unsafe {
        FSEventStreamCreate(
            None,
            Some(collect_fsevents),
            &mut context,
            watched_paths.as_opaque(),
            since,
            FSEVENT_LATENCY_SECONDS,
            FSEVENT_FLAGS,
        )
    };
    if raw.is_null() {
        return Err(FSEventError::Other("FSEventStreamCreate failed".into()));
    }
    let queue = dispatch2::DispatchQueue::new("studio.image-workspace.fsevents", None);
    let mut stream = Stream {
        raw,
        started: false,
        queue,
    };
    // SAFETY: `stream` owns a live stream and `queue` outlives it.
    unsafe { FSEventStreamSetDispatchQueue(stream.raw(), Some(&stream.queue)) };
    // SAFETY: `stream` is the valid stream returned by FSEventStreamCreate.
    if !unsafe { FSEventStreamStart(stream.raw()) } {
        return Err(FSEventError::Other("FSEventStreamStart failed".into()));
    }
    stream.started = true;

    let history_done = collector
        .history_done
        .lock()
        .map_err(|_| FSEventError::Other("FSEvents lock poisoned".into()))?;
    let (history_done, timeout) = collector
        .ready
        .wait_timeout_while(history_done, HISTORY_TIMEOUT, |done| !*done)
        .map_err(|_| FSEventError::Other("FSEvents wait poisoned".into()))?;
    let completed = *history_done;
    drop(history_done);
    drop(stream);
    if !completed || timeout.timed_out() {
        return Err(FSEventError::Other("FSEvents history timed out".into()));
    }

    let events = collector
        .events
        .lock()
        .map_err(|_| FSEventError::Other("FSEvents lock poisoned".into()))?
        .clone();
    if events
        .iter()
        .any(|event| event.flags & HISTORY_LOST_FLAGS != 0)
    {
        return Err(FSEventError::HistoryLost);
    }
    let newest = events.iter().map(|event| event.id).max().unwrap_or(since);
    Ok((events, newest))
}

unsafe extern "C-unwind" fn collect_fsevents(
    _stream: ConstFSEventStreamRef,
    info: *mut c_void,
    count: usize,
    paths: NonNull<c_void>,
    flags: NonNull<FSEventStreamEventFlags>,
    ids: NonNull<FSEventStreamEventId>,
) {
    if info.is_null() {
        return;
    }
    // SAFETY: The context pointer is installed from the stack-owned collector and the stream is
    // stopped and its serial callback queue drained before that collector is dropped.
    let collector = unsafe { &*info.cast::<EventCollector>() };
    // SAFETY: FSEvents supplies arrays of `count` path pointers, flags and IDs for this callback.
    let paths = unsafe { slice::from_raw_parts(paths.as_ptr().cast::<*const c_char>(), count) };
    // SAFETY: FSEvents supplies `count` flags for this callback.
    let flags = unsafe { slice::from_raw_parts(flags.as_ptr(), count) };
    // SAFETY: FSEvents supplies `count` IDs for this callback.
    let ids = unsafe { slice::from_raw_parts(ids.as_ptr(), count) };

    if let Ok(mut events) = collector.events.lock() {
        for index in 0..count {
            if !paths[index].is_null() {
                // SAFETY: FSEvents guarantees each path is a NUL-terminated C string valid for
                // the duration of this callback; the owned PathBuf copies it before return.
                let path = unsafe { CStr::from_ptr(paths[index]) }
                    .to_string_lossy()
                    .into_owned();
                events.push(FSEvent {
                    path: PathBuf::from(path),
                    flags: flags[index],
                    id: ids[index],
                });
            }
        }
    }
    if flags
        .iter()
        .any(|value| value & kFSEventStreamEventFlagHistoryDone != 0)
    {
        if let Ok(mut done) = collector.history_done.lock() {
            *done = true;
            collector.ready.notify_all();
        }
    }
}
