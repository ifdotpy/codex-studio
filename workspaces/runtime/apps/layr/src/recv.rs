//! Receiving btrfs streams from outside this machine (peers, backups) with checks.
//!
//! `btrfs receive` trusts its stream: a snapshot or clone command finds its source subvolume by
//! UUID anywhere on the file system, and a clone source path is not limited to that subvolume.
//! So every stream passes through `check` on its way to `btrfs receive`:
//! - one subvolume, with the expected name and UUID;
//! - a parent only from this project (a state whose signed record names that UUID), and clones
//!   only from the new subvolume or that parent;
//! - relative paths without `.` or `..` components;
//! - known commands of at most 16 MiB each, and nothing after the end command.
//!
//! After the receive, the new subvolume must be read-only, carry the expected received UUID and,
//! for an incremental stream, be a snapshot of the expected local parent.

use crate::btrfs;
use anyhow::{anyhow, bail, Result};
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const MAGIC: &[u8] = b"btrfs-stream\0";
const MAX_CMD: usize = 16 << 20;

const SUBVOL: u16 = 1;
const SNAPSHOT: u16 = 2;
const LINK: u16 = 10;
const CLONE: u16 = 16;
const END: u16 = 21;
/// The highest command number of send stream version 3 (enable verity).
const LAST_CMD: u16 = 26;

const A_UUID: u16 = 1;
const A_CTRANSID: u16 = 2;
const A_PATH: u16 = 15;
const A_PATH_TO: u16 = 16;
const A_PATH_LINK: u16 = 17;
const A_DATA: u16 = 19;
const A_CLONE_UUID: u16 = 20;
const A_CLONE_CTRANSID: u16 = 21;
const A_CLONE_PATH: u16 = 22;

/// What a stream must be.
pub struct Expect<'a> {
    /// The name of the new subvolume.
    pub name: &'a str,
    /// The UUID the signed record names for it (the received UUID after the receive).
    pub uuid: &'a str,
    /// The local subvolume of a state of this project, by the UUID its signed record names.
    pub parent: &'a dyn Fn(&str) -> Option<PathBuf>,
}

fn le16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn uuid_text(b: &[u8]) -> Result<String> {
    let a: [u8; 16] = b.try_into().map_err(|_| anyhow!("bad UUID in the stream"))?;
    Ok(uuid::Uuid::from_bytes(a).hyphenated().to_string())
}

/// A path inside the new subvolume: relative, no empty, `.` or `..` component. The empty path
/// is the subvolume root.
fn check_path(p: &[u8]) -> Result<()> {
    if p.is_empty() {
        return Ok(());
    }
    if p.split(|b| *b == b'/').any(|c| c.is_empty() || c == b"." || c == b"..") {
        bail!("stream path {:?} leaves the subvolume", String::from_utf8_lossy(p));
    }
    Ok(())
}

/// Attributes of one command. In stream version 2 and later the data attribute has no length
/// and takes the rest of the command.
fn attrs(body: &[u8], version: u32) -> Result<Vec<(u16, &[u8])>> {
    let mut v = Vec::new();
    let mut a = 0;
    while a < body.len() {
        if a + 2 > body.len() {
            bail!("truncated attribute");
        }
        let t = le16(&body[a..]);
        if t == A_DATA && version >= 2 {
            v.push((t, &body[a + 2..]));
            break;
        }
        if a + 4 > body.len() {
            bail!("truncated attribute");
        }
        let l = le16(&body[a + 2..]) as usize;
        let s = a + 4;
        if s + l > body.len() {
            bail!("truncated attribute");
        }
        v.push((t, &body[s..s + l]));
        a = s + l;
    }
    Ok(v)
}

fn get<'b>(attrs: &[(u16, &'b [u8])], t: u16) -> Option<&'b [u8]> {
    attrs.iter().find(|(k, _)| *k == t).map(|(_, v)| *v)
}

