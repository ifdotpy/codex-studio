import { afterEach, describe, expect, it, vi } from "vitest";

const workspaceId = "b".repeat(32);
const { syncDatabase } = vi.hoisted(() => ({ syncDatabase: vi.fn() }));
vi.mock("./client", () => ({ syncDatabase }));
vi.mock("../usage/tokenRate", () => ({
  configureTokenRateStream: vi.fn(),
  receiveResourceTokenRates: vi.fn(),
}));
vi.mock("./resume", () => ({ onResume: vi.fn(() => () => {}) }));

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

describe("shared resource event transport", () => {
  afterEach(() => {
    vi.resetModules();
    vi.useRealTimers();
    vi.unstubAllGlobals();
    Source.instances = [];
    syncDatabase.mockClear();
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
});
