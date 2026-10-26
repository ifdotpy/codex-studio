//! Reuse a project only when its Git origin and commit history match the source.
use super::*;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::{collections::BTreeSet, fs, time::Duration};

type Result<T> = std::result::Result<T, NativeLayrError>;

pub(super) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
}

fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = crate::bounded_child::run_without_git_environment(crate::bounded_child::Request {
        program: std::ffi::OsStr::new("git"),
        git_directory: Some(cwd.as_os_str()),
        args,
        environment: &[
            ("GIT_OPTIONAL_LOCKS", "0"),
            ("GIT_NO_LAZY_FETCH", "1"),
            ("GIT_TERMINAL_PROMPT", "0"),
        ],
        input: None,
        stdout_limit: 8 * 1024 * 1024,
        stderr_limit: 4096,
        timeout: Duration::from_secs(30),
    })
    .map_err(|e| NativeLayrError::Refused(format!("Cannot verify project Git history: {e:?}")))?;
    if output.stdout.truncated {
        return Err(NativeLayrError::Refused(
            "Project history exceeds the mapping read limit".into(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout.bytes)
        .trim()
        .to_owned())
}

fn origin(value: &str) -> String {
    let value = value.trim().trim_end_matches('/').trim_end_matches(".git");
    let value = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .or_else(|| value.strip_prefix("ssh://"))
        .unwrap_or(value);
    let value = value.strip_prefix("git@").unwrap_or(value);
    if let Some((host, path)) = value.split_once(':') {
        if !host.contains('/') && !path.starts_with('/') {
            return format!("{}/{path}", host.to_ascii_lowercase());
        }
    }
    value.to_owned()
}

fn github_repository(value: &str) -> Option<&str> {
    let repository = value.strip_prefix("github.com/")?;
    let (owner, name) = repository.split_once('/')?;
    if owner.is_empty()
        || name.is_empty()
        || name.contains('/')
        || !repository
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.'))
    {
        return None;
    }
    Some(repository)
}

fn github_id(repository: &str) -> Result<u64> {
    let endpoint = format!("repos/{repository}");
    let output = crate::bounded_child::run_without_git_environment(crate::bounded_child::Request {
        program: std::ffi::OsStr::new("gh"), git_directory: None,
        args: &["api", &endpoint, "--jq", ".id"], environment: &[], input: None,
        stdout_limit: 128, stderr_limit: 4096, timeout: Duration::from_secs(15),
    }).map_err(|_| NativeLayrError::Refused(format!("Cannot verify GitHub repository identity for {repository}. Connect to GitHub and authenticate gh, then repeat the mapping.")))?;
    String::from_utf8_lossy(&output.stdout.bytes)
        .trim()
        .parse::<u64>()
        .map_err(|_| {
            NativeLayrError::Refused(format!(
                "GitHub returned no repository identity for {repository}"
            ))
        })
}

fn match_origins(
    source: &str,
    project: &str,
    resolve_id: impl Fn(&str) -> Result<u64>,
) -> Result<(bool, Option<u64>)> {
    let source = origin(source);
    let project = origin(project);
    match (github_repository(&source), github_repository(&project)) {
        (Some(source), Some(project)) => {
            let source_id = resolve_id(source)?;
            let project_id = if source == project {
                source_id
            } else {
                resolve_id(project)?
            };
            Ok((source_id == project_id, Some(source_id)))
        }
        _ => Ok((!source.is_empty() && source == project, None)),
    }
}

fn project_origin(root: &Path, project: &str) -> Result<Option<String>> {
    let project = root.join("projects").join(project);
    let old = project.join("meta/layr.sqlite");
    let db = if old.is_file() {
        old
    } else {
        project.join("meta/db/layr.sqlite")
    };
    if !db.is_file() {
        return Ok(None);
    }
    let connection = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| NativeLayrError::Refused(format!("Cannot read project origin: {e}")))?;
    let raw: Option<String> = connection
        .query_row(
            "SELECT value FROM config WHERE key='remote.origin.url'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| NativeLayrError::Refused(format!("Cannot read project origin: {e}")))?;
    raw.map(|raw| {
        serde_json::from_str::<String>(&raw)
            .map_err(|e| NativeLayrError::Refused(format!("Invalid project origin: {e}")))
    })
    .transpose()
}

pub(super) fn verify(root: &Path, source: &Path, project: &str) -> Result<Value> {
    if !valid_name(project) {
        return Err(NativeLayrError::Refused(
            "Invalid native Layr project name".into(),
        ));
    }
    let main = root.join("projects").join(project).join("lines/main");
    let expected = CanonicalPath::existing(root)?
        .into_path_buf()
        .join("projects")
        .join(project)
        .join("lines/main");
    if CanonicalPath::existing(&main)?.into_path_buf() != expected {
        return Err(NativeLayrError::Refused(
            "The mapped project must remain inside its store without symlink ancestors".into(),
        ));
    }
    let wanted = git(source, &["config", "--get", "remote.origin.url"])?;
    let existing = project_origin(root, project)?
        .or_else(|| git(&main, &["config", "--get", "remote.origin.url"]).ok())
        .ok_or_else(|| {
            NativeLayrError::Refused(format!("Project {project} has no verifiable Git origin"))
        })?;
    let (matches, github_id) = match_origins(&wanted, &existing, github_id)?;
    if !matches {
        return Err(NativeLayrError::Refused(format!(
            "Project {project} has a different Git origin"
        )));
    }
    let source_history = git(source, &["rev-list", "HEAD"])?;
    let source_history: BTreeSet<&str> = source_history.lines().collect();
    let existing_history = git(&main, &["rev-list", "HEAD"])?;
    if !existing_history
        .lines()
        .any(|commit| source_history.contains(commit))
    {
        return Err(NativeLayrError::Refused(format!(
            "Project {project} has no shared Git history with {}",
            source.display()
        )));
    }
    Ok(json!({"sourceOrigin":wanted,"projectOrigin":existing,"githubRepositoryId":github_id}))
}

pub fn bind(state: &Path, source: &Path, project: &str) -> Result<Value> {
    let root = settings::selected_store(state)?;
    let source = CanonicalPath::existing(source)?.into_path_buf();
    let identity = verify(&root, &source, project)?;
    let result = json!({"projectId":project,"source":source,"storeRoot":root,"identity":identity});
    let folder = settings::metadata(state, &root);
    fs::create_dir_all(&folder)?;
    crate::atomic_file::replace(
        &folder.join(format!(
            "binding-{}.json",
            studio_runtime_store::sha256_hex(source.to_string_lossy().as_bytes())
        )),
        result.to_string().as_bytes(),
        Some(0o600),
        true,
    )?;
    Ok(result)
}

pub(super) fn resolve(state: &Path, root: &Path, record: &Value) -> Result<String> {
    if record["layrProjectId"].is_string() {
        return project_id(record);
    }
    let source = CanonicalPath::existing(
        record["hostProjectPath"]
            .as_str()
            .ok_or_else(|| NativeLayrError::Refused("Missing project source".into()))?,
    )?
    .into_path_buf();
    let fallback = project_id(record)?;
    let source_key = studio_runtime_store::sha256_hex(source.to_string_lossy().as_bytes());
    let metadata = settings::metadata(state, root);
    let binding = metadata.join(format!("binding-{source_key}.json"));
    if binding.exists() {
        let binding: Value = serde_json::from_slice(&fs::read(binding)?)
            .map_err(|e| NativeLayrError::Refused(e.to_string()))?;
        let project = binding["projectId"]
            .as_str()
            .ok_or_else(|| NativeLayrError::Refused("Invalid project binding".into()))?;
        verify(root, &source, project)?;
        return Ok(project.to_owned());
    }
    // Existing Studio imports retain their original path identity and receipts.
    if metadata.join(format!("{fallback}.json")).is_file() {
        return Ok(fallback);
    }
    let source_origin = match git(&source, &["config", "--get", "remote.origin.url"]) {
        Ok(value) if !value.is_empty() => value,
        _ => return Ok(fallback),
    };
    let mut matches = Vec::new();
    let projects = root.join("projects");
    if projects.is_dir() {
        for entry in fs::read_dir(projects)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !valid_name(&name) || !entry.file_type()?.is_dir() {
                continue;
            }
            let existing = project_origin(root, &name)?;
            if let Some(existing) = existing
                && match_origins(&source_origin, &existing, github_id)?.0
            {
                verify(root, &source, &name)?;
                matches.push(name);
            }
        }
    }
    match matches.as_slice() {
        [] => Ok(fallback),
        [project] => Ok(project.clone()),
        _ => Err(NativeLayrError::Refused(
            "More than one project matches this source. Use native-layr map to select one.".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_renames_require_equal_repository_ids_and_a_successful_lookup() {
        let old = "git@github.com:ifdotpy/codex-studio.git";
        let new = "https://github.com/ifdotpy/layr-studio";
        assert_eq!(
            match_origins(old, new, |_| Ok(1359907045)).expect("same repository"),
            (true, Some(1359907045))
        );
        assert!(
            !match_origins(old, new, |repository| Ok(
                if repository.ends_with("codex-studio") {
                    1
                } else {
                    2
                }
            ))
            .expect("different repositories")
            .0
        );
        assert!(
            match_origins(old, new, |_| Err(NativeLayrError::Refused(
                "offline".into()
            )))
            .is_err()
        );
        assert_eq!(
            match_origins(
                "ssh://git@example.test/org/repo.git",
                "https://example.test/org/repo",
                |_| panic!("non-GitHub URL")
            )
            .expect("normalized origin"),
            (true, None)
        );
    }

    fn commit(path: &Path, content: &str) {
        fs::create_dir_all(path).expect("source");
        fs::write(path.join("code"), content).expect("code");
        git(path, &["init", "-q"]).expect("init");
        git(path, &["-c", "core.hooksPath=/dev/null", "add", "code"]).expect("add");
        git(
            path,
            &[
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
                "commit",
                "-qm",
                content,
            ],
        )
        .expect("commit");
        git(
            path,
            &["remote", "add", "origin", "git@example.test:org/repo.git"],
        )
        .expect("origin");
    }

    #[test]
    fn mapping_reuses_matching_history_and_refuses_an_unrelated_project() {
        let fixture = tempfile::tempdir().expect("fixture");
        let state = fixture.path().join("state");
        let root = fixture.path().join("store");
        let source = fixture.path().join("codex-agents");
        let main = root.join("projects/studio/lines/main");
        commit(&source, "shared");
        fs::create_dir_all(main.parent().expect("lines")).expect("lines");
        git(
            fixture.path(),
            &[
                "clone",
                "-q",
                source.to_str().expect("source"),
                main.to_str().expect("main"),
            ],
        )
        .expect("clone");
        git(
            &main,
            &[
                "remote",
                "set-url",
                "origin",
                "https://example.test/org/repo.git",
            ],
        )
        .expect("origin");
        let db = root.join("projects/studio/meta/db");
        fs::create_dir_all(&db).expect("metadata");
        let connection = Connection::open(db.join("layr.sqlite")).expect("db");
        connection.execute_batch("CREATE TABLE config(key TEXT, value TEXT); INSERT INTO config VALUES('remote.origin.url','\"https://example.test/org/repo.git\"');").expect("origin config");
        settings::configure(&state, Some(&root)).expect("configure");
        let root = settings::selected_store(&state).expect("root");
        let record = json!({"hostProjectPath":source});
        assert_eq!(
            resolve(&state, &root, &record).expect("matching project"),
            "studio"
        );
        assert_eq!(
            bind(&state, &source, "studio").expect("explicit map")["projectId"],
            "studio"
        );
        let unrelated = fixture.path().join("unrelated");
        commit(&unrelated, "unrelated history");
        assert!(
            verify(&root, &unrelated, "studio")
                .expect_err("history mismatch")
                .to_string()
                .contains("shared Git history")
        );
        git(
            &unrelated,
            &[
                "remote",
                "set-url",
                "origin",
                "https://example.test/other/repo",
            ],
        )
        .expect("other origin");
        assert!(
            verify(&root, &unrelated, "studio")
                .expect_err("origin mismatch")
                .to_string()
                .contains("different Git origin")
        );
        assert!(verify(&root, &source, "../studio").is_err());
    }
}
