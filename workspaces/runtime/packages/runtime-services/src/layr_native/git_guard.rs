//! Git reads remain available in native lines; Layr owns state changes.
use super::{NativeLayrError, binary_path, store_path};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};
use studio_runtime_store::path_identity::CanonicalPath;

const MARKER: &str = "# Studio native Layr Git guard.";
const LAUNCHER_MARKER: &str = "# Studio native Layr launcher.";
const COMMIT_REFUSAL: &str =
    "Git commit is disabled inside a native Layr line. Use layr save -m \"message\".";

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn wrapper_script(state_dir: &Path, real_git: &Path) -> String {
    include_str!("git-guard.sh")
        .replace("@REAL_GIT@", &quote(&real_git.to_string_lossy()))
        .replace("@LAYR@", &quote(&binary_path(state_dir).to_string_lossy()))
        .replace("@STORE@", &quote(&store_path(state_dir).to_string_lossy()))
        .replace("@COMMIT_REFUSAL@", &quote(COMMIT_REFUSAL))
}

fn hook_script() -> String {
    format!(
        "#!/bin/sh\nprintf '%s\\n' {} >&2\nexit 1\n",
        quote(COMMIT_REFUSAL)
    )
}

fn find_real_git() -> Result<PathBuf, NativeLayrError> {
    for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let candidate = directory.join("git");
        let Ok(metadata) = fs::metadata(&candidate) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        let mut prefix = Vec::new();
        fs::File::open(&candidate)?
            .take(128)
            .read_to_end(&mut prefix)?;
        if String::from_utf8_lossy(&prefix).contains(MARKER) {
            continue;
        }
        return Ok(CanonicalPath::existing(candidate)?.into_path_buf());
    }
    Err(NativeLayrError::Refused(
        "Git is unavailable on the host PATH".into(),
    ))
}

fn real_git_path(state_dir: &Path) -> PathBuf {
    binary_path(state_dir).with_file_name("git-real.json")
}

/// The managed entry selects real Git for Layr's own child processes only.
pub(super) fn executable_path(state_dir: &Path) -> PathBuf {
    binary_path(state_dir).with_file_name("layr-real")
}

fn install_layr_entry(state_dir: &Path, real_git: &Path) -> Result<(), NativeLayrError> {
    let entry = binary_path(state_dir);
    let executable = executable_path(state_dir);
    if entry.is_file() {
        let mut prefix = Vec::new();
        fs::File::open(&entry)?.take(128).read_to_end(&mut prefix)?;
        if !executable.is_file() && !String::from_utf8_lossy(&prefix).contains(LAUNCHER_MARKER) {
            // Upgrade an existing installation without removing its entry first.
            let temporary = executable.with_extension("new");
            fs::copy(&entry, &temporary)?;
            fs::rename(temporary, &executable)?;
        }
    }
    if !executable.is_file() {
        return Err(NativeLayrError::Refused(
            "The managed Layr executable is unavailable".into(),
        ));
    }
    let git_directory = real_git
        .parent()
        .ok_or_else(|| NativeLayrError::Refused("Missing real Git directory".into()))?;
    let script = format!(
        "#!/bin/sh\n{LAUNCHER_MARKER}\nPATH={}:\"$PATH\"\nexport PATH\nexec {} \"$@\"\n",
        quote(&git_directory.to_string_lossy()),
        quote(&executable.to_string_lossy())
    );
    if fs::read(&entry).ok().as_deref() != Some(script.as_bytes()) {
        crate::atomic_file::replace(&entry, script.as_bytes(), Some(0o755), true)?;
    }
    Ok(())
}

/// Called under the native binary install lock. Resolve Git once, before publishing its wrapper.
pub(super) fn ensure(state_dir: &Path) -> Result<(), NativeLayrError> {
    let state_dir = CanonicalPath::existing(state_dir)?.into_path_buf();
    let bin = binary_path(&state_dir)
        .with_file_name("git")
        .parent()
        .ok_or_else(|| NativeLayrError::Refused("Missing managed binary folder".into()))?
        .to_path_buf();
    fs::create_dir_all(&bin)?;
    let config = real_git_path(&state_dir);
    let real_git: PathBuf = if config.exists() {
        serde_json::from_slice(&fs::read(&config)?)
            .map_err(|error| NativeLayrError::Refused(format!("Invalid Git guard path: {error}")))?
    } else {
        let real_git = find_real_git()?;
        crate::atomic_file::replace(
            &config,
            serde_json::to_string(&real_git)
                .map_err(|error| NativeLayrError::Refused(error.to_string()))?
                .as_bytes(),
            Some(0o600),
            true,
        )?;
        real_git
    };
    let wrapper = bin.join("git");
    if real_git == wrapper || !real_git.is_absolute() || !real_git.is_file() {
        return Err(NativeLayrError::Refused(
            "The saved real Git executable is unavailable".into(),
        ));
    }
    let script = wrapper_script(&state_dir, &real_git);
    if fs::read(&wrapper).ok().as_deref() != Some(script.as_bytes()) {
        crate::atomic_file::replace(&wrapper, script.as_bytes(), Some(0o755), true)?;
    }
    install_layr_entry(&state_dir, &real_git)?;
    Ok(())
}

