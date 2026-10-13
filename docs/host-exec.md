# macOS commands from VM agents

`host_exec` runs one command in a Mac slot and returns its source changes to the
agent's layr line. The tool is available to agents whose chat uses VM mode.

Use these environment variables for output outside the source folder:

- `HOST_EXEC_DERIVED_DATA`: the slot's stable DerivedData folder.
- `HOST_EXEC_PACKAGE_CACHE`: the slot's stable package folder.
- `HOST_EXEC_ARTIFACTS`: files to return to the agent.

For example:

```json
{
  "action": "execute",
  "command": "xcodebuild -version",
  "request_id": "check-xcode"
}
```

The result includes `operationId`, `exitCode`, `reason`, `slotPath`, source
collection output, and artifact file paths. Artifact paths identify files in the
VM that the line owner can read. `hostPath` identifies the Mac copy.

Use `action: status` with `operation_id` to read the saved status and output.
Set `after_seq` to the last sequence already read. Use `action: cancel` with
`operation_id` to stop the command. The timeout applies to the Mac command.
Source synchronization and collection have separate deadlines.

## Channel and identities

The native helper uses a Virtualization.framework listener on port 4051. It
forwards requests to `host-exec.sock` in the VM state folder. The existing port
4050 keeps its host-to-guest role.

At base commit `dea9769`, `connectGuest` creates one connection for each host
client. `bridge` forwards bytes without a request dispatcher. A separate listener
keeps the command lifecycle outside that forwarding path.

The guest service uses a private token. Studio sends it through the existing
host-to-guest channel. Agent users cannot read it. The root broker resolves each
agent's line and Linux owner from its stored association. It executes layr and
file helpers with that owner's credentials.

Each mutation has a durable receipt. An exact retry returns the saved receipt.
A different payload with the same ID fails. A pending receipt after a restart
reports an unknown outcome. It does not repeat the command or a source write.
A lost host owner leaves the slot leased for inspection.

## Slots and source changes

By default, each project has four Mac slots and a 20 GiB disk budget. A project
can change both ("Project settings" below). Idle slots are removed in the order of
their last use. Active slots retain their leases. A command that exceeds the budget
stops. Each slot has a source folder, DerivedData, and package folders at stable paths.

The budget check measures the project folder of the slots at each lease step.
Between two measurements it adds the size of each file that the transfer writes.
It measures again only when this sum reaches the budget. During a command, a
background walk measures the folder. The pause after a walk is at least one second
and at least four times the duration of the walk.

Each guest slot has a private folder for its line owner. The guest saves the line
(`layr save`) and writes that state into the folder with `layr export --all`,
incrementally after the first time; the folder holds that state. Content hashes
select the files to transfer. The Mac checks the complete manifest before the
command starts.

Folders, links and files up to 256 KiB travel in batches. A batch is one read in
the guest and one request to the Mac, with at most 1024 entries and about 1.5 MiB,
below the 2 MiB frame limit. Larger files use 512 KiB chunks. The Mac checks the
SHA-256 of each file. It does not sync the files of a batch to disk: after a crash,
the next command hashes the slot again and sends each damaged file again.

## Hash caches

A hash cache keeps the SHA-256 of each file with its size, modification time,
inode and change time. A scan reads a file again only when one of these four values
changed. The Mac keeps one cache for each slot (`hash-cache.json` in the slot
folder, removed with the slot). The guest keeps one cache for each scanned folder of
a line in `host-slots/<project>/<agent>/hash-cache`; the line owner owns that folder,
and root owns the folders above it. A cache holds only the files of the last scan, plus the files below a skipped path.
A file that changed in the two seconds before the scan, or during its read, stays
out of the cache, so the next scan reads it again.

The Mac compares source content after the command. It sends changed files back
to the guest slot. The guest merges each changed path into the current line
against the held state (`host_exec_slot_io.py`): a path only the Mac changed is
copied, text that both sides changed gets conflict markers, and other files keep
the line version with the Mac version next to it as `<path>.slot-conflict`.
Executable modes and symbolic links are preserved. Hard links become copies.
Linux extended attributes are not copied.

A new slot generation uses an empty guest folder and a full layr sync. A warm
Mac slot receives only content differences, including when a different line
uses it. Output paths map from the source folder to the VM line. The map handles
`/tmp` and `/private/tmp` aliases.

