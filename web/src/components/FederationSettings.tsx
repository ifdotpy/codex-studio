import { useEffect, useMemo, useState } from "react";
import { Button, Checkbox, Switch, Textarea, TextInput } from "@mantine/core";
import { errorText, post, type PostBody, type PostResult } from "../api";
import type { Agent } from "../types";

type FederationResponse = PostResult<"/api/federation">;
type FederationSnapshot = Extract<FederationResponse, { peers: unknown[] }>;
type FederationInvitation = Extract<
  FederationResponse,
  { invitation: unknown }
>["invitation"];

function isFederationInvitation(value: unknown): value is FederationInvitation {
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const invitation = value as Record<string, unknown>;
  return (
    invitation.protocol === 1 &&
    typeof invitation.inviteId === "string" &&
    typeof invitation.token === "string" &&
    typeof invitation.stateId === "string" &&
    typeof invitation.label === "string" &&
    typeof invitation.origin === "string" &&
    typeof invitation.publicKey === "string" &&
    typeof invitation.expires === "number"
  );
}

export function FederationSettings({
  active,
  leadId,
  agents,
  refresh,
  notify,
}: {
  active: boolean;
  leadId: string;
  agents: Agent[];
  refresh: () => Promise<void>;
  notify: (message: string) => void;
}) {
  const [state, setState] = useState<FederationSnapshot | null>(null);
  const [inviteText, setInviteText] = useState("");
  const [serverLabel, setServerLabel] = useState("Studio server");
  const [expectedUser, setExpectedUser] = useState("");
  const [localMembers, setLocalMembers] = useState<string[]>([leadId]);
  const [shareNames, setShareNames] = useState(false);
  const [shareStatus, setShareStatus] = useState(false);
  const [busy, setBusy] = useState(false);
  const team = useMemo(
    () =>
      agents.filter((agent) => agent.id === leadId || agent.rootId === leadId),
    [agents, leadId],
  );
  useEffect(() => setLocalMembers([leadId]), [leadId]);

  const run = async (
    body: PostBody<"/api/federation">,
  ): Promise<FederationResponse | null> => {
    setBusy(true);
    try {
      const result = await post("/api/federation", body);
      if ("peers" in result) setState(result);
      void refresh().catch((error) => notify(errorText(error)));
      return result;
    } catch (error) {
      notify(error instanceof Error ? error.message : String(error));
      return null;
    } finally {
      setBusy(false);
    }
  };
  useEffect(() => {
    if (!active) return;
    void run({ action: "status" });
    // Settings refresh only when the settings panel opens.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [active]);

  if (!active) return null;
  const toggleMember = (id: string, checked: boolean) =>
    setLocalMembers((current) =>
      checked
        ? [...new Set([...current, id])]
        : current.filter((item) => item !== id),
    );
  const pair = async () => {
    try {
      const invitation: unknown = JSON.parse(inviteText);
      if (!isFederationInvitation(invitation)) throw new Error();
      const result = await run({ action: "accept_peer", invitation });
      if (result) setInviteText("");
    } catch {
      notify("Paste a valid federation invitation JSON document.");
    }
  };

  return (
    <section
      className="settings-group federation-settings"
      aria-label="Peer federation"
    >
      <h2>Peer federation</h2>
      <p>Share rooms with another Studio.</p>
      <Switch
        label="Enable federation traffic on this server"
        checked={state?.enabled === true}
        disabled={busy || !state}
        onChange={(event) =>
          void run({
            action: "set_enabled",
            enabled: event.currentTarget.checked,
          })
        }
      />
      {state?.identity && (
        <p className="federation-identity">
          Server identity: {state.identity.label} · key{" "}
          {state.identity.fingerprint}
        </p>
      )}
      <section className="federation-invitation">
        <h3>Create an invitation</h3>
        <TextInput
          label="This server's label"
          value={serverLabel}
          onChange={(event) => setServerLabel(event.currentTarget.value)}
          maxLength={80}
        />
        <TextInput
          label="Expected Tailscale user (optional)"
          description="A mismatch rejects pairing. A missing identity requires explicit acceptance."
          value={expectedUser}
          onChange={(event) => setExpectedUser(event.currentTarget.value)}
          maxLength={254}
        />
        <Button
          variant="light"
          disabled={busy || !state?.enabled}
          onClick={async () => {
            const result = await run({
              action: "create_invite",
              label: serverLabel,
              expected_user: expectedUser || undefined,
            });
            if (result && "invitation" in result)
              setInviteText(JSON.stringify(result.invitation, null, 2));
          }}
        >
          Create pairing invitation
        </Button>
      </section>
      <section className="federation-invitation">
        <h3>Accept an invitation</h3>
        <p>
          Create your invitation and exchange it privately with the other user.
          Paste their invitation below. It expires after 15 minutes.
        </p>
        <Textarea
          label="Exchange invitation JSON privately"
          value={inviteText}
          onChange={(event) => setInviteText(event.currentTarget.value)}
          minRows={4}
          autosize
          maxRows={12}
        />
        <Button
          disabled={busy || !state?.enabled || !inviteText.trim()}
          onClick={() => void pair()}
        >
          Approve pairing
        </Button>
      </section>
      {state?.peers.map((peer) => (
        <div className="federation-peer" key={peer.stateId}>
          <strong>{peer.label}</strong>
          <span>{peer.status === "approved" ? "Paired" : peer.status}</span>
          {peer.whoisStatus === "missing" && (
            <p role="alert">
              Tailscale could not verify this user. Continue only if you
              recognize and accept this peer.
            </p>
          )}
          {peer.lastError && <p role="status">{peer.lastError}</p>}
          {peer.status !== "approved" && peer.status !== "revoked" && (
            <Button
              size="xs"
              disabled={busy}
              onClick={() =>
                void run({
                  action: "approve_peer",
                  state_id: peer.stateId,
                  accept_missing_whois: peer.whoisStatus === "missing",
                })
              }
            >
              {peer.whoisStatus === "missing"
                ? "Accept missing identity and pair"
                : "Approve pairing"}
            </Button>
          )}
          {peer.status !== "revoked" && (
            <Button
              size="xs"
              color="red"
              variant="subtle"
              disabled={busy}
              onClick={() =>
                void run({ action: "revoke_peer", state_id: peer.stateId })
              }
            >
              Revoke
            </Button>
          )}
        </div>
      ))}
      {state?.peers.some((peer) => peer.status === "approved") && (
        <div className="federation-room-create">
          <h3>Create a shared room</h3>
          <p>
            By default, only the local lead display name is shared. Extra names
            and status require opt-in.
          </p>
          {team.map((agent) => (
            <Checkbox
              key={agent.id}
              label={agent.isLead ? `${agent.name} (lead)` : agent.name}
              checked={localMembers.includes(agent.id)}
              disabled={busy || (agent.id !== leadId && !shareNames)}
              onChange={(event) =>
                toggleMember(agent.id, event.currentTarget.checked)
              }
            />
          ))}
          <Checkbox
            label="Share names of selected agents"
            checked={shareNames}
            onChange={(event) => setShareNames(event.currentTarget.checked)}
          />
          <Checkbox
            label="Share selected agents’ current status"
            checked={shareStatus}
            onChange={(event) => setShareStatus(event.currentTarget.checked)}
          />
          {state.peers
            .filter((peer) => peer.status === "approved")
            .map((peer) => (
              <Button
                key={peer.stateId}
                variant="light"
                disabled={
                  busy || !state.enabled || !localMembers.includes(leadId)
                }
                onClick={() =>
                  void run({
                    action: "create_room",
                    peer_id: peer.stateId,
                    local_members: localMembers,
                    share_names: shareNames,
                    share_status: shareStatus,
                  })
                }
              >
                Invite {peer.label} to a room
              </Button>
            ))}
        </div>
      )}
      {state?.rooms.map((room) => (
        <div className="federation-room" key={room.id}>
          <strong>{room.peerLabel}</strong>
          <span>
            {room.status === "approved"
              ? "Shared room"
              : "Room approval pending"}
          </span>
          {room.status === "pending" && room.localMembers.length === 0 && (
            <>
              <p>
                Approve access for the local lead. Select extra members only
                when needed.
              </p>
              {team.map((agent) => (
                <Checkbox
                  key={`${room.id}:${agent.id}`}
                  label={agent.isLead ? `${agent.name} (lead)` : agent.name}
                  checked={localMembers.includes(agent.id)}
                  disabled={busy || (agent.id !== leadId && !shareNames)}
                  onChange={(event) =>
                    toggleMember(agent.id, event.currentTarget.checked)
                  }
                />
              ))}
              <Checkbox
                label="Share names of selected agents"
                checked={shareNames}
                onChange={(event) => setShareNames(event.currentTarget.checked)}
              />
              <Checkbox
                label="Share selected agents’ current status"
                checked={shareStatus}
                onChange={(event) =>
                  setShareStatus(event.currentTarget.checked)
                }
              />
              <Button
                disabled={busy || !localMembers.includes(leadId)}
                onClick={() =>
                  void run({
                    action: "approve_room",
                    room_id: room.id,
                    local_members: localMembers,
                    share_names: shareNames,
                    share_status: shareStatus,
                  })
                }
              >
                Approve room
              </Button>
            </>
          )}
        </div>
      ))}
      {state && state.queued > 0 && (
        <p role="status">{state.queued} message(s) waiting for delivery.</p>
      )}
    </section>
  );
}
