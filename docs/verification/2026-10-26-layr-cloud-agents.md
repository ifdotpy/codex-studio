# Layr Cloud agent recipes (M6), 2026-10-26

The skill-driven session passed with the real Layr 0.3.0 CLI. It opened a change,
read a failing check and an inline comment, amended the same change, replied,
applied a person approval, withdrew and re-added a queue entry, merged through
the queue, attached a release file, and cloned the encrypted result through an
HTTPS gateway stand-in. No session command used `gh`.

The [full transcript](2026-10-26-layr-cloud-session.txt) records exact commands,
stdout, stderr, exit codes, state IDs, request IDs, and queue results. Recipe
commands are extracted directly from the [skill](../../.agents/skills/layr/SKILL.md).
The fixture setup and assertions are separate from the agent's recipes.

## Source and environment

- Layr base: `01038b9`, version `0.3.0`.
- Layr tested source: `b41642ab1f4ba4591072886f7ea8153ffced2c57`, branch
  `cloud/agent-json`. The commit adds `versions[].checks` to change JSON in both
  the CLI and model. Both call the existing `Project::checks(state)`; no new
  records or check storage are introduced.
- Tested binary SHA-256:
  `9bfbbed6f33280f8f4184d2439e2caeef202226ebf7e590681c57e9e923a0fbd`.
- Host: native APFS backend. The scratch store uses a separate case-sensitive
  APFS sparse image mounted without opening a desktop window. No application
  server, user agents, active terminals, or production state were changed.
- Remote: real `file://` encrypted store, exposed through a loopback HTTPS
  stand-in implementing person sessions, encrypted read proofs, object reads,
  listings, and create-only writes. The fixture verifies person signatures with
  the real Layr store library. It does not test the deployed Cloud service.
- Agent credentials: a disposable person age identity and encrypted signing key,
  plus a disposable scoped write token. Paths arrive through environment
  variables. The identity is provisioned directly without a paper slot. The
  fixture operator's store-creation output is deleted immediately, never opened
  or supplied to the agent. No real person credentials are used or recorded.

## Reproduce

Build the external Layr branch, then select a new scratch directory on the
required filesystem. This explicitly selected verification starts one loopback
HTTPS fixture; it starts no synthetic load.

```sh
cd /path/to/layr
cargo build -j 4
cd /path/to/codex-studio
python3 .agents/skills/layr/references/verify-cloud.py \
  --layr-source /path/to/layr \
  --root /path/on/case-sensitive-apfs/new-session
```

The verifier needs Python 3, OpenSSL, `rustc`, and the built Layr source tree.
It compiles the small credential fixture against that tree's `layr-store`
library. It refuses an existing scratch directory, retains its logs and store,
and closes its gateway on success or failure. Only public person metadata and
ciphertext request envelopes appear in the transcript.

The recorded successful command was:

```sh
python3 .agents/skills/layr/references/verify-cloud.py \
  --layr-source /Users/igor/Projects/layr/.worktrees/cloud-agent-json \
  --root /Users/igor/.local/state/layr-m6/volume/session-remote
```

Every marked recipe passed: `cloud-identity`, `cloud-remote`, `cloud-clone`,
`cloud-open`, `cloud-update`, `cloud-status`, `cloud-reply`, `cloud-approve`,
`cloud-person-approve`, `cloud-queue-add`, `cloud-queue-remove`, `cloud-merge`,
and `cloud-release`. A fresh store then read the old failing check and new
passing check, the reply's parent ID, and `main:answer.txt` containing `fixed`.
This checks data received from encrypted remote objects in addition to the
originating store's local state.

## Failure before the fix

The model regression was run before changing production code:

```sh
cargo test -j 4 -p layr-model \
  change_show_includes_active_check_results_for_each_exact_version
```

It failed with `left: Null` for the version's `checks` field, where an array
containing the command's `exit: 1` was required.

The same skill session was also run with a binary built from unmodified
`01038b9`:

```sh
python3 .agents/skills/layr/references/verify-cloud.py \
  --layr-source /Users/igor/Projects/layr/.worktrees/cloud-agent-json \
  --root /Users/igor/.local/state/layr-m6/volume/session-before-5 \
  --binary /Users/igor/.local/state/layr-m6/layr-01038b9
```

Identity, remote setup, change open, and status commands succeeded. The assertion
`agent sees a failing check in change JSON` failed. With the patch, that
assertion and the complete session passed.

## Checks and limits

- `cargo build -j 4`: passed.
- `cargo test -j 4 -p layr-model`: passed, 61 unit tests and 6 auxiliary tests.
  The new regression also checks exact-version association and revoked checks.
