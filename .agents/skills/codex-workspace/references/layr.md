# Layr chats

This guidance applies when the chat workspace mode is `layr`.
The lead, workers, and reviewers run in the Studio Linux VM.
The VM main line is the project source of truth after the first import.
The Mac project folder remains separate for native chats.
Export a copy only when the user asks for it.

Use `layr` for all local version control commands.
Use `layr status`, `layr diff`, `layr log`, and `layr show` to inspect the work.
Use `layr add` and `layr commit` to save the task result.
Submit the full state ID from `layr rev-parse HEAD` as the task revision.
Use Git only for the remote bridge and tools that call Git themselves.
Use `layr push --squash -m <task title>` to export one task commit to the remote when authorized.

The lead owns the main line and may commit in it.
Each worker has its own line and Linux user.
Only the owner of a line changes it.
Reviewers have read-only lines.
Studio saves a state after every agent turn.
`layr access check <user> <action>` explains a refused command.
Inspect the exact submitted state before accepting the result.
Acceptance calls `layr merge <line> --expect <reviewed state>`.
A changed line rejects acceptance.
Resolve merge conflicts in the main line with `layr`.
Archive or removal deletes the worker line and keeps its states.

Use `host_exec` for commands that require macOS.
It transfers the selected state to a Mac slot and returns the command result.
Do not treat the Mac native project folder as a copy of the VM main line.
The read-only Mac share exposes the VM main line and states.

The host owns account sign-in and credential refresh.
The guest receives private access credentials through the existing account sync.
Do not copy refresh credentials or create another account store.
Preserve request IDs after a disconnect.
A disconnect does not prove that the operation failed.

Existing sessions keep their saved tools and role guidance.
This guidance applies only to new layr sessions.
