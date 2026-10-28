//! End to end: a tiny cargo project in a layr line with two tests on two functions.
//! Needs a `layr` with cache overlays (the one built from this repository: `cargo build -p layr`
//! and put `target/debug` first in PATH; the installed 0.4.0 of 2026-10-26 is too old),
//! `cargo nextest`, the registry package `hex` 0.4 in the cargo cache (the test runs offline), `cargo nextest` and the `llvm-tools` rustup component. The store is a temp
//! folder with its own LAYR_ROOT. Takes about 10 s.

use std::path::{Path, PathBuf};
use std::process::Command;

// `tc` reads a file, which coverage cannot track.
const LIB: &str = "pub fn a() -> i32 {\n    1\n}\n\npub fn b() -> i32 {\n    2\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn ta() {\n        assert_eq!(a(), 1);\n    }\n\n    #[test]\n    fn tb() {\n        assert_eq!(b(), 2);\n    }\n\n    #[test]\n    fn th() {\n        assert_eq!(hex::encode([1u8]), \"01\");\n    }\n\n    #[test]\n    fn tc() {\n        assert_eq!(std::fs::read_to_string(\"data.txt\").unwrap(), \"ok\");\n    }\n}\n";

const KID: &str = "fn work() -> i32 {\n    7\n}\n\nfn main() {\n    print!(\"{}\", work());\n}\n";
const KID_TEST: &str = "#[test]\nfn tk() {\n    let o = std::process::Command::new(env!(\"CARGO_BIN_EXE_kid\")).output().unwrap();\n    assert_eq!(o.stdout, b\"7\");\n    // A child with a cleared environment writes a default profile into the cwd.\n    std::process::Command::new(env!(\"CARGO_BIN_EXE_kid\")).env_clear().output().unwrap();\n}\n";

fn sh(dir: &Path, root: &Path, prog: &str, args: &[&str]) -> (String, String) {
    let o = Command::new(prog)
        .args(args)
        .current_dir(dir)
        .env("LAYR_ROOT", root)
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_TARGET_DIR")
        .env("CARGO_NET_OFFLINE", "true")
        .output()
        .unwrap();
    let (out, err) = (String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned());
    assert!(o.status.success(), "{prog} {args:?} failed:\n{out}\n{err}");
    (out, err)
}

/// The newest record file of a line.
fn last_record(line: &Path) -> serde_json::Value {
    let newest = std::fs::read_dir(line.join(".layr-retest"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .max_by_key(|p| p.metadata().unwrap().modified().unwrap())
        .unwrap();
    serde_json::from_slice(&std::fs::read(newest).unwrap()).unwrap()
}

/// Files of the line folder, without `target/`.
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() && p.file_name().is_some_and(|n| n != "target") {
            out.extend(walk(&p));
        } else if p.is_file() {
            out.push(p);
        }
    }
    out
}

fn retest(line: &Path, root: &Path, args: &[&str]) -> String {
    let t = std::time::Instant::now();
    let (_, err) = sh(line, root, env!("CARGO_BIN_EXE_layr-retest"), args);
    eprintln!("layr-retest {args:?}: {:.1}s\n{err}", t.elapsed().as_secs_f32());
    err
}

