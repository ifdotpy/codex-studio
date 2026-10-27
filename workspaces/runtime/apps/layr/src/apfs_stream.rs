//! APFS transport using the existing btrfs v1 wire format.
use anyhow::{bail, Context, Result};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::Path,
};

// btrfs uses CRC32C with zero seed and no final inversion.
const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut bit = 0;
        while bit < 8 {
            c = (c >> 1) ^ (0x82f63b78u32 & 0u32.wrapping_sub(c & 1));
            bit += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};
fn crc(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0u32, |c, b| (c >> 8) ^ CRC_TABLE[((c as u8) ^ b) as usize])
}

fn command(out: &mut dyn Write, code: u16, attrs: &[(u16, &[u8])]) -> Result<()> {
    let mut v = vec![0; 10];
    for &(t, value) in attrs {
        let len = u16::try_from(value.len()).context("stream attribute exceeds 64 KiB")?;
        v.extend(t.to_le_bytes());
        v.extend(len.to_le_bytes());
        v.extend(value);
    }
    let len = (v.len() - 10) as u32;
    v[..4].copy_from_slice(&len.to_le_bytes());
    v[4..6].copy_from_slice(&code.to_le_bytes());
    let checksum = crc(&v);
    v[6..10].copy_from_slice(&checksum.to_le_bytes());
    out.write_all(&v)?;
    Ok(())
}
fn time(sec: i64, nano: i64) -> Vec<u8> {
    let mut v = sec.to_le_bytes().to_vec();
    v.extend((nano as u32).to_le_bytes());
    v
}
/// A full stream is valid even when the caller offers an incremental parent.
/// This preserves old peer and remote copy compatibility without changing their protocol.
pub fn send(root: &Path, out: &mut dyn Write) -> Result<()> {
    let info = crate::storage::subvol_info(root)?;
    if !info.readonly {
        bail!("only immutable snapshots can be sent");
    }
    let uuid = uuid::Uuid::parse_str(info.received_uuid.as_deref().unwrap_or(&info.uuid))?;
    let name = root.file_name().context("snapshot has no name")?.as_bytes();
    out.write_all(b"btrfs-stream\0\x01\0\0\0")?;
    command(out, 1, &[(15, name), (1, uuid.as_bytes()), (2, &1u64.to_le_bytes())])?;
    fn visit(root: &Path, rel: &Path, out: &mut dyn Write, links: &mut BTreeMap<(u64, u64), Vec<u8>>) -> Result<()> {
        let full = root.join(rel);
        let m = fs::symlink_metadata(&full)?;
        if crate::stream_fs::has_acl(&full)? {
            bail!("cannot transfer macOS access control lists to btrfs: {}", full.display());
        }
        let p = rel.as_os_str().as_bytes();
        let ino = m.ino().to_le_bytes();
        let mut linked = false;
        if !p.is_empty() {
            if m.is_dir() {
                command(out, 4, &[(15, p), (3, &ino)])?;
            } else if m.file_type().is_symlink() {
                command(out, 8, &[(15, p), (3, &ino), (17, fs::read_link(&full)?.as_os_str().as_bytes())])?;
            } else if m.is_file() {
                let key = (m.dev(), m.ino());
                if let Some(first) = links.get(&key) {
                    command(out, 10, &[(15, p), (17, first)])?;
                    linked = true;
                } else {
                    links.insert(key, p.to_vec());
                    command(out, 3, &[(15, p), (3, &ino)])?;
                    let mut f = File::open(&full)?;
                    let mut buf = vec![0; 48 * 1024];
                    let mut offset = 0u64;
                    loop {
                        let n = f.read(&mut buf)?;
                        if n == 0 {
                            break;
                        }
                        if buf[..n].iter().any(|b| *b != 0) {
                            command(out, 15, &[(15, p), (18, &offset.to_le_bytes()), (19, &buf[..n])])?;
                        }
                        offset += n as u64;
                    }
                    command(out, 17, &[(15, p), (4, &m.len().to_le_bytes())])?;
                }
            } else {
                bail!("cannot send special file {}", full.display());
            }
        }
        if m.is_dir() {
            for entry in fs::read_dir(&full)? {
                visit(root, &rel.join(entry?.file_name()), out, links)?;
            }
        }
        if !linked {
            for (name, value) in crate::stream_fs::xattrs(&full)? {
                if name == b"com.layr.subvolume"
                    || name == b"com.layr.origin"
                    || name == b"trusted.layr.origin"
                    || name == b"trusted.layr.cache-restore"
                {
                    continue;
                }
                // Linux forbids user.* attributes on symlinks. Its root receiver can
                // retain them in trusted.*, including attributes specific to macOS.
                #[cfg(target_os = "macos")]
                let name = if m.file_type().is_symlink() {
                    format!("trusted.layr.apfs.{}", hex::encode(name)).into_bytes()
                } else if name.starts_with(b"user.") {
                    name
                } else {
                    format!("user.layr.apfs.{}", hex::encode(name)).into_bytes()
                };
                #[cfg(feature = "overlayfs")]
                if !name.starts_with(b"user.") { bail!("cannot transfer attribute {}",String::from_utf8_lossy(&name)); }
                command(out, 13, &[(15, p), (13, &name), (14, &value)])?;
            }
            if !m.file_type().is_symlink() {
                command(out, 18, &[(15, p), (5, &((m.mode() & 0o7777) as u64).to_le_bytes())])?;
            }
            command(
                out,
                20,
                &[
                    (15, p),
                    (9, &time(m.ctime(), m.ctime_nsec())),
                    (10, &time(m.mtime(), m.mtime_nsec())),
                    (11, &time(m.atime(), m.atime_nsec())),
                ],
            )?;
        }
        Ok(())
    }
    visit(root, Path::new(""), out, &mut BTreeMap::new())?;
    command(out, 21, &[])
}

