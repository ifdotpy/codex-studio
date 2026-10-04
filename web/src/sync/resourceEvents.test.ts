import { afterEach, describe, expect, it, vi } from "vitest";

const workspaceId = "b".repeat(32);
const { resumeListeners, syncDatabase } = vi.hoisted(() => ({
  resumeListeners: new Set<() => void>(),
  syncDatabase: vi.fn(),
}));
vi.mock("./client", () => ({ syncDatabase }));
vi.mock("../usage/tokenRate", () => ({
  configureTokenRateStream: vi.fn(),
  receiveResourceTokenRates: vi.fn(),
}));
vi.mock("./resume", () => ({
  onResume: (listener: () => void) => {
    resumeListeners.add(listener);
    return () => resumeListeners.delete(listener);
  },
}));

class Source {
  static instances: Source[] = [];
  listeners = new Map<string, Set<(event: MessageEvent<string>) => void>>();
  onerror: (() => void) | null = null;
  closed = false;
  constructor(readonly url: string) {
    Source.instances.push(this);
  }
  addEventListener(
    name: string,
    listener: (event: MessageEvent<string>) => void,
  ) {
    let callbacks = this.listeners.get(name);
    if (!callbacks) this.listeners.set(name, (callbacks = new Set()));
    callbacks.add(listener);
  }
  emit(name: string, value: unknown) {
    const event = { data: JSON.stringify(value) } as MessageEvent<string>;
    for (const listener of this.listeners.get(name) || []) listener(event);
  }
  close() {
    this.closed = true;
  }
}

class Channel {
  static instances: Channel[] = [];
  onmessage: ((event: MessageEvent) => void) | null = null;
  messages: unknown[] = [];
  constructor(readonly name: string) {
    Channel.instances.push(this);
  }
  postMessage(value: unknown) {
    this.messages.push(value);
  }
  close() {}
}

