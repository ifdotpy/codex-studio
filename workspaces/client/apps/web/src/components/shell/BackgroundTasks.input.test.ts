import { webcrypto } from "node:crypto";
import { readFileSync } from "node:fs";
import { isValidElement, type ReactElement, type ReactNode } from "react";
import { afterEach, expect, it, vi } from "vitest";
import { createApiScope, type ServerApi } from "../../api";
import { createMutationJournal } from "../../servers/mutationIdentity";
import { monitorInputStoreFixture } from "../../servers/monitorInputStore.fixture";
import type { Agent } from "../../types";

const hooks = vi.hoisted(() => ({
  values: [] as unknown[],
  cursor: 0,
  api: undefined as ServerApi | undefined,
}));
vi.mock("../../servers/ServerServicesContext", () => ({
  useServerApi: () => hooks.api,
  useServerServices: () => null,
  useServerResourceReads: () => () => () => {},
}));
vi.mock("react", async (original) => ({
  ...(await original<typeof import("react")>()),
  useState: (initial: unknown) => {
    const index = hooks.cursor++;
    if (!(index in hooks.values))
      hooks.values[index] = typeof initial === "function" ? initial() : initial;
    return [
      hooks.values[index],
      (next: unknown) => {
        hooks.values[index] =
          typeof next === "function" ? next(hooks.values[index]) : next;
      },
    ];
  },
}));
import { monitorTask, ProcessInput } from "./BackgroundTasks";

function memoryStorage(): Storage {
  const rows = new Map<string, string>();
  return {
    get length() {
      return rows.size;
    },
    key: (index) => [...rows.keys()][index] ?? null,
    getItem: (key) => rows.get(key) ?? null,
    setItem: (key, value) => {
      rows.set(key, value);
    },
    removeItem: (key) => {
      rows.delete(key);
    },
    clear: () => rows.clear(),
  };
}

function find(
  tree: ReactNode,
  predicate: (node: ReactElement<Record<string, unknown>>) => boolean,
): ReactElement<Record<string, unknown>> | undefined {
  if (Array.isArray(tree))
    return tree.map((child) => find(child, predicate)).find(Boolean);
  if (!isValidElement<Record<string, unknown>>(tree)) return;
  if (predicate(tree)) return tree;
  return find(tree.props.children as ReactNode, predicate);
}

function fixture() {
  // LAN HTTP has secure randomness but no SubtleCrypto.
  vi.stubGlobal("crypto", {
    getRandomValues: webcrypto.getRandomValues.bind(webcrypto),
  });
  vi.stubGlobal(
    "fetch",
    async () =>
      new Response(
        readFileSync(
          new URL("../../wasm/generated/client_bg.wasm", import.meta.url),
        ),
        { headers: { "Content-Type": "application/wasm" } },
      ),
  );
  const storage = memoryStorage();
  const journal = monitorInputStoreFixture();
  const writes = new Map<string, string>();
  const requests: Request[] = [];
  let lost = true;
  let receiptStatus = "acknowledged";
  let receiptOverride: unknown;
  let receiptOnly = false;
  const fetch = async (request: Request) => {
    requests.push(request);
    if (request.method === "GET") {
      if (receiptOverride) return Response.json(receiptOverride);
      const id = new URL(request.url).searchParams.get("requestId");
      return Response.json({
        found: true,
        result: {
          requestId: id,
          status:
            receiptStatus === "acknowledged" || receiptStatus === "rejected"
              ? "completed"
              : "uncertain",
          response: {
            kind: "monitor_input",
            monitorId: "monitor-one",
            status: receiptStatus,
          },
        },
      });
    }
    const id = request.headers.get("X-Studio-Request-Id")!;
    const body = await request.text();
    if (writes.has(id)) expect(writes.get(id)).toBe(body);
    else writes.set(id, body);
    if (lost) {
      lost = false;
      throw new Error("HTTP success response was lost");
    }
    return Response.json({ requestId: id, status: "acknowledged" });
  };
  const open = () =>
    createApiScope({
      origin: "http://192.0.2.10:49171",
      fetch,
      localStorage: storage,
      sessionStorage: storage,
      beginMutation: createMutationJournal(null, storage, journal.store(null)),
      beginActivity: () => () => {},
    });
  hooks.api = open();
  const task = monitorTask({
    id: "monitor-one",
    agent: "owner",
    created: 1,
    status: "running",
    interactive: true,
  })!;
  const owner = { id: "owner", accountKey: "target", epoch: 3 } as Agent;
  const actions: Promise<boolean>[] = [];
  const render = () => {
    hooks.cursor = 0;
    return ProcessInput({
      task,
      owner,
      pending: false,
      receiptOnly,
      act: (path, body) => {
        const action = hooks.api!.post(path, body).then(
          () => true,
          () => false,
        );
        actions.push(action);
        return action;
      },
    });
  };
  const send = async () => {
    const submit = find(render(), (node) => node.type === "form")?.props
      .onSubmit;
    if (typeof submit !== "function")
      throw new Error("Real input form is missing");
    submit({ preventDefault() {} });
    await Promise.all(actions);
    await Promise.resolve();
  };
  const check = () => {
    const click = find(
      render(),
      (node) => node.props.children === "Check input receipt",
    )?.props.onClick;
    if (typeof click !== "function")
      throw new Error("Real receipt button is missing");
    click();
  };
  return {
    storage,
    writes,
    requests,
    open,
    send,
    check,
    setReceipt: (status: string) => {
      receiptStatus = status;
    },
    metadata: () => journal.read(null, "monitor-one"),
    setReceiptObject: (value: unknown) => {
      receiptOverride = value;
    },
    receiptOnly: () => {
      receiptOnly = true;
    },
    hasForm: () => !!find(render(), (node) => node.type === "form"),
  };
}

