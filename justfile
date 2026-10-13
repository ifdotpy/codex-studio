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

[doc("Check generated API contracts in read-only drift mode.")]
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

[doc("Run one stable shard of the default server suites; index is zero-based.")]
test-server-shard index count:
    @node {{quote(facade)}} test-server-shard {{quote(index)}} {{quote(count)}}

[doc("Run all three manually classified heavy server suites.")]
test-server-heavy:
    @node {{quote(facade)}} test-server-category heavy

[doc("Run one server opt-in category; defaults to the audited automated set.")]
test-server-opt-in category="automated":
    @node {{quote(facade)}} test-server-category {{quote(category)}}

[doc("Run one exact load-class server suite after printing its manual-run conditions.")]
test-server-load suite="":
    @node {{quote(facade)}} test-server-load {{quote(suite)}}

[doc("Measure Rust HTTP operations and discovered harness-suite conformance; writes full results beneath XDG_CACHE_HOME.")]
rust-http-conformance-ledger:
    @cargo build -p studio-runtime-server
    @STUDIO_HTTP_BACKEND_COMMAND={{quote(rust-http-server)}} python3 workspaces/runtime/apps/server/tests/http_runtime_conformance_ledger.py

[doc("Run the full web browser project with its 900-second global cap.")]
test-browser:
    @node {{quote(facade)}} test-browser

[doc("Run the browser smoke subset included in the fast tier.")]
test-browser-smoke:
    @node {{quote(facade)}} test-browser-smoke

[doc("Run heavy server suites followed by the full browser project; no load suites.")]
test-heavy:
    @node {{quote(facade)}} test-heavy

[doc("Run the full Rust lifecycle and notification matrices (manual heavy tier; no load generators).")]
test-rust-runtime-corpora:
    @STUDIO_RUNTIME_CORPUS_FULL=1 cargo nextest run -p studio-runtime-services --test submit_gates_corpus
    @STUDIO_RUNTIME_CORPUS_FULL=1 cargo nextest run -p studio-runtime-services -E 'binary(/^(turn_lifecycle|notification_dispatch)_corpus$/)'

[doc("Run isolated Rust corpus mutations (manual heavy tier; two build jobs, no load generators).")]
test-rust-runtime-corpora-mutations *args:
    @python3 workspaces/runtime/packages/runtime-services/tests/turn_lifecycle/mutation_check.py --target "${CARGO_TARGET_DIR:?set an isolated external CARGO_TARGET_DIR}/corpus-mutations" {{args}}
    @python3 workspaces/runtime/packages/runtime-services/tests/turn_lifecycle/mutation_check.py --target "${CARGO_TARGET_DIR:?set an isolated external CARGO_TARGET_DIR}/submit-gates-mutations" --manifest workspaces/runtime/packages/runtime-services/tests/submit_gates/mutations.json --filter 'binary(submit_gates_corpus)' {{args}}
