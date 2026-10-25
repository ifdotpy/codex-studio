import { describe, it, expect } from "vitest";
import {
  createShellPolicy,
  replaceBackgroundTasks,
  backgroundOutputPath,
} from "./shell-policy.mjs";

function fixture() {
  let now = 0,
    size = 0;
  const active = {
    turn: null,
    tasks: new Map([
      [
        "shell",
        {
          task_id: "shell",
          task_type: "local_bash",
          is_backgrounded: true,
          status: "running",
          output_file: "/output",
        },
      ],
    ]),
  };
  const events = [],
    stops = [];
  const policy = createShellPolicy({
    now: () => now,
    stat: async () => ({ size }),
    emit: (e) => events.push(e),
    stop: async (id) => stops.push(id),
    busy: () => !!active.turn,
  });
  const tick = (extra = {}) =>
    policy.tick(active, {
      settings: { quietThresholdMs: 10000, pressureCleanup: true },
      eligible: true,
      critical: false,
      ...extra,
    });
  return {
    active,
    events,
    stops,
    policy,
    tick,
    time: (t) => {
      now = t;
    },
    output: (n) => {
      size = n;
    },
  };
}

describe("background shell policy", () => {
  it("checks in once per quiet episode and rearms after output", async () => {
    const f = fixture();
    await f.tick();
    f.time(9999);
    await f.tick();
    expect(f.events).toHaveLength(0);
    f.time(10000);
    await f.tick();
    await f.tick();
    expect(f.events.map((e) => e.kind)).toEqual(["quiet"]);
    f.output(1);
    await f.tick();
    f.time(20000);
    await f.tick();
    expect(f.events.map((e) => e.kind)).toEqual(["quiet", "quiet"]);
  });
  it("cleans up only on critical pressure after thirty idle minutes", async () => {
    const f = fixture();
    await f.tick();
    f.time(1799999);
    await f.tick({ critical: true });
    expect(f.stops).toEqual([]);
    f.time(1800000);
    await f.tick();
    expect(f.stops).toEqual([]);
    await f.tick({ critical: true, eligible: false });
    expect(f.stops).toEqual([]);
    await f.tick({ critical: true });
    expect(f.stops).toEqual(["shell"]);
    await f.tick({ critical: true });
    expect(f.stops).toEqual(["shell"]);
    expect(f.events.slice(-2).map((e) => e.kind)).toEqual([
      "cleanup_requested",
      "cleanup_stopped",
    ]);
  });
  it("rechecks the active turn after the output read", async () => {
    const f = fixture();
    await f.tick();
    f.time(1800000);
    f.active.turn = {};
    await f.tick({ critical: true });
    expect(f.stops).toEqual([]);
    f.active.turn = null;
    f.policy.touch();
    await f.tick({ critical: true });
    expect(f.stops).toEqual([]);
  });
  it("does not apply shell policy to native delegates or foreground Bash", async () => {
    const f = fixture();
    f.active.tasks.get("shell").is_backgrounded = false;
    await f.tick();
    f.time(1800000);
    await f.tick({ critical: true });
    expect(f.events).toEqual([]);
    expect(f.stops).toEqual([]);
    f.active.tasks.get("shell").task_type = "local_agent";
    await f.tick({ critical: true });
    expect(f.events).toEqual([]);
  });
});