#[test]
fn runs_only_the_stale_test() {
    // layr refuses a store path with a symlink in it (/var is one on macOS).
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-e2e-{}", std::process::id()));
    let src = tmp.join("src-project");
    std::fs::create_dir_all(src.join("src")).unwrap();
    std::fs::write(
        src.join("Cargo.toml"),
        "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nhex = \"0.4\"\n",
    )
    .unwrap();
    std::fs::write(src.join("src/lib.rs"), LIB).unwrap();
    std::fs::write(src.join("data.txt"), "ok").unwrap();
    // `tk` runs a binary with the inherited environment; `work` runs only in that child.
    std::fs::create_dir_all(src.join("src/bin")).unwrap();
    std::fs::create_dir_all(src.join("tests")).unwrap();
    std::fs::write(src.join("src/bin/kid.rs"), KID).unwrap();
    std::fs::write(src.join("tests/kid.rs"), KID_TEST).unwrap();
    std::fs::write(src.join(".layrattributes"), ".layr-retest regenerable\n").unwrap();
    std::fs::write(src.join(".gitignore"), "/target\n/.layr-retest\n").unwrap();
    let root = tmp.join("store");

    sh(&tmp, &root, "layr", &["init", "app", "--from", src.to_str().unwrap()]);
    let main: PathBuf = root.join("projects/app/lines/main");
    sh(&main, &root, "layr", &["line", "new", "work"]);
    let line = root.join("projects/app/lines/work");
    let lib = line.join("src/lib.rs");

    // 1. No record: every test runs.
    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("5 tests, 5 selected"), "{err}");

    assert!(err.contains("removed 1 stray .profraw"), "{err}");

    // Processes outside a test (the list phase, build scripts) leave no profile in the project.
    let stray: Vec<_> = walk(&line).into_iter().filter(|p| p.extension().is_some_and(|e| e == "profraw")).collect();
    assert!(stray.is_empty(), "{stray:?}");

    // The test that uses `hex` has the registry package in its record; the others do not.
    let rec: serde_json::Value = last_record(&line);
    let deps = |t: &str| rec["tests"][format!("tiny {t}")]["deps"].to_string();
    assert!(deps("tests::th").contains("hex-0.4."), "{rec}");
    assert_eq!(deps("tests::ta"), "[]", "{rec}");

    // 2. No change: nothing runs.
    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("5 tests, 0 selected"), "{err}");

    // 3. Change the body of `a`: only `ta` runs.
    std::fs::write(&lib, LIB.replace("    1\n", "    1 + 0\n")).unwrap();
    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("5 tests, 1 selected"), "{err}");
    assert!(err.contains("ran 1, passed 1"), "{err}");

    // 4. A new function that no test calls: nothing runs.
    let s = std::fs::read_to_string(&lib).unwrap();
    std::fs::write(&lib, s.replace("pub fn b()", "pub fn c() -> i32 {\n    3\n}\n\npub fn b()")).unwrap();
    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("5 tests, 0 selected"), "{err}");

    // 4b. An uncalled function above `a` shifts every line: nothing runs, the records must shift.
    let s = std::fs::read_to_string(&lib).unwrap();
    std::fs::write(&lib, format!("pub fn d() -> i32 {{\n    4\n}}\n\n{s}")).unwrap();
    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("5 tests, 0 selected"), "{err}");

    // 4c. Edit `a` at its new place: `ta` must run, `tb` must not.
    let s = std::fs::read_to_string(&lib).unwrap();
    std::fs::write(&lib, s.replace("    1 + 0\n", "    1 + 0 + 0\n")).unwrap();
    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("5 tests, 1 selected"), "{err}");
    assert!(err.contains("tiny tests::ta") && !err.contains("tiny tests::tb"), "{err}");

    // 4d. Code that runs only in a child process: editing it selects the test that started it.
    let kid = line.join("src/bin/kid.rs");
    std::fs::write(&kid, KID.replace("    7\n", "    7 + 0\n")).unwrap();
    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("5 tests, 1 selected"), "{err}");
    assert!(err.contains("tiny::kid tk") && err.contains("ran 1, passed 1"), "{err}");

    // 5. A file that coverage cannot see changes: nothing runs.
    std::fs::write(line.join("data.txt"), "bad").unwrap();
    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("5 tests, 0 selected"), "{err}");

    // 6. A full run runs all, fails `tc`, and reports the contradiction with the saved pass.
    let o = Command::new(env!("CARGO_BIN_EXE_layr-retest"))
        .args(["run", "--full"])
        .current_dir(&line)
        .env("LAYR_ROOT", &root)
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_TARGET_DIR")
        .env("CARGO_NET_OFFLINE", "true")
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(!o.status.success(), "{err}");
    assert!(err.contains("5 selected") && err.contains("CONTRADICTION: tiny tests::tc"), "{err}");

    // 7. The records are in the cache overlay: a line made from this line takes them.
    std::fs::write(line.join("data.txt"), "ok").unwrap();
    sh(&line, &root, "layr", &["add", "Cargo.lock"]);
    sh(&line, &root, "layr", &["save", "-a", "-m", "work"]);
    retest(&line, &root, &["run"]);
    sh(&main, &root, "layr", &["line", "new", "work2", "work"]);
    let err = retest(&root.join("projects/app/lines/work2"), &root, &["run"]);
    assert!(err.contains("5 tests, 0 selected"), "{err}");

    // Saved states are immutable on macOS; unlock them before the delete.
    let _ = Command::new("chflags").args(["-R", "nouchg"]).arg(&tmp).status();
    let _ = std::fs::remove_dir_all(&tmp);
}

