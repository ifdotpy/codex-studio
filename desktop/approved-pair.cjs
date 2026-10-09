const { serveOrigin } = require("./server-credentials.cjs");

async function pairApproved({
  origin,
  value,
  credentials,
  fetchRequest = fetch,
  trusted,
}) {
  const approval = value.approval;
  const target = serveOrigin(value.origin);
  if (
    !approval ||
    ![
      approval.localServerId,
      approval.serverId,
      approval.inviteRequestId,
      value.requestId,
    ].every(
      (id) => typeof id === "string" && /^[a-zA-Z0-9_-]{1,128}$/.test(id),
    ) ||
    typeof approval.generation !== "string" ||
    approval.generation.length > 8192
  )
    throw new Error("Invalid approved server request.");
  const read = async (path, init) => {
    trusted();
    const response = await fetchRequest(origin + path, {
      ...init,
      redirect: "error",
      signal: AbortSignal.timeout(15000),
    });
    const result = await response.json();
    trusted();
    if (!response.ok)
      throw new Error(result.error || "The local server access check failed.");
    return result;
  };
  const check = async () => {
    const state = await read("/api/multi-server");
    const peer = state.servers?.find(
      (row) => row.serverId === approval.serverId,
    );
    if (
      state.protocol !== 1 ||
      state.identity?.serverId !== approval.localServerId ||
      !peer ||
      peer.kind !== "server" ||
      peer.status !== "paired" ||
      serveOrigin(peer.origin) !== target ||
      JSON.stringify([peer.created, peer.publicKey]) !== approval.generation
    )
      throw new Error("This server is not approved or its identity changed.");
    return peer;
  };
  const peer = await check();
  const session = await read("/api/session");
  if (typeof session.token !== "string" || !session.token)
    throw new Error("The local server session is unavailable.");
  const result = await read("/api/multi-server", {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      "X-Canvas-Token": session.token,
    },
    body: JSON.stringify({
      action: "ui_invite",
      serverId: approval.serverId,
      requestId: approval.inviteRequestId,
    }),
  });
  const invitation = result.invitation;
  if (
    !invitation ||
    invitation.serverId !== peer.serverId ||
    invitation.publicKey !== peer.publicKey ||
    serveOrigin(invitation.origin) !== target ||
    [
      "protocol",
      "inviteId",
      "token",
      "serverId",
      "origin",
      "publicKey",
      "tailscaleUser",
      "expires",
    ].some((key) => invitation[key] !== value.invitation?.[key])
  )
    throw new Error(
      "The invitation does not match this approved server request.",
    );
  await check();
  return credentials.pair({
    origin: target,
    invitation,
    requestId: value.requestId,
  });
}
module.exports = { pairApproved };