/// Check a stream while copying it to `out`. Returns the local parent of an incremental stream.
/// Stops at the first command that fails a check, before that command reaches `out`.
pub fn check(src: &mut dyn Read, out: &mut dyn Write, e: &Expect) -> Result<Option<PathBuf>> {
    let mut head = [0u8; 17];
    src.read_exact(&mut head)?;
    if &head[..MAGIC.len()] != MAGIC {
        bail!("not a btrfs send stream");
    }
    let version = le32(&head[MAGIC.len()..]);
    if !(1..=3).contains(&version) {
        bail!("unsupported send stream version {version}");
    }
    out.write_all(&head)?;
    // (uuid, ctransid) of the new subvolume and of its parent: the only clone sources.
    let mut own: Option<(Vec<u8>, Vec<u8>)> = None;
    let mut parent: Option<(Vec<u8>, Vec<u8>)> = None;
    let mut parent_path = None;
    let mut hdr = [0u8; 10];
    let mut body = Vec::new();
    loop {
        src.read_exact(&mut hdr)?;
        let len = le32(&hdr) as usize;
        let code = le16(&hdr[4..]);
        if len > MAX_CMD {
            bail!("stream command of {len} bytes");
        }
        if code == 0 || code > LAST_CMD {
            bail!("unknown stream command {code}");
        }
        body.resize(len, 0);
        src.read_exact(&mut body)?;
        let at = attrs(&body, version)?;
        for k in [A_PATH, A_PATH_TO, A_CLONE_PATH] {
            if let Some(p) = get(&at, k) {
                check_path(p)?;
            }
        }
        match code {
            SUBVOL | SNAPSHOT => {
                if own.is_some() {
                    bail!("the stream holds more than one subvolume");
                }
                if get(&at, A_PATH) != Some(e.name.as_bytes()) {
                    bail!("the stream is not {}", e.name);
                }
                let uuid = get(&at, A_UUID).ok_or_else(|| anyhow!("stream without a UUID"))?;
                if uuid_text(uuid)? != e.uuid {
                    bail!("the stream of {} is not the subvolume its record names", e.name);
                }
                own = Some((uuid.to_vec(), get(&at, A_CTRANSID).unwrap_or_default().to_vec()));
                if code == SNAPSHOT {
                    let pu = get(&at, A_CLONE_UUID).ok_or_else(|| anyhow!("snapshot without a parent"))?;
                    let pu_text = uuid_text(pu)?;
                    let local =
                        (e.parent)(&pu_text).ok_or_else(|| anyhow!("the parent of {} is not a state of this project", e.name))?;
                    parent = Some((pu.to_vec(), get(&at, A_CLONE_CTRANSID).unwrap_or_default().to_vec()));
                    parent_path = Some(local);
                }
            }
            _ if own.is_none() => bail!("the stream does not start with a subvolume"),
            LINK => check_path(get(&at, A_PATH_LINK).unwrap_or_default())?,
            CLONE => {
                let source = (
                    get(&at, A_CLONE_UUID).unwrap_or_default().to_vec(),
                    get(&at, A_CLONE_CTRANSID).unwrap_or_default().to_vec(),
                );
                if Some(&source) != own.as_ref() && Some(&source) != parent.as_ref() {
                    bail!("the stream clones from a subvolume outside this state and its parent");
                }
            }
            _ => {}
        }
        out.write_all(&hdr)?;
        out.write_all(&body)?;
        if code == END {
            break;
        }
    }
    let mut extra = [0u8; 1];
    if src.read(&mut extra)? != 0 {
        bail!("data after the end of the stream");
    }
    Ok(parent_path)
}

