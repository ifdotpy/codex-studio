//! Parser for `btrfs send --no-data` streams, and a namespace model that turns the command
//! sequence into entries with their path in the parent and in the child.
//!
//! The stream refers to paths in the receiver's tree as it is while the stream is applied, so a
//! path can be a temporary orphan name (`o<ino>-<gen>-<seq>`) or a path below a folder that was
//! renamed earlier in the stream. The model replays renames on a tree of touched entries; an
//! entry that the stream did not create has an origin path in the parent snapshot.

use anyhow::{bail, Result};
use std::collections::HashMap;

const MAGIC: &[u8] = b"btrfs-stream\0";

mod cmd {
    pub const MKFILE: u16 = 3;
    pub const MKDIR: u16 = 4;
    pub const MKNOD: u16 = 5;
    pub const MKFIFO: u16 = 6;
    pub const MKSOCK: u16 = 7;
    pub const SYMLINK: u16 = 8;
    pub const RENAME: u16 = 9;
    pub const LINK: u16 = 10;
    pub const UNLINK: u16 = 11;
    pub const RMDIR: u16 = 12;
    pub const WRITE: u16 = 15;
    pub const CLONE: u16 = 16;
    pub const TRUNCATE: u16 = 17;
    pub const CHMOD: u16 = 18;
    pub const UPDATE_EXTENT: u16 = 22;
    pub const FALLOCATE: u16 = 23;
    pub const ENCODED_WRITE: u16 = 25;
}

mod attr {
    pub const SIZE: u16 = 4;
    pub const MODE: u16 = 5;
    pub const PATH: u16 = 15;
    pub const PATH_TO: u16 = 16;
    pub const PATH_LINK: u16 = 17;
    pub const FILE_OFFSET: u16 = 18;
    pub const CLONE_LEN: u16 = 24;
}

#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    Create { path: String, kind: Kind },
    Symlink { path: String, target: String },
    Rename { from: String, to: String },
    Link { path: String, target: String },
    Unlink { path: String },
    Rmdir { path: String },
    Extent { path: String, offset: u64, len: u64 },
    Truncate { path: String, size: u64 },
    Chmod { path: String, mode: u32 },
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Special,
}

fn le16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}
fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
fn le64(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    u64::from_le_bytes(a)
}

/// Parse a send stream into commands. Unknown commands become `Cmd::Other`.
pub fn parse(data: &[u8]) -> Result<Vec<Cmd>> {
    if data.len() < MAGIC.len() + 4 || &data[..MAGIC.len()] != MAGIC {
        bail!("not a btrfs send stream");
    }
    let mut pos = MAGIC.len() + 4;
    let mut out = Vec::new();
    while pos + 10 <= data.len() {
        let len = le32(&data[pos..]) as usize;
        let code = le16(&data[pos + 4..]);
        let body_start = pos + 10;
        let body_end = body_start + len;
        if body_end > data.len() {
            bail!("truncated send stream");
        }
        let body = &data[body_start..body_end];
        pos = body_end;
        let mut attrs: HashMap<u16, &[u8]> = HashMap::new();
        let mut a = 0;
        while a + 4 <= body.len() {
            let t = le16(&body[a..]);
            let l = le16(&body[a + 2..]) as usize;
            let s = a + 4;
            if s + l > body.len() {
                break;
            }
            attrs.insert(t, &body[s..s + l]);
            a = s + l;
        }
        let path = |k: u16| -> Result<String> {
            match attrs.get(&k) {
                Some(v) => Ok(String::from_utf8_lossy(v).into_owned()),
                None => bail!("send command {code} without attribute {k}"),
            }
        };
        let num = |k: u16| -> u64 { attrs.get(&k).map(|v| if v.len() >= 8 { le64(v) } else { 0 }).unwrap_or(0) };
        let c = match code {
            cmd::MKFILE => Cmd::Create { path: path(attr::PATH)?, kind: Kind::File },
            cmd::MKDIR => Cmd::Create { path: path(attr::PATH)?, kind: Kind::Dir },
            cmd::MKNOD | cmd::MKFIFO | cmd::MKSOCK => Cmd::Create { path: path(attr::PATH)?, kind: Kind::Special },
            cmd::SYMLINK => Cmd::Symlink { path: path(attr::PATH)?, target: path(attr::PATH_LINK)? },
            cmd::RENAME => Cmd::Rename { from: path(attr::PATH)?, to: path(attr::PATH_TO)? },
            cmd::LINK => Cmd::Link { path: path(attr::PATH)?, target: path(attr::PATH_LINK)? },
            cmd::UNLINK => Cmd::Unlink { path: path(attr::PATH)? },
            cmd::RMDIR => Cmd::Rmdir { path: path(attr::PATH)? },
            cmd::UPDATE_EXTENT => Cmd::Extent { path: path(attr::PATH)?, offset: num(attr::FILE_OFFSET), len: num(attr::SIZE) },
            cmd::CLONE => Cmd::Extent { path: path(attr::PATH)?, offset: num(attr::FILE_OFFSET), len: num(attr::CLONE_LEN) },
            cmd::WRITE | cmd::ENCODED_WRITE | cmd::FALLOCATE => {
                Cmd::Extent { path: path(attr::PATH)?, offset: num(attr::FILE_OFFSET), len: u64::MAX }
            }
            cmd::TRUNCATE => Cmd::Truncate { path: path(attr::PATH)?, size: num(attr::SIZE) },
            cmd::CHMOD => Cmd::Chmod { path: path(attr::PATH)?, mode: num(attr::MODE) as u32 },
            _ => Cmd::Other,
        };
        out.push(c);
    }
    Ok(out)
}

