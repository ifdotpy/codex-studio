set shell := ["bash", "-euo", "pipefail", "-c"]
set positional-arguments

facade := justfile_directory() / "workspaces/tooling/apps/repository-checks/facade.mjs"
rust-target := env_var_or_default("CARGO_TARGET_DIR", justfile_directory() / "target")
rust-http-server := rust-target / "debug/studio-runtime-server"

[doc("List pnpm, Cargo, and Python server packages discovered from their manifests.")]
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

[doc("Check generated API and persisted-state contracts in read-only drift mode.")]
contracts-check:
    @node {{quote(facade)}} contracts-check

[doc("Fail on undeclared imports and forbidden workspace boundaries.")]
boundaries-check:
    @node {{quote(facade)}} boundaries-check

[doc("Check changed packages and manifest dependents; accepts --dry-run.")]
check-affected base *args:
    @node {{quote(facade)}} check-affected {{quote(base)}} {{args}}

[doc("Run default server and bridge, Rust nextest, web type and unit checks, and tooling tests.")]
test-fast:
    @node {{quote(facade)}} test-fast

[doc("Run the non-server checks included in the fast tier (for CI sharding).")]
test-fast-rest:
    @node {{quote(facade)}} test-fast-rest

[doc("Run the non-Rust remainder of the fast tier in hosted CI.")]
test-fast-rest-non-rust:
    @node {{quote(facade)}} test-fast-rest-non-rust

[doc("Run one hash partition of the Rust workspace fast tier.")]
test-fast-rust-partition partition count:
    @CODEX_NODE="$(node -p 'process.execPath')" cargo nextest run --workspace --profile ci --locked --partition hash:{{quote(partition)}}/{{quote(count)}}

[doc("Run one stable shard of the default server suites; index is zero-based.")]
test-server-shard index count:
    @node {{quote(facade)}} test-server-shard {{quote(index)}} {{quote(count)}}

[doc("Run the complete deterministic Rust subsystem corpora and state witnesses.")]
test-rust-runtime-corpora *args:
    @STUDIO_RUNTIME_CORPUS_FULL=1 cargo nextest run --workspace --run-ignored all -E 'test(workspace_gate_full_python_corpus) | test(workspace_reads_real_python_written_state) | binary(/_corpus$/) | binary(monitor_recovery) | binary(rules_tick_atomic) | binary(costs_differential) | binary(/^account_transfer/) | test(every_scenario_recovers_at_every_bridge_boundary) | binary(/^claude_/) | binary(/^(recovery_history|recovery_store|task_recovery|supervisor_journal)$/) | package(studio-process-supervisor) | binary(/^continuation_recovery$/) | binary(/^analytics_history_checkpoint$/) | binary(/^portable_history_python_parity$/) | binary(/^payload_safety_oracle$/) | binary(provider_binaries_differential) | binary(/^wave7_p03_message_ingress$/)' {{args}}
[doc("Run the full account-worker crash matrix at every bridge boundary.")]
test-crash-matrix-full *args:
    @STUDIO_RUNTIME_CORPUS_FULL=1 cargo test -p studio-runtime-services --lib account_worker::tests::crash_matrix::every_scenario_recovers_at_every_bridge_boundary -- --exact {{args}}
[doc("Run isolated Rust corpus mutations in an external target directory; no load generators.")]
test-rust-runtime-corpora-mutations *args:
    @python3 workspaces/runtime/packages/runtime-services/tests/turn_lifecycle/mutation_check.py --target "${CARGO_TARGET_DIR:?set an isolated external CARGO_TARGET_DIR}/corpus-mutations" {{args}}

[doc("Run the opt-in provider-session scheduling fairness diagnostic (manual heavy tier).")]
test-rust-session-fairness:
    @cargo nextest run -p studio-runtime-services --run-ignored only -E 'test(replenished_journal_and_settlement_lanes_allow_command_and_receipt_progress)'

[doc("Run all three manually classified heavy server suites.")]
test-server-heavy:
    @node {{quote(facade)}} test-server-category heavy

[doc("Run one server opt-in category; defaults to the audited automated set.")]
test-server-opt-in category="automated":
    @node {{quote(facade)}} test-server-category {{quote(category)}}

[doc("Run one exact load-class server suite after printing its manual-run conditions.")]
test-server-load suite="":
    @node {{quote(facade)}} test-server-load {{quote(suite)}}

