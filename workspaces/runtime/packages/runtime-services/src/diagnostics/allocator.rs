//! On-demand, value-free GNU allocator snapshot for memory diagnosis.

use serde_json::json;

use super::startup::{current_rss_bytes, startup_memory_enabled, startup_memory_marks};

/// Maximum allocator XML retained in a diagnostic log event, in bytes.
const ALLOCATOR_XML_LIMIT_BYTES: usize = 128 * 1024;
/// `fmemopen` needs one extra byte for its trailing NUL.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const ALLOCATOR_XML_BUFFER_BYTES: usize = ALLOCATOR_XML_LIMIT_BYTES + 1;
const ALLOCATOR_MEMORY_LOG_PREFIX: &str = "RUST_ALLOCATOR_MEMORY";
const ALLOCATOR_MEMORY_EVENT_KIND: &str = "rustAllocatorMemory";
const GLIBC_MALLOC_INFO_XML_FORMAT: &str = "glibc-malloc-info-xml";

/// Emit allocator accounting only when the existing startup-memory opt-in is
/// enabled and an operator explicitly requests `/api/diagnostics`.
pub fn log_on_demand_memory_snapshot() {
    if !startup_memory_enabled() {
        return;
    }

    // Capture RSS before allocating the fixed stack buffer used by malloc_info.
    let startup_marks = startup_memory_marks();
    let rss_bytes = current_rss_bytes();
    let allocator = allocator_xml().map_or_else(
        || json!({"available": false}),
        |xml| {
            json!({
                "available": true,
                "format": GLIBC_MALLOC_INFO_XML_FORMAT,
                "xml": xml,
            })
        },
    );
    let event = json!({
        "kind": ALLOCATOR_MEMORY_EVENT_KIND,
        "at": super::now_rfc3339(),
        "rssBytes": rss_bytes,
        "startupMemoryMarks": startup_marks,
        "allocator": allocator,
    });
    eprintln!("{ALLOCATOR_MEMORY_LOG_PREFIX} {event}");
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[allow(unsafe_code, reason = "isolates the GNU libc allocator diagnostic ABI")]
fn allocator_xml() -> Option<String> {
    use nix::libc;

    let mut buffer = [0u8; ALLOCATOR_XML_BUFFER_BYTES];
    let mut stream = AllocatorXmlStream::open(&mut buffer)?;
    let stream_ptr = stream.as_ptr()?;
    // SAFETY: the stream is a new fmemopen stream and no I/O has happened yet;
    // disabling stdio buffering makes writes stop at the fixed buffer boundary.
    unsafe { libc::setbuf(stream_ptr, std::ptr::null_mut()) };
    // SAFETY: the guard owns a live FILE backed by the fixed writable buffer.
    // GNU documents malloc_info as MT-Safe; this runs on the normal blocking
    // diagnostics path, not from a signal or fork handler.
    let result = unsafe { libc::malloc_info(GLIBC_MALLOC_INFO_OPTIONS, stream_ptr) };
    // SAFETY: `stream_ptr` is still the live FILE uniquely owned by `stream`;
    // malloc_info has returned, so checking its stdio error flag is valid.
    let stream_error = unsafe { libc::ferror(stream_ptr) };
    let close_result = stream.close();
    // Drop releases the exclusive mutable borrow before inspecting the buffer.
    drop(stream);
    if result != 0 || stream_error != 0 || close_result != 0 {
        return None;
    }

    let length = buffer.iter().position(|byte| *byte == 0)?;
    checked_xml(&buffer[..length], length).map(str::to_owned)
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn allocator_xml() -> Option<String> {
    None
}

fn checked_xml(buffer: &[u8], length: usize) -> Option<&str> {
    if length > ALLOCATOR_XML_LIMIT_BYTES || length > buffer.len() {
        return None;
    }
    let xml = std::str::from_utf8(&buffer[..length]).ok()?;
    (xml.starts_with("<malloc version=\"") && xml.trim_end().ends_with("</malloc>")).then_some(xml)
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
const GLIBC_MALLOC_INFO_OPTIONS: nix::libc::c_int = 0;

#[cfg(all(target_os = "linux", target_env = "gnu"))]
struct AllocatorXmlStream<'buffer> {
    stream: Option<std::ptr::NonNull<nix::libc::FILE>>,
    // The FILE stream may write to the buffer until it is closed.
    _buffer: std::marker::PhantomData<&'buffer mut [u8]>,
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
impl<'buffer> AllocatorXmlStream<'buffer> {
    #[allow(unsafe_code, reason = "opens the bounded GNU libc FILE stream")]
    fn open(buffer: &'buffer mut [u8]) -> Option<Self> {
        use nix::libc;

        // SAFETY: `buffer` is writable and remains exclusively borrowed by this
        // guard until fclose; the mode is a static NUL-terminated C string.
        let stream =
            unsafe { libc::fmemopen(buffer.as_mut_ptr().cast(), buffer.len(), c"w".as_ptr()) };
        Some(Self {
            stream: Some(std::ptr::NonNull::new(stream)?),
            _buffer: std::marker::PhantomData,
        })
    }

    fn as_ptr(&self) -> Option<*mut nix::libc::FILE> {
        self.stream.map(std::ptr::NonNull::as_ptr)
    }

    #[allow(unsafe_code, reason = "closes the uniquely owned GNU libc FILE stream")]
    fn close(&mut self) -> nix::libc::c_int {
        let Some(stream) = self.stream.take() else {
            return -1;
        };
        // SAFETY: this guard uniquely owns the fmemopen FILE pointer and takes
        // it before closing, so neither this method nor Drop can close twice.
        unsafe { nix::libc::fclose(stream.as_ptr()) }
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
impl Drop for AllocatorXmlStream<'_> {
    #[allow(
        unsafe_code,
        reason = "closes the GNU libc FILE stream on early return"
    )]
    fn drop(&mut self) {
        if let Some(stream) = self.stream.take() {
            // SAFETY: this guard uniquely owns the fmemopen FILE pointer; close
            // finalizes the fixed buffer before its exclusive borrow is released.
            let _ = unsafe { nix::libc::fclose(stream.as_ptr()) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocator_xml_reader_accepts_only_complete_bounded_utf8_xml() {
        let valid = b"<malloc version=\"1\"></malloc>\n";
        assert_eq!(
            checked_xml(valid, valid.len()),
            Some("<malloc version=\"1\"></malloc>\n")
        );
        assert_eq!(checked_xml(b"<malloc version=\"1\">", 19), None);
        assert_eq!(checked_xml(&[b'<', 0xff, b'>'], 3), None);
        assert_eq!(checked_xml(b"small", 6), None);
        assert_eq!(
            checked_xml(
                &vec![b'x'; ALLOCATOR_XML_LIMIT_BYTES + 1],
                ALLOCATOR_XML_LIMIT_BYTES + 1
            ),
            None
        );
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn glibc_allocator_snapshot_is_complete_xml_within_the_log_bound() {
        if let Some(xml) = allocator_xml() {
            assert!(xml.starts_with("<malloc version=\""));
            assert!(xml.trim_end().ends_with("</malloc>"));
            assert!(xml.len() <= ALLOCATOR_XML_LIMIT_BYTES);
        }
    }
}
