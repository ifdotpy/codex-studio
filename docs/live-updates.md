# Server updates

The removed Python backend's in-process patch publisher is no longer available.
Do not publish its temporary patch files or live-update manifests to the Rust server.

Use `just deliver-server <commit>` to update the live server on this Linux host.
That recipe owns release snapshots, the current link, service restart, health
verification, and rollback. Preserve the existing state-directory identity and
active work. Never edit service drop-ins or binary paths by hand.

For installation and update commands, see [the CLI guide](cli.md) and
[the product guide](product-guide.md). Historical patch evidence describes only
the revision where it was collected.
