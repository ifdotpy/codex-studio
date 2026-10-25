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
const COMMIT_REFUSAL: &str =
    "Git commit is disabled inside a native Layr line. Use layr save -m \"message\".";

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn wrapper_script(state_dir: &Path, real_git: &Path) -> String {
    include_str!("git-guard.sh")
        .replace("@REAL_GIT@", &quote(&real_git.to_string_lossy()))
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
            environment: &[],
            input: None,
            stdout_limit: 4096,
            stderr_limit: 4096,
            timeout: Duration::from_secs(10),
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
}
