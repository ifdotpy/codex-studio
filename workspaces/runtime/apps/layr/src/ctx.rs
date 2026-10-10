//! The caller of a command: identity, environment, working folder and output.

use crate::model::{Actor, Author};
use crate::store::Store;
use anyhow::{bail, Result};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Caller {
    pub uid: u32,
    pub gid: u32,
    pub user: String,
    pub home: PathBuf,
}

impl Caller {
    pub fn from_uid(uid: u32) -> Caller {
        match crate::sys::user_by_uid(uid) {
            Some(u) => Caller { uid, gid: u.gid, user: u.name, home: PathBuf::from(u.home) },
            None => Caller { uid, gid: uid, user: format!("uid{uid}"), home: PathBuf::from("/") },
        }
    }
    pub fn is_root(&self) -> bool {
        self.uid == 0
    }
}

pub struct Ctx {
    pub store: Store,
    pub caller: Caller,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    pub out: std::cell::RefCell<Box<dyn Write>>,
}

/// Where the command runs: a project, a line and the folder below the line root.
#[derive(Debug, Clone)]
pub struct Loc {
    pub project: String,
    pub line: Option<String>,
    /// Working folder relative to the line root ("" at the root).
    pub sub: String,
}

impl Ctx {
    pub fn env(&self, k: &str) -> Option<&str> {
        self.env.get(k).map(|s| s.as_str()).filter(|s| !s.is_empty())
    }

    pub fn actor(&self) -> Actor {
        Actor { uid: self.caller.uid, name: self.caller.user.clone(), agent: self.env("LAYR_AGENT").map(|s| s.to_string()) }
    }

    fn git_config(&self, key: &str) -> Option<String> {
        let f = self.caller.home.join(".gitconfig");
        let mut command = std::process::Command::new("git");
        command.args(["config", "--file"]).arg(&f).arg(key);
        crate::privs::command_as(&mut command, self.caller.uid, &[]).ok()?;
        let out = command.output().ok()?;
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }

    pub fn author(&self) -> Author {
        let name = self
            .env("LAYR_AUTHOR_NAME")
            .or(self.env("GIT_AUTHOR_NAME"))
            .map(|s| s.to_string())
            .or_else(|| self.git_config("user.name"))
            .unwrap_or_else(|| self.caller.user.clone());
        let email = self
            .env("LAYR_AUTHOR_EMAIL")
            .or(self.env("GIT_AUTHOR_EMAIL"))
            .map(|s| s.to_string())
            .or_else(|| self.git_config("user.email"))
            .unwrap_or_else(|| format!("{}@{}", self.caller.user, self.store.machine.name));
        Author { name, email, uid: self.caller.uid, agent: self.env("LAYR_AGENT").map(|s| s.to_string()) }
    }

    pub fn turn(&self) -> Option<String> {
        self.env("LAYR_TURN").map(|s| s.to_string())
    }

    /// Find the project and line from `LAYR_PROJECT`/`LAYR_LINE` or from the working folder.
    pub fn locate(&self) -> Result<Loc> {
        if let Some(p) = self.env("LAYR_PROJECT") {
            let line = self.env("LAYR_LINE").map(|s| s.to_string());
            let sub = self.sub_in_line(p, line.as_deref()).unwrap_or_default();
            return Ok(Loc { project: p.to_string(), line, sub });
        }
        match self.loc_from_path(&self.cwd) {
            Some(l) => Ok(l),
            None => bail!("not in a layr line (cwd {}); cd into a line or set LAYR_PROJECT", self.cwd.display()),
        }
    }

    pub fn loc_from_path(&self, path: &Path) -> Option<Loc> {
        let projects = self.store.projects_dir();
        let projects = projects.canonicalize().unwrap_or(projects);
        let rel = path.strip_prefix(&projects).ok()?;
        let comps: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
        if comps.is_empty() {
            return None;
        }
        let project = comps[0].clone();
        if comps.len() >= 3 && comps[1] == "lines" {
            return Some(Loc { project, line: Some(comps[2].clone()), sub: comps[3..].join("/") });
        }
        Some(Loc { project, line: None, sub: String::new() })
    }

    fn sub_in_line(&self, project: &str, line: Option<&str>) -> Option<String> {
        let l = self.loc_from_path(&self.cwd)?;
        if l.project == project && l.line.as_deref() == line {
            Some(l.sub)
        } else {
            None
        }
    }

    pub fn print(&self, s: &str) {
        let _ = self.out.borrow_mut().write_all(s.as_bytes());
    }
}

#[macro_export]
macro_rules! outln {
    ($ctx:expr) => { $ctx.print("\n") };
    ($ctx:expr, $($t:tt)*) => { $ctx.print(&format!("{}\n", format!($($t)*))) };
}

#[macro_export]
macro_rules! out {
    ($ctx:expr, $($t:tt)*) => { $ctx.print(&format!($($t)*)) };
}
