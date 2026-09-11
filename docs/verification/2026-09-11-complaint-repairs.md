# Complaint repairs, 2026-09-11

## Changed behavior

Saved read and validation failures no longer remain unknown solely because the
outer tool response has `success=false`. Reconciliation retains the original
result and completion time. A committed operation receipt overrides any inferred
rejection. Transport failures and unknown mutations remain protected.

Archive inspection and recovery reconcile the exact worker's saved requests.
They use the request index instead of scanning every worker's complete ledger.
Historical account and thread scopes retain actor ownership checks.

Native output reads support saved command and dynamic-tool item references.
The existing search index supplies complete text when the transcript view clips
the JSON. Missing historical text fails explicitly. Actual output truncation stays
visible. A stable send identity prevents duplicate input after a lost response,
including a retry through the workspace bridge.

Monitor cancellation emits one terminal event, including cancellation before
command launch. Startup restores missing historical events without waking old work.
Temporary storage failures retain the first native terminal result in memory and
retry persistence. They do not repeat the command. A process crash while storage
is unavailable can still leave its outcome unknown.

Automatic checkpoints retain exact reservation identities. Completion releases
only that reservation. Capture checks peer turns, tools, and monitors before it
reads files. Restart releases interrupted read-only capture reservations; restore
operations retain their recovery guards.

After transport loss, one bounded native read checks the exact previous turn.
Only confirmed completion with preserved continuation permission can restore its
normal delivery. User pauses, failed turns, lost commands, and unknown deliveries
remain held. Checks back off after failures.

Resource refusal now has a failed outer response, an explicit outcome, and the
actual holder. Team status exposes configured limits and queue blockers. Managed
macOS monitor shells use the selected Xcode SDK when `SDKROOT` is absent. Explicit
project values remain unchanged.

The desktop compares installed source with an identity captured at server startup.
An old process produces an update notice. Reopening the desktop preserves that
process and does not falsely imply that backend changes are active.

## Evidence

Targeted contracts cover request ownership, lost replies, exact send identity,
archive guards, monitor cancellation, transient storage failures, workspace races,
disconnect recovery, resource refusal, and shell configuration.

A read-only check of the reported cleanup cohort classified all 957 saved
request blockers across 34 workers. Three historical output references were
read successfully. This check did not alter the live database or archive workers.
Other task, descendant, native-state, and resource guards still apply.

Real isolated native command checks retain all 19,215 expected output bytes.
Normal exit 0 and cancellation 137 each produce one owner continuation.
Real macOS C compilation passes with the selected Xcode SDK in each tested shell
startup mode. These commands do not use a model response.

Hidden Electron checks cover source identity, the trusted native bridge, the
pending update notice, and clearing that notice after a current-server check.
The TypeScript and production renderer builds pass. All 13 final Python contract
suites pass. The packaged application passes its isolated hidden-window check;
its backend remains available after the window closes.

Reproduce the main regressions:

```sh
python3 -B tests/request-reconciliation-contract.py
python3 -B tests/automatic-connection-recovery-contract.py
python3 -B tests/workspace-reservation-recovery-contract.py
python3 -B tests/monitor-continuation-contract.py
python3 -B tests/resource-receipt-contract.py
python3 -B tests/monitor-continuation-native.py
python3 -B tests/monitor-shell-native.py
node desktop/backend-test.mjs
node desktop/test.mjs
```

## Delivery limits

Source checks and isolated native tests do not prove live backend activation.
Active user work was not stopped. The existing server lacks an atomic update
admission control. Local process inspection through `sys.remote_exec` requires
an operating-system privilege unavailable in this session.

Detailed cross-project complaints and private evidence remain in the local
evidence directory. External provider failures without a request identity and
other projects' product acceptance are not claimed fixed by these Studio changes.