fn value<'a>(attrs: &[(u16, &'a [u8])], key: u16) -> Result<&'a [u8]> {
    attrs.iter().find(|(k, _)| *k == key).map(|(_, v)| *v).with_context(|| format!("stream missing attribute {key}"))
}
fn number(attrs: &[(u16, &[u8])], key: u16) -> Result<u64> {
    Ok(u64::from_le_bytes(value(attrs, key)?.try_into().context("invalid integer")?))
}
fn path(root: &Path, bytes: &[u8], follow: bool) -> Result<std::path::PathBuf> {
    let rel = std::str::from_utf8(bytes)?;
    if bytes.contains(&0) {
        bail!("NUL in stream path");
    }
    let p = crate::fsutil::safe_join(root, rel)?;
    if follow && fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("stream operation follows a symbolic link");
    }
    Ok(p)
}
/// Validate before applying. All writes stay in a private temporary directory.
/// Only a complete, sealed tree becomes visible under its state name.
pub fn receive(src: &mut dyn Read, dir: &Path, expected: &crate::recv::Expect) -> Result<()> {
    use std::{
        io::{Seek, SeekFrom},
        os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    };
    let staging = dir.join(format!(".receive-{}", crate::ids::new_id()));
    fs::DirBuilder::new().mode(0o700).create(&staging)?;
    let result = (|| -> Result<()> {
        let mut spool =
            fs::OpenOptions::new().create_new(true).read(true).write(true).mode(0o600).open(staging.join("stream"))?;
        let parent = crate::recv::check(src, &mut spool, expected)?;
        spool.seek(SeekFrom::Start(0))?;
        let root = staging.join("tree");
        let mut head = [0; 17];
        spool.read_exact(&mut head)?;
        let version = u32::from_le_bytes(head[13..].try_into()?);
        if version > 2 {
            bail!("portable receive supports send stream versions 1 and 2");
        }
        if let Some(p) = &parent {
            crate::storage::snapshot(p, &root, false)?;
        } else {
            crate::storage::create_subvolume(&root)?;
        }
        // Delay permissions until all content is in place. Key by inode to survive renames.
        let mut modes = BTreeMap::new();
        fn prepare(p: &Path, modes: &mut BTreeMap<u64, u32>) -> Result<()> {
            let m = fs::symlink_metadata(p)?;
            if m.file_type().is_symlink() {
                return Ok(());
            }
            modes.insert(m.ino(), m.mode() & 0o7777);
            fs::set_permissions(p, fs::Permissions::from_mode((m.mode() & 0o7777) | 0o700))?;
            if m.is_dir() {
                for e in fs::read_dir(p)? {
                    prepare(&e?.path(), modes)?;
                }
            }
            Ok(())
        }
        prepare(&root, &mut modes)?;
        let mut times: BTreeMap<u64, (i64, i64)> = BTreeMap::new();
        loop {
            let mut hdr = [0; 10];
            spool.read_exact(&mut hdr)?;
            let len = u32::from_le_bytes(hdr[..4].try_into()?) as usize;
            if len > 16 << 20 {
                bail!("stream command exceeds limit");
            }
            let code = u16::from_le_bytes(hdr[4..6].try_into()?);
            let checksum = u32::from_le_bytes(hdr[6..10].try_into()?);
            let mut body = vec![0; len];
            spool.read_exact(&mut body)?;
            hdr[6..].fill(0);
            let mut checked = hdr.to_vec();
            checked.extend(&body);
            if crc(&checked) != checksum {
                bail!("stream checksum mismatch");
            }
            let at = crate::recv::attrs(&body, version)?;
            if code == 1 || code == 2 {
                continue;
            }
            if code == 21 {
                break;
            }
            let p = path(&root, value(&at, 15)?, !matches!(code, 8..=14 | 19 | 20))?;
            // On a volume that ignores case, README and Readme are one entry: refuse instead of
            // failing on an unclear error or replacing the other entry.
            let clash = |q: &Path| -> Result<()> {
                if let Some(stored) = crate::fsutil::case_clash(q) {
                    bail!(
                        "{} and {stored} differ only in case, and this volume does not tell them apart; use a case-sensitive APFS volume for the store",
                        q.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
                    );
                }
                Ok(())
            };
            if matches!(code, 3 | 4 | 8 | 10) {
                clash(&p)?;
            }
            match code {
                3 => {
                    fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&p)?;
                }
                4 => {
                    fs::create_dir(&p)?;
                }
                8 => {
                    std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(value(&at, 17)?), &p)?;
                }
                9 => {
                    let to = path(&root, value(&at, 16)?, false)?;
                    // A rename of an entry to another case of its own name is no clash: the
                    // entry that `to` finds is `p` itself, in the same folder.
                    let itself = to.parent() == p.parent()
                        && crate::fsutil::case_clash(&to).is_some_and(|s| p.file_name() == Some(s.as_ref()));
                    if !itself {
                        clash(&to)?;
                    }
                    fs::rename(&p, to)?;
                }
                10 => {
                    let from = path(&root, value(&at, 17)?, true)?;
                    if !fs::symlink_metadata(&from)?.is_file() {
                        bail!("hardlink source is not a file");
                    }
                    fs::hard_link(from, &p)?;
                }
                11 => fs::remove_file(&p)?,
                12 => fs::remove_dir(&p)?,
                13 | 14 => {
                    let raw = value(&at, 13)?;
                    if raw.starts_with(b"system.posix_acl_") || raw.starts_with(b"security.") {
                        bail!("APFS cannot enforce the Linux security attribute {}", String::from_utf8_lossy(raw));
                    }
                    #[cfg(target_os = "macos")]
                    let name = if let Some(encoded) =
                        raw.strip_prefix(b"user.layr.apfs.").or_else(|| raw.strip_prefix(b"trusted.layr.apfs."))
                    {
                        hex::decode(encoded)?
                    } else {
                        raw.to_vec()
                    };
                    #[cfg(feature = "overlayfs")]
                    let name = {
                        if !raw.starts_with(b"user.") { bail!("unsupported portable attribute namespace"); }
                        raw.to_vec()
                    };
                    if name == b"com.layr.subvolume"
                        || name == b"com.layr.origin"
                        || name == b"trusted.layr.origin"
                        || name == b"trusted.layr.cache-restore"
                    {
                        bail!("stream cannot set internal APFS metadata");
                    }
                    let name = std::ffi::CString::new(name)?;
                    if code == 13 {
                        crate::stream_fs::attribute(&p, &name, Some(value(&at, 14)?))?;
                    } else {
                        crate::stream_fs::remove_attribute(&p, &name)?;
                    }
                }
                15 => {
                    let mut f = fs::OpenOptions::new().write(true).custom_flags(libc::O_NOFOLLOW).open(&p)?;
                    f.seek(SeekFrom::Start(number(&at, 18)?))?;
                    f.write_all(value(&at, 19)?)?;
                }
                16 => {
                    let source_id = uuid::Uuid::from_slice(value(&at, 20)?)?.to_string();
                    let source_root = if source_id == expected.uuid {
                        root.clone()
                    } else if let Some(p) = (expected.parent)(&source_id) {
                        p
                    } else {
                        expected
                            .clones
                            .iter()
                            .find(|(u, _, _)| *u == source_id)
                            .map(|(_, _, p)| p.clone())
                            .context("clone source unavailable")?
                    };
                    let from = path(&source_root, value(&at, 22)?, true)?;
                    if !fs::metadata(&from)?.is_file() {
                        bail!("clone source is not a file");
                    }
                    let mut input = File::open(from)?;
                    let mut output = fs::OpenOptions::new().write(true).custom_flags(libc::O_NOFOLLOW).open(&p)?;
                    let len = number(&at, 24)?;
                    let start = number(&at, 23)?;
                    if start.checked_add(len).is_none_or(|end| end > input.metadata().map(|m| m.len()).unwrap_or(0)) {
                        bail!("clone range exceeds source");
                    }
                    input.seek(SeekFrom::Start(start))?;
                    output.seek(SeekFrom::Start(number(&at, 18)?))?;
                    if std::io::copy(&mut input.take(len), &mut output)? != len {
                        bail!("short clone source");
                    }
                }
                17 => fs::OpenOptions::new().write(true).custom_flags(libc::O_NOFOLLOW).open(&p)?.set_len(number(&at, 4)?)?,
                18 => {
                    modes.insert(fs::symlink_metadata(&p)?.ino(), (number(&at, 5)? & 0o7777) as u32);
                }
                // Numeric owners do not identify the same user on two machines.
                // APFS stores belong to the local user; worktree access is applied by layr.
                19 => {
                    number(&at, 6)?;
                    number(&at, 7)?;
                }
                20 => {
                    let t = value(&at, 10)?;
                    if t.len() != 12 {
                        bail!("invalid stream time");
                    }
                    let sec = i64::from_le_bytes(t[..8].try_into()?);
                    let ns = u32::from_le_bytes(t[8..].try_into()?);
                    if ns >= 1_000_000_000 {
                        bail!("invalid nanoseconds");
                    }
                    times.insert(fs::symlink_metadata(&p)?.ino(), (sec, ns as i64));
                }
                _ => bail!("portable receive cannot apply stream command {code}"),
            }
        }
        fn finish(p: &Path, modes: &BTreeMap<u64, u32>, times: &BTreeMap<u64, (i64, i64)>) -> Result<()> {
            let m = fs::symlink_metadata(p)?;
            if m.is_dir() {
                for e in fs::read_dir(p)? {
                    finish(&e?.path(), modes, times)?;
                }
            }
            if let Some(&(s, n)) = times.get(&m.ino()) {
                crate::stream_fs::mtime(p, s, n)?;
            }
            if !m.file_type().is_symlink() {
                if let Some(&mode) = modes.get(&m.ino()) {
                    fs::set_permissions(p, fs::Permissions::from_mode(mode))?;
                }
            }
            Ok(())
        }
        finish(&root, &modes, &times)?;
        crate::storage::mark_received(&root, expected.uuid)?;
        // An immutable directory cannot be renamed on macOS. Publish inside the locked store,
        // then seal before returning success; remove the tree if sealing fails.
        let target = dir.join(expected.name);
        let mode = fs::symlink_metadata(&root)?.mode() & 0o7777;
        fs::set_permissions(&root, fs::Permissions::from_mode(mode | 0o200))?;
        crate::stream_fs::publish(&root, &target)?;
        let seal = (|| -> Result<()> {
            fs::set_permissions(&target, fs::Permissions::from_mode(mode))?;
            crate::storage::set_readonly(&target)
        })();
        if let Err(e) = seal {
            let _ = crate::storage::delete_tree(&target);
            return Err(e);
        }
        Ok(())
    })();
    let cleanup = crate::storage::delete_tree(&staging);
    result.and(cleanup)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    struct Sandbox(std::path::PathBuf);
    impl Sandbox {
        fn new() -> Self {
            let key=if cfg!(feature="overlayfs") {"LAYR_OVERLAYFS_TEST_ROOT"} else {"LAYR_APFS_TEST_ROOT"};
            let p = std::path::PathBuf::from(std::env::var_os(key).expect("backend test root"))
                .join(format!("stream-test-{}", crate::ids::new_id()));
            fs::create_dir(&p).unwrap();
            #[cfg(feature="overlayfs")]
            crate::storage::recover(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = crate::storage::delete_tree(&self.0);
        }
    }
    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn transport_preserves_content_and_rejects_damage() -> Result<()> {
        let s = Sandbox::new();
        let src = s.0.join("state");
        crate::storage::create_subvolume(&src)?;
        fs::create_dir(src.join("dir"))?;
        fs::write(src.join("dir/f"), b"content")?;
        fs::hard_link(src.join("dir/f"), src.join("hard"))?;
        std::os::unix::fs::symlink("dir/f", src.join("link"))?;
        fs::write(src.join("sparse"), vec![0; 200_000])?;
        crate::stream_fs::attribute(&src.join("dir/f"), c"user.layr.test", Some(b"metadata"))?;
        fs::set_permissions(src.join("dir/f"), fs::Permissions::from_mode(0o444))?;
        crate::storage::set_readonly(&src)?;
        let id = crate::storage::subvol_info(&src)?.uuid;
        let mut bytes = Vec::new();
        send(&src, &mut bytes)?;
        let dir = s.0.join("received");
        fs::create_dir(&dir)?;
        let e = crate::recv::Expect { name: "state", uuid: &id, parent: &|_| None, clones: &[] };
        receive(&mut &bytes[..], &dir, &e)?;
        let dst = dir.join("state");
        assert_eq!(fs::read(dst.join("dir/f"))?, b"content");
        assert_eq!(fs::metadata(dst.join("dir/f"))?.ino(), fs::metadata(dst.join("hard"))?.ino());
        assert_eq!(fs::read_link(dst.join("link"))?, Path::new("dir/f"));
        assert_eq!(fs::metadata(dst.join("dir/f"))?.mode() & 0o777, 0o444);
        assert_eq!(fs::read(dst.join("sparse"))?, vec![0; 200_000]);
        assert_eq!(crate::stream_fs::attribute(&dst.join("dir/f"), c"user.layr.test", None)?, b"metadata");
        assert!(fs::write(dst.join("dir/f"), b"bad").is_err());
        assert!(receive(&mut &bytes[..], &dir, &e).is_err());
        assert_eq!(fs::read(dst.join("dir/f"))?, b"content");
        let bad = s.0.join("bad");
        fs::create_dir(&bad)?;
        bytes[23] ^= 1;
        assert!(receive(&mut &bytes[..], &bad, &e).is_err());
        assert_eq!(fs::read_dir(&bad)?.count(), 0);
        Ok(())
    }
    #[test]
    #[cfg(target_os="macos")]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn transfer_refuses_acl_instead_of_losing_access_rules() -> Result<()> {
        let s = Sandbox::new();
        let root = s.0.join("state");
        crate::storage::create_subvolume(&root)?;
        fs::write(root.join("file"), b"private")?;
        let result =
            std::process::Command::new("/bin/chmod").args(["+a", "everyone deny read"]).arg(root.join("file")).status()?;
        assert!(result.success());
        assert!(crate::stream_fs::has_acl(&root.join("file"))?);
        crate::storage::set_readonly(&root)?;
        let error = send(&root, &mut Vec::new()).unwrap_err();
        assert!(error.to_string().contains("access control lists"));
        crate::storage::set_writable(&root)?;
        crate::stream_fs::clear_acl(&root.join("file"))?;
        assert!(!crate::stream_fs::has_acl(&root.join("file"))?);
        Ok(())
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn receive_uses_only_an_authorized_external_clone() -> Result<()> {
        let s = Sandbox::new();
        let source = s.0.join("source");
        crate::storage::create_subvolume(&source)?;
        fs::write(source.join("file"), b"source data")?;
        crate::storage::set_readonly(&source)?;
        let source_id = crate::storage::subvol_info(&source)?.uuid;
        let source_uuid = uuid::Uuid::parse_str(&source_id)?;
        let id = uuid::Uuid::now_v7();
        let text = id.to_string();
        let dir = s.0.join("received");
        fs::create_dir(&dir)?;
        let mut bytes = b"btrfs-stream\0\x01\0\0\0".to_vec();
        command(&mut bytes, 1, &[(15, b"state"), (1, id.as_bytes()), (2, &1u64.to_le_bytes())])?;
        command(&mut bytes, 3, &[(15, b"file")])?;
        command(
            &mut bytes,
            16,
            &[
                (15, b"file"),
                (18, &0u64.to_le_bytes()),
                (20, source_uuid.as_bytes()),
                (21, &7u64.to_le_bytes()),
                (22, b"file"),
                (23, &0u64.to_le_bytes()),
                (24, &11u64.to_le_bytes()),
            ],
        )?;
        command(&mut bytes, 21, &[])?;
        let e = crate::recv::Expect { name: "state", uuid: &text, parent: &|_| None, clones: &[] };
        assert!(receive(&mut &bytes[..], &dir, &e).is_err());
        assert_eq!(fs::read_dir(&dir)?.count(), 0);
        let clones = [(source_id, None, source)];
        let e = crate::recv::Expect { clones: &clones, ..e };
        receive(&mut &bytes[..], &dir, &e)?;
        assert_eq!(fs::read(dir.join("state/file"))?, b"source data");
        Ok(())
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn receive_cannot_write_through_a_symlink() -> Result<()> {
        let s = Sandbox::new();
        let dir = s.0.join("received");
        fs::create_dir(&dir)?;
        let outside = s.0.join("outside");
        fs::create_dir(&outside)?;
        fs::write(outside.join("keep"), b"keep")?;
        let id = uuid::Uuid::now_v7();
        let text = id.to_string();
        let e = crate::recv::Expect { name: "state", uuid: &text, parent: &|_| None, clones: &[] };
        for (code, attrs) in [
            (3, vec![(15, b"link/new".as_slice())]),
            (15, vec![(15, b"link/keep".as_slice()), (18, b"\0\0\0\0\0\0\0\0".as_slice()), (19, b"bad".as_slice())]),
        ] {
            let mut bytes = b"btrfs-stream\0\x01\0\0\0".to_vec();
            command(&mut bytes, 1, &[(15, b"state"), (1, id.as_bytes()), (2, &1u64.to_le_bytes())])?;
            command(&mut bytes, 8, &[(15, b"link"), (17, outside.as_os_str().as_bytes())])?;
            command(&mut bytes, code, &attrs)?;
            command(&mut bytes, 21, &[])?;
            assert!(receive(&mut &bytes[..], &dir, &e).is_err());
            assert_eq!(fs::read_dir(&dir)?.count(), 0);
            assert_eq!(fs::read(outside.join("keep"))?, b"keep");
            assert!(!outside.join("new").exists());
        }
        Ok(())
    }
}