/// vitest adapter with a fake tool: a script that logs its arguments and exits with the code in a file.
/// No real vitest, jest or pytest-testmon is used.
#[test]
fn vitest_adapter_with_a_fake_tool() {
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-tool-{}", std::process::id()));
    let src = tmp.join("src-project");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.ts"), "export const a = 1;\n").unwrap();
    std::fs::write(src.join("b.ts"), "export const b = 2;\n").unwrap();
    std::fs::write(src.join(".layrattributes"), ".layr-retest regenerable\n").unwrap();
    std::fs::write(src.join(".gitignore"), "/.layr-retest\n").unwrap();
    let (log, code) = (tmp.join("argv.log"), tmp.join("code"));
    let fake = tmp.join("fake-vitest");
    std::fs::write(&fake, format!("#!/bin/sh\necho \"$@\" >> {}\nexit $(cat {})\n", log.display(), code.display())).unwrap();
    std::fs::write(&code, "0").unwrap();
    Command::new("chmod").arg("+x").arg(&fake).status().unwrap();
    let root = tmp.join("store");
    sh(&tmp, &root, "layr", &["init", "app", "--from", src.to_str().unwrap()]);
    sh(&root.join("projects/app/lines/main"), &root, "layr", &["line", "new", "work"]);
    let line = root.join("projects/app/lines/work");

    let run = |args: &[&str]| -> (bool, String) {
        let o = Command::new(env!("CARGO_BIN_EXE_layr-retest"))
            .args(args)
            .current_dir(&line)
            .env("LAYR_ROOT", &root)
            .env("LAYR_RETEST_VITEST", &fake)
            .output()
            .unwrap();
        let _ = std::fs::read_to_string(&log); // the log is read by `calls`
        (o.status.success(), String::from_utf8_lossy(&o.stderr).into_owned())
    };
    let calls = || -> Vec<String> { std::fs::read_to_string(&log).unwrap_or_default().lines().map(str::to_string).collect() };

    // 1. No baseline: the whole suite.
    assert!(run(&["run", "--tool", "vitest"]).0);
    assert_eq!(calls(), vec!["run"]);

    // 2. No change: the tool does not run, and the run passes.
    let (ok, err) = run(&["run", "--tool", "vitest"]);
    assert!(ok && err.contains("nothing to run"), "{err}");
    assert_eq!(calls().len(), 1);

    // 3. One file changed: only `related --run` that file.
    std::fs::write(line.join("a.ts"), "export const a = 10;\n").unwrap();
    assert!(run(&["run", "--tool", "vitest", "--", "--bail"]).0);
    assert_eq!(calls().last().unwrap(), "related --run a.ts --bail");

    // 4. A failing run writes no pass record: the next run starts from the same baseline again.
    std::fs::write(&code, "1").unwrap();
    std::fs::write(line.join("b.ts"), "export const b = 20;\n").unwrap();
    assert!(!run(&["run", "--tool", "vitest"]).0);
    assert_eq!(calls().last().unwrap(), "related --run b.ts");
    std::fs::write(&code, "0").unwrap();
    assert!(run(&["run", "--tool", "vitest"]).0);
    assert_eq!(calls().last().unwrap(), "related --run b.ts");
    assert_eq!(calls().len(), 4);

    // 5. After a pass nothing is left; `--full` runs everything.
    let (ok, err) = run(&["run", "--tool", "vitest"]);
    assert!(ok && err.contains("nothing to run"), "{err}");
    assert!(run(&["run", "--tool", "vitest", "--full"]).0);
    assert_eq!(calls().last().unwrap(), "run");

    // The records of the tool are apart from the nextest records.
    assert!(line.join(".layr-retest/vitest").is_dir());

    let _ = Command::new("chflags").args(["-R", "nouchg"]).arg(&tmp).status();
    let _ = std::fs::remove_dir_all(&tmp);
}
