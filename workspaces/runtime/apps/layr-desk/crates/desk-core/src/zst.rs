//! zstd frames of the vault. A delta frame takes the previous content as a raw dictionary, as
//! `zstd --patch-from` and the writer make it.

use anyhow::{anyhow, bail, Result};
use zstd_safe::{DCtx, InBuffer, OutBuffer};

/// The largest content one frame may decode to; more is refused as a damaged or hostile frame.
pub const LIMIT: usize = 4 << 30;

fn error(code: usize) -> anyhow::Error {
    anyhow!("zstd: {}", zstd_safe::get_error_name(code))
}

/// One frame decoded; `base` is the raw dictionary of a delta frame.
pub fn decompress(frame: &[u8], base: Option<&[u8]>) -> Result<Vec<u8>> {
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
        let hint = dctx
            .decompress_stream(&mut output, &mut input)
            .map_err(error)?;
        let written = output.pos();
        out.extend_from_slice(&chunk[..written]);
        if out.len() > LIMIT {
            bail!("a zstd frame decodes to more than {LIMIT} bytes");
        }
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

/// The zstd level of the writer.
pub const LEVEL: i32 = 3;

/// The largest window of a delta frame: what every decoder accepts by default.
const WINDOW_LOG_MAX: u32 = 27;

/// One frame of `data` at `LEVEL`, with its content size. With `base` it is a delta frame that
/// takes the base as a raw dictionary (a prefix), made as `zstd --patch-from` makes it: the
/// window covers the larger input, so a match may reach any byte of the base, and long-distance
/// matching finds matches beyond what the level searches. Without that, a file above 2 MiB
/// gets hardly any gain from its base.
pub fn compress(data: &[u8], base: Option<&[u8]>) -> Result<Vec<u8>> {
    use zstd_safe::{CCtx, CParameter};
    let mut cctx = CCtx::create();
    cctx.set_parameter(CParameter::CompressionLevel(LEVEL))
        .map_err(error)?;
    cctx.set_parameter(CParameter::ContentSizeFlag(true))
        .map_err(error)?;
    if let Some(base) = base.filter(|b| !b.is_empty()) {
        let larger = base.len().max(data.len()) as u64;
        // One more than the highest bit of the larger input, as the zstd command sets it.
        let window_log = (u64::BITS - larger.leading_zeros() + 1).clamp(10, WINDOW_LOG_MAX);
        cctx.set_parameter(CParameter::WindowLog(window_log))
            .map_err(error)?;
        if window_log > 16 {
            cctx.set_parameter(CParameter::EnableLongDistanceMatching(true))
                .map_err(error)?;
        }
        cctx.ref_prefix(base).map_err(error)?;
    }
    let mut out = Vec::with_capacity(zstd_safe::compress_bound(data.len()));
    cctx.compress2(&mut out, data).map_err(error)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: usize) -> Vec<u8> {
        let mut seed = 1u64;
        let mut out = Vec::new();
        for i in 0..lines {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let line = format!(
                "def f{:x}(a, b): return g{:x}(a) * {i}\n",
                seed >> 40,
                seed >> 20
            );
            out.extend(line.as_bytes());
        }
        out
    }

    #[test]
    fn frames_decode_with_and_without_a_base() {
        for data in [Vec::new(), b"x".to_vec(), text(1000)] {
            assert_eq!(
                decompress(&compress(&data, None).unwrap(), None).unwrap(),
                data
            );
        }
        let base = text(1000);
        let mut new = base.clone();
        new.extend(b"one more line\n");
        let delta = compress(&new, Some(&base)).unwrap();
        assert!(delta.len() < 100, "{}", delta.len());
        assert_eq!(decompress(&delta, Some(&base)).unwrap(), new);
        assert_eq!(
            compress(&new, Some(b"")).unwrap(),
            compress(&new, None).unwrap()
        );
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
        assert!(
            delta.len() * 100 < full.len(),
            "{} {}",
            delta.len(),
            full.len()
        );
        assert_eq!(decompress(&delta, Some(&base)).unwrap(), new);
    }
}
