//! Real native preparation against an existing project's saved Rust cache.
use super::*;
use std::{
    fs,
    time::{Duration, Instant},
};

#[cfg(target_os = "macos")]
struct UnsealFixture(PathBuf);
#[cfg(target_os = "macos")]
impl Drop for UnsealFixture {
    fn drop(&mut self) {
        let _result = crate::bounded_child::run(crate::bounded_child::Request {
            program: std::ffi::OsStr::new("chflags"),
            git_directory: None,
            args: &["-RP", "nouchg", self.0.to_str().unwrap_or_default()],
            environment: &[],
            input: None,
            stdout_limit: 4096,
            stderr_limit: 4096,
            timeout: Duration::from_secs(30),
        });
    }
}

fn cargo_build(cwd: &Path) -> (Duration, Vec<Value>) {
    let target = cwd.join("target");
    let manifest = cwd.join("Cargo.toml");
    let started = Instant::now();
    let output = crate::bounded_child::run_without_git_environment(crate::bounded_child::Request {
        program: std::ffi::OsStr::new("cargo"),
        git_directory: None,
        args: &[
            "build",
            "--manifest-path",
            manifest.to_str().expect("manifest"),
            "--offline",
            "-j",
            "4",
            "--message-format=json",
        ],
        environment: &[("CARGO_TARGET_DIR", target.to_str().expect("target"))],
        input: None,
        stdout_limit: 1024 * 1024,
        stderr_limit: 64 * 1024,
        timeout: Duration::from_secs(120),
    })
    .expect("fixture Cargo build");
    let artifacts = String::from_utf8(output.stdout.bytes)
        .expect("Cargo JSON")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("Cargo message"))
        .filter(|message| message["reason"] == "compiler-artifact")
        .collect();
    (started.elapsed(), artifacts)
}

