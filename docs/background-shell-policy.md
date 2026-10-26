# Background shell policy

Studio applies this policy to Claude native background Bash tasks and local
managed command monitors. Settings are in **Agent orchestration**.

- `shellPolicy.quietThresholdMs`: `0` by default (off). A nonzero value must be
  at least 10,000 milliseconds. Studio sends one check-in per quiet episode.
  New output starts another episode. A check-in does not mean completion.
- `shellPolicy.pressureCleanup`: `true` by default. Critical memory pressure
  permits cleanup only after 30 idle minutes, with no active turn, pending input,
  or active delegates. Native foreground Bash tasks are excluded.
- `CLAUDE_CODE_DISABLE_BG_SHELL_PRESSURE_REAP` also disables native shell cleanup,
  as in Claude Code. Set the Studio setting to disable cleanup for managed monitors.

The bridge retains the output path from the Bash result. It preserves that path
when the SDK replaces its background-task list. An unreadable output file does
not prove silence. The bridge checks its active turn again after each file read.
The backend checks current agent eligibility before each native hand-off.

On macOS, Studio subscribes to Apple's `DISPATCH_SOURCE_TYPE_MEMORYPRESSURE`
through `dispatch2`. It uses the kernel's critical verdict. It does not estimate
pressure from free memory or use a private `sysctl` key.
See [Apple's memory-pressure source](https://developer.apple.com/documentation/dispatch/dispatchsourcememorypressure).

On Linux, Studio requires both memory scarcity and sustained full memory stalls.
Available memory must be below 10% and below 4 GiB. The memory Pressure Stall
Information (PSI) `full avg10` value must be at least 10%. PSI has no native
critical level, so this threshold is a Studio policy. Unreadable pressure data
does not permit cleanup. Other platforms do not supply a critical verdict.
See [the Linux PSI interface](https://docs.kernel.org/accounting/psi.html).

Managed monitors also require 30 minutes without output. Rule monitors, remote
owners, virtual-machine sessions, and monitors without exact operation metadata
are excluded. A new output generation or an active turn blocks native termination.
Existing monitor stall checks and the quiet policy share one output generation.
They cannot send two quiet notices for that generation.

Studio records the cleanup request before it sends a stop. The transcript shows
the request. A native stop result or monitor exit confirms the outcome and
notifies the agent. An unknown result is recorded and is not retried. All policy
notices use next-turn delivery. Cleanup does not interrupt the agent's active turn.
Shells started within 30 minutes after a cleanup are protected until new activity.

Claude Code 2.1.302 source: `readable/chunk-hqtnbrjd.js:200841` (Linux scarcity),
`:200862` (quiet threshold), `:201060` (quiet eligibility), and `:201257` to
`:201304` (pressure cleanup). SDK-mode exclusion: `readable/chunk-hdxpvbx1.js:5726`.
Studio sends one check-in per quiet episode, as requested, instead of Claude's
repeated check-ins with exponential backoff.

Tests use fake clocks and a fake pressure source. They do not create memory load.
