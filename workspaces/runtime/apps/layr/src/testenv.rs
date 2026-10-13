//! Tests that run layr commands on a real btrfs file system (a loop file) as the service does:
//! in one root process, with the umask of its service unit (0077), for callers of other users.
//! They need root on Linux, btrfs-progs and git (`sudo cargo test`, as for `tests/run.sh`).
//! Elsewhere `TestFs::new` gives `None` and the test returns without running.

use crate::ctx::{Caller, Ctx};
use crate::store::Store;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::{Mutex, MutexGuard};

/// One such test at a time: the umask is process wide.
static SERIAL: Mutex<()> = Mutex::new(());

pub struct TestFs {
    pub dir: PathBuf,
    pub root: PathBuf,
    mount: PathBuf,
    umask: rustix::fs::Mode,
    _serial: MutexGuard<'static, ()>,
}

/// Run a program; true if it exits with 0.
pub fn sh(argv: &[&str]) -> bool {
    Command::new(argv[0]).args(&argv[1..]).status().map(|s| s.success()).unwrap_or(false)
}

struct Out(Rc<RefCell<Vec<u8>>>);

impl Write for Out {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl TestFs {
    /// A new store on its own btrfs below /var/tmp, or None without root and btrfs-progs.
    pub fn new(name: &str) -> Option<TestFs> {
        let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        if !rustix::process::geteuid().is_root() || !sh(&["sh", "-c", "command -v mkfs.btrfs >/dev/null"]) {
            eprintln!("skipped: the test needs root and btrfs-progs");
            return None;
        }
        let dir = PathBuf::from(format!("/var/tmp/layr-unit-{name}-{}", std::process::id()));
        let (img, mount) = (dir.join("fs.img"), dir.join("fs"));
        std::fs::create_dir_all(&mount).unwrap();
        for d in [&dir, &mount] {
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let (i, m) = (img.to_str().unwrap(), mount.to_str().unwrap());
        assert!(sh(&["truncate", "-s", "2G", i]) && sh(&["mkfs.btrfs", "-q", "-f", i]));
        assert!(sh(&["mount", "-o", "loop,user_subvol_rm_allowed", i, m]));
        std::fs::set_permissions(&mount, std::fs::Permissions::from_mode(0o755)).unwrap();
        let umask = rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o077));
        Some(TestFs { root: mount.join("store"), dir, mount, umask, _serial: serial })
    }

    /// A Linux user for the test (made once), and its uid.
    pub fn user(&self, name: &str) -> u32 {
        if crate::sys::user_by_name(name).is_none() {
            assert!(sh(&["useradd", "-m", name]), "useradd {name}");
        }
        crate::sys::user_by_name(name).unwrap().uid
    }

    /// The context of a command of `uid` with the given environment.
    pub fn ctx(&self, uid: u32, env: &[(&str, &str)], out: Box<dyn Write>) -> Ctx {
        let mut vars: HashMap<String, String> = HashMap::new();
        vars.insert("LAYR_AUTHOR_NAME".into(), "Tester".into());
        vars.insert("LAYR_AUTHOR_EMAIL".into(), "tester@example.com".into());
        vars.insert("PATH".into(), "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into());
        for (k, v) in env {
            vars.insert(k.to_string(), v.to_string());
        }
        Ctx {
            store: Store::open(&self.root).unwrap(),
            caller: Caller::from_uid(uid),
            cwd: PathBuf::from("/"),
            env: vars,
            out: RefCell::new(out),
        }
    }

    /// Run a layr command as the service runs it for `uid`: its exit code and its output (the
    /// error message after the output, if any).
    pub fn run(&self, uid: u32, env: &[(&str, &str)], argv: &[&str]) -> (i32, String) {
        let buf = Rc::new(RefCell::new(Vec::new()));
        let ctx = self.ctx(uid, env, Box::new(Out(buf.clone())));
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let code = match crate::cmd::run(&ctx, &argv) {
            Ok(c) => c,
            Err(e) => {
                buf.borrow_mut().extend_from_slice(format!("{e:#}\n").as_bytes());
                e.downcast_ref::<crate::cmd::Exit>().map(|x| x.0).unwrap_or(128)
            }
        };
        drop(ctx);
        let out = String::from_utf8_lossy(&buf.borrow()).into_owned();
        (code, out)
    }

    /// `run` that must succeed; its output.
    pub fn ok(&self, uid: u32, env: &[(&str, &str)], argv: &[&str]) -> String {
        let (code, out) = self.run(uid, env, argv);
        assert_eq!(code, 0, "layr {argv:?} failed: {out}");
        out
    }

    /// The working folder of a line.
    pub fn line(&self, project: &str, line: &str) -> PathBuf {
        self.root.join("projects").join(project).join("lines").join(line)
    }

    /// Write a file below `root` (with its folders).
    pub fn write(&self, root: &Path, rel: &str, data: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }
}

impl Drop for TestFs {
    fn drop(&mut self) {
        rustix::process::umask(self.umask);
        let _ = sh(&["umount", "-l", self.mount.to_str().unwrap()]);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