/// The local Git setting also overrides a global core.hooksPath supplied by the host account.
pub(super) fn install_hook(state_dir: &Path, line: &Path) -> Result<(), NativeLayrError> {
    let git = line.join(".git");
    if !git.exists() {
        return Ok(());
    }
    if fs::symlink_metadata(&git)?.file_type().is_symlink() || !git.is_dir() {
        return Err(NativeLayrError::Refused(
            "The native line Git directory must be a real directory".into(),
        ));
    }
    let hooks = git.join("hooks");
    if fs::symlink_metadata(&hooks).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(NativeLayrError::Refused(
            "The native line hooks directory must not be a symlink".into(),
        ));
    }
    fs::create_dir_all(&hooks)?;
    crate::atomic_file::replace(
        &hooks.join("pre-commit"),
        hook_script().as_bytes(),
        Some(0o755),
        true,
    )?;
    let real_git: PathBuf = serde_json::from_slice(&fs::read(real_git_path(state_dir))?)
        .map_err(|error| NativeLayrError::Refused(error.to_string()))?;
    crate::bounded_child::run_without_git_environment(crate::bounded_child::Request {
        program: real_git.as_os_str(),
        git_directory: Some(line.as_os_str()),
        args: &["config", "--local", "core.hooksPath", ".git/hooks"],
        environment: &[],
        input: None,
        stdout_limit: 4096,
        stderr_limit: 4096,
        timeout: Duration::from_secs(30),
    })
    .map_err(|error| NativeLayrError::Refused(format!("Install native Git hook: {error:?}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bounded_child::{self, ChildError, Request};
    use std::{ffi::OsStr, fs, time::Duration};

    fn invoke(program: &Path, cwd: &Path, args: &[&str]) -> Result<String, ChildError> {
        invoke_environment(program, cwd, args, &[])
    }

    fn invoke_environment(
        program: &Path,
        cwd: &Path,
        args: &[&str],
        environment: &[(&str, &str)],
    ) -> Result<String, ChildError> {
        let mut command = vec![
            "-c",
            "cd \"$1\"; shift; exec \"$@\"",
            "guard-test",
            cwd.to_str().expect("fixture path"),
            program.to_str().expect("fixture path"),
        ];
        command.extend(args);
        bounded_child::run_without_git_environment(Request {
            program: OsStr::new("/bin/sh"),
            git_directory: None,
            args: &command,
            environment,
            input: None,
            stdout_limit: 4096,
            stderr_limit: 4096,
            timeout: Duration::from_secs(60),
        })
        .map(|output| String::from_utf8_lossy(&output.stdout.bytes).into_owned())
    }

    #[test]
    fn native_git_guard_allows_reads_refuses_writes_and_preserves_outside_arguments() {
        let root = tempfile::tempdir().expect("fixture directory");
        let state = root.path().join("state with quote'");
        let line = super::super::store_path(&state).join("projects/project/lines/main");
        fs::create_dir_all(&line).expect("line fixture");
        let state = CanonicalPath::existing(&state)
            .expect("canonical fixture state")
            .into_path_buf();
        fs::create_dir_all(binary_path(&state).parent().expect("binary parent"))
            .expect("managed binary directory");
        crate::atomic_file::replace(
            &binary_path(&state),
            b"#!/bin/sh\nprintf '%s\\n' \"$@\"\n",
            Some(0o755),
            false,
        )
        .expect("fake Layr");
        let real_git = root.path().join("real git'");
        crate::atomic_file::replace(
            &real_git,
            b"#!/bin/sh\nprintf '%s\\n' \"$@\"\n",
            Some(0o755),
            false,
        )
        .expect("fake real Git");
        let wrapper = root.path().join("git");
        crate::atomic_file::replace(
            &wrapper,
            wrapper_script(&state, &real_git).as_bytes(),
            Some(0o755),
            false,
        )
        .expect("guard executable");
        let reads: &[&[&str]] = &[
            &["status"],
            &["log"],
            &["diff"],
            &["show"],
            &["rev-parse", "HEAD"],
            &["ls-files"],
            &["blame", "file"],
            &["cat-file", "-t", "HEAD"],
            &["describe"],
            &["config", "--get", "remote.origin.url"],
            &["for-each-ref"],
            &["merge-base", "HEAD", "main"],
            &["grep", "needle"],
            &["remote", "-v"],
            &["branch", "--list"],
            &["tag", "--list"],
            &["--version"],
            &["-c", "color.ui=false", "status"],
            &["--no-pager", "log"],
        ];
        for args in reads {
            assert_eq!(
                invoke(&wrapper, &line, args).expect("Git read passes"),
                format!("{}\n", args.join("\n")),
                "{args:?}"
            );
        }
        let writes: &[(&[&str], &str)] = &[
            (&["commit", "-m", "message"], "layr save -m"),
            (&["add", "."], "layr add"),
            (&["rm", "file"], "layr"),
            (&["mv", "a", "b"], "layr"),
            (&["checkout", "main"], "layr"),
            (&["switch", "main"], "layr"),
            (&["restore", "file"], "layr"),
            (&["reset", "--hard"], "layr"),
            (&["stash"], "layr park"),
            (&["branch", "new"], "layr line new"),
            (&["branch", "-D", "old"], "layr line"),
            (&["tag", "v1"], "layr"),
            (&["merge", "other"], "layr"),
            (&["rebase", "main"], "layr"),
            (&["cherry-pick", "state"], "layr pick"),
            (&["revert", "HEAD"], "layr"),
            (&["push"], "layr git push"),
            (&["pull"], "layr"),
            (&["worktree", "add", "../other"], "layr"),
            (&["clean", "-fd"], "layr"),
            (&["config", "user.name", "changed"], "layr"),
            (&["config", "--get", "x", "--add", "y"], "layr"),
        ];
        for (args, message) in writes {
            let error = invoke(&wrapper, &line, args).expect_err("Git write must fail");
            assert!(
                matches!(&error,ChildError::Failed(reason) if reason.contains(message)),
                "{args:?}: {error:?}"
            );
            assert_eq!(
                invoke(&wrapper, root.path(), args).expect("outside-line Git is unchanged"),
                format!("{}\n", args.join("\n"))
            );
        }
        let args = ["-C", line.to_str().expect("line path"), "commit"];
        assert!(
            invoke(&wrapper, root.path(), &args).is_err(),
            "-C must not bypass the guard"
        );
    }

    #[test]
    fn native_git_guard_hook_refuses_absolute_git_commit() {
        let root = tempfile::tempdir().expect("fixture directory");
        let hook = root.path().join("pre-commit");
        crate::atomic_file::replace(&hook, hook_script().as_bytes(), Some(0o755), false)
            .expect("hook executable");
        let error = invoke(&hook, root.path(), &[]).expect_err("hook must refuse commit");
        assert!(matches!(error,ChildError::Failed(reason) if reason.contains("layr save -m")));
    }

    fn managed_layr_calls_real_git(args: &[&str], expected: &str) {
        let root = tempfile::tempdir().expect("fixture directory");
        let state = root.path().join("state with quote'");
        let line = store_path(&state).join("projects/project/lines/worker");
        let bin = binary_path(&state)
            .parent()
            .expect("binary parent")
            .to_path_buf();
        fs::create_dir_all(&line).expect("line");
        fs::create_dir_all(&bin).expect("managed bin");
        let real = root.path().join("real bin'");
        fs::create_dir_all(&real).expect("real bin");
        crate::atomic_file::replace(
            &real.join("git"),
            b"#!/bin/sh\nprintf 'real\\n'; printf '%s\\n' \"$@\"\n",
            Some(0o755),
            false,
        )
        .expect("real Git fixture");
        fs::write(
            real_git_path(&state),
            serde_json::to_vec(&real.join("git")).expect("path JSON"),
        )
        .expect("saved Git path");
        crate::atomic_file::replace(&binary_path(&state), b"#!/bin/sh\ncase \"$1:$2\" in save:-m) exec git update-index ;; save:) exec git write-tree ;; git:sync) exec git update-ref ;; git:push) shift; exec git \"$@\" ;; esac\n", Some(0o755), false).expect("old managed Layr binary");
        ensure(&state).expect("install launcher");
        ensure(&state).expect("repeat install");
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        assert_eq!(
            invoke_environment(&binary_path(&state), &line, args, &[("PATH", &path)])
                .expect("Layr internal Git"),
            expected
        );
        assert!(
            invoke_environment(&bin.join("git"), &line, &["push"], &[("PATH", &path)]).is_err(),
            "agent Git writes remain refused"
        );
    }

    #[test]
    fn managed_layr_named_save_uses_real_git() {
        managed_layr_calls_real_git(&["save", "-m", "message"], "real\nupdate-index\n");
    }

    #[test]
    fn managed_layr_automatic_save_uses_real_git() {
        managed_layr_calls_real_git(&["save"], "real\nwrite-tree\n");
    }

    #[test]
    fn managed_layr_git_sync_uses_real_git() {
        managed_layr_calls_real_git(&["git", "sync"], "real\nupdate-ref\n");
    }

    #[test]
    fn managed_layr_git_push_uses_real_git() {
        managed_layr_calls_real_git(
            &["git", "push", "origin", "worker"],
            "real\npush\norigin\nworker\n",
        );
    }

    fn real_managed_layr_git_operation(operation: &str) {
        let binary = PathBuf::from(
            std::env::var_os("STUDIO_LAYR_NATIVE_TEST_BINARY")
                .expect("Set STUDIO_LAYR_NATIVE_TEST_BINARY"),
        );
        let root = tempfile::tempdir().expect("fixture directory");
        let root_path = CanonicalPath::existing(root.path())
            .expect("canonical root")
            .into_path_buf();
        let state = root_path.join("state");
        let source = root_path.join("source");
        fs::create_dir_all(&source).expect("source");
        assert_eq!(
            invoke(&binary, &source, &["--version"])
                .expect("Layr version")
                .trim(),
            format!("layr version {}", super::super::LAYR_VERSION)
        );
        fs::create_dir_all(binary_path(&state).parent().expect("binary parent")).expect("bin");
        fs::copy(binary, binary_path(&state)).expect("copy real Layr");
        fs::write(source.join("file"), "first\n").expect("source file");
        let real_git = find_real_git().expect("host Git");
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["add", "."],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
                "commit",
                "-qm",
                "initial",
            ],
        ] {
            invoke(&real_git, &source, &args).expect("source Git repository");
        }
        let remote = root_path.join("remote.git");
        invoke(
            &real_git,
            &source,
            &["init", "--bare", "-q", remote.to_str().expect("remote")],
        )
        .expect("local bare remote");
        invoke(
            &real_git,
            &source,
            &["remote", "add", "origin", remote.to_str().expect("remote")],
        )
        .expect("origin");
        ensure(&state).expect("install managed entries");
        let path = format!(
            "{}:{}",
            binary_path(&state).parent().expect("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let store = store_path(&state);
        let environment = [
            ("PATH", path.as_str()),
            ("LAYR_ROOT", store.to_str().expect("store")),
            ("LAYR_DIRECT", "1"),
            ("LAYR_SOCKET", ""),
        ];
        let layr = |cwd: &Path, args: &[&str]| {
            let mut command = vec!["-c", "exec \"$@\" 2>&1", "layr-test"];
            let entry = binary_path(&state);
            command.push(entry.to_str().expect("entry"));
            command.extend(args);
            invoke_environment(Path::new("/bin/sh"), cwd, &command, &environment)
                .expect("real managed Layr command")
        };
        layr(
            &source,
            &[
                "init",
                "project",
                "--from",
                source.to_str().expect("source"),
            ],
        );
        let main = store.join("projects/project/lines/main");
        layr(&main, &["line", "new", "worker", "main"]);
        let line = main.with_file_name("worker");
        let before = invoke(&real_git, &line, &["rev-parse", "HEAD"]).expect("initial Git head");
        fs::write(line.join("file"), "first\nsecond\n").expect("worker edit");
        if operation != "proxy" {
            layr(&line, &["add", "file"]);
        }
        match operation {
            "named" => {
                let output = layr(&line, &["save", "-m", "named"]);
                assert!(!output.contains("warning:"), "{output}");
                assert_ne!(
                    invoke(&real_git, &line, &["rev-parse", "HEAD"]).expect("new Git head"),
                    before
                );
                assert_eq!(
                    invoke(&real_git, &line, &["show", "HEAD:file"]).expect("saved Git content"),
                    "first\nsecond\n"
                );
                invoke(&real_git, &line, &["diff", "--cached", "--exit-code"])
                    .expect("index matches head");
            }
            "automatic" => {
                let output = layr(&line, &["save"]);
                assert!(!output.contains("warning:"), "{output}");
                assert_eq!(layr(&line, &["show", "head@{0}:file"]), "first\nsecond\n");
                assert_eq!(
                    invoke(&real_git, &line, &["rev-parse", "HEAD"]).expect("named head"),
                    before
                );
            }
            "sync" => {
                layr(&line, &["save", "-m", "named"]);
                let output = layr(&line, &["git", "sync"]);
                let head =
                    invoke(&real_git, &line, &["rev-parse", "HEAD"]).expect("synced Git head");
                assert!(output.starts_with(head.trim()), "{output}: {head}");
            }
            "push" => {
                layr(&line, &["save", "-m", "named"]);
                layr(&line, &["git", "push", "origin", "worker"]);
                let remote_head = invoke(
                    &real_git,
                    &source,
                    &[
                        "--git-dir",
                        remote.to_str().expect("remote"),
                        "rev-parse",
                        "refs/heads/worker",
                    ],
                )
                .expect("pushed branch");
                assert_eq!(
                    remote_head,
                    invoke(&real_git, &line, &["rev-parse", "HEAD"]).expect("exported head")
                );
            }
            "proxy" => {
                let wrapper = binary_path(&state).with_file_name("git");
                for args in [
                    vec!["rev-parse", "HEAD"],
                    vec!["rev-parse", "--short=12", "HEAD"],
                    vec!["rev-parse", "HEAD^{tree}"],
                    vec!["rev-parse", "HEAD:file"],
                    vec!["rev-parse", "--show-toplevel"],
                    vec!["rev-parse", "--git-dir"],
                    vec!["rev-parse", "--is-inside-work-tree"],
                    vec!["rev-parse", "--abbrev-ref", "HEAD"],
                    vec!["rev-parse", "--verify", "HEAD^{tree}"],
                    vec!["rev-parse", "--short=8", "HEAD:file"],
                    vec!["status", "--porcelain", "--untracked-files=no"],
                    vec!["status", "--porcelain=v1", "-z"],
                    vec!["diff"],
                    vec!["diff", "HEAD"],
                    vec!["show", "HEAD:file"],
                    vec!["log", "-1", "--format=%ct"],
                    vec!["log", "-1", "--format=%H"],
                    vec!["blame", "file"],
                    vec!["grep", "first"],
                    vec!["grep", "first", "HEAD"],
                    vec!["ls-files"],
                    vec!["ls-files", "--stage", "-z"],
                    vec!["merge-base", "HEAD", "HEAD"],
                    vec!["merge-base", "--is-ancestor", "HEAD", "HEAD"],
                    vec!["cat-file", "-t", "HEAD"],
                    vec!["describe", "--always"],
                ] {
                    assert_eq!(
                        invoke_environment(&wrapper, &line, &args, &environment)
                            .expect("proxy read"),
                        invoke(&real_git, &line, &args).expect("Git read"),
                        "{args:?}"
                    );
                }
                for args in [vec!["grep", "missing"], vec!["diff", "--exit-code"]] {
                    assert!(
                        invoke_environment(&wrapper, &line, &args, &environment).is_err(),
                        "{args:?}"
                    );
                    assert!(invoke(&real_git, &line, &args).is_err(), "{args:?}");
                }
                fs::create_dir(line.join("nested")).expect("nested folder");
                assert_eq!(
                    invoke_environment(
                        &wrapper,
                        &line.join("nested"),
                        &["rev-parse", "--show-toplevel"],
                        &environment
                    )
                    .expect("nested root"),
                    invoke(&real_git, &line, &["rev-parse", "--show-toplevel"]).expect("Git root")
                );
                assert_eq!(
                    invoke_environment(
                        &wrapper,
                        &source,
                        &[
                            "-C",
                            line.to_str().expect("line"),
                            "-C",
                            "nested",
                            "rev-parse",
                            "--show-prefix"
                        ],
                        &environment
                    )
                    .expect("relative -C"),
                    "nested/\n"
                );
                // Compare Layr's index with an independently staged Git fixture.
                fs::write(source.join("file"), "first\nsecond\n").expect("reference edit");
                invoke(&real_git, &source, &["add", "file"]).expect("reference Git stage");
                layr(&line, &["add", "file"]);
                for args in [
                    vec!["status", "--porcelain", "-z"],
                    vec!["ls-files", "--stage", "-z"],
                ] {
                    assert_eq!(
                        invoke_environment(&wrapper, &line, &args, &environment)
                            .expect("Layr index read"),
                        invoke(&real_git, &source, &args).expect("reference Git index read"),
                        "{args:?}"
                    );
                }
                for args in [
                    vec!["diff", "--cached"],
                    vec!["diff", "--staged", "--raw"],
                    vec!["status", "--porcelain=v2"],
                ] {
                    assert!(
                        invoke_environment(&wrapper, &line, &args, &environment).is_err(),
                        "unsupported stage read: {args:?}"
                    );
                }
                let probe = line.join("build-probe");
                fs::create_dir_all(probe.join("src")).expect("build probe");
                fs::write(
                    probe.join("Cargo.toml"),
                    "[package]\nname='git-proxy-probe'\nversion='0.0.0'\nedition='2024'\n",
                )
                .expect("Cargo manifest");
                fs::write(probe.join("src/lib.rs"), "pub fn probe() {}\n").expect("Cargo source");
                fs::write(
                    probe.join("build.rs"),
                    r#"
fn main() {
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git").args(args).output().unwrap();
        assert!(output.status.success(), "{args:?}: {:?}", output.stderr);
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    };
    let commit = git(&["rev-parse", "HEAD"]);
    assert_eq!(commit.len(), 40);
    assert!(commit.bytes().all(|b| b.is_ascii_hexdigit()));
    git(&["log", "-1", "--format=%ct"]).parse::<u64>().unwrap();
    assert!(!git(&["rev-parse", "--abbrev-ref", "HEAD"]).is_empty());
    assert!(!git(&["rev-parse", "--show-toplevel"]).is_empty());
    git(&["status", "--porcelain", "--untracked-files=no"]);
}
"#,
                )
                .expect("build.rs probe");
                let cargo_target = root_path.join("cargo-target");
                let mut build_environment = environment.to_vec();
                build_environment.extend([
                    ("CARGO_INCREMENTAL", "0"),
                    (
                        "CARGO_TARGET_DIR",
                        cargo_target.to_str().expect("Cargo target"),
                    ),
                    ("CARGO_PROFILE_DEV_DEBUG", "0"),
                ]);
                invoke_environment(
                    Path::new("cargo"),
                    &probe,
                    &["build", "-j", "4", "--offline", "--quiet"],
                    &build_environment,
                )
                .expect("Cargo build.rs uses managed Git");
                fs::write(
                    probe.join("package.json"),
                    "{\"name\":\"git-proxy-probe\",\"private\":true}",
                )
                .expect("pnpm manifest");
                invoke_environment(Path::new("pnpm"), &probe, &["exec", "node", "-e", "require('node:fs').writeFileSync('git-stamp',require('node:child_process').execFileSync('git',['rev-parse','HEAD']))"], &environment).expect("pnpm uses managed Git");
                assert_eq!(
                    fs::read_to_string(probe.join("git-stamp")).expect("pnpm build stamp"),
                    invoke(&real_git, &line, &["rev-parse", "HEAD"]).expect("Git build stamp")
                );
                for name in ["space file", "tab\tfile", "line\nfile", "żółć.txt"] {
                    fs::write(line.join(name), "first\n").expect("quoted path");
                    layr(&line, &["add", name]);
                }
                layr(&line, &["save", "-m", "quoted paths"]);
                fs::write(line.join("tab\tfile"), "first\nchanged\n").expect("quoted path edit");
                for args in [
                    vec!["status", "--porcelain"],
                    vec!["status", "--porcelain", "-z"],
                    vec!["ls-files"],
                    vec!["ls-files", "-z"],
                    vec!["grep", "-F", "first"],
                    vec!["log", "-1", "--format=%f"],
                ] {
                    assert_eq!(
                        invoke_environment(&wrapper, &line, &args, &environment)
                            .expect("Git syntax read"),
                        invoke(&real_git, &line, &args).expect("reference Git syntax read"),
                        "{args:?}"
                    );
                }
            }
            _ => panic!("unknown fixture operation"),
        }
        assert!(
            invoke_environment(
                &binary_path(&state).with_file_name("git"),
                &line,
                &["push", "origin", "worker"],
                &environment
            )
            .is_err(),
            "agent Git writes must fail"
        );
    }

    #[test]
    #[ignore = "requires STUDIO_LAYR_NATIVE_TEST_BINARY and APFS or btrfs"]
    fn real_managed_layr_named_save_updates_git_without_warning() {
        real_managed_layr_git_operation("named");
    }

    #[test]
    #[ignore = "requires STUDIO_LAYR_NATIVE_TEST_BINARY and APFS or btrfs"]
    fn real_managed_layr_automatic_save_keeps_named_git_head() {
        real_managed_layr_git_operation("automatic");
    }

    #[test]
    #[ignore = "requires STUDIO_LAYR_NATIVE_TEST_BINARY and APFS or btrfs"]
    fn real_managed_layr_git_sync_records_the_exported_head() {
        real_managed_layr_git_operation("sync");
    }

    #[test]
    #[ignore = "requires STUDIO_LAYR_NATIVE_TEST_BINARY and APFS or btrfs"]
    fn real_managed_layr_git_push_updates_local_bare_remote() {
        real_managed_layr_git_operation("push");
    }

    #[test]
    #[ignore = "requires STUDIO_LAYR_NATIVE_TEST_BINARY and APFS or btrfs"]
    fn real_managed_git_proxy_preserves_tool_outputs_and_build_stamps() {
        real_managed_layr_git_operation("proxy");
    }
    #[test]
    fn native_git_proxy_routes_reads_preserves_paths_and_never_retries_failures() {
        let root = tempfile::tempdir().expect("fixture directory");
        let state = root.path().join("state with quote'");
        let line = store_path(&state).join("projects/project/lines/main");
        fs::create_dir_all(line.join("nested")).expect("line fixture");
        fs::create_dir_all(binary_path(&state).parent().expect("binary parent"))
            .expect("binary fixture");
        let state = CanonicalPath::existing(&state)
            .expect("canonical state")
            .into_path_buf();
        let real_git = root.path().join("real git'");
        for (program, body) in [
            (
                &real_git,
                "#!/bin/sh\n[ \"$1:$2\" != grep:missing ] || exit 1\nprintf 'git\\n'; printf '%s\\n' \"$@\"\n",
            ),
            (
                &binary_path(&state),
                "#!/bin/sh\n[ \"$1\" != grep ] || exit 1\nprintf 'layr\\n'; printf '%s\\n' \"$@\"\n",
            ),
        ] {
            crate::atomic_file::replace(program, body.as_bytes(), Some(0o755), false)
                .expect("fake tool");
        }
        let wrapper = binary_path(&state).with_file_name("git");
        crate::atomic_file::replace(
            &wrapper,
            wrapper_script(&state, &real_git).as_bytes(),
            Some(0o755),
            false,
        )
        .expect("wrapper");
        fs::write(line.join("file"), "fixture").expect("show file fixture");
        for (args, expected) in [
            (
                vec!["status", "--porcelain=v1", "-z"],
                "layr\nstatus\n--porcelain=v1\n-z\n",
            ),
            (
                vec!["diff", "HEAD~1...HEAD", "--", "HEAD", "path with space"],
                "git\ndiff\nHEAD~1...HEAD\n--\nHEAD\npath with space\n",
            ),
            (vec!["show", "HEAD:file"], "layr\nshow\nhead:file\n"),
            (
                vec!["rev-parse", "HEAD^{tree}"],
                "layr\nrev-parse\nhead^{tree}\n",
            ),
            (
                vec!["rev-parse", "HEAD:file"],
                "layr\nrev-parse\nhead:file\n",
            ),
            (
                vec!["rev-parse", "--show-toplevel"],
                "layr\nrev-parse\n--show-toplevel\n",
            ),
            (
                vec!["rev-parse", "--is-inside-work-tree"],
                "layr\nrev-parse\n--is-inside-work-tree\n",
            ),
            (
                vec!["log", "-1", "--format=%ct"],
                "layr\nlog\n-1\n--format=%ct\n",
            ),
            (
                vec!["merge-base", "--is-ancestor", "HEAD~1", "HEAD"],
                "layr\nmerge-base\n--is-ancestor\nhead~1\nhead\n",
            ),
            (vec!["ls-files", "-z"], "layr\nls-files\n-z\n"),
            (vec!["rev-parse", "HEAD"], "git\nrev-parse\nHEAD\n"),
            (
                vec!["rev-parse", "--short=12", "HEAD"],
                "git\nrev-parse\n--short=12\nHEAD\n",
            ),
            (vec!["log", "--format=%H"], "git\nlog\n--format=%H\n"),
            (vec!["cat-file", "-t", "HEAD"], "git\ncat-file\n-t\nHEAD\n"),
            (
                vec!["describe", "--always", "--dirty"],
                "git\ndescribe\n--always\n--dirty\n",
            ),
            (vec!["blame", "file"], "git\nblame\nfile\n"),
        ] {
            assert_eq!(
                invoke(&wrapper, &line, &args).expect("read route"),
                expected,
                "{args:?}"
            );
        }
        assert!(
            invoke(&wrapper, &line, &["grep", "missing"]).is_err(),
            "grep exit 1 must not fall back"
        );
        for args in [
            vec!["diff", "--output=changed"],
            vec!["log", "--output", "changed"],
            vec!["log", "--format=%H", "--out=changed"],
            vec!["ls-files", "--debug", "--output=changed"],
        ] {
            assert!(
                invoke(&wrapper, &line, &args).is_err(),
                "read output must not write: {args:?}"
            );
        }
        assert_eq!(
            invoke(&wrapper, root.path(), &["status", "--porcelain"]).expect("outside"),
            "git\nstatus\n--porcelain\n"
        );
        assert_eq!(
            invoke(
                &wrapper,
                root.path(),
                &["-C", line.to_str().expect("line"), "diff", "HEAD"]
            )
            .expect("selected line"),
            format!("git\n-C\n{}\ndiff\nHEAD\n", line.display())
        );
        assert_eq!(
            invoke(&wrapper, &line, &["-C", "", "diff", "HEAD"]).expect("empty -C"),
            "git\n-C\n\ndiff\nHEAD\n"
        );
        let missing_git = line.join("missing.git");
        assert!(
            invoke(
                &wrapper,
                root.path(),
                &[
                    "--git-dir",
                    missing_git.to_str().expect("missing Git directory"),
                    "init"
                ]
            )
            .is_err(),
            "a missing Git directory must not bypass the guard"
        );
        assert_eq!(
            invoke_environment(
                &wrapper,
                &line,
                &["rev-parse", "--show-toplevel"],
                &[("GIT_DIR", "missing.git")]
            )
            .expect("explicit Git selector"),
            "git\nrev-parse\n--show-toplevel\n"
        );
    }

    fn review_proxy_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().expect("fixture");
        let state = root.path().join("state");
        let line = store_path(&state).join("projects/project/lines/worker");
        fs::create_dir_all(&line).expect("line");
        fs::create_dir_all(binary_path(&state).parent().expect("bin")).expect("bin");
        let state = CanonicalPath::existing(&state)
            .expect("state")
            .into_path_buf();
        let git = root.path().join("real-git");
        for (program, body) in [
            (&git, "#!/bin/sh\nprintf 'git\\n'; printf '%s\\n' \"$@\"\n"),
            (
                &binary_path(&state),
                "#!/bin/sh\nprintf 'layr\\n'; printf '%s\\n' \"$@\"\n",
            ),
        ] {
            crate::atomic_file::replace(program, body.as_bytes(), Some(0o755), false)
                .expect("tool");
        }
        let wrapper = binary_path(&state).with_file_name("git");
        crate::atomic_file::replace(
            &wrapper,
            wrapper_script(&state, &git).as_bytes(),
            Some(0o755),
            false,
        )
        .expect("wrapper");
        (root, wrapper, line)
    }

    #[test]
    fn proxy_optional_values_do_not_hide_output_writes() {
        let (_root, wrapper, line) = review_proxy_fixture();
        for option in ["--pretty", "--format", "--abbrev", "--color", "--decorate"] {
            assert!(
                invoke(&wrapper, &line, &["log", option, "--output=written"]).is_err(),
                "{option}"
            );
        }
    }

    #[test]
    fn proxy_global_options_do_not_hide_a_later_native_target() {
        let (root, wrapper, line) = review_proxy_fixture();
        for prefix in [
            vec!["--exec-path=/tmp"],
            vec!["-p"],
            vec!["--namespace", "test"],
            vec!["--namespace=test"],
            vec!["--no-pager"],
            vec!["--literal-pathspecs"],
            vec!["--bare"],
        ] {
            let mut args = prefix.clone();
            args.extend([
                "-C",
                line.to_str().expect("line"),
                "-c",
                "color.ui=false",
                "commit",
            ]);
            assert!(invoke(&wrapper, root.path(), &args).is_err(), "{prefix:?}");
        }
    }

    #[test]
    fn proxy_classifies_the_final_directory_after_all_c_options() {
        let (root, wrapper, line) = review_proxy_fixture();
        let args = ["-C", root.path().to_str().expect("outside"), "add", "."];
        assert_eq!(
            invoke(&wrapper, &line, &args).expect("outside write"),
            format!("git\n{}\n", args.join("\n"))
        );
        let args = [
            "-C",
            line.to_str().expect("line"),
            "-C",
            root.path().to_str().expect("outside"),
            "commit",
        ];
        assert_eq!(
            invoke(&wrapper, root.path(), &args).expect("final outside"),
            format!("git\n{}\n", args.join("\n"))
        );
    }

    #[test]
    fn proxy_preserves_head_dot_paths() {
        let (_root, wrapper, line) = review_proxy_fixture();
        fs::write(line.join("HEAD.md"), "path").expect("path");
        assert_eq!(
            invoke(&wrapper, &line, &["diff", "HEAD.md"]).expect("path read"),
            "git\ndiff\nHEAD.md\n"
        );
    }

    #[test]
    fn proxy_defers_existing_head_paths_to_git() {
        let (_root, wrapper, line) = review_proxy_fixture();
        fs::write(line.join("HEAD"), "path").expect("path");
        assert_eq!(
            invoke(&wrapper, &line, &["diff", "HEAD"]).expect("path parser"),
            "git\ndiff\nHEAD\n"
        );
    }

    #[test]
    fn proxy_preserves_paths_after_the_separator() {
        let (_root, wrapper, line) = review_proxy_fixture();
        let args = ["diff", "--", "HEAD", "HEAD.md"];
        let output = invoke(&wrapper, &line, &args).expect("path read");
        assert!(output.ends_with("--\nHEAD\nHEAD.md\n"), "{output}");
    }

    #[test]
    fn proxy_keeps_git_porcelain_path_quoting_without_nul() {
        let (_root, wrapper, line) = review_proxy_fixture();
        assert_eq!(
            invoke(&wrapper, &line, &["status", "--porcelain"]).expect("quoting"),
            "git\nstatus\n--porcelain\n"
        );
    }

    #[test]
    fn proxy_keeps_git_ls_files_path_quoting_without_nul() {
        let (_root, wrapper, line) = review_proxy_fixture();
        assert_eq!(
            invoke(&wrapper, &line, &["ls-files"]).expect("quoting"),
            "git\nls-files\n"
        );
    }

    #[test]
    fn proxy_uses_git_for_all_grep_forms() {
        let (_root, wrapper, line) = review_proxy_fixture();
        for args in [
            vec!["grep", "needle"],
            vec!["grep", "-G", "-e", "a\\{1\\}"],
            vec!["grep", "-F", "-z", "needle"],
        ] {
            assert_eq!(
                invoke(&wrapper, &line, &args).expect("grep grammar"),
                format!("git\n{}\n", args.join("\n"))
            );
        }
    }

    #[test]
    fn proxy_log_format_requires_a_tested_placeholder() {
        let (_root, wrapper, line) = review_proxy_fixture();
        for format in [
            "--format=%Q",
            "--format=%C(auto)%s",
            "--format=%f",
            "--format=%ct%G?",
            "--format=%ct%Q",
        ] {
            assert_eq!(
                invoke(&wrapper, &line, &["log", "-1", format]).expect("format fallback"),
                format!("git\nlog\n-1\n{format}\n")
            );
        }
    }

    #[test]
    fn proxy_rev_parse_verify_keeps_git_exit_and_output_semantics() {
        let (_root, wrapper, line) = review_proxy_fixture();
        let args = ["rev-parse", "--verify", "HEAD^{tree}"];
        assert_eq!(
            invoke(&wrapper, &line, &args).expect("verify fallback"),
            format!("git\n{}\n", args.join("\n"))
        );
    }

    #[test]
    fn proxy_rev_parse_flags_do_not_change_revision_arity() {
        let (_root, wrapper, line) = review_proxy_fixture();
        for args in [
            vec!["rev-parse", "--short"],
            vec!["rev-parse", "--show-toplevel", "HEAD^{tree}"],
            vec!["rev-parse", "--short=8", "HEAD:file"],
        ] {
            assert_eq!(
                invoke(&wrapper, &line, &args).expect("flag fallback"),
                format!("git\n{}\n", args.join("\n"))
            );
        }
    }

    #[test]
    fn proxy_cached_formats_never_fall_back_to_the_head_only_index() {
        let (_root, wrapper, line) = review_proxy_fixture();
        for args in [
            vec!["diff", "--cached", "--raw"],
            vec!["diff", "--staged", "--numstat"],
            vec!["-c", "color.ui=false", "diff", "--cached"],
        ] {
            let error = invoke(&wrapper, &line, &args).expect_err("unsupported cached read");
            assert!(
                matches!(error, ChildError::Failed(reason) if reason.contains("layr diff --cached"))
            );
        }
    }

    #[test]
    fn proxy_porcelain_v2_refuses_the_head_only_index() {
        let (_root, wrapper, line) = review_proxy_fixture();
        for args in [
            vec!["status", "--porcelain=v2"],
            vec!["status", "--porcelain=2", "-z"],
        ] {
            let error = invoke(&wrapper, &line, &args).expect_err("unsupported staged format");
            assert!(matches!(error, ChildError::Failed(reason) if reason.contains("layr status")));
        }
    }
}
