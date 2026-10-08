import { afterEach, describe, expect, it, vi } from "vitest";
import type { Agent, PeerTeam } from "../../types";
import {
  createAppCatalogSelector,
  createSidebarCatalogSelector,
  createSidebarSearchSelector,
  isVisibleSidebarAgent,
  type SidebarOrder,
  type SidebarOverrides,
} from "./catalog";

const agent = (id: string, fields: Partial<Agent> = {}): Agent => ({
  id,
  name: id,
  source: "managed",
  isLead: true,
  cwd: "/work",
  created: 1,
  ...fields,
});
const order: SidebarOrder = {};
const teams: PeerTeam[] = [];
const overrides = {};
afterEach(() => vi.unstubAllGlobals());

// Preserve the original observable order, including legacy rank fallback and
// equal timestamp source order. This is a separate reference implementation.
function reference(rows: Agent[], orders: SidebarOrder, peerTeams: PeerTeam[]) {
  const teamFor = (id: string) =>
    peerTeams.find((team) => team.members?.includes(id));
  const rank = (group: string, id: string) => {
    const i = orders[group]?.indexOf(id) ?? -1;
    return i < 0 ? Number.MAX_SAFE_INTEGER : i;
  };
  const legacy = (a: Agent) =>
    JSON.stringify([
      "chats",
      a.cwd,
      !!a.pinned,
      !!a.archived,
      ...(teamFor(a.id) ? ["team", teamFor(a.id)!.id] : []),
      ...(a.projectFolder ? [a.projectFolder] : []),
    ]);
  const position = (a: Agent) => {
    const group =
      !teamFor(a.id) && !a.pinned
        ? JSON.stringify(["items", a.cwd || "", a.projectFolder || null])
        : legacy(a);
    const i = rank(group, a.id);
    return i === Number.MAX_SAFE_INTEGER ? rank(legacy(a), a.id) : i;
  };
  return rows
    .filter(isVisibleSidebarAgent)
    .sort(
      (a, b) =>
        Number(!!b.pinned) - Number(!!a.pinned) ||
        position(a) - position(b) ||
        (("updated" in b ? b.updated : b.created) || 0) -
          (("updated" in a ? a.updated : a.created) || 0),
    )
    .map((row) => row.id);
}

