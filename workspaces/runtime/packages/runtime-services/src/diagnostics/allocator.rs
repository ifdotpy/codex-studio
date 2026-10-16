//! On-demand, value-free GNU allocator summary for memory diagnosis.

use serde_json::json;

use super::startup::{current_rss_bytes, startup_memory_enabled, startup_memory_marks};

/// Maximum private allocator XML captured for parsing, in bytes.
const ALLOCATOR_XML_LIMIT_BYTES: usize = 128 * 1024;
/// `fmemopen` needs one extra byte for its trailing NUL.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const ALLOCATOR_XML_BUFFER_BYTES: usize = ALLOCATOR_XML_LIMIT_BYTES + 1;
const ALLOCATOR_MEMORY_LOG_PREFIX: &str = "RUST_ALLOCATOR_MEMORY";
const ALLOCATOR_MEMORY_EVENT_KIND: &str = "rustAllocatorMemory";

/// Emit allocator accounting only when the existing startup-memory opt-in is
/// enabled and an operator explicitly requests `/api/diagnostics`.
pub fn log_on_demand_memory_snapshot() {
    if !startup_memory_enabled() {
        return;
    }

    // Capture RSS before allocating the fixed stack buffer used by malloc_info.
    let startup_marks = startup_memory_marks();
    let rss_bytes = current_rss_bytes();
    let allocator = allocator_snapshot().map_or_else(
        || json!({"available": false}),
        |summary| {
            json!({
                "available": true,
                "arenas": summary.arenas,
                "systemCurrentBytes": summary.system_current_bytes,
                "fastFreeBytes": summary.fast_free_bytes,
                "restFreeBytes": summary.rest_free_bytes,
                "mmapBytes": summary.mmap_bytes,
                "allocatorUsedBytesApprox": summary.allocator_used_bytes_approx,
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
fn allocator_snapshot() -> Option<AllocatorSummary> {
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
    let xml = checked_xml(&buffer[..length], length)?;
    parse_allocator_summary(xml)
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn allocator_snapshot() -> Option<AllocatorSummary> {
    None
}

fn checked_xml(buffer: &[u8], length: usize) -> Option<&str> {
    if length > ALLOCATOR_XML_LIMIT_BYTES || length > buffer.len() {
        return None;
    }
    let xml = std::str::from_utf8(&buffer[..length]).ok()?;
    (xml.starts_with("<malloc version=\"") && xml.trim_end().ends_with("</malloc>")).then_some(xml)
}

#[derive(Debug, Eq, PartialEq)]
struct AllocatorSummary {
    arenas: u64,
    system_current_bytes: u64,
    fast_free_bytes: u64,
    rest_free_bytes: u64,
    mmap_bytes: u64,
    allocator_used_bytes_approx: u64,
}

/// Parse only glibc's aggregate footer after its last per-heap section.
fn parse_allocator_summary(xml: &str) -> Option<AllocatorSummary> {
    if !xml.starts_with("<malloc version=\"") || !xml.trim_end().ends_with("</malloc>") {
        return None;
    }
    let final_heap_end = xml.rfind("</heap>")?.checked_add("</heap>".len())?;
    let footer_end = xml.rfind("</malloc>")?;
    let per_heap_xml = xml.get(..final_heap_end)?;
    let footer = xml.get(final_heap_end..footer_end)?;
    let arenas = per_heap_xml
        .match_indices("<heap ")
        .try_fold(0u64, |count, _| count.checked_add(1))?;
    if arenas == 0 {
        return None;
    }

    // This glibc version omits the global fast total when its value is zero.
    // A present fast row is still parsed strictly; only absence maps to zero.
    let fast_free_bytes = global_total_size(footer, "fast")?.unwrap_or_default();
    let rest_free_bytes = global_total_size(footer, "rest")??;
    let mmap_bytes = global_total_size(footer, "mmap")??;
    let system_current_bytes = global_system_current(footer)?;
    let free_bytes = fast_free_bytes.checked_add(rest_free_bytes)?;
    let allocator_used_bytes_approx = system_current_bytes
        .checked_sub(free_bytes)?
        .checked_add(mmap_bytes)?;

    Some(AllocatorSummary {
        arenas,
        system_current_bytes,
        fast_free_bytes,
        rest_free_bytes,
        mmap_bytes,
        allocator_used_bytes_approx,
    })
}

fn global_total_size(footer: &str, expected_type: &str) -> Option<Option<u64>> {
    let mut found = None;
    for tag in tags_with_prefix(footer, "<total ") {
        if attribute(tag, "type")? != expected_type {
            continue;
        }
        if found.is_some() {
            return None;
        }
        // Validate both numeric attributes on required totals, even though only
        // size contributes to the emitted summary.
        attribute(tag, "count")?.parse::<u64>().ok()?;
        found = Some(attribute(tag, "size")?.parse::<u64>().ok()?);
    }
    Some(found)
}

fn global_system_current(footer: &str) -> Option<u64> {
    let mut found = None;
    for tag in tags_with_prefix(footer, "<system ") {
        if attribute(tag, "type")? != "current" {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some(attribute(tag, "size")?.parse::<u64>().ok()?);
    }
    found
}

fn tags_with_prefix<'a>(text: &'a str, prefix: &str) -> impl Iterator<Item = &'a str> {
    text.match_indices(prefix).filter_map(move |(start, _)| {
        let tail = text.get(start..)?;
        let end = tail.find('>')?.checked_add(1)?;
        tail.get(..end)
    })
}

fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut value = None;
    for part in tag.split_ascii_whitespace() {
        let part = part.strip_suffix("/>").unwrap_or(part);
        if let Some(raw) = part
            .strip_prefix(name)
            .and_then(|rest| rest.strip_prefix("=\""))
        {
            let raw = raw.strip_suffix('"')?;
            if value.is_some() {
                return None;
            }
            value = Some(raw);
        }
    }
    value
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

    const VALID_FIXTURE: &str = "<malloc version=\"1\">\n<heap nr=\"0\"><total type=\"fast\" count=\"2\" size=\"10\"/><total type=\"rest\" count=\"3\" size=\"20\"/><system type=\"current\" size=\"40\"/></heap>\n<total type=\"fast\" count=\"4\" size=\"100\"/>\n<total type=\"rest\" count=\"5\" size=\"200\"/>\n<total type=\"mmap\" count=\"1\" size=\"50\"/>\n<system type=\"current\" size=\"400\"/>\n</malloc>\n";

    #[test]
    fn parser_uses_only_global_footer_after_last_heap() {
        let summary = parse_allocator_summary(VALID_FIXTURE).expect("complete allocator fixture");
        assert_eq!(
            summary,
            AllocatorSummary {
                arenas: 1,
                system_current_bytes: 400,
                fast_free_bytes: 100,
                rest_free_bytes: 200,
                mmap_bytes: 50,
                allocator_used_bytes_approx: 150,
            }
        );
    }

    #[test]
    fn parser_reads_final_attributes_from_self_closing_footer_tags() {
        assert_eq!(
            attribute("<total type=\"fast\" count=\"4\" size=\"100\"/>", "size"),
            Some("100")
        );
        assert_eq!(
            attribute("<system type=\"current\" size=\"400\"/>", "size"),
            Some("400")
        );
    }

    #[test]
    fn parser_accepts_glibc_footer_omitting_zero_fast_total() {
        let without_global_fast =
            VALID_FIXTURE.replace("<total type=\"fast\" count=\"4\" size=\"100\"/>\n", "");
        let summary = parse_allocator_summary(&without_global_fast).expect("zero fast-bin fixture");
        assert_eq!(summary.fast_free_bytes, 0);
        assert_eq!(summary.rest_free_bytes, 200);
        assert_eq!(summary.mmap_bytes, 50);
        assert_eq!(summary.system_current_bytes, 400);
        assert_eq!(summary.allocator_used_bytes_approx, 250);
    }

    #[test]
    fn parser_rejects_missing_malformed_and_overflowing_global_totals() {
        let missing = VALID_FIXTURE.replace("<total type=\"mmap\" count=\"1\" size=\"50\"/>\n", "");
        assert_eq!(parse_allocator_summary(&missing), None);

        let malformed = VALID_FIXTURE.replace(
            "<total type=\"fast\" count=\"4\" size=\"100\"/>",
            "<total type=\"fast\" count=\"x\" size=\"100\"/>",
        );
        assert_eq!(parse_allocator_summary(&malformed), None);

        let overflow = VALID_FIXTURE
            .replace("size=\"100\"/>", "size=\"18446744073709551615\"/>")
            .replace("size=\"200\"/>", "size=\"1\"/>");
        assert_eq!(parse_allocator_summary(&overflow), None);
    }

    #[test]
    fn bounded_xml_validation_rejects_invalid_or_incomplete_data() {
        assert_eq!(
            checked_xml(VALID_FIXTURE.as_bytes(), VALID_FIXTURE.len()),
            Some(VALID_FIXTURE)
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
}
