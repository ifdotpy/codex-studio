import type { GetResult, PostResult, JsonInput } from "../../src/apiTypes.ts";
import { sidebarFixtureRadio } from "./sidebar-fixture-values.ts";
import type { TestRuntimeView } from "../playwright.ts";
import type { ApiPostPath, PostBody } from "../../src/apiTypes.ts";
import type { BrowserContext, Page } from "playwright";
import type { components } from "../../src/generated/api.ts";
import type { TestAgent } from "../playwright.ts";
import type { PreferenceFields } from "../../src/sync/uiPreferenceMerge.ts";
import type { MultiServerFixtureOptions } from "../servers/multi-server-fixture.ts";
import { fixturePostBody } from "../servers/browser-fixture.ts";
import type { ServerFixture } from "../servers/browser-fixture.ts";
import assert from "node:assert/strict";
import {
  fixture,
  fixtureWorkspaceId,
} from "../servers/multi-server-fixture.ts";
import {
  entityPullFixture,
  syncIdentityFixture,
  validateSyncPullRequest,
} from "../playwright.ts";

type LegacyDocument = Partial<components["schemas"]["SyncDocument"]> &
  Pick<components["schemas"]["SyncDocument"], "id">;
// This fixture still reads the legacy payload format. V4 entities intentionally
// retain their absent payload here; aligning this stale fixture is separate work.
function legacyDocument(value: unknown): LegacyDocument {
  assert.ok(
    value &&
      typeof value === "object" &&
      "id" in value &&
      typeof value.id === "string",
  );
  const payload =
    "payload" in value && typeof value.payload === "string"
      ? value.payload
      : undefined;
  const deleted =
    "_deleted" in value && typeof value._deleted === "boolean"
      ? value._deleted
      : undefined;
  const seq =
    "seq" in value && typeof value.seq === "number" ? value.seq : undefined;
  return {
    id: value.id,
    ...(payload === undefined ? {} : { payload }),
    ...(deleted === undefined ? {} : { _deleted: deleted }),
    ...(seq === undefined ? {} : { seq }),
  };
}

export const scope = "/same/state";
export type Owner = "local" | "remote";
type HandleContext = Parameters<
  NonNullable<MultiServerFixtureOptions["handle"]>
>[0];
export type HeldRequest = Pick<HandleContext, "url" | "body"> & {
  owner: Owner;
  method: string | undefined;
  path: string;
};
export type Hold = (
  request: HeldRequest,
) => number | { deliver: number } | undefined;
export type SidebarParityFixture = Awaited<
  ReturnType<typeof sidebarParityFixture>
>;
const workspaceIds = {
  local: fixtureWorkspaceId("Local"),
  remote: "c".repeat(32),
};
export const sourcePath = (owner: Owner) =>
  owner === "local" ? "/same/project" : "/remote/project";
const treeId = (path: string, kind: string, id: string) =>
  JSON.stringify([path, kind, id]);