[doc("Run the isolated 16-worker HTTP message acceptance performance measurement; announce it as a load run.")]
runtime-load-message-acceptance *args:
    @node workspaces/runtime/apps/server/src/studio_api/benchmarks/runtime_load/message_acceptance.mjs {{args}}

[doc("Run the announced one-tab, 16-worker Runtime callback and UI load profile.")]
runtime-load-browser-16 *args:
    @node workspaces/runtime/apps/server/src/studio_api/benchmarks/runtime_load/run.mjs --teams 1 --workers-per-team 16 --rounds 1 --steady-seconds 3 --offered-rate 16 --transports 1 --producer-pool-size 1 {{args}}

[doc("Measure Rust HTTP operations and discovered harness-suite conformance; writes full results beneath XDG_CACHE_HOME.")]
rust-http-conformance-ledger:
    @cargo build -p studio-runtime-server
    @STUDIO_HTTP_BACKEND_COMMAND={{quote(rust-http-server)}} python3 workspaces/runtime/apps/server/tests/http_runtime_conformance_ledger.py

[doc("Run the full web browser project with its 900-second global cap.")]
test-browser:
    @node {{quote(facade)}} test-browser

[doc("Manual heavy check: combined multi-server browser routing and offline recovery.")]
test-browser-multi-server:
    @pnpm --filter codex-agents-web run build
    @pnpm --filter codex-agents-web exec node scripts/run-browser-tests.mjs --config playwright.config.ts --project=client tests/servers/multi-server-ui.spec.mjs --workers=1

[doc("Manual client interaction benchmark: five cumulative builds, one browser worker, three samples each; no synthetic load. Output must be outside the checkout.")]
benchmark-client-interactions baseline output:
    @pnpm --filter codex-agents-web run build:wasm
    @pnpm --filter codex-agents-web exec node tests/performance/build-interaction-variants.mjs {{quote(baseline)}} {{quote(output)}}
    @STUDIO_BENCHMARK_VARIANTS={{quote(output + "/variants.json")}} STUDIO_BENCHMARK_OUTPUT={{quote(output + "/measurements.json")}} PLAYWRIGHT_INCLUDE_SPECIAL=1 pnpm --filter codex-agents-web exec node scripts/run-browser-tests.mjs --config playwright.config.ts --project=performance tests/performance/interaction-benchmark.spec.mjs --workers=1

[doc("Run the browser smoke subset included in the fast tier.")]
test-browser-smoke:
    @node {{quote(facade)}} test-browser-smoke

[doc("Run heavy server suites followed by the full browser project; no load suites.")]
test-heavy:
    @node {{quote(facade)}} test-heavy

[doc("Run seeded monitor mutations in an external isolated target directory.")]
test-rust-monitors-mutations target:
    @python3 workspaces/runtime/packages/runtime-services/tests/turn_lifecycle/mutation_check.py --target {{quote(target)}} --manifest workspaces/runtime/packages/runtime-services/tests/monitors/mutations.json --filter 'binary(/monitor_(corpus|recovery)/)'

[doc("Run seeded relocation mutations in an external isolated target directory.")]
test-rust-relocation-mutations target:
    @python3 workspaces/runtime/packages/runtime-services/tests/turn_lifecycle/mutation_check.py --target {{quote(target)}} --manifest workspaces/runtime/packages/runtime-services/tests/relocation/mutations.json --filter 'binary(relocation_corpus)'

[doc("Run the rules mutation corpus in an isolated target outside the checkout.")]
test-rust-rules-mutations target *args:
    @python3 workspaces/runtime/packages/runtime-services/tests/turn_lifecycle/mutation_check.py --target {{quote(target)}} --manifest workspaces/runtime/packages/runtime-services/tests/rules_engine/mutations.json --filter 'binary(rules_engine_corpus) or binary(rules_tick_atomic)' {{args}}

[doc("Run the full isolated continuation corpus and store fault matrix; no provider requests.")]
test-rust-usage-resume-corpora:
    @STUDIO_RUNTIME_CORPUS_FULL=1 cargo nextest run --workspace -E 'binary(/^continuation_(corpus|recovery)$/)'

[doc("Manual heavy check: migrate an isolated Electron profile with hidden windows.")]
test-native-profile-migration:
    @pnpm --filter codex-agents-desktop run test:profile-migration

[doc("Manual heavy check: build the Rust desktop host and run isolated hidden WebViews.")]
test-native-webview:
    @pnpm --filter codex-agents-web run build
    @cargo build -p studio-native-client --features native-check --locked
    @python3 workspaces/client/apps/native/check-native.py
