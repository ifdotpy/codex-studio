//! `.gitignore` rules, read from one snapshot root. A path below an ignored folder is ignored.

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct Rules {
    root: PathBuf,
    cache: RefCell<HashMap<String, Option<Gitignore>>>,
}

impl Rules {
    pub fn new(root: &Path) -> Rules {
        Rules { root: root.to_path_buf(), cache: RefCell::new(HashMap::new()) }
    }

    fn matcher(&self, dir: &str) -> Option<Gitignore> {
        if let Some(m) = self.cache.borrow().get(dir) {
            return m.clone();
        }
        let base = if dir.is_empty() { self.root.clone() } else { self.root.join(dir) };
        let mut b = GitignoreBuilder::new(&base);
        let mut any = false;
        for name in [".gitignore", ".layrignore"] {
            let rel = if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") };
            if crate::fsutil::lmeta(&self.root, &rel).map(|m| m.is_file()).unwrap_or(false) {
                b.add(base.join(name));
                any = true;
            }
        }
        let m = if any { b.build().ok() } else { None };
        self.cache.borrow_mut().insert(dir.to_string(), m.clone());
        m
    }

    /// Ignore state of one entry, checking only the rules in its parent folders.
    fn direct(&self, path: &str, is_dir: bool) -> bool {
        let comps: Vec<&str> = path.split('/').collect();
        // Deeper files take precedence.
        for depth in (0..comps.len()).rev() {
            let dir = comps[..depth].join("/");
            if let Some(m) = self.matcher(&dir) {
                let rel = comps[depth..].join("/");
                let full = if dir.is_empty() { self.root.join(&rel) } else { self.root.join(&dir).join(&rel) };
                let r = m.matched(&full, is_dir);
                if r.is_ignore() {
                    return true;
                }
                if r.is_whitelist() {
                    return false;
                }
            }
        }
        false
    }

    pub fn ignored(&self, path: &str, is_dir: bool) -> bool {
        if path == ".git" || path.starts_with(".git/") {
            return true;
        }
        let comps: Vec<&str> = path.split('/').collect();
        for i in 1..comps.len() {
            if self.direct(&comps[..i].join("/"), true) {
                return true;
            }
        }
        self.direct(path, is_dir)
    }
}
