# layr: ideas beyond the current design

Status: proposals from the discussion on 2026-10-09. These ideas are not accepted
requirements or implemented features. Command examples show proposed syntax.

Related contract: [Local version control on btrfs](btrfs-local-vcs.md).
This document concerns standalone `layr`, without Studio integration.

## Direction

Make `layr` a version control system for the whole work folder. Its main features
are isolated changes, checks of exact results, and safe undo.

Keep the git-like commands. Add operations that use states and lines directly.
btrfs remains the primary local storage mechanism.

## 1. Run commands in a separate version of the folder

Proposed flow:

```sh
layr run -- npm update
layr diff
layr accept
```

The user can choose `layr discard` instead of `layr accept`.

The command runs in a separate line. The original folder stays unchanged until
the user accepts the result. A failed package update, generator, or file migration
does not leave the original folder partly changed.

This guarantee covers files inside the line. Network requests, external databases,
and other external effects need separate rules. A snapshot cannot undo them.

## 2. Keep two levels of history

The detailed history records exact states after commands. The change history
groups those states by purpose, such as "update dependencies" or "add authentication".

The user can change a description, split a change, or combine changes.
The original states remain available within the retention period.

The ordinary `log` shows meaningful changes. The detailed history supports undo
without hundreds of technical entries in that view.

## 3. Attach checks to exact states

A check result identifies the exact state, command, and environment.
A change to the files or check conditions invalidates that result for the new state.

Proposed syntax:

```sh
layr check -- npm test
layr merge feature --require-check tests
```

The syntax for check names remains open.

A merge first creates the candidate result. Required checks run against that
result. The main line accepts that exact state only after those checks pass.

If the main line changes before acceptance, the old result does not authorize
acceptance of a different merge result.

## 4. Record dependencies and support selective undo

The user can request: "Remove change B, but keep A and C."

`layr` prepares the result separately, reports conflicts, and runs the required
checks. If C depends on B, it reports that dependency.

Explicit dependencies supplement file conflicts. File overlap alone does not
describe all dependencies between changes.

The same model supports transfer of one change together with its dependencies.
Selective undo creates a new result. It does not erase the original history.

## 5. Give file groups explicit policies

Each file group has separate rules for:

- Local history.
- Transfer between machines.
- Backup.
- Publication through Git.
- Recreation from other inputs.

Source files, dependencies, build outputs, and secrets need distinct policies.
`.gitignore` does not need to decide all these policies.

For example, a dependency cache can move with a work folder but stay outside
long-term backups. Secret data needs an explicit policy before any snapshot or transfer.

## 6. Separate state identity from btrfs storage

A state identity, its contents, and its storage location are separate concepts.
btrfs remains the primary local backend.

Transfer between stores with compatible snapshot history uses `btrfs send`.
Other transfers use a format based on file contents.

This format supports recovery without the original disk. It also supports a check
that the recovered files match the state. The identity and verification format
remain design questions.

## 7. Test the exact index contents

The current design already represents the index as a separate line with files.
A temporary writable snapshot of that index can serve as the test environment.

Proposed syntax:

```sh
layr test --staged -- npm test
```

The test sees the proposed commit, including partial file changes.
Unstaged changes cannot supply files that accidentally make the test pass.
Test outputs stay in the temporary line. The check result identifies the exact
index state from which that line was created.

## 8. Give each line the same absolute path

Separate Linux mount namespaces can expose each command's line at `/workspace`.
Private mounts prevent mount changes from propagating between these namespaces.
See [Linux mount namespaces](https://man7.org/linux/man-pages/man7/mount_namespaces.7.html).

Potential benefits:

- Commands, logs, and configuration use the same paths across lines.
- An old state runs at the same path as the current state.
- A snapshot supplies a copy of a prepared environment.
- Local and remote runs have fewer path differences.

Build cache reuse needs a separate measurement. An identical path does not prove
that a cache remains valid for different inputs or tools.

## 9. Associate a line with all command processes

Each `layr run` can use a separate cgroup, a Linux process control group.
The cgroup includes descendant processes and supports resource limits and suspension.
See [Linux cgroup v2](https://docs.kernel.org/admin-guide/cgroup-v2.html).

This lets `layr` detect background processes after the initial shell exits.
Line deletion can account for those processes instead of leaving them unnoticed.

A possible protocol for snapshots of multiple layers is:

1. Ask applications to finish their writes.
2. Suspend the command's cgroup and wait for confirmation.
3. Exclude external writers and wait for outstanding input/output operations.
4. Create the layer snapshots.
5. Resume the processes.

This protocol needs an experiment. Process suspension alone does not guarantee
application consistency or an atomic snapshot across layers.

## 10. Use more of the btrfs send protocol

The current design uses one common parent through `btrfs send -p`.
Two additional mechanisms deserve measurement:

- Multiple `-c` options supply additional snapshots as clone sources.
- `--compressed-data` sends compressed file data without first decompressing it.

A receiver with the required support can also write that data without decompression.
Compressed transfer requires send protocol version 2 or later.
See [btrfs send](https://btrfs.readthedocs.io/en/latest/btrfs-send.html).

Clone sources must contain identical data on both machines.
The proposed experiment compares transfer size and processor time across many lines
with shared dependencies. It also compares compressed transfer with external stream compression.

## 11. Set history budgets by disk use

A fixed count of retained states does not account for the size of their changes.
Per-line and per-project disk budgets can supplement retention by count and age.

btrfs qgroups account for shared and exclusive extents. Simple quotas (`squota`)
instead charge extents to the subvolume that first allocated them.
Full qgroup accounting can increase latency with many snapshots.
See [btrfs quota groups](https://btrfs.readthedocs.io/en/latest/Qgroups.html).

The proposed experiment compares both modes with a representative project history.
Space charged to a line is not necessarily the space that deletion of that line
will release. Other snapshots can still reference the same blocks.

## Recommended first capability

`layr run` is the proposed first distinguishing capability. It is useful without
agents or a separate interface: run a command, inspect its changes, run checks,
then accept or discard the result.

This priority does not change the current contract's decision against a staged rollout.

For the technical experiments, start with tests of the index snapshot. Then measure
the fixed `/workspace` path and process control through cgroups.
