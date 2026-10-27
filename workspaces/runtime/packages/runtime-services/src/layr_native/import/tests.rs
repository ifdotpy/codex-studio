#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_import_name_admission_preserves_case_and_unicode_rules() {
        for names in [["Src/one", "src/two"], ["café", "cafe\u{301}"]] {
            let error =
                validate_names(names.iter().map(Path::new), false, true).expect_err("collision");
            assert!(
                error
                    .to_string()
                    .contains(names[0].split('/').next().expect("prefix"))
            );
            assert!(
                error
                    .to_string()
                    .contains(names[1].split('/').next().expect("prefix"))
            );
        }
        let unicode = ["café", "cafe\u{301}"];
        assert!(validate_names(unicode.iter().map(Path::new), true, true).is_err());
        assert!(validate_names(unicode.iter().map(Path::new), true, false).is_ok());
        assert!(validate_names(["Src/one", "src/two"].iter().map(Path::new), true, true).is_ok());
    }

    #[test]
    fn sibling_name_admission_matches_full_prefix_rules() {
        fn previous_rule(paths: &[&str], sensitive: bool, normalizing: bool) -> bool {
            let mut seen: BTreeMap<String, String> = BTreeMap::new();
            for path in paths {
                let mut prefix = PathBuf::new();
                for component in Path::new(path).components() {
                    prefix.push(component.as_os_str());
                    let name = prefix.to_str().expect("fixture path");
                    let key = if !sensitive {
                        case_key(name)
                    } else if normalizing {
                        icu_normalizer::DecomposingNormalizerBorrowed::new_nfd()
                            .normalize(name)
                            .into_owned()
                    } else {
                        name.to_owned()
                    };
                    if seen
                        .insert(key, name.to_owned())
                        .is_some_and(|old| old != name)
                    {
                        return false;
                    }
                }
            }
            true
        }
        let names = [
            "Src/one",
            "src/two",
            "Src/two",
            "café/one",
            "cafe\u{301}/two",
            "parent/Σ/one",
            "parent/ς/two",
            "parent/same/a",
            "parent/same/b",
            "parent/.hidden",
            "Parent/same/a",
            "parent",
            "parent/same/a",
            "a-b/child",
            "a/child",
            "a",
            "a./child",
            "a0/child",
        ];
        for sensitive in [false, true] {
            for normalizing in [false, true] {
                for a in names {
                    for b in names {
                        let paths = [a, "parent/same/a", b];
                        assert_eq!(
                            validate_names(paths.iter().map(Path::new), sensitive, normalizing)
                                .is_ok(),
                            previous_rule(&paths, sensitive, normalizing),
                            "{paths:?}, sensitive={sensitive}, normalizing={normalizing}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn internal_alternate_objects_refuse_until_private_materialization() {
        let source = tempfile::tempdir().expect("project");
        let reference = source.path().join("reference");
        fs::create_dir(&reference).expect("object source");
        let git =
            |at: &Path, args: &[&str]| super::super::inventory::git(at, args).expect("fixture Git");
        git(&reference, &["init", "-q"]);
        let mut state = 0x1432719u32;
        let bytes: Vec<u8> = (0..200_000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        fs::write(reference.join("payload"), bytes).expect("payload");
        git(&reference, &["add", "."]);
        git(
            &reference,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "-qm",
                "Base",
            ],
        );
        git(source.path(), &["init", "-q"]);
        fs::write(
            source.path().join(".git/objects/info/alternates"),
            format!(
                "{}\n",
                CanonicalPath::existing(reference.join(".git/objects"))
                    .map(CanonicalPath::into_path_buf)
                    .expect("objects")
                    .display()
            ),
        )
        .expect("internal alternate");
        let head = String::from_utf8(git(&reference, &["rev-parse", "HEAD"])).expect("head");
        git(
            source.path(),
            &["update-ref", "refs/heads/main", head.trim()],
        );
        git(source.path(), &["symbolic-ref", "HEAD", "refs/heads/main"]);
        let oid = String::from_utf8(git(&reference, &["hash-object", "payload"])).expect("blob id");
        let oid = oid.trim();
        let object = reference
            .join(".git/objects")
            .join(&oid[..2])
            .join(&oid[2..]);
        let target = source.path().join("retained-object");
        fs::rename(&object, &target).expect("internal linked object");
        std::os::unix::fs::symlink(
            CanonicalPath::existing(&target)
                .map(CanonicalPath::into_path_buf)
                .expect("canonical object"),
            &object,
        )
        .expect("object link");
        let plan = Plan::new(
            source.path(),
            false,
            source.path().parent().expect("parent"),
        )
        .expect("plan");
        assert!(plan.admit().is_err());
        assert!(
            plan.refusals
                .iter()
                .any(|reason| reason.contains("private materialization"))
        );
    }

    #[test]
    fn migration_space_counts_inherited_subtrees_and_only_one_temporary_peak() {
        let source = tempfile::tempdir().expect("main");
        for directory in ["alpha/child", "alphabet", "beta"] {
            fs::create_dir_all(source.path().join(directory)).expect("directory");
            fs::write(source.path().join(directory).join("file"), "data").expect("file");
        }
        let worktree = tempfile::tempdir().expect("worktree");
        fs::write(worktree.path().join("code"), "replacement").expect("worktree file");
        let destination = tempfile::tempdir().expect("destination");
        let main = Plan::new(source.path(), false, destination.path()).expect("main plan");
        let line = Plan::new(worktree.path(), false, destination.path()).expect("line plan");
        assert!(
            main.entries
                .windows(2)
                .all(|pair| pair[0].relative <= pair[1].relative)
        );
        for name in ["", "alpha", "alpha/child", "alphabet", "missing"] {
            let repository = Path::new(name);
            let expected = main
                .entries
                .iter()
                .filter(|entry| !entry.relative.starts_with(repository))
                .count() as u64;
            let size = line.migration_size(&main, repository);
            assert_eq!(size.inherited_entries, expected);
            assert_eq!(
                size.inherited_metadata_bytes,
                expected * ENTRY_METADATA_BYTES
            );
        }
        let mut total = Size {
            metadata_bytes: 20,
            transient_metadata_bytes: 10,
            ..Size::default()
        };
        total.refresh_estimate();
        let mut next = Size {
            metadata_bytes: 30,
            transient_metadata_bytes: 15,
            ..Size::default()
        };
        next.refresh_estimate();
        total.include(next);
        assert_eq!(total.metadata_bytes, 50);
        assert_eq!(total.transient_metadata_bytes, 15);
        assert_eq!(total.estimated_new_bytes, 65);
    }

    #[test]
    fn native_import_space_admission_includes_margin_and_refuses_short_capacity() {
        let size = Size {
            estimated_new_bytes: 2 * 1024 * 1024 * 1024,
            ..Size::default()
        };
        let required = size.estimated_new_bytes + size.estimated_new_bytes / 10;
        assert!(size.require_free(required).is_ok());
        let error = size
            .require_free(required - 1)
            .expect_err("capacity shortfall")
            .to_string();
        assert!(error.contains("bytes available"));
        assert!(error.contains("estimated new bytes"));
        assert!(
            Size::default()
                .require_free(SPACE_MARGIN_BYTES - 1)
                .is_err()
        );
    }

    #[test]
    fn native_import_rejects_changed_source_before_writing_entries() {
        let source = tempfile::tempdir().expect("source");
        let destination = tempfile::tempdir().expect("destination");
        fs::write(source.path().join("code"), "first").expect("file");
        let plan = Plan::new(source.path(), false, destination.path()).expect("selection");
        fs::write(source.path().join("code"), "second version").expect("change");
        assert!(
            plan.stage(destination.path())
                .expect_err("changed source")
                .to_string()
                .contains("source changed")
        );
        assert_eq!(
            fs::read_dir(destination.path())
                .expect("destination")
                .count(),
            0
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_import_stages_independent_clones_symlinks_and_file_modes() {
        let source = tempfile::tempdir().expect("source");
        let destination = tempfile::tempdir().expect("destination");
        fs::write(source.path().join("code"), "source").expect("file");
        fs::set_permissions(
            source.path().join("code"),
            fs::Permissions::from_mode(0o444),
        )
        .expect("mode");
        std::os::unix::fs::symlink("code", source.path().join("link")).expect("link");
        let plan = Plan::new(source.path(), false, destination.path()).expect("selection");
        assert_eq!(plan.size.files, 2);
        assert!(plan.size.estimated_new_bytes < 1024 * 1024);
        plan.stage(destination.path()).expect("clone staging");
        assert_eq!(
            fs::read(destination.path().join("code")).expect("cloned bytes"),
            b"source"
        );
        assert_eq!(
            fs::metadata(destination.path().join("code"))
                .expect("mode")
                .mode()
                & 0o777,
            0o444
        );
        assert_eq!(
            fs::read_link(destination.path().join("link")).expect("link"),
            Path::new("code")
        );
        fs::set_permissions(
            destination.path().join("code"),
            fs::Permissions::from_mode(0o644),
        )
        .expect("mode");
        fs::write(destination.path().join("code"), "staging change").expect("change");
        assert_eq!(
            fs::read(source.path().join("code")).expect("source bytes"),
            b"source"
        );
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use serde_json::json;

    #[cfg(target_os = "macos")]
    #[test]
    fn native_import_generated_name_collision_refuses_before_state_creation() {
        let fixture = tempfile::tempdir().expect("fixture");
        if case_sensitive(fixture.path()).expect("volume policy") {
            return;
        }
        let source = fixture.path().join("source");
        fs::create_dir(&source).expect("source");
        fs::write(source.join(".LAYRIGNORE"), "original").expect("case alias");
        let state = fixture.path().join("absent-state");
        let mut record = json!({"id":"lead","isLead":true,"autoWake":true});
        super::super::configure_new_chat(&mut record, source.to_str().expect("source path"));
        let error =
            super::super::runtime::prepare_files(&state, &record).expect_err("case collision");
        assert!(error.to_string().contains("case or Unicode"));
        assert!(!state.exists());
        fs::remove_file(source.join(".LAYRIGNORE")).expect("remove alias");
        fs::create_dir(source.join(".GIT")).expect("Git case alias");
        super::super::inventory::git(&source, &["init", "-q"]).expect("valid repository");
        let error = super::super::runtime::prepare_files(&state, &record)
            .expect_err("Git bridge case collision");
        assert!(error.to_string().contains("case or Unicode"));
        assert!(!state.exists());
    }

    #[test]
    fn native_import_selects_ignored_data_and_classifies_regenerable_caches() {
        let source = tempfile::tempdir().expect("source");
        fs::write(source.path().join(".gitignore"), "data/\n.env\n").expect("rules");
        fs::write(source.path().join(".env"), "secret").expect("environment");
        for directory in ["data", "node_modules", "target", "nested", "nested/target"] {
            fs::create_dir_all(source.path().join(directory)).expect("directory");
            fs::write(source.path().join(directory).join("file"), "data").expect("file");
        }
        fs::write(source.path().join("nested/Cargo.toml"), "[package]\n").expect("manifest");
        let plan = Plan::new(source.path(), false, source.path()).expect("plan");
        assert!(plan.files().any(|path| path == Path::new("data/file")));
        assert!(plan.files().any(|path| path == Path::new("target/file")));
        assert!(plan.files().any(|path| path == Path::new(".env")));
        assert_eq!(
            plan.caches,
            vec![
                PathBuf::from("nested/target"),
                PathBuf::from("node_modules")
            ]
        );
        assert_eq!(plan.size.cache_bytes, 8);
    }

    #[test]
    fn native_import_reports_reserved_paths_and_external_git_metadata_without_writes() {
        let source = tempfile::tempdir().expect("source");
        fs::create_dir(source.path().join(".studio-native-import")).expect("reserved path");
        let destination = source.path().join("absent");
        let plan = Plan::new(source.path(), false, &destination).expect("report");
        assert!(
            plan.admit()
                .expect_err("reserved path")
                .to_string()
                .contains("Reserved")
        );
        assert!(!destination.exists());
    }

    #[test]
    #[ignore = "requires the real vendored Layr binary on APFS or btrfs"]
    fn native_import_versions_ignored_data_and_keeps_main_and_migration_caches_warm() {
        let binary =
            PathBuf::from(std::env::var_os("STUDIO_LAYR_NATIVE_TEST_BINARY").expect("Layr binary"));
        let state = tempfile::tempdir().expect("state");
        let source = tempfile::tempdir().expect("source");
        let git = |args: &[&str]| {
            crate::bounded_child::run_without_git_environment(crate::bounded_child::Request {
                program: std::ffi::OsStr::new("git"),
                git_directory: Some(source.path().as_os_str()),
                args,
                environment: &[],
                input: None,
                stdout_limit: 64 * 1024,
                stderr_limit: 64 * 1024,
                timeout: std::time::Duration::from_secs(60),
            })
            .expect("Git fixture command");
        };
        git(&["init", "-q"]);
        fs::write(source.path().join("code"), "base").expect("code");
        fs::write(source.path().join(".gitignore"), "data/\nnode_modules/\n").expect("rules");
        git(&["add", "."]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-qm",
            "Base",
        ]);
        fs::create_dir(source.path().join("data")).expect("data");
        fs::write(source.path().join("data/file"), "ignored data").expect("data");
        fs::create_dir_all(source.path().join("evidence/.git/refs")).expect("incomplete evidence");
        fs::write(
            source.path().join("evidence/.git/HEAD"),
            "ref: refs/heads/main\n",
        )
        .expect("evidence bytes");
        fs::create_dir_all(source.path().join("uv-cache/source")).expect("cached source");
        fs::write(
            source.path().join("uv-cache/source/.git"),
            "not a Git pointer\n",
        )
        .expect("plain Git marker");
        fs::create_dir(source.path().join("node_modules")).expect("cache");
        fs::write(source.path().join("node_modules/warm"), "main cache").expect("cache");
        let installed = super::super::binary_path(state.path());
        fs::create_dir_all(installed.parent().expect("bin directory")).expect("bin");
        fs::copy(binary, &installed).expect("install fixture binary");
        let mut record = json!({"id":"lead","isLead":true,"autoWake":true});
        let canonical_source = CanonicalPath::existing(source.path())
            .map(CanonicalPath::into_path_buf)
            .expect("canonical source");
        super::super::configure_new_chat(
            &mut record,
            canonical_source.to_str().expect("source path"),
        );
        let ready =
            super::super::runtime::prepare_files(state.path(), &record).expect("native import");
        let main = PathBuf::from(ready["cwd"].as_str().expect("main"));
        assert_eq!(
            super::super::run(state.path(), &main, &["show", "head:data/file"])
                .expect("versioned ignored data"),
            "ignored data"
        );
        assert!(
            super::super::run(state.path(), &main, &["show", "head:node_modules/warm"]).is_err()
        );
        assert!(
            super::super::run(state.path(), &main, &["cache", "list"])
                .expect("cache list")
                .contains("node_modules\tcache")
        );
        assert!(main.join(".studio-native-import/git/objects").is_dir());
        assert_eq!(
            super::super::run(state.path(), &main, &["show", "head:evidence/.git/HEAD"])
                .expect("evidence in history"),
            "ref: refs/heads/main"
        );
        assert_eq!(
            super::super::run(state.path(), &main, &["show", "head:uv-cache/source/.git"])
                .expect("plain marker in history"),
            "not a Git pointer"
        );
        super::super::run(state.path(), &main, &["line", "new", "warm-worker", "main"])
            .expect("worker line");
        let worker = main.parent().expect("lines").join("warm-worker");
        assert_eq!(
            fs::read_to_string(worker.join("node_modules/warm")).expect("warm worker"),
            "main cache"
        );
        let worktree = source.path().join(".worktrees/wt");
        git(&[
            "worktree",
            "add",
            "-b",
            "worker",
            worktree.to_str().expect("worktree"),
        ]);
        fs::create_dir(worktree.join("node_modules")).expect("worktree cache");
        fs::write(worktree.join("node_modules/warm"), "worktree cache").expect("worktree cache");
        fs::create_dir(worktree.join("data")).expect("worktree data");
        fs::write(worktree.join("data/file"), "worktree ignored data").expect("worktree data");
        let migrated = super::super::migration::migrate(state.path(), source.path(), false)
            .expect("migration");
        assert_eq!(migrated["projectId"], ready["layrProjectId"]);
        let line = main
            .parent()
            .expect("lines")
            .join(migrated["lines"][0]["line"].as_str().expect("line"));
        assert_eq!(
            fs::metadata(&line)
                .expect("private line root")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert!(
            super::super::run(state.path(), &line, &["cache", "list"])
                .expect("migrated caches")
                .contains("node_modules\tcache")
        );
        assert_eq!(
            fs::read_to_string(line.join("node_modules/warm")).expect("migrated warm cache"),
            "worktree cache"
        );
        assert_eq!(
            super::super::run(state.path(), &line, &["show", "head:data/file"])
                .expect("migrated data history"),
            "worktree ignored data"
        );
        assert!(!main.join(".worktrees").exists());
    }

    #[test]
    #[ignore = "requires the real vendored Layr binary on APFS or btrfs"]
    fn native_import_without_git_preserves_source_and_records_receipt() {
        let fixture = tempfile::tempdir().expect("fixture");
        let source = fixture.path().join("source");
        fs::create_dir_all(source.join(".git/objects/info")).expect("broken Git tree");
        fs::write(
            source.join(".git/objects/info/alternates"),
            "/absent/objects\n",
        )
        .expect("alternates");
        fs::write(source.join("code"), "plain folder").expect("code");
        let state = fixture.path().join("state");
        fs::create_dir_all(
            super::super::binary_path(&state)
                .parent()
                .expect("binary parent"),
        )
        .expect("state");
        fs::copy(
            std::env::var_os("STUDIO_LAYR_NATIVE_TEST_BINARY").expect("binary"),
            super::super::binary_path(&state),
        )
        .expect("binary copy");
        let report = super::super::migration::migrate_with_options(
            &state,
            &source,
            false,
            super::super::migration::Options {
                without_git: true,
                ..Default::default()
            },
        )
        .expect("plain-folder migration");
        assert_eq!(report["withoutGit"], true);
        let main = PathBuf::from(report["mainPath"].as_str().expect("main path"));
        assert!(!main.join(GIT_BACKUP).exists());
        assert_eq!(
            super::super::run(&state, &main, &["show", "head:code"]).expect("history"),
            "plain folder"
        );
        let receipt: serde_json::Value = serde_json::from_slice(
            &fs::read(
                state
                    .join(super::super::NATIVE_LAYR_DIRECTORY)
                    .join(format!(
                        "{}.json",
                        report["projectId"].as_str().expect("project id")
                    )),
            )
            .expect("receipt"),
        )
        .expect("JSON receipt");
        assert_eq!(receipt["withoutGit"], true);
        assert_eq!(
            fs::read_to_string(source.join(".git/objects/info/alternates"))
                .expect("source alternates"),
            "/absent/objects\n"
        );
        assert_eq!(
            fs::read_to_string(source.join("code")).expect("source code"),
            "plain folder"
        );
    }

    #[test]
    #[ignore = "operator-selected read-only size report for real projects"]
    fn native_import_real_projects_dry_run() {
        let paths: Vec<PathBuf> = serde_json::from_str(
            &std::env::var("STUDIO_NATIVE_LAYR_DRY_RUN_PROJECTS").expect("project list"),
        )
        .expect("JSON paths");
        let state = PathBuf::from(
            std::env::var_os("STUDIO_NATIVE_LAYR_DRY_RUN_STATE").expect("state path"),
        );
        let without_git: Vec<PathBuf> = serde_json::from_str(
            &std::env::var("STUDIO_NATIVE_LAYR_DRY_RUN_WITHOUT_GIT")
                .unwrap_or_else(|_| "[]".into()),
        )
        .expect("without-Git paths");
        let mut reports = Vec::new();
        for project in paths {
            let options = super::super::migration::Options {
                without_git: without_git.contains(&project),
                ..Default::default()
            };
            let report = match super::super::migration::migrate_with_options(
                &state, &project, true, options,
            ) {
                Ok(report) => report,
                Err(error) => json!({"source":project,"error":error.to_string()}),
            };
            println!("{}", serde_json::to_string(&report).expect("report"));
            reports.push(report);
        }
        let output = PathBuf::from(
            std::env::var_os("STUDIO_NATIVE_LAYR_DRY_RUN_REPORT").expect("report path"),
        );
        fs::write(
            output,
            serde_json::to_vec_pretty(&reports).expect("reports"),
        )
        .expect("write report outside projects");
    }
}