function seed(server: ServerFixture, owner: Owner) {
  const path = sourcePath(owner);
  const label = owner === "local" ? "Local" : "Remote";
  const base = server.snapshot.threads[0];
  assert.ok(base);
  const now = Date.now() / 1000;
  const chat = (
    id: string,
    name: string,
    extra: Partial<TestAgent> = {},
  ): TestAgent => ({
    ...base,
    id,
    rootId: id,
    name: `${label} ${name}`,
    cwd: path,
    created: 1,
    updated: 1,
    status: "completed",
    autoWake: true,
    projectFolderRevision: 2,
    ...extra,
  });
  const rows = [
    chat("overlap", "Saved pin", { pinned: true }),
    chat("root-a", "Root A", { updated: now }),
    chat("root-b", "Root B", { updated: now }),
    chat("nested-chat", "Nested chat", {
      projectFolder: "nested",
      updated: now,
    }),
    chat("member-a", "Team A", { updated: now }),
    chat("member-b", "Team B", { updated: now }),
    chat("archived", "Archived", { archived: true }),
    chat("unread", "Unread", {
      updated: now,
      threadId: "thread-unread",
      lastCompletedTurn: "turn-unread",
      lastCompletedTurnStatus: "completed",
      readStateSupported: true,
      readState: {
        threadId: "thread-unread",
        turnId: "turn-unread",
        read: false,
        revision: 1,
      },
    }),
    chat("read", "Read answer", {
      updated: now,
      threadId: "thread-read",
      lastCompletedTurn: "turn-read",
      lastCompletedTurnStatus: "completed",
      readStateSupported: true,
      readState: {
        threadId: "thread-read",
        turnId: "turn-read",
        read: true,
        revision: 1,
      },
    }),
    chat("running", "Running", {
      inFlight: true,
      status: "running",
      updated: now,
    }),
    chat("approval", "Approval", { status: "approval", updated: now }),
    ...Array.from({ length: 6 }, (_, index) =>
      chat(`old-${index}`, `Old ${index}`, {
        tail: index === 0 ? `${label} tail needle` : "",
      }),
    ),
  ];
  const team = {
    id: "team",
    name: `${label} Saved team`,
    projectPath: path,
    members: ["member-a", "member-b"],
    revision: 3,
  };
  const folders = [
    { id: "parent", name: `${label} Parent`, parentId: null },
    { id: "nested", name: `${label} Nested`, parentId: "parent" },
    { id: "empty", name: `${label} Empty`, parentId: null },
  ];
  const project = {
    ...server.snapshot.runtime.projects[0],
    id: owner === "local" ? "logical-project" : "remote-project",
    name: owner === "local" ? "Home project" : "Remote project",
    path,
    homeServerId: owner,
    folders,
    peerTeams: [team],
    organizationRevision: owner === "local" ? 3 : 9,
    peerTeamsRevision: owner === "local" ? 4 : 8,
    accountKey: "default",
    accountRevision: 1,
    ...(owner === "remote"
      ? {
          projectAliases: [
            {
              serverId: "local",
              projectId: "logical-project",
              name: "Home project",
            },
          ],
        }
      : {}),
  };
  const order = {
    projects: [path],
    [JSON.stringify(["items", path, null])]: [
      treeId(path, "team", "team"),
      treeId(path, "folder", "parent"),
      "root-b",
      "root-a",
      treeId(path, "folder", "empty"),
    ],
    [JSON.stringify(["items", path, "parent"])]: [
      treeId(path, "folder", "nested"),
    ],
    [JSON.stringify(["chats", path, true, false])]: ["overlap"],
    unknown: ["unknown-saved-id"],
  };
  const prefs = {
    compact: { [path]: true },
    collapsed: {
      [treeId(path, "folder", "nested")]: true,
      [treeId(path, "team", "team")]: true,
    },
    order,
  };
  server.setChats(rows);
  Object.assign(server.snapshot.runtime, {
    projectOrganizationVersion: 1,
    peerTeamsVersion: 1,
    projects: [project],
    peerTeams: [team],
    sidebarOrder: { revision: owner === "local" ? 3 : 7, groups: order },
    requests: [
      {
        id: "approval-request",
        agent: "approval",
        status: "pending",
        deferred: false,
      },
    ],
    rooms: [
      {
        id: "shared",
        name: `${label} Saved shared chat`,
        projectPath: path,
        members: ["member-a", "member-b"],
        radio: structuredClone(sidebarFixtureRadio),
      },
    ],
  } satisfies Partial<TestRuntimeView>);
  return prefs;
}

