# macOS commands from VM agents

`host_exec` runs one command in a Mac slot and returns its source changes to the
agent's layr line. The tool is available to agents whose chat uses VM mode.

Use these environment variables for output outside the source folder:

- `HOST_EXEC_DERIVED_DATA`: the slot's stable DerivedData folder.
- `HOST_EXEC_PACKAGE_CACHE`: the slot's stable package folder.
- `HOST_EXEC_ARTIFACTS`: files to return to the agent.

A command also gets `BROWSER`, `STUDIO_BROWSER_SOCKET` and `STUDIO_BROWSER_SESSION`. A sign-in
that the command opens in a browser then shows a card in the calling chat, and its result
goes to the command on the Mac ([VM browser bridge](vm-browser-bridge.md), "Native chats").

`HOST_EXEC_STATE` is the ID of the layr state that the slot holds: the guest saves the line
and writes that state into the slot before the command. The `.git` folder of the slot is the
line's git layer and can name an older commit; use this ID to identify the sources.

`HOST_EXEC_TREE` is the git tree ID of that state: the 40 hexadecimal digits that
`layr rev-parse <state>^{tree}` prints in the guest. It is the tree of the commit that
`layr git push` exports for the state, so it equals `git rev-parse HEAD^{tree}` after a git
sync of that state. Use it to record the whole source tree of a build. A tree computed
in the slot is not this value: the slot holds ignored build output, and layr-only files
of the state do not have to be there. The
guest asks layr for the tree when it saves the line for the command. A layr from before
2026-10-17 has no tree form; then the command gets no `HOST_EXEC_TREE`, never a guessed
value. The Mac service (`runtime-services/src/host_exec_service/mod.rs`) owns the names
of these variables.

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

Use `action: list` to find your operations without an ID, for example after a lost
result. A lost result names its operation in the error. Use `action: release` with
`operation_id` to free the slot of a command that exited, or that did not start,
when no request of the guest runs it. The Mac refuses to release a running command.
The output of a released command stays readable with `status`. Its changes and
artifacts stay in the Mac operation folder and do not return to the line.

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

A Studio restart during a call never repeats the command. Studio saves the exact
guest request (request ID and parameters) on the tool receipt before it sends it.
The guest keeps a receipt for each `host.exec` request ID. After the restart, the
call answers "outcome unknown" with its operation ID. Read the result with `status`
or `list`; do not repeat the command. Studio does not yet send the saved request
again to deliver the real result to the waiting caller.

A failure before the run request (slot preparation, manifest, or transfer)
releases the Mac slot. The saved status records `sync_failed` and `preRunError`.
The Mac slot gets a new generation, so failed guest staging is never used again.
A failure to forget the old guest slot appears in the error. An explicit `sync_conflict` response to the run request also releases both leases
and changes the slot generation: the server checks the manifest before command startup.
A lost response to the run request keeps the lease for inspection.

### Guest restarts

The guest service runs each command request in its own process, so a VM crash or a
guest service restart ends every request of that process. The guest has one service
process at a time. Each process has a random owner ID, and `acquire` records it in the
operation. Before its first Mac request, a new process sends `recover` with its owner ID.
The Mac then handles each leased operation of another owner:

- An exited command or an unstarted command releases its slot at once.
- A running command keeps its slot. The slot becomes free when the command exits.
- A command whose run or prime request is in progress never starts. Its slot becomes free.
- An unknown outcome of a lost Mac owner keeps its slot for inspection.

These releases (and `action: release`) give the slot a new generation, because no guest
request owns the old guest staging. The saved results stay readable. On 2026-10-20 a
VM restart left a Chromium slot leased: the run request arrived before the crash, the
Mac still hashed the slot when the VM stopped and started the command after the restart,
and no guest request remained to collect and release it.

Keep a command that uses the slot in the `host_exec` command lifetime. Do not detach
it into a new session and let `host_exec` finish. The next execute synchronizes the
source tree, even if its command only reads logs. A detached process can then read
or change files during that transfer. Use `status` to read output without a new
source transfer. Keep independent background jobs in separately owned workspaces.

The Mac service is a separate process, `studio-runtime-server host-exec-service
<vm-state-folder> --build <id>`, in its own session. A running Mac command survives
a backend restart, and the service keeps its code after an app update. Its health
reports the build of the backend that started it. Before each `host_exec` call, the
backend makes sure that a service of its own build serves the VM state folder: it
creates the channel token once, probes `host-exec.sock`, and starts the service when
none answers. An idle service of another build accepts an authenticated retire
request and stops; then the backend starts its own build. A running command or
request keeps the old service, and the backend reports the reason; a later call
replaces it. A service without a build identity is never stopped by the backend.
Receipts, tokens, leases, and unknown outcomes stay in the same state folder. The
runtime-server witnesses `host_exec_service::*` check this with real service processes.
After securing its Unix socket, the service emits the stdout readiness line owned by
[`SERVICE_READY_LINE`](../workspaces/runtime/packages/runtime-services/src/host_exec_service/mod.rs).
The launcher writes stdout to the service log; fixtures await that line before using the socket.

