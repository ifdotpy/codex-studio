set shell := ["bash", "-euo", "pipefail", "-c"]
set positional-arguments

export CARGO_BUILD_JOBS := `node workspaces/tooling/apps/repository-checks/gate-config.ts`

facade := justfile_directory() / "workspaces/tooling/apps/repository-checks/facade.ts"
rust-target := env_var_or_default("CARGO_TARGET_DIR", justfile_directory() / "target")
rust-http-server := rust-target / "debug/studio-runtime-server"
# Native tests run on the prepared CI builder; its complement owns the shard archive.
native-test-filter := "package(=studio-native-client)"

[doc("Build and install the Rust command links, migrating same-checkout Python links. Does not install or restart services.")]
install *args:
    cargo build --locked -p studio-runtime-server -p studio-process-supervisor -p studio-diagnostics -p studio-operator-cli
    {{quote(rust-http-server)}} install-cli --project {{quote(justfile_directory())}} "$@"

[doc("Build the web UI and release Rust runtime server for service installation and delivery.")]
build-server:
    pnpm install --frozen-lockfile
    pnpm --filter codex-agents-web run build
    cargo build --release --locked -p studio-runtime-server

[doc("Build the Rust release binaries once for HEAD into a shared directory with a commit and SHA-256 manifest. Default directory: CODEX_RELEASE_TARGET_DIR or ~/.cache/codex-studio/release-target.")]
build-release-binaries dir="":
    @node workspaces/client/apps/desktop/release-binaries.ts build {{quote(dir)}}

[doc("Check that the prebuilt release binaries in the directory match HEAD and their SHA-256 manifest.")]
verify-release-binaries dir="":
    @node workspaces/client/apps/desktop/release-binaries.ts verify {{quote(dir)}}

[doc("Deliver a commit (default HEAD) on this host. Linux uses its source release and systemd owner. macOS verifies a signed package and its upgrade canary, then uses the existing recovery owner. Use --app PATH for a prepared Mac package and --dry-run to verify without an install. Explicitly approved Mac installs may use --app PATH --skip-canary.")]
deliver-server commit="HEAD" *args:
    @node workspaces/runtime/apps/runtime-server/deliver.ts "$@"

[doc("Check the package signing setup in seconds: configuration, file modes, keychain unlock, identity. Prints PASS or the exact FAIL reason, never the password. Run it before package-native.")]
signing-check:
    @node workspaces/client/apps/desktop/signing-check.ts

[doc("Native package cycle (checks signing first, then signs with the configured identity): host check, release binaries once per commit, package, package checks. Flag --hosted-ci-passed skips the dev-profile host check.")]
package-native out *flags:
    @node workspaces/client/apps/native/package-flow.ts {{quote(out)}} {{flags}}

[doc("List pnpm and Cargo packages discovered from their manifests.")]
packages:
    @node {{quote(facade)}} packages

[doc("Run one package test suite or pass a file/name filter to its native runner.")]
test package case="" *args:
    @node {{quote(facade)}} test {{quote(package)}} {{quote(case)}} {{args}}

[doc("Run one package test and emit a single JSON result object.")]
test-json package case="" *args:
    @node {{quote(facade)}} test {{quote(package)}} {{quote(case)}} --json {{args}}

[doc("Print the focused target and native command without running it.")]
test-plan package case="" *args:
    @node {{quote(facade)}} test-plan {{quote(package)}} {{quote(case)}} {{args}}

[doc("Run the package's manifest-defined lint, type, and unit checks; accepts --dry-run.")]
check package *args:
    @node {{quote(facade)}} check {{quote(package)}} {{args}}

[doc("Lint every runtime workspace target with warnings treated as errors.")]
clippy-runtime:
    node workspaces/tooling/apps/repository-checks/clippy-ratchet.ts

[doc("Check generated API and persisted-state contracts in read-only drift mode.")]
contracts-check:
    @node {{quote(facade)}} contracts-check

[doc("Fail on undeclared imports and forbidden workspace boundaries.")]
boundaries-check:
    @node {{quote(facade)}} boundaries-check

[doc("Check changed packages and manifest dependents; accepts --dry-run.")]
check-affected base *args:
    @node {{quote(facade)}} check-affected {{quote(base)}} {{args}}

[doc("Run the fast nextest profile and JS/guest witnesses in five bounded concurrent lanes.")]
test-fast:
    @node {{quote(facade)}} test-fast

[doc("Run the broad nextest profile and JS/guest checks with one renderer build and browser smoke.")]
test-comprehensive:
    @node {{quote(facade)}} test-comprehensive

