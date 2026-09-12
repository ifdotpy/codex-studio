# Legacy panel feed compatibility

`orchestration_panel_feed` is retired. Studio now reads an ordinary per-agent
`PROGRESS.md` file. Follow [the progress guide](panel.md) and use the exact path
from your runtime instructions or `orchestration_context topic=panel`.

Use ordinary file tools to record verified status. The display does not connect
a command's stdout, execute a script, or wake the model. A separate script can
write the file for automatic status updates. Do not poll unchanged status through
model calls.

Stored legacy panel documents, callback receipts, and feed records remain intact.
The change does not cancel active work or copy legacy state into `PROGRESS.md`.
Existing pending callbacks can finish under their original identities.
The retained EC2 producer and JSON assets belong to the legacy implementation;
they are not the current panel workflow.

Continue to use the existing monitor tools for commands that need a result.
A resource's service status does not prove that a build or agent uses it.
Check the actual command result and resource reservation before making that claim.
