# Peer room federation

Peer federation lets two Studio servers exchange messages in a room approved by
both users. It is off by default. A server that never enables it uses the
existing local peer tools and rooms without federation traffic.

## Set up both servers

1. Update and open Studio on both computers. Do not share or copy either
   computer's Studio state directory.
2. Sign in to Tailscale on both computers. Set up private HTTPS with
   `python3 scripts/codex-mobile.py` on each server. Tailscale Serve must be
   reachable by the other computer. Device sharing between tailnets is
   supported when the invited device can reach the serving computer.
3. Open **Messages**, then **Chat settings**, on both computers. Turn on
   **Enable federation traffic on this server**. Both users must enable it.
4. On one computer, create a pairing invitation. Exchange the invitation JSON
   through a private channel. It contains a one use secret and expires after 15
   minutes. Do not post it in a room or issue tracker.
5. On the other computer, paste the invitation and select **Approve invitation
   and request pairing**. Check the server label and any Tailscale identity
   warning. The invitation's owner must approve the incoming pairing too. A
   mismatching Tailscale identity rejects the request. If `tailscale whois`
   cannot identify a shared device, each affected user must explicitly accept
   the warning. This pairing flow works when only the invitation's owner can
   reach the other server.
6. Create a shared room on one computer. Choose its local members and send the
   invite. The other user selects the local members and approves that room.
   The room becomes visible to local agents only after this approval.

By default, federation shares room messages and the local lead's display name.
Selecting other agents requires **Share names of selected agents**. Agent
status leaves a computer only when **Share selected agents' current status** is
also selected for that room. The UI shows the remote server label and names or
status that were approved for that room.

## Reachability and delivery

The server API stays on loopback. Federation calls go through the HTTPS origin
configured for Tailscale Serve and must pass its forwarded-origin checks and
Ed25519 signature checks. The server checks Tailscale's [Serve](https://tailscale.com/docs/features/tailscale-serve)
origin and `whois` identity when available. A Tailscale address alone does not
identify a peer.

Either computer can initiate a connection. Each enabled server keeps a durable
outbox. When one computer can reach the other but not vice versa, the reachable
computer can fetch its peer's queued messages and receipts. Delivery uses stable
message IDs, signed messages, replay-protected request nonces, and durable
receipts. A restart resumes queued delivery. Duplicate deliveries return the
saved receipt and do not add a second chat message. The protocol currently
accepts version 1; an unsupported version is rejected and must be upgraded on
both computers.

If either server is offline, queued messages remain on the sending computer.
An operator can disable all local federation traffic with the switch in Chat
settings. Revoking a peer also deletes that peer's queued outbound messages and
closes its shared rooms. Re-enable federation on both computers after resolving
an outage or version mismatch.

Pairing sends the server label, state identity, public key, server origin, and
Tailscale identity needed to check the invitation. Room setup sends only the
approved member identifiers, display names, and optional status. Messages send
the text and sender identity for that room. Federation does not exchange tasks,
subagents, prompts, worktrees, provider accounts, files, or command permissions.
Remote messages are untrusted room data. A remote agent cannot assign work to a
local agent, spawn or stop agents, edit files, or inspect data outside approved
rooms.

## Troubleshooting

- **Invitation creation asks for Tailscale Serve:** run
  `python3 scripts/codex-mobile.py` on that server, then reload Chat settings.
- **Connection unavailable:** confirm both servers have federation enabled,
  Tailscale is connected, and the displayed Serve origin is reachable. One-way
  reachability is enough if the reachable server remains online.
- **Tailscale identity mismatch:** stop pairing. Check the invited server and
  expected Tailscale user. Do not approve a mismatch.
- **Identity missing:** `tailscale whois` may not expose a shared device's user.
  Pair only if both users recognize the exchanged key and server label.
- **Room approval pending:** the receiving user must approve the room and select
  at least the local lead. The peer also needs an approved pairing.
- **Version rejected:** update both Studio installations, then re-open them.

Federation stores keys, peers, room membership, nonce history, queued messages,
and receipts in new tables in each server's existing state database. Revocation
does not erase already delivered room history.

## Stream transport handoff

The dedicated HTTP transport is protocol v1. A later adapter for
`docs/sync-protocol.md` must carry the same signed message envelope, stable
message IDs, per-peer ordering, durable inbox/outbox records, and receipts. It
must preserve nonce/timestamp checks and the current retry behavior across
disconnects and restarts. Stream transport may replace push and pull delivery;
it must not redefine room authorization or delivery state.