- Native APFS `tests/t18-core.sh`: passed, 136 checks including the CLI JSON
  regression with passing and failing command exits.
- Native APFS `tests/t37-model.sh`: CLI/model change JSON parity passed. Both
  the branch and pristine `01038b9` finished with **43 passes and the same 4
  failures**. A dedicated clean Cargo target compiled the pristine model example,
  so the baseline did not reuse the patched model. See the comparison below.
- Native APFS `tests/t41-requests.sh`: person approval, comment, reply, queue
  removal, refusal, signature, expiry and replay checks passed before a
  replica step failed on both this branch and pristine `01038b9`: the native writer lacks the admin `sync` permission that
  its root-oriented fixture assumes. This is not a passing whole-script result.
- Whole-tree `cargo fmt --check`: fails on pre-existing formatting in unrelated
  sections of the base. The changed CLI file passes focused rustfmt checking.
- Studio Markdown formatting: passed with repository-pinned Oxfmt. No existing
  Studio test directly executes these skill recipes; the new verifier supplies
  that caller check.
- Studio `pnpm run test:pre-commit`: passed.
- Studio `just test-fast`: failed; see the lane evidence below. No full gate is
  claimed as passing.

The Studio bundled CLI still uses its older command names. The new skill
requires the external 0.3.0 CLI with this check-results addition and explicitly
stops on a missing command or missing JSON field. Updating the bundled runtime
is outside M6. A Cloud queue merge and Studio task acceptance remain separate
operations. No branch was merged into `main`, no application was released,
and no infrastructure was changed.

## Pristine Layr base comparison

The four failed assertion labels and final counts match byte for byte between
`b41642a` and pristine `01038b931575e1880fbc7b5829fec649720886f1`.
These are failures already on main in this native environment; they are not new
failures from the check-results addition. The failed assertion entry points in
`tests/t37-model.sh` are:

| Line | Assertion                                                                                  | Branch                                                                                | Pristine main |
| ---- | ------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------- | ------------- |
| 232  | `the merge refusal reports its required check`                                             | Failed: expected `check 'denymerge' failed`, received the required `--expect` refusal | Same          |
| 274  | `overlay state IDs stay in the overlay transfer record`                                    | Failed                                                                                | Same          |
| 379  | `overlay CLI goldens match the overlays own signed records for visible and hidden readers` | Failed                                                                                | Same          |
| 417  | `page outputs contain verified metadata and text bounds`                                   | Failed                                                                                | Same          |

Both runs passed `model change`, including CLI/model JSON parity. No assertion
that passes on main fails on this branch. The line numbers identify the exact
unchanged shell/Python assertion blocks, not an inferred cause for the failures.

```sh
cd /Users/igor/.local/state/layr-m6/source-01038b9
LAYR=/Users/igor/.local/state/layr-m6/layr-01038b9 \
LAYR_APFS_TEST_ROOT=/Users/igor/.local/state/layr-m6/volume \
LAYR_TEST_DIRECT_MAIN=1 CARGO_BUILD_JOBS=4 \
CARGO_TARGET_DIR=/Users/igor/.local/state/layr-m6/baseline-target \
PATH=/opt/homebrew/opt/gnu-sed/libexec/gnubin:/opt/homebrew/opt/coreutils/libexec/gnubin:$PATH \
bash tests/t37-model.sh
```

The source directory is a `git archive 01038b9` export. The baseline binary was
built before the patch; its SHA-256 is
`4d96e60ffac758b2cde3ae0b760dc0cf844ec8e297c8c624db743c4f8fb1c732`.
The dedicated Cargo target prevents reuse of a model example from the branch.
An earlier comparison with a shared Cargo target reused the patched example;
that invalid comparison was discarded.

Local full logs:

- Branch t37: `/Users/igor/.local/state/codex-agents/monitor-logs/7a3f8b37-1361-557a-b6fc-ec21520a01fc.log`.
- Pristine t37: `/Users/igor/.local/state/codex-agents/monitor-logs/715ae3be-6137-56da-abaf-ca08aed5e695.log`.
- Branch t41: `/Users/igor/.local/state/codex-agents/monitor-logs/6b8e941b-cbc4-5362-9b8c-74b7a5c8952e.log`.
- Pristine t41: `/Users/igor/.local/state/codex-agents/monitor-logs/d0a66770-2f16-59b4-b79d-1a62d795ed2e.log`.