#[test]
#[ignore = "requires STUDIO_LAYR_NATIVE_TEST_BINARY, the cached memchr crate, and APFS or btrfs"]
fn real_native_preparation_reuses_an_existing_projects_saved_rust_cache() {
    let binary =
        PathBuf::from(std::env::var_os("STUDIO_LAYR_NATIVE_TEST_BINARY").expect("host binary"));
    let fixture = tempfile::tempdir().expect("fixture");
    let fixture_root = CanonicalPath::existing(fixture.path())
        .expect("canonical fixture")
        .into_path_buf();
    #[cfg(target_os = "macos")]
    let _unseal = UnsealFixture(fixture_root.clone());
    let state = fixture_root.join("state");
    let source = fixture_root.join("codex-agents");
    fs::create_dir_all(source.join("src")).expect("source");
    fs::write(source.join("Cargo.toml"), "[package]\nname=\"native-warm-fixture\"\nversion=\"0.1.0\"\nedition=\"2021\"\n[dependencies]\nmemchr=\"=2.8.3\"\n").expect("manifest");
    fs::write(
        source.join("src/main.rs"),
        "fn main() { assert_eq!(memchr::memchr(b'x', b\"fixture\"), Some(2)); }\n",
    )
    .expect("source");
    fs::write(source.join(".gitignore"), "target/\n").expect("ignore");
    fs::write(source.join(".layrattributes"), "target regenerable\n").expect("cache policy");
    let manifest = source.join("Cargo.toml");
    crate::bounded_child::run_without_git_environment(crate::bounded_child::Request {
        program: std::ffi::OsStr::new("cargo"),
        git_directory: None,
        args: &[
            "generate-lockfile",
            "--offline",
            "--manifest-path",
            manifest.to_str().expect("manifest"),
        ],
        environment: &[],
        input: None,
        stdout_limit: 4096,
        stderr_limit: 4096,
        timeout: Duration::from_secs(30),
    })
    .expect("commit the fixture lockfile before saving main caches");
    let git = |args: &[&str]| super::inventory::git(&source, args).expect("fixture Git command");
    git(&["init", "-q"]);
    git(&["-c", "core.hooksPath=/dev/null", "add", "."]);
    git(&[
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.test",
        "commit",
        "-qm",
        "Fixture source",
    ]);
    git(&[
        "remote",
        "add",
        "origin",
        "https://example.test/org/warm-fixture.git",
    ]);
    fs::create_dir_all(binary_path(&state).parent().expect("bin folder")).expect("bin folder");
    fs::copy(binary, binary_path(&state)).expect("private binary");
    install::ensure_binary(&state).expect("private installation");
    let root = store_path(&state);
    run(
        &state,
        &state,
        &["init", "studio", "--from", source.to_str().expect("source")],
    )
    .expect("existing project");
    let main = root.join("projects/studio/lines/main");
    run(&state, &main, &["protect", "main", "--direct"]).expect("main owner");
    let (cold, cold_artifacts) = cargo_build(&main);
    assert!(
        cold_artifacts
            .iter()
            .any(|artifact| artifact["fresh"] == false)
    );
    fs::create_dir_all(main.join("target/studio-test-results")).expect("test-result cache");
    fs::write(
        main.join("target/studio-test-results/warm-proof.json"),
        "{\"saved\":true}\n",
    )
    .expect("cache witness");
    run(&state, &main, &["cache", "add", "target"]).expect("register cache");
    run(&state, &main, &["save", "--caches"]).expect("save cache");
    settings::configure(&state, Some(&root)).expect("select existing store");
    let mut lead = json!({"id":"warm-lead","isLead":true,"autoWake":true});
    configure_new_chat(&mut lead, source.to_str().expect("source"));
    let ready = runtime::prepare_files(&state, &lead).expect("real lead preparation");
    assert_eq!(ready["layrProjectId"], "studio");
    for (key, value) in ready.as_object().expect("ready") {
        lead[key] = value.clone();
    }
    let mut worker =
        json!({"id":"warm-worker","isLead":false,"role":"implementer","autoWake":true});
    configure_child(&mut worker, &lead, &lead);
    let ready = runtime::prepare_files(&state, &worker).expect("real worker preparation");
    for (key, value) in ready.as_object().expect("worker ready") {
        worker[key] = value.clone();
    }
    let worker_cwd = PathBuf::from(ready["cwd"].as_str().expect("worker cwd"));
    assert_eq!(
        fs::read_to_string(worker_cwd.join("target/studio-test-results/warm-proof.json"))
            .expect("saved test-result cache"),
        "{\"saved\":true}\n"
    );
    let (warm, artifacts) = cargo_build(&worker_cwd);
    let dependencies: Vec<_> = artifacts
        .iter()
        .filter(|artifact| artifact["target"]["name"] == "memchr")
        .collect();
    assert!(
        !dependencies.is_empty(),
        "the dependency must participate in the build"
    );
    assert!(
        dependencies
            .iter()
            .all(|artifact| artifact["fresh"] == true),
        "saved dependency artifacts must remain fresh: {artifacts:?}"
    );
    assert!(
        !root
            .join("projects")
            .join(studio_runtime_store::sha256_hex(
                source.to_string_lossy().as_bytes()
            ))
            .exists()
    );
    println!(
        "native warm build: cold_ms={}, warm_ms={}, fresh_artifacts={}, total_artifacts={}, project=studio",
        cold.as_millis(),
        warm.as_millis(),
        artifacts
            .iter()
            .filter(|artifact| artifact["fresh"] == true)
            .count(),
        artifacts.len()
    );
    let next_root = fixture_root.join("next-store");
    fs::create_dir(&next_root).expect("next store");
    settings::configure(&state, Some(&next_root)).expect("switch destination");
    save(&state, &worker, "save-after-root-switch").expect("old chat keeps its root");
    assert!(!next_root.join("projects").exists());
    assert_eq!(
        thread_environment_for_record(&state, &worker).expect("bound environment")["LAYR_ROOT"],
        json!(root)
    );
    fs::write(
        root.join("store-version.json"),
        json!({"format":999,"version":"9.9.9","build":"future-build"}).to_string(),
    )
    .expect("incompatible fixture marker");
    let error = runtime::prepare_files(&state, &lead)
        .expect_err("incompatible store must refuse")
        .to_string();
    assert!(
        error.contains("9.9.9")
            && error.contains(LAYR_VERSION)
            && error.contains(root.to_str().expect("root")),
        "{error}"
    );
}