afterEach(() => {
  hooks.values = [];
  hooks.cursor = 0;
  vi.unstubAllGlobals();
});

it("the real Send line handler retries a lost success with the same ID and permits a fresh deliberate identical line", async () => {
  const f = fixture();
  hooks.values[0] = "private stdin";
  await f.send();
  expect(hooks.values[0]).toBe("private stdin");
  expect(f.writes.size).toBe(1);
  expect(f.storage.length).toBe(0);
  expect(JSON.stringify(f.metadata())).not.toContain("private stdin");
  hooks.values[0] = "different input";
  await f.send();
  expect(f.requests).toHaveLength(1);
  hooks.values[0] = "private stdin";
  await f.send();
  expect(f.requests[1]?.headers.get("X-Studio-Request-Id")).toBe(
    f.requests[0]?.headers.get("X-Studio-Request-Id"),
  );
  expect(f.writes.size).toBe(1);
  expect(hooks.values[0]).toBe("");
  hooks.values[0] = "private stdin";
  await f.send();
  expect(f.writes.size).toBe(2);
  expect(f.requests[2]?.headers.get("X-Studio-Request-Id")).not.toBe(
    f.requests[0]?.headers.get("X-Studio-Request-Id"),
  );
});

it("the real receipt button recovers after reload without the secret and never clears an unknown result", async () => {
  const f = fixture();
  hooks.values[0] = "private stdin";
  await f.send();
  hooks.values = [];
  hooks.api = f.open();
  f.receiptOnly();
  expect(f.hasForm()).toBe(false);
  f.setReceipt("unknown");
  f.check();
  await vi.waitFor(() =>
    expect(hooks.values[4]).toBe(
      "The input result is unknown. Do not send different input.",
    ),
  );
  expect(await hooks.api.pendingMonitorInput("monitor-one")).not.toBeNull();
  expect(hooks.values[0]).toBe("");
  f.setReceipt("acknowledged");
  f.check();
  await vi.waitFor(() =>
    expect(hooks.values[4]).toBe("The process acknowledged the input."),
  );
  expect(await hooks.api.pendingMonitorInput("monitor-one")).toBeNull();
  expect(f.writes.size).toBe(1);
  const checks = f.requests.filter((request) => request.method === "GET");
  expect(checks).toHaveLength(2);
  expect(new URL(checks[0]!.url).searchParams.get("accountKey")).toBe("target");
  expect(checks.every((request) => !request.url.includes("private"))).toBe(
    true,
  );
});

it("the receipt reader keeps a pending identity for missing or mismatched evidence and releases an exact rejection", async () => {
  const f = fixture();
  hooks.values[0] = "private stdin";
  await f.send();
  const pending = await hooks.api!.pendingMonitorInput("monitor-one");
  const requestId = pending!.requestId;
  const exact = {
    requestId,
    status: "completed",
    response: {
      kind: "monitor_input",
      monitorId: "monitor-one",
      status: "acknowledged",
    },
  };
  for (const receipt of [
    { found: false, result: null },
    { found: true, result: { ...exact, requestId: "another-request" } },
    { found: true, result: { ...exact, status: "uncertain" } },
    {
      found: true,
      result: { ...exact, response: { ...exact.response, kind: "stop" } },
    },
    {
      found: true,
      result: {
        ...exact,
        response: { ...exact.response, monitorId: "another-monitor" },
      },
    },
    {
      found: true,
      result: { ...exact, response: { ...exact.response, status: "pending" } },
    },
  ]) {
    f.setReceiptObject(receipt);
    expect(
      await hooks.api!.checkMonitorInputReceipt("monitor-one", {
        agent: "owner",
        accountKey: "target",
        epoch: 3,
      }),
    ).toBe("unknown");
    expect(await hooks.api!.pendingMonitorInput("monitor-one")).toEqual(
      pending,
    );
  }
  f.setReceiptObject({
    found: true,
    result: { ...exact, response: { ...exact.response, status: "rejected" } },
  });
  expect(
    await hooks.api!.checkMonitorInputReceipt("monitor-one", {
      agent: "owner",
      accountKey: "target",
      epoch: 3,
    }),
  ).toBe("rejected");
  expect(await hooks.api!.pendingMonitorInput("monitor-one")).toBeNull();
  expect(f.writes.size).toBe(1);
});
