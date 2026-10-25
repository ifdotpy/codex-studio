//! zstd frames of the vault. A delta frame takes the previous content as a raw dictionary, as
//! `zstd --patch-from` and the writer make it.

#[cfg(feature = "native")]
use anyhow::anyhow;
use anyhow::{bail, Result};
#[cfg(feature = "native")]
use zstd_safe::{DCtx, InBuffer, OutBuffer};

/// Default decoded-size limit for metadata and intermediate delta frames.
// A 32-bit target can address at most usize::MAX bytes.
pub const LIMIT: usize = if usize::BITS > 32 { (4u64 << 30) as usize } else { usize::MAX };

#[cfg(feature = "native")]
fn error(code: usize) -> anyhow::Error {
    anyhow!("zstd: {}", zstd_safe::get_error_name(code))
}

/// One frame decoded; `base` is the raw dictionary of a delta frame.
pub fn decompress(frame: &[u8], base: Option<&[u8]>) -> Result<Vec<u8>> {
    decompress_with_limit(frame, base, LIMIT)
}

/// Decode a file frame within the size declared by its authenticated tree entry.
/// Metadata and intermediate frames continue to use the default limit.
#[cfg(feature = "native")]
pub fn decompress_with_limit(frame: &[u8], base: Option<&[u8]>, limit: usize) -> Result<Vec<u8>> {
    if zstd_safe::get_frame_content_size(frame)
        .map_err(|_| anyhow!("invalid zstd frame header"))?
        .is_some_and(|size| size > limit as u64)
    {
        bail!("a zstd frame decodes to more than {limit} bytes");
    }
    let mut dctx = DCtx::create();
    if let Some(base) = base.filter(|b| !b.is_empty()) {
        // A prefix is a raw-content dictionary for one frame, whatever its first bytes are.
        dctx.ref_prefix(base).map_err(error)?;
    }
    let mut out = Vec::new();
    let mut input = InBuffer::around(frame);
    let mut chunk = vec![0u8; DCtx::out_size()];
    loop {
        let mut output = OutBuffer::around(&mut chunk[..]);
        let hint = dctx.decompress_stream(&mut output, &mut input).map_err(error)?;
        let written = output.pos();
        if written > limit.saturating_sub(out.len()) {
            bail!("a zstd frame decodes to more than {limit} bytes");
        }
        out.try_reserve(written)?;
        out.extend_from_slice(&chunk[..written]);
        if hint == 0 {
            if input.pos() != frame.len() {
                bail!("bytes after the zstd frame");
            }
            return Ok(out);
        }
        if input.pos() == frame.len() && written == 0 {
            bail!("the zstd frame is cut short");
        }
    }
}

/// Decode a frame with the pure Rust decoder on targets without native support.
#[cfg(all(feature = "wasm", not(feature = "native")))]
pub fn decompress_with_limit(frame: &[u8], base: Option<&[u8]>, limit: usize) -> Result<Vec<u8>> {
    decompress_rust(frame, base, limit)
}

#[cfg(all(feature = "wasm", any(not(feature = "native"), test)))]
fn decompress_rust(frame: &[u8], base: Option<&[u8]>, limit: usize) -> Result<Vec<u8>> {
    use ruzstd::decoding::{Dictionary, StreamingDecoder};
    use std::io::Read;

    let mut decoder = StreamingDecoder::new(frame)?;
    let size = decoder.decoder.content_size();
    if size > limit as u64 {
        bail!("a zstd frame decodes to more than {limit} bytes");
    }
    if let Some(base) = base.filter(|bytes| !bytes.is_empty()) {
        // A raw prefix has no entropy tables and uses the initial repeat offsets.
        decoder.decoder.add_dict(Dictionary {
            id: 1,
            fse: Default::default(),
            huf: Default::default(),
            dict_content: base.to_vec(),
            offset_hist: [1, 4, 8],
        })?;
        decoder.decoder.force_dict(1)?;
    }
    let mut out = Vec::new();
    let mut chunk = [0u8; 64 << 10];
    loop {
        let written = decoder.read(&mut chunk)?;
        if written == 0 {
            break;
        }
        if written > limit.saturating_sub(out.len()) {
            bail!("a zstd frame decodes to more than {limit} bytes");
        }
        out.try_reserve(written)?;
        out.extend_from_slice(&chunk[..written]);
    }
    if !decoder.get_ref().is_empty() {
        bail!("bytes after the zstd frame");
    }
    // The descriptor indicates whether zero means an empty frame or an unknown size.
    let descriptor = frame[4];
    if (descriptor & 0x20 != 0 || descriptor >> 6 != 0) && out.len() as u64 != size {
        bail!("zstd frame content size does not match");
    }
    if let Some(checksum) = decoder.decoder.get_checksum_from_data() {
        if decoder.decoder.get_calculated_checksum() != Some(checksum) {
            bail!("zstd frame checksum does not match");
        }
    }
    Ok(out)
}

/// The zstd level of the writer.
pub const LEVEL: i32 = 3;

/// The largest window of a delta frame: what every decoder accepts by default.
#[cfg(feature = "native")]
const WINDOW_LOG_MAX: u32 = 27;

