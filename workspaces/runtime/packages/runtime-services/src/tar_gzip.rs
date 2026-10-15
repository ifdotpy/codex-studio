//! Shared USTAR entry serialization and gzip framing.

use std::{
    io::{self, Read, Write},
    path::Path,
};

use flate2::{Compression, write::GzEncoder};

pub struct TarGzipWriter<W: Write> {
    gzip: GzEncoder<W>,
    pax_sequence: u64,
}

impl<W: Write> TarGzipWriter<W> {
    pub fn new(output: W) -> Self {
        Self {
            gzip: GzEncoder::new(output, Compression::default()),
            pax_sequence: 0,
        }
    }

    pub fn append_directory(&mut self, path: &str, mode: u32) -> io::Result<()> {
        self.append_header(path, mode, 0, b'5', "")
    }

    pub fn append_symlink(&mut self, path: &str, mode: u32, target: &str) -> io::Result<()> {
        self.append_header(path, mode, 0, b'2', target)
    }

    pub fn append_file(
        &mut self,
        path: &str,
        mode: u32,
        size: u64,
        input: &mut impl Read,
    ) -> io::Result<()> {
        self.append_header(path, mode, size, b'0', "")?;
        let copied = io::copy(&mut input.take(size), &mut self.gzip)?;
        if copied != size {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "file became shorter while creating the archive",
            ));
        }
        let mut extra = [0u8; 1];
        if input.read(&mut extra)? != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file grew while creating the archive",
            ));
        }
        let padding = ((512 - size % 512) % 512) as usize;
        if padding != 0 {
            self.gzip.write_all(&[0; 512][..padding])?;
        }
        Ok(())
    }

    fn append_header(
        &mut self,
        path: &str,
        mode: u32,
        size: u64,
        kind: u8,
        link: &str,
    ) -> io::Result<()> {
        let path = Path::new(path);
        let whole = path.to_string_lossy();
        let file_name = path.file_name().unwrap_or_default().to_string_lossy();
        let parent = path.parent().and_then(Path::to_str).unwrap_or("");
        let path_fits = file_name.len() <= 100 && parent.len() <= 155;
        let size_fits = size <= MAX_USTAR_SIZE;
        let mut pax = String::new();
        if !path_fits {
            append_pax_record(&mut pax, "path", &whole);
        }
        if link.len() > 100 {
            append_pax_record(&mut pax, "linkpath", link);
        }
        if !size_fits {
            append_pax_record(&mut pax, "size", &size.to_string());
        }
        if !pax.is_empty() {
            self.pax_sequence = self.pax_sequence.checked_add(1).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "PAX header sequence overflow")
            })?;
            let pax_name = format!("PaxHeaders/{}", self.pax_sequence);
            self.write_ustar_header(&pax_name, 0o644, pax.len() as u64, b'x', "")?;
            self.gzip.write_all(pax.as_bytes())?;
            let padding = (512 - pax.len() % 512) % 512;
            if padding != 0 {
                self.gzip.write_all(&[0; 512][..padding])?;
            }
        }
        let fallback = if path_fits {
            whole.as_ref()
        } else {
            "PaxEntry"
        };
        let fallback_link = if link.len() > 100 { "PaxLink" } else { link };
        self.write_ustar_header(
            fallback,
            mode,
            if size_fits { size } else { 0 },
            kind,
            fallback_link,
        )
    }

    fn write_ustar_header(
        &mut self,
        path: &str,
        mode: u32,
        size: u64,
        kind: u8,
        link: &str,
    ) -> io::Result<()> {
        let path = Path::new(path);
        let file_name = path.file_name().unwrap_or_default().to_string_lossy();
        let parent = path.parent().and_then(Path::to_str).unwrap_or("");
        if file_name.len() > 100 || parent.len() > 155 || link.len() > 100 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid USTAR fallback path",
            ));
        }

        let mut header = [0u8; 512];
        put_octal(&mut header[100..108], mode as u64)?;
        put_octal(&mut header[108..116], 0)?;
        put_octal(&mut header[116..124], 0)?;
        put_octal(&mut header[124..136], size)?;
        put_octal(&mut header[136..148], 0)?;
        header[148..156].fill(b' ');
        header[156] = kind;
        header[..file_name.len()].copy_from_slice(file_name.as_bytes());
        header[157..157 + link.len()].copy_from_slice(link.as_bytes());
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        header[345..345 + parent.len()].copy_from_slice(parent.as_bytes());
        let checksum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
        put_octal(&mut header[148..156], checksum)?;
        self.gzip.write_all(&header)
    }

    pub fn finish(mut self) -> io::Result<W> {
        self.gzip.write_all(&[0; 1024])?;
        self.gzip.finish()
    }
}

