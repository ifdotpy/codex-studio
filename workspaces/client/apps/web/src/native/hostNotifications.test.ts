import { afterEach, beforeEach, expect, it, vi, type Mock } from "vitest";
import { fileURLToPath } from "node:url";
import { readFile } from "node:fs/promises";
import { checkNativeImportGraph } from "../../scripts/nativeImportGraph.mjs";
import type { DesktopBridge } from "../desktop";

const ports = vi.hoisted(() => ({
  available: vi.fn(),
  request: vi.fn(),
  listen: vi.fn(),
}));
vi.mock("./hostClient", () => ({
  hasNativeClient: ports.available,
  nativeRequest: ports.request,
  listenHostEvent: ports.listen,
}));
import {
  hasDesktopNotifications,
  notifyDesktop,
  onDesktopNavigate,
} from "./hostNotifications";

type Target = Parameters<Parameters<DesktopBridge["onNavigate"]>[0]>[0];
type Callback = (value: unknown) => void;
let receive: Callback;
let dispatch: ReturnType<typeof vi.fn>;
let stopHost: Mock<() => void>;
beforeEach(() => {
  ports.available.mockReset().mockReturnValue(true);
  ports.request.mockReset().mockResolvedValue(true);
  ports.listen.mockReset();
  stopHost = vi.fn();
  ports.listen.mockImplementation((_topic, callback) => {
    receive = callback;
    return Promise.resolve(stopHost);
  });
  dispatch = vi.fn();
  vi.stubGlobal("window", { dispatchEvent: dispatch });
  vi.stubGlobal(
    "CustomEvent",
    class {
      constructor(
        public type: string,
        public init: { detail: unknown },
      ) {}
      get detail() {
        return this.init.detail;
      }
    },
  );
});
afterEach(() => vi.unstubAllGlobals());
const settle = async () => {
  await Promise.resolve();
  await Promise.resolve();
};