describe("shared resource event transport", () => {
  afterEach(() => {
    vi.resetModules();
    vi.useRealTimers();
    vi.unstubAllGlobals();
    Source.instances = [];
    Channel.instances = [];
    syncDatabase.mockClear();
    resumeListeners.clear();
  });

  it("notifies only the matching resource once per revision and preserves liveness on reconfigure", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);

    const transport = await import("./resourceEvents");
    const panel = { kind: "panel", agentId: "agent-one" } as const;
    const terminal = { kind: "terminal", terminalId: "terminal-one" } as const;
    const panelChanges = vi.fn();
    const terminalChanges = vi.fn();
    const states: string[] = [];
    const stopPanel = transport.watchResourceChanges(panel, panelChanges);
    const stopStatus = transport.watchResourceConnection((state) =>
      states.push(state),
    );
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const first = Source.instances[0]!;
    first.emit("resources", {
      protocol: 3,
      workspaceId,
      epoch: "epoch-one",
      revision: 1,
      reason: "initial",
      resources: [panel],
    });
    await vi.waitFor(() => expect(panelChanges).toHaveBeenCalledTimes(1));
    first.emit("resources", {
      protocol: 3,
      workspaceId,
      epoch: "epoch-one",
      revision: 1,
      reason: "change",
      resources: [panel],
    });
    await new Promise((resolve) => setTimeout(resolve, 30));
    expect(panelChanges).toHaveBeenCalledTimes(1);
    first.emit("resources", {
      protocol: 3,
      workspaceId,
      epoch: "epoch-one",
      revision: 2,
      reason: "change",
      resources: [{ kind: "panel", agentId: "another-agent" }],
    });
    await new Promise((resolve) => setTimeout(resolve, 40));
    expect(panelChanges).toHaveBeenCalledTimes(1);

    const stopTerminal = transport.watchResourceChanges(
      terminal,
      terminalChanges,
    );
    await vi.waitFor(() => expect(Source.instances).toHaveLength(2));
    const replacement = Source.instances[1]!;
    expect(states.slice(2).every((state) => state === "live")).toBe(true);
    expect(states.at(-1)).toBe("live");
    replacement.onerror?.();
    expect(states.at(-1)).toBe("degraded");
    expect(terminalChanges).not.toHaveBeenCalled();

    stopTerminal();
    stopPanel();
    stopStatus();
    expect(Source.instances.every((source) => source.closed)).toBe(true);
    expect(syncDatabase).toHaveBeenCalledTimes(1);
  });

  it("keeps a quiet stream live across repeated server heartbeat intervals", async () => {
    vi.useFakeTimers();
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);

    const transport = await import("./resourceEvents");
    const stop = transport.watchResourceChanges(
      { kind: "panel", agentId: "agent-one" },
      vi.fn(),
    );
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const stream = Source.instances[0]!;

    for (let revision = 1; revision <= 4; revision++) {
      stream.emit("heartbeat", {
        protocol: 3,
        workspaceId,
        epoch: "epoch-one",
        revision,
      });
      await vi.advanceTimersByTimeAsync(15_000);
    }

    expect(Source.instances).toHaveLength(1);
    expect(stream.closed).toBe(false);
    expect(syncDatabase).toHaveBeenCalledTimes(1);
    stop();
    vi.useRealTimers();
  });

  it("opens only one replacement stream when online and pageshow resume overlap", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    const windowListeners = new Map<string, () => void>();
    const windowStub = {
      addEventListener: vi.fn((name: string, listener: () => void) =>
        windowListeners.set(name, listener),
      ),
      removeEventListener: vi.fn(),
    };
    const navigatorStub = { onLine: true };
    vi.stubGlobal("window", windowStub);
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", navigatorStub);
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);

    const transport = await import("./resourceEvents");
    const stop = transport.watchResourceChanges(
      { kind: "voice", agentId: "agent-one" },
      vi.fn(),
    );
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const first = Source.instances[0]!;
    first.emit("resources", {
      protocol: 3,
      workspaceId,
      epoch: "epoch-one",
      revision: 1,
      reason: "initial",
      resources: [{ kind: "voice", agentId: "agent-one" }],
    });
    await vi.waitFor(() => expect(first.closed).toBe(false));

    navigatorStub.onLine = false;
    windowListeners.get("offline")?.();
    expect(first.closed).toBe(true);
    navigatorStub.onLine = true;
    windowListeners.get("online")?.();
    for (const listener of resumeListeners) listener();

    await vi.waitFor(() => expect(Source.instances).toHaveLength(2));
    expect(Source.instances.filter((source) => !source.closed)).toHaveLength(1);
    stop();
  });

  it("treats valid busy-stream events as liveness and reconnects after silence", async () => {
    vi.useFakeTimers();
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);

    const transport = await import("./resourceEvents");
    const resource = { kind: "costs" } as const;
    const onChange = vi.fn();
    const states: string[] = [];
    const stopChanges = transport.watchResourceChanges(resource, onChange);
    const stopStatus = transport.watchResourceConnection((state) =>
      states.push(state),
    );
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const first = Source.instances[0]!;

    for (let revision = 1; revision <= 6; revision++) {
      first.emit("resources", {
        protocol: 3,
        workspaceId,
        epoch: "epoch-one",
        revision,
        reason: revision === 1 ? "initial" : "change",
        resources: [resource],
      });
      await vi.advanceTimersByTimeAsync(10_000);
    }
    expect(Source.instances).toHaveLength(1);
    expect(states).not.toContain("degraded");
    expect(onChange).toHaveBeenCalled();

    await vi.advanceTimersByTimeAsync(46_000);
    expect(Source.instances).toHaveLength(2);
    expect(states).toContain("degraded");
    const replacement = Source.instances[1]!;
    replacement.emit("resources", {
      protocol: 3,
      workspaceId,
      epoch: "epoch-one",
      revision: 7,
      reason: "reconnect",
      resources: [resource],
    });
    await vi.advanceTimersByTimeAsync(25);
    expect(states.at(-1)).toBe("live");
    stopChanges();
    stopStatus();
    vi.useRealTimers();
  });

  it("delivers resource and token-rate events that share a hub revision", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);

    const transport = await import("./resourceEvents");
    const resource = { kind: "costs" } as const;
    const onResourceChange = vi.fn();
    const onTokenRates = vi.fn();
    const stopResource = transport.watchResourceChanges(
      resource,
      onResourceChange,
    );
    const stopTokenRates = transport.watchTokenRateEvents(onTokenRates);
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const stream = Source.instances[0]!;

    stream.emit("token-rates", {
      protocol: 3,
      workspaceId,
      epoch: "epoch-one",
      revision: 3,
      rates: {},
      teams: {},
    });
    stream.emit("resources", {
      protocol: 3,
      workspaceId,
      epoch: "epoch-one",
      revision: 3,
      reason: "change",
      resources: [resource],
    });

    await vi.waitFor(() => expect(onResourceChange).toHaveBeenCalledTimes(1));
    expect(onTokenRates).toHaveBeenCalledTimes(1);
    stopTokenRates();
    stopResource();
  });

  it("replays a baseline for a late peer ref already present in an earlier SSE event", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", {
      onLine: true,
      locks: {
        request: (
          _name: string,
          _options: unknown,
          callback: (lock: {}) => Promise<void>,
        ) => Promise.resolve().then(() => callback({})),
      },
    });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-owner" });
    vi.stubGlobal("EventSource", Source);
    vi.stubGlobal("BroadcastChannel", Channel);

    const transport = await import("./resourceEvents");
    const local = { kind: "state" } as const;
    const latePeerRef = { kind: "panel", agentId: "late-panel" } as const;
    const localChanges = vi.fn();
    const stop = transport.watchResourceChanges(local, localChanges);
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const ownerChannel = Channel.instances[0]!;
    Source.instances[0]!.emit("resources", {
      protocol: 3,
      workspaceId,
      epoch: "epoch-one",
      revision: 7,
      reason: "initial",
      resources: [local, latePeerRef],
    });
    await new Promise((resolve) => setTimeout(resolve, 30));

    expect(localChanges).toHaveBeenCalledTimes(1);

    const sentBeforeLateSubscription = ownerChannel.messages.length;
    ownerChannel.onmessage?.({
      data: {
        kind: "subscriptions",
        workspaceId,
        tabId: "tab-follower",
        resources: [latePeerRef],
        tokenRates: false,
        reset: true,
      },
    } as MessageEvent);

    expect(
      ownerChannel.messages
        .slice(sentBeforeLateSubscription)
        .some(
          (message: any) =>
            message.kind === "resource-event" &&
            message.event.resources.some(
              (resource: any) =>
                resource.kind === "panel" && resource.agentId === "late-panel",
            ),
        ),
    ).toBe(true);
    stop();
  });

  it("bounds inactive versions and reconciles an evicted late peer ref", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", {
      onLine: true,
      locks: {
        request: (
          _name: string,
          _options: unknown,
          callback: (lock: {}) => Promise<void>,
        ) => Promise.resolve().then(() => callback({})),
      },
    });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-owner" });
    vi.stubGlobal("EventSource", Source);
    vi.stubGlobal("BroadcastChannel", Channel);

    const transport = await import("./resourceEvents");
    const local = { kind: "state" } as const;
    const bulkRefs = Array.from({ length: 5_000 }, (_, index) => ({
      kind: "panel" as const,
      agentId: `inactive-${index}`,
    }));
    const evictedRef = bulkRefs[0]!;
    const localChanges = vi.fn();
    const stop = transport.watchResourceChanges(local, localChanges);
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const ownerChannel = Channel.instances[0]!;
    Source.instances[0]!.emit("resources", {
      protocol: 3,
      workspaceId,
      epoch: "epoch-one",
      revision: 7,
      reason: "initial",
      resources: [local, ...bulkRefs],
    });
    await new Promise((resolve) => setTimeout(resolve, 30));

    const beforeCacheInspection = ownerChannel.messages.length;
    ownerChannel.onmessage?.({
      data: {
        kind: "subscriptions",
        workspaceId,
        tabId: "tab-cache-inspection",
        resources: [local, ...bulkRefs],
        tokenRates: false,
        reset: true,
      },
    } as MessageEvent);
    const cachedBaseline = ownerChannel.messages
      .slice(beforeCacheInspection)
      .find((message: any) => message.kind === "resource-event") as
      | { event: { resources: unknown[] } }
      | undefined;
    const cachedResources = cachedBaseline?.event.resources ?? [];
    expect(cachedResources).toHaveLength(129);
    expect(cachedResources).toContainEqual(local);
    expect(cachedResources).toContainEqual(bulkRefs.at(-1));
    expect(cachedResources).not.toContainEqual(evictedRef);
    ownerChannel.onmessage?.({
      data: {
        kind: "subscriptions",
        workspaceId,
        tabId: "tab-cache-inspection",
        resources: [],
        tokenRates: false,
        reset: true,
      },
    } as MessageEvent);

    const sentBeforeLateSubscription = ownerChannel.messages.length;
    const sourceCountBeforeLateSubscription = Source.instances.length;
    ownerChannel.onmessage?.({
      data: {
        kind: "subscriptions",
        workspaceId,
        tabId: "tab-follower",
        resources: [evictedRef],
        tokenRates: false,
        reset: true,
      },
    } as MessageEvent);

    expect(Source.instances).toHaveLength(
      sourceCountBeforeLateSubscription + 1,
    );
    expect(
      ownerChannel.messages
        .slice(sentBeforeLateSubscription)
        .some((message: any) => message.kind === "resource-event"),
    ).toBe(false);

    Source.instances.at(-1)!.emit("resources", {
      protocol: 3,
      workspaceId,
      epoch: "epoch-one",
      revision: 8,
      reason: "initial",
      resources: [local, evictedRef],
    });
    await new Promise((resolve) => setTimeout(resolve, 30));
    expect(
      ownerChannel.messages
        .slice(sentBeforeLateSubscription)
        .some(
          (message: any) =>
            message.kind === "resource-event" &&
            message.event.resources.some(
              (resource: any) =>
                resource.kind === "panel" &&
                resource.agentId === evictedRef.agentId,
            ),
        ),
    ).toBe(true);
    expect(localChanges).toHaveBeenCalledTimes(2);
    stop();
  });

  it("reconciles an unchanged owner baseline once after reconnect", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.useFakeTimers();
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", {
      onLine: true,
      locks: {
        request: (
          _name: string,
          _options: unknown,
          callback: (lock: {}) => Promise<void>,
        ) => Promise.resolve().then(() => callback({})),
      },
    });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-owner" });
    vi.stubGlobal("EventSource", Source);
    vi.stubGlobal("BroadcastChannel", Channel);

    const transport = await import("./resourceEvents");
    const local = { kind: "panel", agentId: "resume-panel" } as const;
    const onChange = vi.fn();
    const stop = transport.watchResourceChanges(local, onChange);
    await Promise.resolve();
    await Promise.resolve();
    expect(Source.instances).toHaveLength(1);
    const initial = {
      protocol: 3,
      workspaceId,
      epoch: "epoch-one",
      revision: 7,
      reason: "initial",
      resources: [local],
    };
    Source.instances[0]!.emit("resources", initial);
    await vi.advanceTimersByTimeAsync(20);
    expect(onChange).toHaveBeenCalledTimes(1);

    Source.instances[0]!.onerror?.();
    await vi.advanceTimersByTimeAsync(5_000);
    expect(Source.instances).toHaveLength(2);
    Source.instances[1]!.emit("resources", {
      ...initial,
      reason: "reconnect",
    });
    await vi.advanceTimersByTimeAsync(20);
    expect(onChange).toHaveBeenCalledTimes(2);

    Source.instances[1]!.emit("resources", initial);
    await vi.advanceTimersByTimeAsync(20);
    expect(onChange).toHaveBeenCalledTimes(2);

    Channel.instances[0]!.onmessage?.({
      data: {
        kind: "resource-event",
        workspaceId,
        tabId: "tab-peer",
        event: initial,
      },
    } as MessageEvent);
    await vi.advanceTimersByTimeAsync(20);
    expect(onChange).toHaveBeenCalledTimes(2);
    stop();
  });

  it("reconciles an unchanged follower baseline once and forgets unsubscribed markers", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", {
      onLine: true,
      locks: {
        request: (
          _name: string,
          _options: unknown,
          callback: (lock: null) => Promise<void>,
        ) => Promise.resolve().then(() => callback(null)),
      },
    });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-follower" });
    vi.stubGlobal("EventSource", Source);
    vi.stubGlobal("BroadcastChannel", Channel);

    const transport = await import("./resourceEvents");
    const a = { kind: "panel", agentId: "follower-resume-a" } as const;
    const b = { kind: "panel", agentId: "follower-resume-b" } as const;
    const onA = vi.fn();
    const onB = vi.fn();
    let stopA = transport.watchResourceChanges(a, onA);
    const stopB = transport.watchResourceChanges(b, onB);
    await Promise.resolve();
    await Promise.resolve();
    expect(Source.instances).toHaveLength(0);
    const followerChannel = Channel.instances[0]!;
    const baseline = {
      protocol: 3,
      workspaceId,
      epoch: "epoch-one",
      revision: 7,
      reason: "initial",
      resources: [a, b],
    };
    const send = (kind: string, value: unknown) =>
      followerChannel.onmessage?.({
        data: {
          kind,
          workspaceId,
          tabId: "tab-owner",
          ...(kind === "resource-event" ? { event: value } : { status: value }),
        },
      } as MessageEvent);

    send("resource-event", baseline);
    await vi.waitFor(() => {
      expect(onA).toHaveBeenCalledTimes(1);
      expect(onB).toHaveBeenCalledTimes(1);
    });
    send("status", "degraded");
    send("resource-event", { ...baseline, reason: "reconnect" });
    await vi.waitFor(() => {
      expect(onA).toHaveBeenCalledTimes(2);
      expect(onB).toHaveBeenCalledTimes(2);
    });

    send("resource-event", { ...baseline, reason: "reconnect" });
    send("leader-heartbeat", undefined);
    send("leader-heartbeat", undefined);
    await new Promise((resolve) => setTimeout(resolve, 40));
    expect(onA).toHaveBeenCalledTimes(2);
    expect(onB).toHaveBeenCalledTimes(2);
    expect(Source.instances).toHaveLength(0);

    send("status", "degraded");
    stopA();
    const resumedA = vi.fn();
    stopA = transport.watchResourceChanges(a, resumedA);
    expect(resumedA).toHaveBeenCalledTimes(1);
    send("resource-event", { ...baseline, reason: "reconnect" });
    await vi.waitFor(() => expect(onB).toHaveBeenCalledTimes(3));
    await new Promise((resolve) => setTimeout(resolve, 40));
    expect(resumedA).toHaveBeenCalledTimes(1);
    expect(Source.instances).toHaveLength(0);

    stopA();
    stopB();
  });

  it("drops a recovery marker when its last local subscriber leaves", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.useFakeTimers();
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", {
      onLine: true,
      locks: {
        request: (
          _name: string,
          _options: unknown,
          callback: (lock: {}) => Promise<void>,
        ) => Promise.resolve().then(() => callback({})),
      },
    });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-owner" });
    vi.stubGlobal("EventSource", Source);
    vi.stubGlobal("BroadcastChannel", Channel);

    const transport = await import("./resourceEvents");
    const a = { kind: "panel", agentId: "resume-panel-a" } as const;
    const b = { kind: "panel", agentId: "resume-panel-b" } as const;
    const onA = vi.fn();
    const onB = vi.fn();
    let stopA = transport.watchResourceChanges(a, onA);
    const stopB = transport.watchResourceChanges(b, onB);
    await Promise.resolve();
    await Promise.resolve();
    expect(Source.instances).toHaveLength(1);

    const baseline = {
      protocol: 3,
      workspaceId,
      epoch: "epoch-one",
      revision: 7,
      reason: "initial",
      resources: [a, b],
    };
    Source.instances[0]!.emit("resources", baseline);
    await vi.advanceTimersByTimeAsync(20);
    expect(onA).toHaveBeenCalledTimes(1);
    expect(onB).toHaveBeenCalledTimes(1);

    Source.instances[0]!.onerror?.();
    stopA();
    await vi.advanceTimersByTimeAsync(5_000);
    expect(Source.instances).toHaveLength(2);
    Source.instances[1]!.emit("resources", {
      ...baseline,
      reason: "reconnect",
    });
    await vi.advanceTimersByTimeAsync(20);
    expect(onB).toHaveBeenCalledTimes(2);

    const resumedA = vi.fn();
    stopA = transport.watchResourceChanges(a, resumedA);
    expect(resumedA).toHaveBeenCalledTimes(1);
    Source.instances[1]!.emit("resources", baseline);
    await vi.advanceTimersByTimeAsync(20);
    expect(resumedA).toHaveBeenCalledTimes(1);
    stopA();
    stopB();
  });
});