fn append_pax_record(output: &mut String, key: &str, value: &str) {
    let body = format!("{key}={value}\n");
    let mut length = body.len() + 2;
    loop {
        let next = body.len() + length.to_string().len() + 1;
        if next == length {
            break;
        }
        length = next;
    }
    output.push_str(&format!("{length} {body}"));
}

const MAX_USTAR_SIZE: u64 = 0o77777777777;

fn put_octal(output: &mut [u8], value: u64) -> io::Result<()> {
    let width = output.len().saturating_sub(1);
    if width == 0 || width >= 22 || value >= 8u64.pow(width as u32) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "value does not fit in a USTAR octal field",
        ));
    }
    let text = format!("{value:0width$o}\0", width = output.len() - 1);
    if text.len() != output.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "value does not fit in a USTAR octal field",
        ));
    }
    output.copy_from_slice(text.as_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::TarGzipWriter;
    use flate2::read::GzDecoder;
    use std::io::{Cursor, Read};

    #[test]
    fn long_paths_and_links_use_pax_extended_headers() {
        let mut output = Vec::new();
        let mut writer = TarGzipWriter::new(&mut output);
        let path = format!("{}/file.txt", "component".repeat(18));
        let link = format!("/{}", "target".repeat(24));
        writer
            .append_symlink(&path, 0o777, &link)
            .expect("PAX symlink");
        writer.finish().expect("finish archive");

        let mut tar = Vec::new();
        GzDecoder::new(output.as_slice())
            .read_to_end(&mut tar)
            .expect("decompress archive");
        assert_eq!(tar[156], b'x');
        let size = u64::from_str_radix(
            std::str::from_utf8(&tar[124..136])
                .unwrap()
                .trim_matches(char::from(0))
                .trim(),
            8,
        )
        .unwrap() as usize;
        let pax = std::str::from_utf8(&tar[512..512 + size]).unwrap();
        assert!(pax.contains(&format!("path={path}\n")));
        assert!(pax.contains(&format!("linkpath={link}\n")));
        assert_eq!(tar[512 + ((size + 511) / 512) * 512 + 156], b'2');
    }

    #[test]
    fn file_payload_must_match_the_observed_size() {
        let mut output = Vec::new();
        let mut writer = TarGzipWriter::new(&mut output);
        assert!(
            writer
                .append_file("short", 0o644, 4, &mut Cursor::new(b"abc"))
                .is_err()
        );

        let mut output = Vec::new();
        let mut writer = TarGzipWriter::new(&mut output);
        assert!(
            writer
                .append_file("grown", 0o644, 3, &mut Cursor::new(b"four"))
                .is_err()
        );
    }

    #[test]
    fn large_file_sizes_use_pax_without_overflowing_ustar() {
        let mut output = Vec::new();
        let mut writer = TarGzipWriter::new(&mut output);
        let size = 8 * 1024 * 1024 * 1024;
        writer
            .append_header("large.bin", 0o644, size, b'0', "")
            .expect("PAX size header");
        writer.finish().expect("finish archive");

        let mut tar = Vec::new();
        GzDecoder::new(output.as_slice())
            .read_to_end(&mut tar)
            .expect("decompress archive");
        assert_eq!(tar[156], b'x');
        let pax_size = u64::from_str_radix(
            std::str::from_utf8(&tar[124..136])
                .unwrap()
                .trim_matches(char::from(0))
                .trim(),
            8,
        )
        .unwrap() as usize;
        let pax = std::str::from_utf8(&tar[512..512 + pax_size]).unwrap();
        assert!(pax.contains("size=8589934592\n"));
        let file_offset = 512 + pax_size.div_ceil(512) * 512;
        assert_eq!(tar[file_offset + 156], b'0');
        assert_eq!(&tar[file_offset + 124..file_offset + 136], b"00000000000\0");
    }
}