describe("native output and lifecycle boundaries", () => {
  it("does not stop a replacement run after an output read", async () => {
    let now = 0;
    const active = fixture().active,
      stops = [];
    active.tasks.get("shell").run_id = "old";
    const policy = createShellPolicy({
      now: () => now,
      stat: async () => {
        if (now > 0)
          active.tasks.set("shell", {
            ...active.tasks.get("shell"),
            run_id: "new",
          });
        return { size: 0 };
      },
      busy: () => false,
      emit: () => {},
      stop: async (id) => stops.push(id),
    });
    const args = {
      settings: { quietThresholdMs: 0, pressureCleanup: true },
      eligible: true,
      critical: true,
    };
    await policy.tick(active, args);
    now = 1800000;
    await policy.tick(active, args);
    expect(stops).toEqual([]);
  });

  it("honors the native pressure opt-out", async () => {
    let now = 0;
    const active = fixture().active,
      stops = [];
    const policy = createShellPolicy({
      now: () => now,
      pressureDisabled: true,
      stat: async () => ({ size: 0 }),
      busy: () => false,
      emit: () => {},
      stop: async (id) => stops.push(id),
    });
    const args = {
      settings: { quietThresholdMs: 0, pressureCleanup: true },
      eligible: true,
      critical: true,
    };
    await policy.tick(active, args);
    now = 1800000;
    await policy.tick(active, args);
    expect(stops).toEqual([]);
  });

  it("retains Bash output paths across replace snapshots, but not across runs", () => {
    const first = new Map([
      [
        "shell",
        {
          task_id: "shell",
          run_id: "run-a",
          output_file: "/tmp/a.output",
          is_backgrounded: true,
        },
      ],
    ]);
    expect(
      replaceBackgroundTasks(first, [
        { task_id: "shell", run_id: "run-a", task_type: "local_bash" },
      ]).get("shell").output_file,
    ).toBe("/tmp/a.output");
    expect(
      replaceBackgroundTasks(first, [
        { task_id: "shell", run_id: "run-b", task_type: "local_bash" },
      ]).get("shell").output_file,
    ).toBeUndefined();
    expect(
      backgroundOutputPath(
        {},
        "Command running in background with ID: a. Output is being written to: /tmp/a.output. You will be notified when it completes.",
      ),
    ).toBe("/tmp/a.output");
    expect(
      backgroundOutputPath({ backgroundOutputPath: "/tmp/remote.output" }, ""),
    ).toBe("/tmp/remote.output");
  });
  it("keeps defaults off for quiet checks and supports the cleanup opt-out", async () => {
    const f = fixture();
    await f.tick({ settings: { quietThresholdMs: 0, pressureCleanup: false } });
    f.time(1800000);
    await f.tick({
      critical: true,
      settings: { quietThresholdMs: 0, pressureCleanup: false },
    });
    expect(f.events).toEqual([]);
    expect(f.stops).toEqual([]);
    await expect(
      f.tick({ settings: { quietThresholdMs: 9999, pressureCleanup: true } }),
    ).rejects.toThrow("Invalid");
  });
  it("never retries an unknown stop and reports its uncertainty", async () => {
    let now = 0,
      stops = 0;
    const events = [],
      active = fixture().active;
    const policy = createShellPolicy({
      now: () => now,
      stat: async () => ({ size: 0 }),
      busy: () => false,
      emit: (report) => events.push(report),
      stop: async () => {
        stops++;
        throw new Error("lost");
      },
    });
    const args = {
      settings: { quietThresholdMs: 0, pressureCleanup: true },
      eligible: true,
      critical: true,
    };
    await policy.tick(active, args);
    now = 1800000;
    await policy.tick(active, args);
    await policy.tick(active, args);
    expect(stops).toBe(1);
    expect(events.map((e) => e.kind)).toEqual([
      "cleanup_requested",
      "cleanup_unknown",
    ]);
  });
  it("rechecks active work after an asynchronous output read", async () => {
    let now = 0,
      running = false;
    const active = fixture().active,
      events = [],
      stops = [];
    const policy = createShellPolicy({
      now: () => now,
      stat: async () => {
        if (now > 0) running = true;
        return { size: 0 };
      },
      busy: () => running,
      emit: (e) => events.push(e),
      stop: async (id) => stops.push(id),
    });
    const args = {
      settings: { quietThresholdMs: 10000, pressureCleanup: true },
      eligible: true,
      critical: true,
    };
    await policy.tick(active, args);
    now = 1800000;
    await policy.tick(active, args);
    expect(events).toEqual([]);
    expect(stops).toEqual([]);
  });
  it("treats unreadable output as unknown and blocks native delegates", async () => {
    let now = 0;
    const active = fixture().active,
      events = [];
    const policy = createShellPolicy({
      now: () => now,
      stat: async () => {
        throw new Error("gone");
      },
      busy: () => false,
      emit: (e) => events.push(e),
      stop: async () => {
        throw new Error("must not stop");
      },
    });
    const args = {
      settings: { quietThresholdMs: 10000, pressureCleanup: true },
      eligible: true,
      critical: false,
    };
    await policy.tick(active, args);
    now = 10000;
    await policy.tick(active, args);
    expect(events).toEqual([]);
    active.tasks.set("delegate", {
      task_id: "delegate",
      task_type: "local_agent",
      status: "running",
    });
    now = 1800000;
    await policy.tick(active, { ...args, critical: true });
    expect(events).toEqual([]);
  });
});