it("resolves only the host transport graph and erases the DesktopBridge type owner", async () => {
  const root = fileURLToPath(new URL("../../", import.meta.url));
  const report = await checkNativeImportGraph({
    root,
    entry: fileURLToPath(new URL("./hostNotifications.ts", import.meta.url)),
  });
  expect(report.findings).toEqual([]);
  expect(
    report.modules.some(
      (name) =>
        /native\/(?:frame|client|notifications)\.ts$/.test(name) ||
        name === "src/desktop.ts",
    ),
  ).toBe(false);
});
it("uses only host availability even when another bridge is present", () => {
  ports.available.mockReturnValue(false);
  Object.assign(window, { codexDesktop: { notify: vi.fn() } });
  expect(hasDesktopNotifications()).toBe(false);
});
it("preserves the notification and its exact server/chat destination through one host command", async () => {
  const notification: Parameters<DesktopBridge["notify"]>[0] = {
    title: "Remote",
    body: "Reply",
    sound: true,
    target: {
      serverId: "remote",
      agentId: "chat",
      section: "messages",
      itemId: "message",
    },
  };
  await expect(notifyDesktop(notification)).resolves.toBe(true);
  expect(ports.request).toHaveBeenCalledExactlyOnceWith(
    "desktop.notify",
    notification,
  );
});
it("does not retry an uncertain notification command", async () => {
  ports.request.mockRejectedValue(new Error("notification outcome unknown"));
  await expect(
    notifyDesktop({
      title: "Reply",
      body: "Message",
      target: { agentId: "chat", section: "messages" },
    }),
  ).rejects.toThrow("notification outcome unknown");
  expect(ports.request).toHaveBeenCalledOnce();
});
it("registers the one fixed topic and preserves valid navigation fields", async () => {
  const callback = vi.fn();
  const stop = onDesktopNavigate(callback);
  await settle();
  expect(ports.listen).toHaveBeenCalledExactlyOnceWith(
    "studio-native-navigate",
    expect.any(Function),
  );
  const target: Target = {
    serverId: "remote",
    agentId: "chat",
    section: "messages",
    itemId: "message",
  };
  receive(target);
  expect(callback).toHaveBeenCalledExactlyOnceWith(target);
  stop();
  stop();
  expect(stopHost).toHaveBeenCalledOnce();
  receive(target);
  expect(callback).toHaveBeenCalledOnce();
});
it.each([
  null,
  [],
  {},
  { agentId: "", section: "messages" },
  { agentId: 7, section: "messages" },
  { agentId: "chat", section: "files" },
  { agentId: "chat", section: "messages", serverId: "" },
  { agentId: "chat", section: "messages", serverId: null },
  { agentId: "chat", section: "messages", itemId: undefined },
  { agentId: "chat", section: "messages", itemId: "" },
  { agentId: "chat", section: "messages", path: "/private" },
])(
  "rejects navigation outside the Rust broker payload contract: %j",
  async (payload) => {
    const callback = vi.fn();
    const stop = onDesktopNavigate(callback);
    await settle();
    receive(payload);
    expect(callback).not.toHaveBeenCalled();
    stop();
  },
);
it("accepts absent optional fields and preserves Rust's nonempty string rule", async () => {
  const callback = vi.fn();
  const stop = onDesktopNavigate(callback);
  await settle();
  receive({ agentId: " ", section: "messages" });
  expect(callback).toHaveBeenCalledWith({ agentId: " ", section: "messages" });
  stop();
});
it("copies validated fields so one consumer cannot change another navigation input", async () => {
  const payload = { serverId: "remote", agentId: "chat", section: "messages" };
  const callback = vi.fn((target: Target) => {
    target.agentId = "changed";
  });
  const stop = onDesktopNavigate(callback);
  await settle();
  receive(payload);
  expect(payload.agentId).toBe("chat");
  stop();
});
it("stops a late registration without delivering a stale callback", async () => {
  let finish!: (stop: () => void) => void;
  ports.listen.mockImplementation((_topic, callback) => {
    receive = callback;
    return new Promise((resolve) => {
      finish = resolve;
    });
  });
  const callback = vi.fn();
  const stop = onDesktopNavigate(callback);
  stop();
  stop();
  receive({ agentId: "chat", section: "messages" });
  expect(callback).not.toHaveBeenCalled();
  finish(stopHost);
  await settle();
  expect(stopHost).toHaveBeenCalledOnce();
  receive({ agentId: "chat", section: "messages" });
  expect(callback).not.toHaveBeenCalled();
});
it("surfaces active registration failure through the existing UI error event without retry", async () => {
  ports.listen.mockRejectedValue(new Error("registration failed"));
  const stop = onDesktopNavigate(vi.fn());
  await settle();
  expect(dispatch).toHaveBeenCalledOnce();
  expect(dispatch.mock.calls[0][0]).toMatchObject({
    type: "desktop-error",
    detail: "Error: registration failed",
  });
  expect(ports.listen).toHaveBeenCalledOnce();
  stop();
});
it("does not surface a registration error after its watcher stops", async () => {
  let fail!: (error: unknown) => void;
  ports.listen.mockImplementation(
    () =>
      new Promise((_resolve, reject) => {
        fail = reject;
      }),
  );
  const stop = onDesktopNavigate(vi.fn());
  stop();
  fail(new Error("late failure"));
  await settle();
  expect(dispatch).not.toHaveBeenCalled();
});
it("rejects inherited required fields and drops inherited optional metadata", async () => {
  const callback = vi.fn();
  const stop = onDesktopNavigate(callback);
  await settle();
  receive(
    Object.assign(Object.create({ agentId: "foreign" }), {
      section: "messages",
    }),
  );
  expect(callback).not.toHaveBeenCalled();
  receive(
    Object.assign(Object.create({ serverId: "foreign" }), {
      agentId: "chat",
      section: "messages",
    }),
  );
  expect(callback).toHaveBeenCalledExactlyOnceWith({
    agentId: "chat",
    section: "messages",
  });
  stop();
});
it("keeps payload validation in parity with the named Rust broker owner", async () => {
  const source = await readFile(
    fileURLToPath(
      new URL("../../../native/src-tauri/src/ui_events.rs", import.meta.url),
    ),
    "utf8",
  );
  const contract = source.match(
    /keys\(&\[("agentId"[\s\S]*?)\]\)([\s\S]*?)\n    };/,
  );
  expect(contract).not.toBeNull();
  const keys = contract![1]
    .match(/"[^"]+"/g)
    ?.map((value) => JSON.parse(value));
  expect(keys).toEqual(["agentId", "section", "serverId", "itemId"]);
  const renderer = await readFile(
    fileURLToPath(new URL("./hostNotifications.ts", import.meta.url)),
    "utf8",
  );
  const fields = renderer
    .match(/const NAVIGATION_FIELDS\s*=\s*\[([^\]]*)\]/)?.[1]
    .match(/"[^"]+"/g)
    ?.map((value) => JSON.parse(value));
  expect(fields).toEqual(keys);
  expect(contract![2]).toContain('text("agentId")');
  expect(contract![2]).toContain('payload["section"] == "messages"');
  expect(contract![2]).toContain('optional_text("serverId")');
  expect(contract![2]).toContain('optional_text("itemId")');
  expect(source).toContain("!value.is_empty()");
});

it("surfaces a synchronous registration failure without changing the public stop signature", () => {
  ports.listen.mockImplementation(() => {
    throw new Error("setup failed");
  });
  const stop = onDesktopNavigate(vi.fn());
  expect(stop).toBeTypeOf("function");
  expect(dispatch.mock.calls[0][0]).toMatchObject({
    type: "desktop-error",
    detail: "Error: setup failed",
  });
  expect(ports.listen).toHaveBeenCalledOnce();
  stop();
});
it("keeps a later watcher active when an earlier watcher stops", async () => {
  const callbacks: Callback[] = [],
    stops = [vi.fn(), vi.fn()];
  ports.listen.mockImplementation((_topic, callback) => {
    callbacks.push(callback);
    return Promise.resolve(stops[callbacks.length - 1]);
  });
  const first = vi.fn(),
    second = vi.fn();
  const stopFirst = onDesktopNavigate(first),
    stopSecond = onDesktopNavigate(second);
  await settle();
  stopFirst();
  callbacks[0]({ agentId: "old", section: "messages" });
  callbacks[1]({ agentId: "current", section: "messages" });
  expect(first).not.toHaveBeenCalled();
  expect(second).toHaveBeenCalledExactlyOnceWith({
    agentId: "current",
    section: "messages",
  });
  expect(stops[0]).toHaveBeenCalledOnce();
  expect(stops[1]).not.toHaveBeenCalled();
  stopSecond();
});
