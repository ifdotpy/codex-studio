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

Each project has four Mac slots and a 20 GiB disk budget. Idle slots are removed
in the order of their last use. Active slots retain their leases. A command that
exceeds the budget stops. Each slot has a source folder, DerivedData, and package
folders at stable paths.

Each guest slot has a private folder for its line owner. The guest saves the line
(`layr save`) and writes that state into the folder with `layr export --all`,
incrementally after the first time; the folder holds that state. Content hashes
select the files to transfer. File data uses 512 KiB chunks. The Mac checks the
complete manifest before the command starts.

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

## Checks

Run checks one at a time:

```sh
python3 -B tests/host-exec-contract.py
python3 -B workspaces/runtime/apps/vm-guest/test_service.py
npm run typecheck:runtime
xcrun swiftc -typecheck -target arm64-apple-macos13.0 -framework Virtualization desktop/native/linux-vm/main.swift
```

The contract uses a layr fixture. It does not prove VM transport or real layr
collection. The live check must use the native VM, real layr, `xcodebuild -version`,
a Swift build, a source edit from the Mac, and a second command at the same slot
path.