describe("incremental sidebar catalog", () => {
  it("retains all visible indexes when a hidden worker changes", () => {
    const select = createSidebarCatalogSelector();
    const rows = [
      agent("lead"),
      agent("worker", { isLead: false, rootId: "lead" }),
    ];
    const before = select(rows, overrides, teams, order);
    const next = select(
      [rows[0], { ...rows[1], status: "failed" }],
      overrides,
      teams,
      order,
    );
    expect(next.agents).toBe(before.agents);
    expect(next.ordered).toBe(before.ordered);
    expect(next.byProject).toBe(before.byProject);
    expect(next.grouped).toBe(before.grouped);
  });
  it("publishes titles, status and unread without sorting unchanged order keys", () => {
    const counts: Record<string, number> = {};
    vi.stubGlobal("window", {
      __studioSidebarModelProbe: (work: string, n: number) => {
        counts[work] = (counts[work] || 0) + n;
      },
    });
    const select = createSidebarCatalogSelector();
    const rows = Array.from({ length: 2000 }, (_, i) =>
      agent(`lead-${i}`, { updated: i }),
    );
    select(rows, overrides, teams, order);
    counts["order-compare"] = 0;
    const renamed = rows.map((row, i) =>
      i === 700
        ? {
            ...row,
            name: "New name",
            status: "failed" as const,
            hasUnread: true,
          }
        : row,
    );
    const result = select(renamed, overrides, teams, order);
    expect(result.byId.get("lead-700")?.name).toBe("New name");
    expect(result.byId.get("lead-700")?.hasUnread).toBe(true);
    expect(counts["order-compare"]).toBe(0);
    const moved = renamed.map((row, i) =>
      i === 700 ? { ...row, updated: 3000 } : row,
    );
    expect(select(moved, overrides, teams, order).ordered[0].id).toBe(
      "lead-700",
    );
    expect(counts["order-compare"]).toBeLessThan(12);
  });
  it("sorts a bulk timestamp change once instead of inserting every row", () => {
    let comparisons = 0;
    vi.stubGlobal("window", {
      __studioSidebarModelProbe: (work: string, count: number) => {
        if (work === "order-compare") comparisons += count;
      },
    });
    const select = createSidebarCatalogSelector();
    const rows = Array.from({ length: 1000 }, (_, i) =>
      agent(`a-${i}`, { updated: i }),
    );
    select(rows, overrides, teams, order);
    comparisons = 0;
    const next = rows.map((row, i) => ({ ...row, updated: 2000 - i }));
    expect(
      select(next, overrides, teams, order).ordered.map((row) => row.id),
    ).toEqual(reference(next, order, teams));
    expect(comparisons).toBeLessThan(3000);
  });
  it("matches original ordering through inserts, removals, reorders and optimistic changes", () => {
    const select = createSidebarCatalogSelector();
    const peerTeams: PeerTeam[] = [
      { id: "team", projectPath: "/work", members: ["a", "b"] },
    ];
    let rows = [
      agent("a"),
      agent("b"),
      agent("c", { updated: 3 }),
      agent("hidden", { isLead: false }),
    ];
    let optimistic: SidebarOverrides = {};
    let orders: SidebarOrder = {};
    const verify = () =>
      expect(
        select(rows, optimistic, peerTeams, orders).ordered.map(
          (row) => row.id,
        ),
      ).toEqual(
        reference(
          rows.map((row) => ({
            ...row,
            ...optimistic[row.id],
          })),
          orders,
          peerTeams,
        ),
      );
    verify();
    rows = [rows[1], rows[0], rows[2], rows[3]];
    verify();
    rows = [...rows, agent("new", { updated: 8 })];
    verify();
    rows = rows.filter((row) => row.id !== "b");
    verify();
    optimistic = { a: { pinned: true }, c: { archived: true } };
    verify();
    orders = {
      '["chats","/work",true,false,"team","team"]': ["a"],
      '["items","/work",null]': ["new", "c"],
    };
    verify();
    orders = { '["items","/work",null]': ["c", "new"] };
    verify();
    rows = rows.map((row) => (row.id === "a" ? { ...row, pinned: true } : row));
    optimistic = {};
    verify();
    rows = rows.map((row) =>
      row.id === "new" ? { ...row, deletedAt: 1 } : row,
    );
    verify();
    rows = [agent("replacement")];
    verify();
  });
  it("preserves the presence-sensitive timestamp fallback", () => {
    const select = createSidebarCatalogSelector();
    const rows = [agent("a", { created: 4 }), agent("b", { created: 2 })];
    expect(
      select(rows, overrides, teams, order).ordered.map((row) => row.id),
    ).toEqual(["a", "b"]);
    const next = [{ ...rows[0], updated: undefined }, rows[1]];
    expect(
      select(next, overrides, teams, order).ordered.map((row) => row.id),
    ).toEqual(["b", "a"]);
  });
  it("retains unaffected project arrays and tracks team/path changes", () => {
    const select = createSidebarCatalogSelector();
    const rows = [agent("a"), agent("b", { cwd: "/other" })];
    const before = select(rows, overrides, teams, order);
    const after = select(
      [{ ...rows[0], projectFolder: "folder", archived: true }, rows[1]],
      overrides,
      [{ id: "team", projectPath: "/work", members: ["a"] }],
      order,
    );
    expect(after.byProject.get("/other")).toBe(before.byProject.get("/other"));
    expect(after.grouped.has("a")).toBe(true);
    expect(after.byId.get("a")?.projectFolder).toBe("folder");
  });
  it("only constructs search labels for changed rows or metadata", () => {
    const select = createSidebarSearchSelector();
    const context = {};
    const rows = [agent("a"), agent("b")];
    const label = vi.fn((row: Agent) => row.name || "");
    expect(select(rows, context, "a", label)).toEqual([rows[0]]);
    select(rows, context, "b", label);
    expect(label).toHaveBeenCalledTimes(2);
    const renamed = { ...rows[0], name: "renamed" };
    expect(select([renamed, rows[1]], context, "renamed", label)).toEqual([
      renamed,
    ]);
    expect(label).toHaveBeenCalledTimes(3);
    select([renamed, rows[1]], {}, "renamed", label);
    expect(label).toHaveBeenCalledTimes(5);
  });
});

describe("App catalog", () => {
  it("retains another tree and updates membership, source order and root selection", () => {
    const select = createAppCatalogSelector();
    const rows = [
      agent("lead"),
      agent("worker", { isLead: false, rootId: "lead" }),
      agent("other"),
    ];
    const before = select(rows);
    const beforeTeam = before.team("lead");
    const beforeWorkers = before.workers("lead");
    expect(before.team("lead")).toEqual(rows.slice(0, 2));
    expect(select(rows).leads).toBe(before.leads);
    const after = select([rows[0], rows[1], { ...rows[2], name: "Changed" }]);
    expect(after.team("lead")).toBe(beforeTeam);
    expect(after.workers("lead")).toBe(beforeWorkers);
    const moved = select([rows[0], { ...rows[1], rootId: "other" }, rows[2]]);
    expect(moved.workers("lead")).toEqual([]);
    expect(moved.team("other")?.map((row) => row.id)).toEqual([
      "worker",
      "other",
    ]);
    expect(select([]).byId.size).toBe(0);
  });
});