[doc("Run the comprehensive checks excluding browser smoke (legacy CI composition).")]
test-fast-rest:
    @node {{quote(facade)}} test-fast-rest

[doc("Run JS and guest checks against prebuilt WASM/supervisor artifacts; never compile Rust.")]
test-fast-rest-non-rust:
    @node {{quote(facade)}} test-fast-rest-non-rust

[doc("Run one hash partition of the comprehensive Rust workspace checks. With an archive from build-rust-test-archive, run its prebuilt tests instead of compiling them.")]
test-fast-rust-partition partition count archive="":
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -n "$3" ]; then
      # The archive holds ./target of the build checkout; compiled paths such as
      # CARGO_BIN_EXE_* point there, so extract it into this checkout's ./target.
      source=(--archive-file "$3" --workspace-remap . --extract-to . --extract-overwrite)
      supervisor="$PWD/target/debug/codex-process-supervisor"
    else
      cargo build --workspace --tests --bins --examples --locked
      source=(--workspace --locked)
      supervisor="$(node -p 'require("node:path").resolve(process.env.CARGO_TARGET_DIR || "target", "debug", "codex-process-supervisor")')"
    fi
    STUDIO_SUPERVISOR_BINARY="$supervisor" CODEX_NODE="$(node -p 'process.execPath')" cargo nextest run "${source[@]}" --profile ci --partition "hash:$1/$2"

[doc("Run native Rust tests on the CI builder that already has WebView libraries installed.")]
test-native-ci:
    cargo nextest run --workspace --locked --profile ci -E '{{native-test-filter}}'

[doc("Build the full workspace and archive non-native tests for test-fast-rust-partition. Run native tests separately with test-native-ci.")]
build-rust-test-archive file:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -n "${CARGO_TARGET_DIR:-}" ]; then
      echo "Unset CARGO_TARGET_DIR: test-fast-rust-partition extracts the archive into ./target" >&2
      exit 1
    fi
    cargo build --workspace --tests --bins --examples --locked
    cargo nextest archive --workspace --locked -E 'not {{native-test-filter}}' --archive-file "$1"
    if ! tar -tf "$1" | grep -x 'target/debug/codex-process-supervisor' >/dev/null; then
      echo "The archive lacks target/debug/codex-process-supervisor, which test-fast-rust-partition selects" >&2
      exit 1
    fi

[doc("Run the complete deterministic Rust subsystem corpora and state witnesses.")]
test-rust-runtime-corpora *args:
    @STUDIO_RUNTIME_CORPUS_FULL=1 cargo nextest run --workspace --run-ignored all -E 'test(every_scenario_recovers_at_every_bridge_boundary) | package(studio-process-supervisor) | (binary(integration) & (test(/^[^:]*_corpus::/) | test(/^monitor_recovery::/) | test(/^rules_tick_atomic::/) | test(/^costs_differential::/) | test(/^account_transfer[^:]*::/) | test(/^claude_[^:]*::/) | test(/^(recovery_history|recovery_store|task_recovery|supervisor_journal)::/) | test(/^continuation_recovery::/) | test(/^analytics_history_checkpoint::/) | test(/^payload_safety_oracle::/) | test(/^provider_binaries_differential::/) | test(/^wave7_p03_message_ingress::/)))' {{args}}
[doc("Run the full account-worker crash matrix at every bridge boundary.")]
test-crash-matrix-full *args:
    @STUDIO_RUNTIME_CORPUS_FULL=1 cargo test -p studio-runtime-services --lib account_worker::tests::crash_matrix::every_scenario_recovers_at_every_bridge_boundary -- --exact {{args}}

[doc("Run the opt-in provider-session scheduling fairness diagnostic (manual heavy tier).")]
test-rust-session-fairness:
    @cargo nextest run -p studio-runtime-services --run-ignored only -E 'test(replenished_journal_and_settlement_lanes_allow_command_and_receipt_progress)'

[doc("Manual heavy check: a provider child must exit within the 1-second reap grace on an idle machine.")]
test-provider-reap-timing:
    @cargo nextest run -p studio-runtime-process --test integration --run-ignored only -E 'test(provider_stdio::explicit_cancel_stops_and_reaps_child)'

[doc("Run the 200-agent single-account two-exchange scenario for both providers (manual heavy tier).")]
test-rust-many-agents:
    @cargo nextest run -p studio-runtime-services --lib --run-ignored only -E 'test(two_hundred_agents_complete_two_exchanges)'

