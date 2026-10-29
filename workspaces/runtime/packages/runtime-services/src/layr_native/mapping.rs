//! Reuse a project only when its Git origin and commit history match the source.
use super::*;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::{collections::BTreeSet, fs, time::Duration};

pub(super) mod response;

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
    if let Some((host, path)) = value.split_once(':')
        && !host.contains('/')
        && !path.starts_with('/')
    {
        return format!("{}/{path}", host.to_ascii_lowercase());
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

/// Inspect at most 200 recent history records when selecting a saved cache on main's line.
const CACHE_HISTORY_RECORDS: &str = "200";

pub(super) fn cached_main_start(state: &Path, main: &Path) -> Result<(String, Option<String>)> {
    let output = super::runtime::run(state, main, &["log", "--json", "-n", "1", "main"])?;
    let head: response::StateEntry = serde_json::from_str(&output).map_err(|error| {
        NativeLayrError::Refused(format!("Cannot inspect main caches: {error}"))
    })?;
    let warning = cache_warning_for_head(main, &head)?;
    let id = head.id().expect("validated state ID").to_owned();
    if warning.is_none() {
        return Ok((id, None));
    }
    let lines: response::Lines = serde_json::from_str(&super::runtime::run(
        state,
        main,
        &["line", "list", "--json"],
    )?)
    .map_err(|error| NativeLayrError::Refused(format!("Cannot inspect main line: {error}")))?;
    let line = lines
        .main_id()
        .ok_or_else(|| NativeLayrError::Refused("Cannot find main line identity".into()))?;
    let output = super::runtime::run(
        state,
        main,
        &["log", "--auto", "--json", "-n", CACHE_HISTORY_RECORDS],
    )?;
    let records = output
        .lines()
        .map(serde_json::from_str::<response::StateEntry>)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| {
            NativeLayrError::Refused(format!("Cannot inspect main cache history: {error}"))
        })?;
    let candidate = newest_main_cache(&records, line);
    if let Some(candidate) = candidate {
        let differences =
            super::runtime::run(state, main, &["diff", "--name-only", "main", candidate])?;
        if differences.is_empty() {
            return Ok((candidate.to_owned(), None));
        }
    }
    Ok((id, warning))
}

fn newest_main_cache<'a>(records: &'a [response::StateEntry], line: &str) -> Option<&'a str> {
    records
        .iter()
        .filter(|record| record.matches_line(line) && record.has_caches())
        .max_by_key(|record| record.order())
        .and_then(response::StateEntry::id)
}

fn cache_warning_for_head(main: &Path, head: &response::StateEntry) -> Result<Option<String>> {
    let id = head.id().filter(|id| !id.is_empty()).ok_or_else(|| {
        NativeLayrError::Refused("Main cache inspection returned no state ID".into())
    })?;
    let quoted = format!("'{}'", main.to_string_lossy().replace('\'', "'\\''"));
    Ok((!head.has_caches()).then(|| format!(
        "Main head {id} has no saved cache record. Caches on another line do not warm this head. Build once in {}, keep tracked content unchanged, then run `layr -C {quoted} save --caches`. See docs/native-layr.md.", main.display(),
    )))
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
    fn native_layr_cached_start_considers_only_main_lines_newest_automatic_cache() {
        let records: Vec<response::StateEntry> = vec![
            json!({"id":"old-main","kind":"auto","line":"main-line","time":1,"layers":{"target":"old"}}),
            json!({"id":"new-main","kind":"auto","line":"main-line","time":2,"layers":{"target":"new"}}),
            json!({"id":"other-line","kind":"auto","line":"worker-line","time":3,"layers":{"target":"other"}}),
        ].into_iter().map(|value| serde_json::from_value(value).unwrap()).collect();
        assert_eq!(newest_main_cache(&records, "main-line"), Some("new-main"));
        assert_eq!(newest_main_cache(&records, "unknown-line"), None);
    }

    #[test]
    fn cache_history_orders_timestamps_then_ids_and_rejects_bad_json() {
        let input = r#"{"id":"a","kind":"auto","line":"main","time":2,"layers":{"x":"y"}}
{"id":"b","kind":"auto","line":"main","time":2,"caches":{"x":"y"}}
{"id":"ignored","kind":"manual","line":"main","time":9,"layers":{"x":"y"}}
null"#;
        let records: Vec<response::StateEntry> = input
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(newest_main_cache(&records, "main"), Some("b"));
        assert!(serde_json::from_str::<response::StateEntry>("{").is_err());
    }

    #[test]
    fn native_layr_mapped_main_reports_missing_caches_and_accepts_saved_caches() {
        let main = Path::new("/store/projects/studio/lines/main");
        let warning = cache_warning_for_head(
            main,
            &serde_json::from_str(r#"{"id":"main-state"}"#).unwrap(),
        )
        .expect("inspection")
        .expect("missing cache warning");
        assert!(warning.contains("main-state") && warning.contains("save --caches"));
        for field in ["layers", "caches"] {
            let mut head = json!({"id":"main-state"});
            head[field] = json!({"target":"saved-target"});
            assert!(
                cache_warning_for_head(main, &serde_json::from_value(head).unwrap())
                    .expect("saved cache")
                    .is_none()
            );
        }
    }

    #[test]
    fn native_layr_github_renames_require_equal_repository_ids_and_a_successful_lookup() {
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
    fn native_layr_mapping_reuses_matching_history_and_refuses_an_unrelated_project() {
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