The t41 failure is identical at `tests/t41-requests.sh:182`: applying requests
in the replica returns `permission denied: 'sync' needs the role admin; you are
writer`. Its earlier approval, reply, queue and request-safety witnesses pass on
both revisions. The lead owns the Linux full-suite gate before any merge.

## Studio fast-gate evidence

`just test-fast` finished with exit 1 after 560.285 seconds. Rust ran 836 tests:
835 passed and one guest-broker witness failed because `python3` selected 3.9.6
instead of the required 3.11 or newer. The provider lifecycle tests had seven
failures. Retained/recovery Python helpers, the guest runner and one tooling
assertion also failed. WASM, renderer, workspace compilation, generated
contracts, all type checks, web unit tests, desktop checks, sync/native fixtures,
and workspace boundaries passed.

Full branch gate log:
`/Users/igor/.cache/codex-studio/checks/run-1793000069979-mrT5DU/full.log`.
Each lane's `full.log` lives under that same directory (`05-rust-tests`,
`10-studio-claude-bridge`, `12-retained-helpers`, `15-recovery-helpers`,
`18-tooling`, and `19-guest`).

Only failing witnesses were rerun in a clean detached worktree of
`ae3eae4449b5ddf5b7f4968507d3cda4d3f2e326`, at
`/Users/igor/.local/state/layr-m6/studio-base-ae3eae4`, with pinned dependencies
installed using `pnpm install --frozen-lockfile`. The original environment was
retained for a like-for-like comparison; no second full gate was run.

| Witness                                                                                | Branch failure                                                                                                     | Clean base result and log                                                                                                                                             |
| -------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Rust `host_exec_service::the_guest_broker_runs_against_the_mac_service`, assertion 285 | Python 3.9.6 selected instead of required 3.11+                                                                    | Same version failure after rebuilding the focused test from clean base. `/Users/igor/.local/state/codex-agents/monitor-logs/bcc9a4a5-d640-572b-a1ff-8eec89c4fff1.log` |
| Claude `lifecycle-safety.test.mjs`                                                     | Seven failures at assertions 208, 308 and 943: `unknown` versus delivered/not-applied; `preparing` versus accepted | The exact same seven failure names and assertions; 41 passed, 7 failed. `/Users/igor/.local/state/codex-agents/monitor-logs/ebcebfdc-dfd8-524f-bec8-cddb9d7d86db.log` |
| Retained Python helpers                                                                | Missing `StrEnum` and unsupported `type \| None` under Python 3.9                                                  | Same two failed helper boundaries. `/Users/igor/.local/state/layr-m6/base-retained.log`                                                                               |
| Recovery Python helpers                                                                | Missing `StrEnum` under Python 3.9                                                                                 | Same failure. `/Users/igor/.local/state/layr-m6/base-recovery.log`                                                                                                    |
| VM guest runner                                                                        | Requires Python 3.11 or newer                                                                                      | Same version refusal. `/Users/igor/.local/state/layr-m6/base-guest.log`; the runner's own three lifecycle tests pass in `base-guest-runner.log`                       |
| Tooling archive witness, `gates.test.mjs:367`                                          | Actual `/private/var/...` path differs from expected `/var/...` path                                               | Same path assertion. `/Users/igor/.local/state/codex-agents/monitor-logs/818ec0b1-74a3-54fa-95b1-815ed4c910cd.log`                                                    |

Every failing witness reproduced on the clean base. These failures were already
present at the base in this environment; no failing lane passed on the base.
The base worktree stayed clean. The changed branch contains only skills,
documentation, and the standalone verifier/credential fixture.

The base witness commands were:

```sh
CARGO_TARGET_DIR=/Users/igor/Projects/codex-agents/.worktrees/codex-agents/983722ef-35b9-5d81-a1e0-1e7b3019c02b/target \
CARGO_BUILD_JOBS=4 cargo test -j 4 --locked -p studio-runtime-server \
  --test integration \
  host_exec_service::the_guest_broker_runs_against_the_mac_service -- --exact
pnpm --filter studio-claude-bridge exec vitest run \
  --config vitest.fast.config.mjs --project fast lifecycle-safety.test.mjs
node --test workspaces/client/apps/desktop/retained-helper-witness.test.cjs
node --test workspaces/client/apps/desktop/recovery-helper-witness.test.cjs
node --test workspaces/runtime/apps/vm-guest/run-guest-suite.test.mjs
node workspaces/runtime/apps/vm-guest/run-guest-suite.mjs
node --test \
  --test-name-pattern='archived CI partitions reuse prepared binaries without Cargo build options' \
  workspaces/tooling/apps/repository-checks/tests/gates.test.mjs
```
