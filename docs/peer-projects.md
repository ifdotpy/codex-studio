# Projects of paired servers

A browser client has no server registry and no signed transport to paired servers. Its own
server therefore publishes the projects of each paired server, so that the browser lists them.

## Design

The owner is `runtime-services/src/peer_projects.rs`.

- The server reads each paired server's project list with the existing `projects` action of the
  signed orchestration exchange.
- It publishes each project as a `project` sync entity with the id `peer:<server>:<project>`,
  `homeServerId` set to the paired server and `serverLabel` set to the name of that server. The
  entity travels on the normal `/api/sync/stream`; no client reads a peer.
- A project that this server already lists under another identity (a project alias that names the
  paired server) is not published twice.
- The web client puts entities with a `serverLabel` in `runtime.remoteProjects` and keeps
  `runtime.projects` for local projects. The sidebar shows them read-only, grouped by server name.

## Refresh events

No timer reads a peer. A refresh runs:

1. at startup, and when a server becomes paired;
2. when a paired server that failed a read makes a signed request to this server again;
3. when a paired server sends `projects.changed` after its own project list changed (the
   observed table `runtime_projects`).

A failed read keeps the published projects unchanged. An unpaired server loses its entities.

## Limits

- The browser lists remote projects and their server name. It cannot open their chats, because
  chats of a paired server need the signed transport of a native client.
- A paired server that runs an older build answers the reads (events 1 and 2) but sends no
  `projects.changed` notice. Its changes appear at the next contact or restart.