[doc("Run the full web browser project with its 900-second global cap.")]
test-browser:
    @node {{quote(facade)}} test-browser

[doc("Manual heavy check: combined multi-server browser routing and offline recovery.")]
test-browser-multi-server:
    @pnpm --filter codex-agents-web run build
    @pnpm --filter codex-agents-web exec node scripts/run-browser-tests.ts --config playwright.config.ts --project=client tests/servers/multi-server-ui.spec --workers=1

[doc("Manual client interaction benchmark: five cumulative builds, one browser worker, three samples each; no synthetic load. Output must be outside the checkout.")]
benchmark-client-interactions baseline output:
    @pnpm --filter codex-agents-web run build:wasm
    @pnpm --filter codex-agents-web exec node tests/performance/build-interaction-variants.ts {{quote(baseline)}} {{quote(output)}}
    @STUDIO_BENCHMARK_VARIANTS={{quote(output + "/variants.json")}} STUDIO_BENCHMARK_OUTPUT={{quote(output + "/measurements.json")}} PLAYWRIGHT_INCLUDE_SPECIAL=1 pnpm --filter codex-agents-web exec node scripts/run-browser-tests.ts --config playwright.config.ts --project=performance tests/performance/interaction-benchmark.spec --workers=1

[doc("Run the browser smoke subset included in the comprehensive tier and hosted CI.")]
test-browser-smoke *args:
    @node {{quote(facade)}} test-browser-smoke {{args}}

[doc("Check the main user scenarios on the app installed on this Mac, with its own test chats. See docs/testing.md, Installed scenario smoke.")]
smoke-installed *args:
    @node {{quote(justfile_directory() / "workspaces/tooling/apps/installed-smoke/smoke.ts")}} "$@"

[doc("Install a newer build over the current one on an isolated clone with real processes, before a live install. See docs/testing.md, Upgrade canary.")]
upgrade-canary *args:
    @node {{quote(justfile_directory() / "workspaces/tooling/apps/upgrade-canary/canary.ts")}} "$@"

[doc("Run the full browser project as a manual heavy check.")]
test-heavy:
    @node {{quote(facade)}} test-heavy

[doc("Run the full isolated continuation corpus and store fault matrix; no provider requests.")]
test-rust-usage-resume-corpora:
    @STUDIO_RUNTIME_CORPUS_FULL=1 cargo nextest run --workspace -E '(binary(integration) & test(/^continuation_(corpus|recovery)::/))'

[doc("Manual heavy check: migrate an isolated Electron profile with hidden windows.")]
test-native-profile-migration:
    @pnpm --filter codex-agents-desktop run test:profile-migration

[doc("Manual heavy check: build the Rust desktop host and run isolated hidden WebViews.")]
test-native-webview:
    @pnpm --filter codex-agents-web run build
    @cargo build -p studio-native-client --features native-check --locked
    @node workspaces/client/apps/native/check-native.ts

[doc("Manual check: run real guest module worker aggregation and crash scenarios.")]
test-guest-runner-processes:
    @node --test workspaces/runtime/apps/vm-guest/run-guest-suite-processes.test.mts

[doc("Manual process check: run detached host_exec startup, replacement and guest broker scenarios.")]
test-real-process-probe:
    @STUDIO_REAL_PROCESS_TESTS=1 python3 -I -B workspaces/client/apps/desktop/startup-delivery-test.py StartupDeliveryTests.test_owned_launcher_probe_observes_its_real_kernel_companion_without_runtime_state

test-live-handover:
    @cargo nextest run -j 4 -p studio-runtime-server --test integration --run-ignored only --test-threads 1 -E 'test(/^live_handover::/)'

test-quarantine:
    @cargo nextest run --build-jobs 4 -p studio-runtime-services --run-ignored only --test-threads 1 -E 'test(=wave7_p04_review::answer_and_cancellation_restart_at_each_real_store_queue_and_transport_boundary) | test(=workspace_ready_git::tests::worktree_children_spawned_together_in_one_repository_all_start) | test(=terminal_pty::tests::printf_output_is_delivered_losslessly) | test(=account_worker::tests::publication_off_path::analytics_are_logged_in_the_journal_commit_and_projected_off_the_writer | test(=account_worker::tests::idle_unload::codex_a_finished_worker_whose_turn_failed_unloads_its_thread | test(=account_worker::tests::accepted_turn_reattaches_and_keeps_notification_effects)'

test-host-exec-processes:
    @cargo nextest run -j 4 -p studio-runtime-server --test integration --run-ignored only --test-threads 1 -E 'test(/^host_exec_service::/)'