fn names(dir: &Path) -> Result<BTreeSet<String>> {
    Ok(std::fs::read_dir(dir)?.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
}

/// Receive one checked stream into `dir`. On any failure nothing new stays in `dir`.
pub fn receive(src: &mut dyn Read, dir: &Path, e: &Expect) -> Result<()> {
    let before = names(dir)?;
    // `-e`: stop at the end command.
    let mut child = Command::new("btrfs")
        .args(["receive", "-q", "-e"])
        .arg(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
    let checked = check(src, &mut stdin, e);
    drop(stdin);
    let out = child.wait_with_output()?;
    let new: Vec<String> = names(dir)?.difference(&before).cloned().collect();
    let res = (|| -> Result<()> {
        let parent = checked?;
        if !out.status.success() {
            bail!("btrfs receive failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        if new.len() != 1 || new[0] != e.name {
            bail!("the stream did not make exactly {} (got {:?})", e.name, new);
        }
        let info = btrfs::subvol_info(&dir.join(e.name))?;
        if !info.readonly || info.received_uuid.as_deref() != Some(e.uuid) {
            bail!("incomplete receive of {}", e.name);
        }
        // `btrfs receive` found the parent by UUID: it must be the local state we checked.
        let expected_parent = match &parent {
            Some(p) => Some(btrfs::subvol_info(p)?.uuid),
            None => None,
        };
        if info.parent_uuid != expected_parent {
            bail!("{} was received onto another subvolume than its parent state", e.name);
        }
        Ok(())
    })();
    if res.is_err() {
        for n in &new {
            let _ = btrfs::delete_tree(&dir.join(n));
        }
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(code: u16, attrs: &[(u16, &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        for (t, v) in attrs {
            body.extend_from_slice(&t.to_le_bytes());
            body.extend_from_slice(&(v.len() as u16).to_le_bytes());
            body.extend_from_slice(v);
        }
        let mut c = (body.len() as u32).to_le_bytes().to_vec();
        c.extend_from_slice(&code.to_le_bytes());
        c.extend_from_slice(&[0, 0, 0, 0]);
        c.extend_from_slice(&body);
        c
    }

    fn stream(cmds: &[Vec<u8>]) -> Vec<u8> {
        let mut s = MAGIC.to_vec();
        s.extend_from_slice(&1u32.to_le_bytes());
        for c in cmds {
            s.extend_from_slice(c);
        }
        s
    }

    const U: [u8; 16] = [1; 16];
    const P: [u8; 16] = [2; 16];
    const X: [u8; 16] = [3; 16];

    fn run(s: &[u8]) -> Result<Option<PathBuf>> {
        let uuid = uuid_text(&U).unwrap();
        let p_text = uuid_text(&P).unwrap();
        let parent = move |u: &str| if u == p_text { Some(PathBuf::from("/parent")) } else { None };
        let e = Expect { name: "s1", uuid: &uuid, parent: &parent };
        check(&mut &s[..], &mut Vec::new(), &e)
    }

    #[test]
    fn checks_streams() {
        let t = 7u64.to_le_bytes();
        let snap = cmd(SNAPSHOT, &[(A_PATH, b"s1"), (A_UUID, &U), (A_CTRANSID, &t), (A_CLONE_UUID, &P), (A_CLONE_CTRANSID, &t)]);
        let end = cmd(END, &[]);
        let ok = stream(&[snap.clone(), cmd(3, &[(A_PATH, b"a/b")]), end.clone()]);
        assert_eq!(run(&ok).unwrap(), Some(PathBuf::from("/parent")));
        let foreign = cmd(SNAPSHOT, &[(A_PATH, b"s1"), (A_UUID, &U), (A_CLONE_UUID, &X)]);
        assert!(run(&stream(&[foreign, end.clone()])).is_err(), "a parent outside the project");
        let up = cmd(3, &[(A_PATH, b"../escape")]);
        assert!(run(&stream(&[snap.clone(), up, end.clone()])).is_err(), "a path with ..");
        let clone = cmd(CLONE, &[(A_PATH, b"f"), (A_CLONE_UUID, &X), (A_CLONE_CTRANSID, &t), (A_CLONE_PATH, b"g")]);
        assert!(run(&stream(&[snap.clone(), clone, end.clone()])).is_err(), "a clone from another subvolume");
        let clone_path = cmd(CLONE, &[(A_PATH, b"f"), (A_CLONE_UUID, &P), (A_CLONE_CTRANSID, &t), (A_CLONE_PATH, b"../../x")]);
        assert!(run(&stream(&[snap.clone(), clone_path, end.clone()])).is_err(), "a clone path with ..");
        let other_name = cmd(SUBVOL, &[(A_PATH, b"s2"), (A_UUID, &U)]);
        assert!(run(&stream(&[other_name, end.clone()])).is_err(), "another name");
        let mut trailing = ok.clone();
        trailing.push(0);
        assert!(run(&trailing).is_err(), "data after the end");
        let two = stream(&[snap.clone(), cmd(SUBVOL, &[(A_PATH, b"s1"), (A_UUID, &U)]), end]);
        assert!(run(&two).is_err(), "two subvolumes");
    }
}