/// One frame of `data` at `LEVEL`, with its content size. With `base` it is a delta frame that
/// takes the base as a raw dictionary (a prefix), made as `zstd --patch-from` makes it: the
/// window covers the larger input, so a match may reach any byte of the base, and long-distance
/// matching finds matches beyond what the level searches. Without that, a file above 2 MiB
/// gets hardly any gain from its base.
#[cfg(feature = "native")]
pub fn compress(data: &[u8], base: Option<&[u8]>) -> Result<Vec<u8>> {
    use zstd_safe::{CCtx, CParameter};
    let mut cctx = CCtx::create();
    cctx.set_parameter(CParameter::CompressionLevel(LEVEL)).map_err(error)?;
    cctx.set_parameter(CParameter::ContentSizeFlag(true)).map_err(error)?;
    if let Some(base) = base.filter(|b| !b.is_empty()) {
        let larger = base.len().max(data.len()) as u64;
        // One more than the highest bit of the larger input, as the zstd command sets it.
        let window_log = (u64::BITS - larger.leading_zeros() + 1).clamp(10, WINDOW_LOG_MAX);
        cctx.set_parameter(CParameter::WindowLog(window_log)).map_err(error)?;
        if window_log > 16 {
            cctx.set_parameter(CParameter::EnableLongDistanceMatching(true)).map_err(error)?;
        }
        cctx.ref_prefix(base).map_err(error)?;
    }
    let mut out = Vec::with_capacity(zstd_safe::compress_bound(data.len()));
    cctx.compress2(&mut out, data).map_err(error)?;
    Ok(out)
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;

    pub(super) fn text(lines: usize) -> Vec<u8> {
        let mut seed = 1u64;
        let mut out = Vec::new();
        for i in 0..lines {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let line = format!("def f{:x}(a, b): return g{:x}(a) * {i}\n", seed >> 40, seed >> 20);
            out.extend(line.as_bytes());
        }
        out
    }

    #[test]
    #[ignore = "explicit large-file check: use an isolated host with at least 24 GiB RAM"]
    fn authenticated_file_above_four_gib_restores() {
        let size = 5usize * 1024 * 1024 * 1024 + 1;
        let input = vec![0u8; size];
        let compressed = compress(&input, None).unwrap();
        assert!(decompress(&compressed, None).is_err());
        let restored = decompress_with_limit(&compressed, None, size).unwrap();
        assert_eq!(restored.len(), size);
        assert!(restored.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn file_size_limit_is_enforced_before_output_is_returned() {
        let data = b"authenticated file size";
        let frame = compress(data, None).unwrap();
        assert!(decompress_with_limit(&frame, None, data.len() - 1).is_err());
        assert_eq!(decompress_with_limit(&frame, None, data.len()).unwrap(), data);
        assert_eq!(decompress_with_limit(&frame, None, LIMIT + 1).unwrap(), data);
    }

    #[test]
    fn frames_decode_with_and_without_a_base() {
        for data in [Vec::new(), b"x".to_vec(), text(1000)] {
            assert_eq!(decompress(&compress(&data, None).unwrap(), None).unwrap(), data);
        }
        let base = text(1000);
        let mut new = base.clone();
        new.extend(b"one more line\n");
        let delta = compress(&new, Some(&base)).unwrap();
        assert!(delta.len() < 100, "{}", delta.len());
        assert_eq!(decompress(&delta, Some(&base)).unwrap(), new);
        assert_eq!(compress(&new, Some(b"")).unwrap(), compress(&new, None).unwrap());
    }

    #[test]
    fn a_large_file_gets_a_small_delta() {
        // 3.6 MB: beyond the 2 MiB window of level 3.
        let base = text(80_000);
        let mut new = base.clone();
        let middle = base.len() / 2;
        new.splice(middle..middle, b"a changed line\n".iter().copied());
        let full = compress(&new, None).unwrap();
        let delta = compress(&new, Some(&base)).unwrap();
        assert!(delta.len() * 100 < full.len(), "{} {}", delta.len(), full.len());
        assert_eq!(decompress(&delta, Some(&base)).unwrap(), new);
    }
}

#[cfg(all(test, feature = "native", feature = "wasm"))]
mod rust_tests {
    use super::*;

    #[test]
    fn rust_decoder_reads_native_frames_and_raw_prefixes() {
        for data in [Vec::new(), b"x".to_vec(), tests::text(1000)] {
            let frame = compress(&data, None).unwrap();
            assert_eq!(decompress_rust(&frame, None, LIMIT).unwrap(), data);
            assert!(decompress_rust(&frame[..frame.len() - 1], None, LIMIT).is_err());
            let mut trailing = frame.clone();
            trailing.push(0);
            assert!(decompress_rust(&trailing, None, LIMIT).is_err());
            if !data.is_empty() {
                assert!(decompress_rust(&frame, None, data.len() - 1).is_err());
            }
        }
        let mut base = tests::text(80_000);
        // Dictionary magic must remain raw content when supplied as a prefix.
        base[..4].copy_from_slice(&[0x37, 0xa4, 0x30, 0xec]);
        let mut changed = base.clone();
        changed.splice(base.len() / 2..base.len() / 2, b"a changed line\n".iter().copied());
        let delta = compress(&changed, Some(&base)).unwrap();
        assert_eq!(decompress_rust(&delta, Some(&base), changed.len()).unwrap(), changed);
        assert!(decompress_rust(&delta, None, LIMIT).is_err());
    }

    #[test]
    fn rust_decoder_checks_checksum_and_unknown_size_limit() {
        use zstd_safe::{CCtx, CParameter};
        let data = b"checked frame content";
        let mut context = CCtx::create();
        context.set_parameter(CParameter::ChecksumFlag(true)).unwrap();
        context.set_parameter(CParameter::ContentSizeFlag(false)).unwrap();
        let mut frame = Vec::with_capacity(zstd_safe::compress_bound(data.len()));
        context.compress2(&mut frame, data).unwrap();
        assert_eq!(decompress_rust(&frame, None, data.len()).unwrap(), data);
        assert!(decompress_rust(&frame, None, data.len() - 1).is_err());
        *frame.last_mut().unwrap() ^= 1;
        assert!(decompress_rust(&frame, None, LIMIT).is_err());
    }
}
