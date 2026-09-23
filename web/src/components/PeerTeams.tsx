import {
  ActionIcon,
  Button,
  Checkbox,
  Menu,
  TextInput,
  UnstyledButton,
} from "@mantine/core";
import { ChevronDown, MoreHorizontal } from "lucide-react";
import { useRef, useState, type HTMLAttributes, type ReactNode } from "react";
import { api, ApiError, errorText } from "../api";
import type { Agent, PeerTeam } from "../types";
import type { Project } from "./ProjectOrganization";
import "./peer-teams.css";

export function PeerTeamGroup({
  team,
  closed,
  toggle,
  edit,
  dissolve,
  children,
  drop,
}: {
  team: PeerTeam;
  closed: boolean;
  toggle: () => void;
  edit: () => void;
  dissolve: () => void;
  children: ReactNode;
  drop?: HTMLAttributes<HTMLElement> & { "data-folder-drop"?: string };
}) {
  return (
    <section className="peer-team" data-peer-team={team.id} {...drop}>
      <div className="project-tree-heading">
        <UnstyledButton
          className="project-tree-toggle peer-team-toggle"
          aria-expanded={!closed}
          onClick={toggle}
        >
          <ChevronDown size={14} />
          <span>{team.name}</span>
        </UnstyledButton>
        <Menu withinPortal position="bottom-end">
          <Menu.Target>
            <ActionIcon
              className="project-tree-action"
              aria-label={`Options for team ${team.name}`}
            >
              <MoreHorizontal size={15} />
            </ActionIcon>
          </Menu.Target>
          <Menu.Dropdown>
            <Menu.Item onClick={edit}>Edit team</Menu.Item>
            <Menu.Item onClick={dissolve}>Dissolve team</Menu.Item>
          </Menu.Dropdown>
        </Menu>
      </div>
      {children}
    </section>
  );
}

export function PeerTeamForm({
  project,
  team,
  teams,
  agents,
  action,
  refresh,
  close,
}: {
  project: Project;
  team?: PeerTeam;
  teams: PeerTeam[];
  agents: Agent[];
  action: "save" | "delete";
  refresh?: () => Promise<void>;
  close: () => void;
}) {
  const [name, setName] = useState(team?.name || "");
  const [members, setMembers] = useState(team?.members || []);
  const [error, setError] = useState("");
  const [pending, setPending] = useState(false);
  const [frozen, setFrozen] = useState(false);
  const [rejected, setRejected] = useState(false);
  const request = useRef<Record<string, unknown> | null>(null);
  const committed = useRef(false);
  const locked = useRef(false);
  const id = useRef(team?.id || crypto.randomUUID());
  const revision = useRef(project.peerTeamsRevision || 0);
  const available = agents.filter(
    (a) =>
      a.source === "managed" &&
      a.isLead &&
      !a.deletedAt &&
      a.cwd === project.path &&
      !teams.some((t) => t.id !== team?.id && t.members.includes(a.id)),
  );
  const submit = async () => {
    if (locked.current) return;
    locked.current = true;
    setPending(true);
    setError("");
    setFrozen(true);
    request.current ||= {
      action,
      path: project.path,
      team_id: id.current,
      expected_revision: revision.current,
      request_id: crypto.randomUUID(),
      ...(action === "save" ? { name: name.trim(), members } : {}),
    };
    try {
      if (!committed.current) {
        await api("/api/peer-teams", request.current, { timeoutMs: 15000 });
        committed.current = true;
      }
      await refresh?.();
      close();
    } catch (e) {
      setError(errorText(e));
      if (
        !committed.current &&
        e instanceof ApiError &&
        e.status >= 400 &&
        e.status < 500
      )
        setRejected(true);
    } finally {
      locked.current = false;
      setPending(false);
    }
  };
  return (
    <form
      className="peer-team-form"
      aria-label={
        action === "delete" ? "Dissolve team" : team ? "Edit team" : "New team"
      }
      onSubmit={(e) => {
        e.preventDefault();
        void submit();
      }}
    >
      <strong>
        {action === "delete"
          ? `Dissolve ${team?.name}?`
          : team
            ? "Edit team"
            : "New team"}
      </strong>
      {action === "delete" ? (
        <p>The chats stay in this project.</p>
      ) : (
        <>
          <TextInput
            label="Team name"
            value={name}
            maxLength={80}
            disabled={frozen}
            onChange={(e) => setName(e.currentTarget.value)}
            autoFocus
          />
          <p>Chosen chats can message each other.</p>
          <div className="peer-team-options">
            {available.map((a) => (
              <Checkbox
                key={a.id}
                label={a.name}
                checked={members.includes(a.id)}
                disabled={frozen}
                onChange={(e) =>
                  setMembers(
                    e.currentTarget.checked
                      ? [...members, a.id]
                      : members.filter((id) => id !== a.id),
                  )
                }
              />
            ))}
          </div>
          {!available.length && <p>No available chats.</p>}
        </>
      )}
      {error && (
        <p role="alert">
          {error}
          {committed.current
            ? " The change was saved. Retry to refresh the sidebar."
            : rejected
              ? " Reload the project before you edit again."
              : " The result is unknown. Retry the same request."}
        </p>
      )}
      <div className="peer-team-form-actions">
        {rejected ? (
          <Button
            size="compact-xs"
            disabled={pending}
            onClick={async () => {
              if (locked.current) return;
              locked.current = true;
              setPending(true);
              try {
                await refresh?.();
                close();
              } catch (e) {
                setError(errorText(e));
              } finally {
                locked.current = false;
                setPending(false);
              }
            }}
          >
            Reload project
          </Button>
        ) : (
          <Button
            type="submit"
            size="compact-xs"
            loading={pending}
            disabled={action === "save" && (!name.trim() || members.length < 2)}
          >
            {frozen ? "Retry" : action === "delete" ? "Dissolve" : "Save team"}
          </Button>
        )}
        <Button
          size="compact-xs"
          variant="subtle"
          disabled={pending || (frozen && !rejected && !committed.current)}
          onClick={close}
        >
          Cancel
        </Button>
      </div>
    </form>
  );
}