### Backend

The backend gives the tool to VM agents that are not reviewers
(`runtime-services/src/host_exec.rs`, which owns the tool contract and the bundled
definition `host_exec_tool.json`). Before each call, it checks the Mac service as
above and sends `host.configure` with the channel token, because the backend does
not start the VM. The guest calls run on their own thread, so a long command never
holds a thread of the backend's blocking pool.

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

A file with the same size and content but another mode gets only a mode change, on
the Mac and on the way back into the line. A seeded layer can have other file modes
than the project folder on the Mac, so a cloned file would otherwise travel again.
The Mac checks the content against its hash cache, or reads the file when the cache
has no current entry, and keeps the hash for the run check.

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

A changed path that the held state does not have and that the line ignores (`.gitignore`,
`.layrignore`) is build output of the Mac command, for example a Mach-O binary in `target/`.
It stays on the Mac, also when the line has its own file at that path, and the collect output
counts it as kept. Without this rule a Mac build in a line that had no `target` cache yet put
its binaries into the Linux line, and a Linux build took them as its own output. An ignored file
that is part of the state, such as an imported `.env`, still returns. To check the ignore rules,
the guest puts an empty placeholder at each such path for one `layr status --ignored` run and
removes it again.

A new slot generation uses an empty guest folder and a full layr sync. A warm
Mac slot receives only content differences, including when a different line
uses it. Output paths map from the source folder to the VM line. The map handles
`/tmp` and `/private/tmp` aliases.

### Guest staging cleanup

Each guest slot folder is
`/var/lib/codex-studio/host-slots/<project>/<agent>/<slot ID>/<generation folder>`. The
Mac gives each new generation a random ID and a generation number. The number comes from
a counter of the project folder (`.generation-counter.json`), so the numbers of one slot
only grow, also after a slot was removed and made again. `acquire` returns both values,
and the guest folder name is `<number>-<ID>`. A slot from before generation numbers keeps
its old folder name, the ID only, until its next new generation.

`host.slot.prepare` removes the staging of older generations of the same agent and slot.
It does this after it writes the lease of the current generation and before the layr
export. A folder is removed only when all of these are true:

- The broker root resolves to `/var/lib/codex-studio/host-slots` (`GUEST_SLOTS_ROOT` in
  `host_exec_slot.py`). The broker refuses an empty root, `/`, or a home folder.
- The folder is a direct child of the slot folder, and its name is `<number>-<ID>`.
- Its number is lower than the number of the current generation.
- It is a real folder of the broker user. A symbolic link is skipped and never followed.
- It has no guest lease (`lease.json`), and no broker request uses it now.
- The platform has the file descriptor based `shutil.rmtree`, which does not follow a link
  that the line owner puts into the staging.

A folder with the ID-only name of an earlier version is never removed; the broker logs it.
The broker logs each removed folder. A request for a folder under removal fails with
`lease_lost`.

When `recover` releases the slot of an earlier guest owner, its result lists the released
slots. The new guest service then forgets the guest lease of each one
(`host.slot.forget`). `action: release` forgets the guest lease in the same way. The next
generation of that slot can then remove the old staging. A forget that fails keeps the
staging and the lease.

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
`plain folder` or `not a cache`) is part of the state and follows the normal source rules.

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

Before the guest sends the content of a file above 256 KiB, the Mac clones that
file from the same path in the project folder when the file there has the same size
and SHA-256. For example, a slot that lost a Git pack gets it back without a
transfer. A hash cache of the project folder (`host-exec-hashes.json` in
`layr-projects/<layr project ID>/`, at most 100,000 files) keeps the hashes of the
files that the Mac checked, and the run check does not read the clone again. The
command result shows these bytes as `clonedBytes`. Small files travel in batches
without this check, and the Mac does not look for the content at another path.

The budget counts a clone at its full size (`st_blocks`), as the walk of the slot
folder counts it, although the clone shares its blocks. So the budget can stop a
command before the disk is full, but not after. The command result lists each cache
folder in `primed`, with the reason when the Mac did not clone it. The first scan of
a clone reads every file, because the clone has new inodes.

## Checks

Run checks one at a time:

```sh
cargo test -p studio-runtime-services --lib host_exec
cargo nextest run -p studio-runtime-server --test integration -E 'test(/^host_exec_service::/)'
python3 -B workspaces/runtime/apps/vm-guest/test_host_exec_slot_io.py
python3 -B workspaces/runtime/apps/vm-guest/test_service.py
```

The runtime-server witness `the_guest_broker_runs_against_the_mac_service` runs the
unchanged guest broker (`host_exec.py` and `host_exec_slot.py`) with fixture layr
against a real service process (`vm-guest/host_exec_mac_contract.py`). It needs
`python3` 3.11 or newer on `PATH`.

The contract uses a layr fixture. It does not prove VM transport or real layr
collection. The live check must use the native VM, real layr, `xcodebuild -version`,
a Swift build, a source edit from the Mac, and a second command at the same slot
path.
