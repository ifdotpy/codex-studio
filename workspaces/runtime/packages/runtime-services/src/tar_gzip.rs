//! Shared streaming tar archive construction with gzip framing.

use std::io::{self, Read, Write};

use flate2::{Compression, write::GzEncoder};
use tar::{Builder, EntryType, Header, HeaderMode};

pub struct TarGzipWriter<W: Write> {
    tar: Builder<GzEncoder<W>>,
}

impl<W: Write> TarGzipWriter<W> {
    pub fn new(output: W) -> Self {
        let mut tar = Builder::new(GzEncoder::new(output, Compression::default()));
        tar.mode(HeaderMode::Deterministic);
        Self { tar }
    }

    pub fn append_directory(&mut self, path: &str, mode: u32) -> io::Result<()> {
        let mut header = header(EntryType::Directory, mode, 0);
        self.tar.append_data(&mut header, path, io::empty())
    }

    pub fn append_symlink(&mut self, path: &str, mode: u32, target: &str) -> io::Result<()> {
        let mut header = header(EntryType::Symlink, mode, 0);
        self.tar.append_link(&mut header, path, target)
    }

    pub fn append_file(
        &mut self,
        path: &str,
        mode: u32,
        size: u64,
        input: &mut impl Read,
    ) -> io::Result<()> {
        let mut header = header(EntryType::Regular, mode, size);
        let copied = {
            let mut limited = CountReader {
                inner: input.take(size),
                read: 0,
            };
            self.tar.append_data(&mut header, path, &mut limited)?;
            limited.read
        };
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
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<W> {
        self.tar.finish()?;
        self.tar.into_inner()?.finish()
    }
}

fn header(kind: EntryType, mode: u32, size: u64) -> Header {
    let mut header = Header::new_gnu();
    header.set_entry_type(kind);
    header.set_mode(mode);
    header.set_uid(0);
    header.set_gid(0);
    header.set_size(size);
    header.set_mtime(0);
    header.set_cksum();
    header
}

struct CountReader<R> {
    inner: R,
    read: u64,
}

impl<R: Read> Read for CountReader<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(output)?;
        self.read = self.read.saturating_add(read as u64);
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::disallowed_methods)]

    use super::TarGzipWriter;
    use flate2::read::GzDecoder;
    use std::io::{Cursor, Read};
    use tar::Archive;

    #[test]
    fn long_unicode_paths_and_symlink_targets_round_trip_with_modes() {
        let mut output = Vec::new();
        let mut writer = TarGzipWriter::new(&mut output);
        let path = format!("目录/{}/café.txt", "component".repeat(18));
        let link = format!("../{}", "target".repeat(24));
        writer
            .append_symlink(&format!("{path}.link"), 0o777, &link)
            .expect("long symlink");
        writer
            .append_file(&path, 0o751, 4, &mut Cursor::new(b"data"))
            .expect("long unicode file");
        writer.finish().expect("finish archive");

        let mut archive = Archive::new(GzDecoder::new(output.as_slice()));
        let mut entries = archive.entries().expect("entries");
        let symlink = entries.next().expect("symlink entry").expect("valid entry");
        assert!(symlink.header().entry_type().is_symlink());
        assert_eq!(symlink.header().mode().unwrap(), 0o777);
        assert_eq!(
            symlink.path().unwrap().to_string_lossy(),
            format!("{path}.link")
        );
        assert_eq!(
            symlink
                .link_name()
                .unwrap()
                .expect("symlink target")
                .to_string_lossy(),
            link
        );
        let file = entries.next().expect("file entry").expect("valid entry");
        assert_eq!(file.path().unwrap().to_string_lossy(), path);
        assert_eq!(file.header().mode().unwrap(), 0o751);
        let mut content = String::new();
        file.take(16).read_to_string(&mut content).unwrap();
        assert_eq!(content, "data");
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
}