/// One entry touched by the stream.
#[derive(Debug, Clone, Default)]
pub struct Entry {
    /// Path in the parent snapshot, if the entry existed there.
    pub origin: Option<String>,
    /// Path in the child snapshot, if the entry exists there.
    pub path: Option<String>,
    /// Set when the stream created the entry (a new inode or a new name).
    pub created: Option<Kind>,
    /// For a new name made by `link`: the origin path of the inode it links to.
    pub linked_from: Option<String>,
    pub content: bool,
    pub mode: Option<u32>,
    pub ranges: Vec<(u64, u64)>,
    pub truncated: Option<u64>,
}

#[derive(Default)]
struct Node {
    entry: Entry,
    /// Children known to the model. `None` marks a name known to be absent.
    children: HashMap<String, Option<usize>>,
    touched: bool,
}

struct Model {
    nodes: Vec<Node>,
}

impl Model {
    fn new() -> Self {
        let mut root = Node::default();
        root.entry.origin = Some(String::new());
        Model { nodes: vec![root] }
    }

    fn join(base: &str, name: &str) -> String {
        if base.is_empty() {
            name.to_string()
        } else {
            format!("{base}/{name}")
        }
    }

    /// Find the node for `path`, creating implicit nodes for entries of the parent snapshot.
    fn lookup(&mut self, path: &str) -> usize {
        let mut cur = 0;
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            let next = match self.nodes[cur].children.get(comp) {
                Some(Some(n)) => *n,
                _ => {
                    let origin = if self.nodes[cur].entry.created.is_some() {
                        None
                    } else {
                        self.nodes[cur].entry.origin.as_ref().map(|o| Self::join(o, comp))
                    };
                    let id = self.nodes.len();
                    let mut n = Node::default();
                    n.entry.origin = origin;
                    self.nodes.push(n);
                    self.nodes[cur].children.insert(comp.to_string(), Some(id));
                    id
                }
            };
            cur = next;
        }
        cur
    }

    fn split(path: &str) -> (&str, &str) {
        match path.rfind('/') {
            Some(i) => (&path[..i], &path[i + 1..]),
            None => ("", path),
        }
    }

    fn detach(&mut self, path: &str) -> usize {
        let id = self.lookup(path);
        let (parent, name) = Self::split(path);
        let p = self.lookup(parent);
        self.nodes[p].children.insert(name.to_string(), None);
        id
    }

    fn attach(&mut self, path: &str, id: usize) {
        let (parent, name) = Self::split(path);
        let p = self.lookup(parent);
        if let Some(Some(old)) = self.nodes[p].children.get(name).cloned() {
            if old != id {
                self.nodes[old].touched = true;
            }
        }
        self.nodes[p].children.insert(name.to_string(), Some(id));
    }

    fn create(&mut self, path: &str, kind: Kind) -> usize {
        let id = self.nodes.len();
        let mut n = Node::default();
        n.entry.created = Some(kind);
        n.touched = true;
        self.nodes.push(n);
        self.attach(path, id);
        id
    }

    fn apply(&mut self, c: &Cmd) {
        match c {
            Cmd::Create { path, kind } => {
                self.create(path, *kind);
            }
            Cmd::Symlink { path, .. } => {
                self.create(path, Kind::Symlink);
            }
            Cmd::Rename { from, to } => {
                let id = self.detach(from);
                self.nodes[id].touched = true;
                self.attach(to, id);
            }
            Cmd::Link { path, target } => {
                let t = self.lookup(target);
                let linked = self.nodes[t].entry.origin.clone();
                let id = self.create(path, Kind::File);
                self.nodes[id].entry.linked_from = linked;
            }
            Cmd::Unlink { path } | Cmd::Rmdir { path } => {
                let id = self.detach(path);
                self.nodes[id].touched = true;
            }
            Cmd::Extent { path, offset, len } => {
                let id = self.lookup(path);
                let n = &mut self.nodes[id];
                n.touched = true;
                n.entry.content = true;
                n.entry.ranges.push((*offset, *len));
            }
            Cmd::Truncate { path, size } => {
                let id = self.lookup(path);
                let n = &mut self.nodes[id];
                n.touched = true;
                n.entry.content = true;
                n.entry.truncated = Some(*size);
            }
            Cmd::Chmod { path, mode } => {
                let id = self.lookup(path);
                let n = &mut self.nodes[id];
                n.touched = true;
                n.entry.mode = Some(*mode);
            }
            Cmd::Other => {}
        }
    }

    fn finish(mut self) -> Vec<Entry> {
        // Final paths: walk the tree from the root.
        let mut stack = vec![(0usize, String::new())];
        while let Some((id, path)) = stack.pop() {
            self.nodes[id].entry.path = Some(path.clone());
            let kids: Vec<(String, usize)> =
                self.nodes[id].children.iter().filter_map(|(k, v)| v.map(|v| (k.clone(), v))).collect();
            for (name, kid) in kids {
                stack.push((kid, Self::join(&path, &name)));
            }
        }
        self.nodes
            .into_iter()
            .skip(1)
            .filter(|n| n.touched || n.entry.origin.as_deref() != n.entry.path.as_deref())
            .map(|n| n.entry)
            .filter(|e| e.origin.is_some() || e.path.is_some())
            .collect()
    }
}