A name conflict stops the command. This includes case and Unicode normalization
conflicts. The adapter checks the layr report because layr returns exit code zero
when it skips conflicting names. A conflict after the command retains the lease
and reports that source collection was refused.

## Project settings

The operator sets the limits of one project in a file on the Mac:
`<Studio state>/linux-vm/layr-projects/<layr project ID>/host-exec.json`. The layr
project ID is the name of the folder that holds the project's `project.json`. For
example, a Chromium fork uses one slot, 300 GiB, and a Mac-only build folder:

```json
{ "slots": 1, "diskBudgetGiB": 300, "macOnly": ["chromium/src/out"] }
```

- `slots`: an integer from 1 to 64. The default is 4.
- `diskBudgetGiB`: a number above 0. The default is 20.
- `macOnly`: paths relative to the line root. The default is no paths.

The Mac service reads the file at the start of each command, so a change needs no
restart. The command result shows the values as `diskBudget` (bytes) and `macOnly`.
An invalid file or an unknown key stops each command of the project with the code
`invalid_config`. The VM cannot write the file, so line content cannot change the
limits. When the slot count decreases, the next command removes idle slots, the
least recently used first.

## Mac-only paths

A Mac-only path stays in the slot between commands and between lines:

- The guest does not send it, also when the line has a folder at that path.
- The source transfer does not delete it. The folders above it stay.
- Changes in it do not return to the line.

Use it for build output in the source folder, for example the ninja `out` folder of
Chromium. Also use it for Mac tools that a setup step downloads into the source
folder. Otherwise the next transfer replaces them with the line version.

## Cache folders

layr keeps cache folders outside states (`layr cache list`; the folders that
`.layrattributes` marks `regenerable`). `layr export --all` does not write their
files. The guest reads the line's cache folders from the line folder itself and
sends them to the slot. Content hashes select the files, so a warm slot receives
only the files that differ from the previous line.

Changes that a Mac command makes in a cache folder do not return to the line. The
next command replaces them with the line version. Do not change a cache folder while
a command transfers it: a changed file stops the transfer with a checksum error.
Then cancel the operation to release its slot.

A command from a line without the cache folder removes the folder from the slot,
but not its Mac-only paths. A plain folder at a cache path (`layr cache list` shows
`plain folder`) is part of the state and follows the normal source rules.

### Clones from the project folder on the Mac

The Mac usually holds the same project folder (`source` in
`layr-projects/<layr project ID>/project.json`), for example a Chromium checkout that
the VM main received as a seeded layer ([vm-layr.md](vm-layr.md)). When a slot has no
copy of a cache folder, or only its Mac-only paths, the Mac clones that folder from
the project folder before the transfer:

- The clone uses APFS `clonefile` for each file, so the files share their blocks
  with the project folder. A failed clone never becomes a byte copy: the Mac removes
  the partial clone and the normal transfer sends the folder.
- The Mac does not clone when the project folder or the cache folder in it is
  missing, or when the path crosses a symbolic link.
- Links stay links. Sockets and pipes stay out.
- The slot keeps its own Mac-only paths. The clone has the Mac-only paths of the
  project folder only when the slot had none, for example the Mac tools of a hook.
- Then the hash comparison sends only the files of the line that differ.

The budget counts a clone at its full size (`st_blocks`), as the walk of the slot
folder counts it, although the clone shares its blocks. So the budget can stop a
command before the disk is full, but not after. The command result lists each cache
folder in `primed`, with the reason when the Mac did not clone it. The first scan of
a clone reads every file, because the clone has new inodes.

## Checks

Run checks one at a time:

```sh
python3 -B workspaces/runtime/apps/server/tests/host-exec-contract.py
python3 -B workspaces/runtime/apps/vm-guest/test_host_exec_slot_io.py
python3 -B workspaces/runtime/apps/vm-guest/test_service.py
npm run typecheck:runtime
xcrun swiftc -typecheck -target arm64-apple-macos13.0 -framework Virtualization desktop/native/linux-vm/main.swift
```

The manual benchmark `workspaces/runtime/apps/server/tests/host-exec-transfer-bench.py`
times a cold slot, a warm slot and a small change (`--files 50000`; `--cache` puts
the files in a cache folder). It runs the file helper as a process, without the VM
socket and without real layr.

The contract uses a layr fixture. It does not prove VM transport or real layr
collection. The live check must use the native VM, real layr, `xcodebuild -version`,
a Swift build, a source edit from the Mac, and a second command at the same slot
path.
