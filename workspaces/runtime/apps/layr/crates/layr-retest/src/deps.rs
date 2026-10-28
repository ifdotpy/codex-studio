//! Dependency rules: a test is stale when a package that it executed code of has gone from
//! `Cargo.lock`, or a `Cargo.toml` or `build.rs` changed in a folder whose files it executed.

use crate::parse::FileChange;
use crate::record::TestRec;

/// Name of the package that owns an executed file outside the project: `<name>-<version>` for the
/// cargo registry, `git:<checkout folder>/<short rev>` for a git checkout.
pub fn package_of(path: &str) -> Option<String> {
    let parts = |marker: &str, n: usize| -> Option<Vec<&str>> {
        let rest = path.split_once(marker)?.1;
        let v: Vec<&str> = rest.split('/').take(n).collect();
        (v.len() == n).then_some(v)
    };
    if let Some(v) = parts("/registry/src/", 2) {
        return Some(v[1].to_string());
    }
    parts("/git/checkouts/", 2).map(|v| format!("git:{}/{}", v[0], v[1]))
}

#[derive(Debug, PartialEq, Clone)]
pub struct Pkg {
    pub name: String,
    pub version: String,
    pub source: String,
}

/// The `[[package]]` entries of a `Cargo.lock`. Packages of the project have no source.
pub fn parse_lock(text: &str) -> Vec<Pkg> {
    let mut out: Vec<Pkg> = Vec::new();
    for l in text.lines() {
        if l == "[[package]]" {
            out.push(Pkg { name: String::new(), version: String::new(), source: String::new() });
        } else if let (Some(p), Some((k, v))) = (out.last_mut(), l.split_once(" = ")) {
            let v = v.trim_matches('"').to_string();
            match k {
                "name" => p.name = v,
                "version" => p.version = v,
                "source" => p.source = v,
                _ => {}
            }
        }
    }
    out
}

/// What changed outside source files, between the baseline and the current state.
#[derive(Default, Debug)]
pub struct Ext {
    /// Folders ("" is the root) where a `Cargo.toml` or `build.rs` changed.
    pub dirs: Vec<String>,
    /// Packages of the old `Cargo.lock` that the new one does not have.
    pub removed: Vec<Pkg>,
}

impl Ext {
    pub fn new(changes: &[FileChange], old_lock: &str, new_lock: &str) -> Ext {
        let dirs = changes
            .iter()
            .filter_map(|c| {
                let (dir, file) = c.old_path.rsplit_once('/').unwrap_or(("", &c.old_path));
                matches!(file, "Cargo.toml" | "build.rs").then(|| dir.to_string())
            })
            .collect();
        let new = parse_lock(new_lock);
        let removed = parse_lock(old_lock).into_iter().filter(|p| !p.source.is_empty() && !new.contains(p)).collect();
        Ext { dirs, removed }
    }

    pub fn lock_changed(changes: &[FileChange]) -> bool {
        changes.iter().any(|c| c.old_path == "Cargo.lock")
    }

    pub fn hits(&self, rec: &TestRec) -> bool {
        let removed = |dep: &String| {
            self.removed.iter().any(|p| match dep.strip_prefix("git:") {
                // `git:<folder>/<short rev>`; the lock source ends with `#<full rev>`.
                Some(g) => {
                    p.source.starts_with("git+") && g.rsplit('/').next().is_some_and(|rev| p.source.contains(&format!("#{rev}")))
                }
                None => !p.source.starts_with("git+") && *dep == format!("{}-{}", p.name, p.version),
            })
        };
        rec.deps.iter().any(removed)
            || self.dirs.iter().any(|d| {
                let prefix = format!("{d}/");
                rec.files.keys().any(|f| d.is_empty() || f.starts_with(&prefix))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_hunks;
    use std::collections::BTreeMap;

    const OLD: &str = "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"serde\",\n]\n\n[[package]]\nname = \"serde\"\nversion = \"1.0.1\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"x\"\n\n[[package]]\nname = \"gitdep\"\nversion = \"0.2.0\"\nsource = \"git+https://example.org/g.git#abcdef0123456789\"\n\n[[package]]\nname = \"keep\"\nversion = \"2.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n";
    const NEW: &str = "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"serde\"\nversion = \"1.0.2\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n\n[[package]]\nname = \"gitdep\"\nversion = \"0.2.0\"\nsource = \"git+https://example.org/g.git#fedcba9876543210\"\n\n[[package]]\nname = \"keep\"\nversion = \"2.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n";

    fn rec(deps: &[&str], files: &[&str]) -> TestRec {
        TestRec {
            status: "pass".into(),
            files: files.iter().map(|f| (f.to_string(), Default::default())).collect::<BTreeMap<_, _>>(),
            deps: deps.iter().map(|d| d.to_string()).collect(),
        }
    }

    fn change(path: &str) -> Vec<FileChange> {
        parse_hunks(&format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-a\n+b\n"))
    }

    #[test]
    fn package_of_registry_and_git_paths() {
        let r = "/Users/u/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/serde-1.0.1/src/lib.rs";
        assert_eq!(package_of(r).as_deref(), Some("serde-1.0.1"));
        let g = "/Users/u/.cargo/git/checkouts/g-1a2b3c/abcdef0/src/lib.rs";
        assert_eq!(package_of(g).as_deref(), Some("git:g-1a2b3c/abcdef0"));
        assert_eq!(package_of("/work/line/src/lib.rs"), None);
    }

    #[test]
    fn lock_parse_and_removed_packages() {
        assert_eq!(parse_lock(OLD).len(), 4);
        let ext = Ext::new(&change("Cargo.lock"), OLD, NEW);
        let names: Vec<_> = ext.removed.iter().map(|p| format!("{}-{}", p.name, p.version)).collect();
        assert_eq!(names, vec!["serde-1.0.1", "gitdep-0.2.0"]);
    }

    #[test]
    fn lock_change_marks_only_tests_that_used_a_removed_package() {
        let ext = Ext::new(&change("Cargo.lock"), OLD, NEW);
        assert!(ext.hits(&rec(&["serde-1.0.1"], &[])));
        assert!(ext.hits(&rec(&["git:g-1a2b3c/abcdef0"], &[])));
        assert!(!ext.hits(&rec(&["keep-2.0.0"], &["src/lib.rs"])));
        assert!(!ext.hits(&rec(&["serde-1.0.2"], &[])));
        assert!(!ext.hits(&rec(&[], &["src/lib.rs"])));
    }

    #[test]
    fn manifest_change_marks_tests_that_executed_files_under_the_folder() {
        let ext = Ext::new(&change("crates/a/Cargo.toml"), "", "");
        assert!(ext.hits(&rec(&[], &["crates/a/src/lib.rs"])));
        assert!(!ext.hits(&rec(&[], &["crates/b/src/lib.rs", "src/main.rs"])));
        let ext = Ext::new(&change("crates/a/build.rs"), "", "");
        assert!(ext.hits(&rec(&[], &["crates/a/src/x.rs"])));
        let root = Ext::new(&change("Cargo.toml"), "", "");
        assert!(root.hits(&rec(&[], &["crates/b/src/lib.rs"])));
        assert!(!root.hits(&rec(&["serde-1.0.1"], &[])));
    }
}