/// Turn a parsed stream into touched entries.
pub fn entries(cmds: &[Cmd]) -> Vec<Entry> {
    let mut m = Model::new();
    for c in cmds {
        m.apply(c);
    }
    m.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: &str) -> String {
        x.to_string()
    }

    #[test]
    fn rename_of_folder_maps_children() {
        let cmds = vec![Cmd::Rename { from: s("a"), to: s("b") }, Cmd::Extent { path: s("b/x.txt"), offset: 0, len: 10 }];
        let e = entries(&cmds);
        let x = e.iter().find(|e| e.path.as_deref() == Some("b/x.txt")).unwrap();
        assert_eq!(x.origin.as_deref(), Some("a/x.txt"));
        let d = e.iter().find(|e| e.path.as_deref() == Some("b")).unwrap();
        assert_eq!(d.origin.as_deref(), Some("a"));
    }

    #[test]
    fn orphan_delete_and_new_file() {
        let cmds = vec![
            Cmd::Rename { from: s("d"), to: s("o259-7-0") },
            Cmd::Unlink { path: s("o259-7-0/f") },
            Cmd::Rmdir { path: s("o259-7-0") },
            Cmd::Create { path: s("o260-8-0"), kind: Kind::File },
            Cmd::Rename { from: s("o260-8-0"), to: s("new.txt") },
        ];
        let e = entries(&cmds);
        assert!(e.iter().any(|e| e.origin.as_deref() == Some("d/f") && e.path.is_none()));
        assert!(e.iter().any(|e| e.origin.as_deref() == Some("d") && e.path.is_none()));
        assert!(e.iter().any(|e| e.origin.is_none() && e.path.as_deref() == Some("new.txt")));
    }

    #[test]
    fn link_then_unlink_is_a_rename() {
        let cmds = vec![Cmd::Link { path: s("new"), target: s("old") }, Cmd::Unlink { path: s("old") }];
        let e = entries(&cmds);
        let n = e.iter().find(|e| e.path.as_deref() == Some("new")).unwrap();
        assert_eq!(n.linked_from.as_deref(), Some("old"));
        assert!(e.iter().any(|e| e.origin.as_deref() == Some("old") && e.path.is_none()));
    }
}