// Keep the exact mutation after a lost response. A new drag must not replace it.
export function usePeerTeamMove(
  refresh?: () => Promise<void>,
  notify?: (text: string) => void,
) {
  const request = useRef<Record<string, unknown> | null>(null);
  const committed = useRef(false);
  const running = useRef(false);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState("");
  const [rejected, setRejected] = useState(false);
  const clear = () => {
    request.current = null;
    committed.current = false;
    setError("");
    setRejected(false);
  };
  const submit = async () => {
    if (running.current || !request.current) return;
    running.current = true;
    setPending(true);
    setError("");
    try {
      if (!committed.current) {
        await api("/api/peer-teams", request.current, { timeoutMs: 15000 });
        committed.current = true;
      }
      await refresh?.();
      clear();
      notify?.("Team membership updated.");
    } catch (e) {
      setError(errorText(e));
      setRejected(
        !committed.current &&
          e instanceof ApiError &&
          e.status >= 400 &&
          e.status < 500,
      );
    } finally {
      running.current = false;
      setPending(false);
    }
  };
  const move = (project: Project, member: string, teamId: string | null) => {
    if (request.current) return;
    request.current = {
      action: "move",
      path: project.path,
      member,
      team_id: teamId,
      expected_revision: project.peerTeamsRevision || 0,
      request_id: crypto.randomUUID(),
    };
    void submit();
  };
  const reload = async () => {
    if (running.current) return;
    running.current = true;
    setPending(true);
    try {
      await refresh?.();
      clear();
    } catch (e) {
      setError(errorText(e));
    } finally {
      running.current = false;
      setPending(false);
    }
  };
  return {
    move,
    blocked: () => !!request.current,
    feedback:
      pending || error ? (
        <div className="peer-team-form">
          {pending ? (
            <p role="status">Updating team…</p>
          ) : (
            <>
              <p role="alert">
                {error}{" "}
                {committed.current
                  ? "The change was saved. Retry to refresh."
                  : rejected
                    ? "Reload the project before another move."
                    : "The result is unknown. Retry the same move."}
              </p>
              <Button
                size="compact-xs"
                onClick={() => void (rejected ? reload() : submit())}
              >
                {rejected ? "Reload project" : "Retry move"}
              </Button>
            </>
          )}
        </div>
      ) : null,
  };
}