export async function sidebarParityFixture(
  context: BrowserContext,
  {
    paired = true,
    remoteEmpty = false,
    hold,
  }: { paired?: boolean; remoteEmpty?: boolean; hold?: Hold | undefined } = {},
) {
  const calls: Record<
    Owner,
    {
      [Path in ApiPostPath]: { path: Path; body: PostBody<Path> };
    }[ApiPostPath][]
  > = { local: [], remote: [] };
  const reads: Record<Owner, string[]> = { local: [], remote: [] };
  const preferences: Record<
    Owner,
    Record<"user" | "server", PreferenceFields>
  > = {
    local: { user: {}, server: {} },
    remote: { user: {}, server: {} },
  };
  const documents: Record<
    Owner,
    Map<string, components["schemas"]["SyncDocument"]>
  > = { local: new Map(), remote: new Map() };
  const sequences = { local: 0, remote: 0 };
  const servers: Partial<Record<Owner, ServerFixture>> = {};
  // `hold` makes a server slow. It returns a delay in milliseconds before the server reads
  // and changes its state, or `{ deliver }` to compute the answer at once and deliver it
  // late, so that a later request can be answered before an earlier one.
  const handler = (owner: Owner) => {
    const respond = serve(owner);
    return (value: HandleContext) => {
      const delay = hold?.({
        owner,
        method: value.request.method,
        path: value.url.pathname,
        url: value.url,
        body: value.body,
      });
      if (!delay) return respond(value);
      if (typeof delay === "object")
        return respond({
          ...value,
          json: (...answer) =>
            setTimeout(() => value.json(...answer), delay.deliver),
        });
      setTimeout(() => {
        if (!respond(value)) value.json({ error: "Unhandled request" }, 500);
      }, delay);
      return true;
    };
  };
  const receipts: Record<
    Owner,
    Map<string, components["schemas"]["SidebarOrderDto"]>
  > = { local: new Map(), remote: new Map() };
  const serve =
    (owner: Owner) =>
    ({ request, url, body: rawBody, json, snapshot }: HandleContext) => {
      const path = url.pathname;
      if (request.method === "GET") reads[owner].push(path + url.search);
      const project = (path: string) =>
        snapshot.runtime.projects.find((row) => row.path === path);
      const row = (id: string) => {
        const value = snapshot.threads.find((chat) => chat.id === id);
        assert.ok(value, `Unknown ${owner} chat: ${id}`);
        return value;
      };
      const reply = (value: unknown) => {
        if (request.method === "POST" && path !== "/api/sync/preferences")
          servers[owner]?.complete("sidebar-changed");
        json(value);
        return true;
      };
      const workspaceId = workspaceIds[owner];
      if (path === "/api/sync/identity")
        return reply(syncIdentityFixture(workspaceId));
      if (path === "/api/sync/preferences") {
        const body = fixturePostBody<"/api/sync/preferences">(rawBody);
        const target = preferences[owner][body.scope];
        assert.ok(body.fields);
        for (const [name, field] of Object.entries(body.fields)) {
          const old = target[name];
          if (
            !old ||
            field.timestamp > old.timestamp ||
            (field.timestamp === old.timestamp && field.writer >= old.writer)
          )
            target[name] = field;
        }
        servers[owner]?.complete("preferences-changed");
        return reply({ scope: body.scope, fields: target });
      }
      if (
        path === "/api/sync/pull" &&
        url.searchParams.get("scope") === "state:entities:v1"
      ) {
        const pull = entityPullFixture(snapshot);
        assert.ok("documents" in pull);
        const next: LegacyDocument[] = pull.documents.map(legacyDocument);
        for (const kind of ["user", "server"] as const)
          next.push({
            id: `entity:uiPreferences:${kind}`,
            payload: JSON.stringify({
              collection: "uiPreferences",
              id: kind,
              value: { scope: kind, fields: preferences[owner][kind] },
            }),
            _deleted: false,
          });
        const saved = documents[owner];
        const ids = new Set(next.map((value) => value.id));
        for (const [id, value] of saved)
          if (!ids.has(id) && !("_deleted" in value && value._deleted))
            saved.set(id, {
              ...value,
              _deleted: true,
              seq: ++sequences[owner],
            });
        for (const value of next)
          if (
            saved.get(value.id)?.payload !==
              ("payload" in value ? value.payload : undefined) ||
            saved.get(value.id)?._deleted
          ) {
            assert.ok(
              value.payload !== undefined && value._deleted !== undefined,
            );
            saved.set(value.id, {
              id: value.id,
              payload: value.payload,
              _deleted: value._deleted,
              seq: ++sequences[owner],
            });
          }
        const parsed = validateSyncPullRequest(url);
        assert.ok(parsed.ok);
        // Preserve legacy paging; the shared v4 helper accepts structured entities.
        const {
          after,
          limit,
          fresh,
          initialHigh: requestedHigh,
        } = parsed.params;
        const high = sequences[owner];
        const initialHigh = fresh
          ? requestedHigh > 0
            ? Math.min(high, requestedHigh)
            : high
          : 0;
        const eligible = [...saved.values()]
          .sort((a, b) => a.seq - b.seq)
          .filter((document) => document.seq > after);
        const page = eligible.slice(0, limit);
        return reply({
          workspaceId,
          generation: 1,
          documents: page,
          checkpoint: {
            seq: page.length === limit ? (page.at(-1)?.seq ?? 0) : high,
          },
          maxSeq: high,
          initialHigh,
        });
      }
      if (path === "/api/models")
        return reply({
          data: [
            {
              model: "fixture-model",
              displayName: "Fixture",
              isDefault: true,
              supportedReasoningEfforts: [],
            },
          ],
        } satisfies GetResult<"/api/models">);
      if (request.method === "GET" && path === "/api/projects")
        return reply({
          items: snapshot.runtime
            .projects satisfies components["schemas"]["ProjectEntityDto"][],
        });
      if (path === "/api/directories")
        return reply({
          path: url.searchParams.get("path") || sourcePath(owner),
          parent: "/",
          directories: [
            { name: "New directory", path: `/${owner}/new-directory` },
          ],
          files: [],
        } satisfies GetResult<"/api/directories"> & { files: string[] });
      if (path === "/api/project-servers")
        return reply({
          servers: [
            {
              id: "local",
              label: owner === "local" ? "Local" : "Remote",
              status: "paired",
              reachability: "online",
            },
          ],
        } satisfies JsonInput);
      if (path === "/api/project-locations" && request.method === "GET")
        return reply({
          items: [],
          matches: [],
        } satisfies GetResult<"/api/project-locations"> & {
          items: JsonInput[];
        });
      if (request.method !== "POST") return false;
      if (
        ![
          /^\/api\/organization$/,
          /^\/api\/projects$/,
          /^\/api\/peer-teams$/,
          /^\/api\/rename$/,
          /^\/api\/conversation(?:\/delete)?$/,
          /^\/api\/leads$/,
        ].some((pattern) => pattern.test(path))
      )
        return false;
      if (path === "/api/organization") {
        const body = fixturePostBody<"/api/organization">(rawBody);
        calls[owner].push({
          path: "/api/organization",
          body: structuredClone(body),
        });
        const chat = row(body.id);
        for (const key of ["pinned", "archived"] as const)
          if (key in body) chat[key] = body[key];
        if ("project_folder" in body) {
          assert.equal(body.project_path, chat.cwd);
          assert.equal(body.expected_revision, chat.projectFolderRevision);
          chat.projectFolder = body.project_folder;
          assert.ok(
            chat.projectFolderRevision !== null &&
              chat.projectFolderRevision !== undefined,
          );
          chat.projectFolderRevision++;
        }
        if (body.read_state) {
          assert.equal(body.read_state.thread_id, chat.threadId);
          assert.equal(body.read_state.turn_id, chat.lastCompletedTurn);
          assert.ok(chat.threadId && chat.lastCompletedTurn);
          chat.readState = {
            threadId: chat.threadId,
            turnId: chat.lastCompletedTurn,
            read: body.read_state.read,
            revision: (chat.readState?.revision || 0) + 1,
          };
        }
        return reply({
          id: chat.id,
          projectFolder: chat.projectFolder || null,
          pinned: !!chat.pinned,
          archived: !!chat.archived,
          readState: chat.readState,
        } satisfies Pick<
          components["schemas"]["AgentEntityDto"],
          "id" | "projectFolder" | "pinned" | "archived" | "readState"
        >);
      }
      if (path === "/api/rename") {
        const body = fixturePostBody<"/api/rename">(rawBody);
        calls[owner].push({ path: "/api/rename", body: structuredClone(body) });
        row(body.id).name = body.name;
        // The legacy fixture acknowledges renames without the current receipt.
        return reply({ ok: true } satisfies JsonInput);
      }
      if (path === "/api/conversation/delete") {
        const body = fixturePostBody<"/api/conversation/delete">(rawBody);
        calls[owner].push({
          path: "/api/conversation/delete",
          body: structuredClone(body),
        });
        row(body.id).deletedAt = Date.now() / 1000;
        return reply({
          deleted: [body.id],
        } satisfies PostResult<"/api/conversation/delete">);
      }
      if (path === "/api/conversation") {
        const body = fixturePostBody<"/api/conversation">(rawBody);
        calls[owner].push({
          path: "/api/conversation",
          body: structuredClone(body),
        });
        row(body.id).cwd = body.cwd;
        return reply(row(body.id));
      }
      if (path === "/api/leads") {
        const body = fixturePostBody<"/api/leads">(rawBody);
        calls[owner].push({ path: "/api/leads", body: structuredClone(body) });
        // The server keeps one chat for each creation identity.
        assert.ok(body.id);
        const existing = snapshot.threads.find((row) => row.id === body.id);
        if (existing) return reply(existing);
        const base = snapshot.threads[0];
        assert.ok(base);
        const chat = {
          ...base,
          id: body.id,
          rootId: body.id,
          name: `${owner} new chat`,
          pinned: false,
          cwd: body.cwd || sourcePath(owner),
          projectFolder: body.project_folder || null,
          updated: Date.now() / 1000,
        };
        snapshot.threads.push(chat);
        snapshot.runtime.agents = snapshot.threads;
        return reply(chat);
      }
      if (path === "/api/projects") {
        const body = fixturePostBody<"/api/projects">(rawBody);
        calls[owner].push({
          path: "/api/projects",
          body: structuredClone(body),
        });
        if (body.action === "reorder") {
          // A repeated request ID returns the first receipt, as the server does.
          const receipt = receipts[owner].get(body.request_id);
          if (receipt) return reply(receipt);
          assert.ok(snapshot.runtime.sidebarOrder);
          assert.equal(
            body.expected_revision,
            snapshot.runtime.sidebarOrder.revision,
          );
          snapshot.runtime.sidebarOrder = {
            revision: body.expected_revision + 1,
            groups: body.groups,
          };
          receipts[owner].set(body.request_id, snapshot.runtime.sidebarOrder);
          return reply(snapshot.runtime.sidebarOrder);
        }
        assert.ok(body.path);
        const value = project(body.path);
        assert.ok(value, `Unknown ${owner} project: ${body.path}`);
        if (
          "expected_revision" in body &&
          body.action &&
          ["rename", "rename_folder", "add_folder", "remove_folder"].includes(
            body.action,
          )
        ) {
          assert.ok(
            value.folders &&
              value.organizationRevision !== null &&
              value.organizationRevision !== undefined,
          );
          assert.equal(body.expected_revision, value.organizationRevision);
          if (body.action === "rename") value.name = body.name;
          if (body.action === "rename_folder") {
            const folder = value.folders.find(
              (folder) => folder.id === body.folder_id,
            );
            assert.ok(folder && body.name);
            folder.name = body.name;
          }
          if (body.action === "add_folder") {
            assert.ok(body.folder_id && body.name);
            value.folders.push({
              id: body.folder_id,
              name: body.name,
              parentId: body.parent_id,
            });
          }
          if (body.action === "remove_folder")
            value.folders = value.folders.filter(
              (folder) => folder.id !== body.folder_id,
            );
          value.organizationRevision++;
        }
        if (body.action === "set_accounts") {
          value.accountKeys = body.account_keys;
          value.accountKey = body.account_key;
          assert.ok(
            value.accountRevision !== null &&
              value.accountRevision !== undefined,
          );
          value.accountRevision++;
        }
        return reply({
          project: value,
          revision: value.organizationRevision,
        } satisfies JsonInput);
      }
      if (path === "/api/peer-teams") {
        const body = fixturePostBody<"/api/peer-teams">(rawBody);
        calls[owner].push({
          path: "/api/peer-teams",
          body: structuredClone(body),
        });
        const value = project(body.path);
        assert.ok(value, `Unknown ${owner} team project: ${body.path}`);
        if (body.action !== "radio")
          assert.equal(body.expected_revision, value.peerTeamsRevision);
        const teams = snapshot.runtime.peerTeams;
        if (body.action === "save") {
          const existing = teams.find((team) => team.id === body.team_id);
          assert.ok(body.team_id);
          const next = {
            id: body.team_id,
            name: body.name,
            projectPath: body.path,
            members: body.members,
          };
          for (const member of body.members) row(member);
          if (existing) Object.assign(existing, next);
          else teams.push(next);
        }
        if (body.action === "delete")
          snapshot.runtime.peerTeams = teams.filter(
            (team) => team.id !== body.team_id,
          );
        if (body.action === "move" || body.action === "convert") {
          row(body.member);
          for (const team of teams) {
            assert.ok(team.members);
            team.members = team.members.filter((id) => id !== body.member);
          }
          if ("team_id" in body && body.team_id) {
            const team = teams.find((team) => team.id === body.team_id);
            assert.ok(team?.members);
            team.members.push(body.member);
          }
          snapshot.runtime.peerTeams = teams.filter(
            (team) => (team.members?.length ?? 0) >= 2,
          );
          if (body.action === "convert") {
            row(body.target);
            Object.assign(row(body.member), {
              isLead: false,
              rootId: body.target,
              parentId: body.target,
              status: "paused",
              autoWake: false,
            } satisfies Partial<TestAgent>);
          }
        }
        if (body.action === "radio") {
          const room: components["schemas"]["RoomEntityDto"] = {
            id: `room-${body.request_id}`,
            name: ("name" in body && body.name) || `${owner} Team shared chat`,
            projectPath: body.path,
            members:
              ("participants" in body
                ? body.participants.map((_, index) => `radio-${index}`)
                : undefined) ||
              teams.find(
                (team) =>
                  team.id === ("team_id" in body ? body.team_id : undefined),
              )?.members ||
              [],
            radio: {
              active: null,
              error: null,
              next: [],
              revision: 0,
              speaker: "",
              status: "idle",
              direct: body.radio_action === "create",
              teamId: ("team_id" in body ? body.team_id : undefined) || "",
            },
          };
          snapshot.runtime.rooms.push(room);
          return reply({ room, peerTeams: teams } satisfies JsonInput);
        }
        value.peerTeams = snapshot.runtime.peerTeams.map((team) => {
          assert.ok(team.name && team.members);
          return { id: team.id, name: team.name, members: team.members };
        });
        assert.ok(
          value.peerTeamsRevision !== null &&
            value.peerTeamsRevision !== undefined,
        );
        value.peerTeamsRevision++;
        return reply({
          peerTeams: value.peerTeams,
          revision: value.peerTeamsRevision,
        } satisfies JsonInput);
      }
      return false;
    };
  const accounts: NonNullable<MultiServerFixtureOptions["accounts"]> = [
    { id: "default", label: "Fixture", provider: "codex", status: "ready" },
    { id: "secondary", label: "Second", provider: "codex", status: "ready" },
  ];
  const local = await fixture("Local", false, {
    handle: handler("local"),
    resourceBaseline: true,
    workspaceId: workspaceIds.local,
    accounts: structuredClone(accounts),
  });
  const remote = await fixture("Remote", true, {
    handle: handler("remote"),
    resourceBaseline: true,
    workspaceId: workspaceIds.remote,
    accounts: structuredClone(accounts),
  });
  Object.assign(servers, { local, remote });
  const prefs = { local: seed(local, "local"), remote: seed(remote, "remote") };
  if (remoteEmpty) {
    remote.setChats([]);
    Object.assign(remote.snapshot.runtime, {
      projects: [],
      rooms: [],
      peerTeams: [],
      requests: [],
    } satisfies Partial<TestRuntimeView>);
  }
  local.aliases.remote = "REM";
  if (paired) local.discoverPeer(remote);
  await context.addInitScript(
    ({ origin, destination, prefs, scope }) => {
      // Both owner scopes now share the shell document instead of iframes.
      for (const owner of ["local", "remote"] as const) {
        const prefix = owner === "remote" ? ":server:remote:" : "";
        for (const [kind, value] of [
          ["compact", prefs[owner].compact],
          ["tree", prefs[owner].collapsed],
          ["sidebar-order", prefs[owner].order],
        ] as const) {
          const key =
            kind === "sidebar-order"
              ? `codex-sidebar-order:${scope}`
              : `codex-project-${kind}:${scope}`;
          if (!localStorage.getItem(prefix + key))
            localStorage.setItem(prefix + key, JSON.stringify(value));
        }
      }
      const nativeFetch = window.fetch.bind(window);
      window.fetch = async (input, init) => {
        const request = new Request(input, init);
        const url = new URL(request.url);
        if (url.origin !== origin) return nativeFetch(request);
        const body = await request.clone().arrayBuffer();
        return nativeFetch(destination + url.pathname + url.search, {
          method: request.method,
          headers: request.headers,
          ...(body.byteLength ? { body } : {}),
          signal: request.signal,
          redirect: "error",
          credentials: "omit",
        });
      };
    },
    {
      origin: remote.invitation.origin,
      destination: remote.origin,
      prefs,
      scope,
    },
  );
  return {
    local,
    remote,
    calls,
    reads,
    prefs,
    preferences,
    async open(
      page: Page,
      {
        combined = true,
        owner = "local",
      }: { combined?: boolean; owner?: Owner } = {},
    ) {
      await page.goto(
        local.origin + (combined ? "/" : "/?studio-navigation=classic"),
      );
      if (!combined && paired && owner === "remote")
        await page.getByLabel("Studio server").selectOption("remote");
      const sidebar =
        !combined && paired
          ? page
              .frameLocator(
                `iframe[title="Studio on ${owner === "local" ? "Local" : "Remote"}"]`,
              )
              .locator("#sidebar")
          : page.locator("#sidebar");
      if ((page.viewportSize()?.width || 1440) < 760) {
        if (combined && paired)
          await page
            .getByRole("button", { name: "Servers and chats", exact: true })
            .click();
        else {
          const surface = paired
            ? page.frameLocator(
                `iframe[title="Studio on ${owner === "local" ? "Local" : "Remote"}"]`,
              )
            : page;
          await surface
            .getByRole("button", { name: "Toggle conversations", exact: true })
            .click();
        }
      }
      await sidebar
        .getByRole("button", {
          name: `${owner === "local" ? "Local" : "Remote"} Saved pin`,
        })
        .first()
        .waitFor();
      return sidebar;
    },
    async close() {
      await remote.close();
      await local.close();
    },
  };
}
