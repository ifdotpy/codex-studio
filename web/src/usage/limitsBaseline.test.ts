import { describe, expect, it, vi } from "vitest";
import type { ResourceVersion } from "../sync/resourceEvents";
import {
  type AccountLimitsSnapshot,
  limitsHydrationSucceeded,
  limitsReadSucceeded,
  limitsSnapshotIsFresh,
} from "./accountUsage";
import { limitsBaselineReader } from "./limitsBaseline";

const version = (revision: number, epoch = "workspace") =>
  ({ epoch, revision }) satisfies ResourceVersion;

describe("limits baseline hydration", () => {
  it("keeps the baseline open after a versionless reset notification", async () => {
    const hydrate = vi.fn(async () => true);
    const read = vi.fn(async () => {});
    const onBaseline = limitsBaselineReader(hydrate, read);

    await onBaseline();
    await onBaseline(version(3));
    await onBaseline(version(3));

    expect(hydrate).toHaveBeenCalledTimes(1);
    expect(read).not.toHaveBeenCalled();
  });

  it("does not repeat the fallback after a versionless reset notification", async () => {
    const hydrate = vi.fn(async () => false);
    const read = vi.fn(async () => {});
    const onBaseline = limitsBaselineReader(hydrate, read);

    await onBaseline();
    await onBaseline(version(3));
    await onBaseline(version(3));

    expect(hydrate).toHaveBeenCalledTimes(1);
    expect(read).toHaveBeenCalledTimes(1);
  });

  it("runs one fresh read when cache hydration rejects and shows its limits", async () => {
    let shownLimits: number | undefined;
    const requests = { cached: 0, fresh: 0 };
    const hydrate = vi.fn(async () => {
      requests.cached++;
      throw new Error("cache unavailable");
    });
    const freshRead = vi.fn(async () => {
      requests.fresh++;
      shownLimits = 42;
    });
    const onBaseline = limitsBaselineReader(hydrate, freshRead);

    await onBaseline(version(1));

    expect(freshRead).toHaveBeenCalledTimes(1);
    expect(requests).toEqual({ cached: 1, fresh: 1 });
    expect(shownLimits).toBe(42);
  });

  it("suppresses an unchanged reconnect baseline and reads a newer version", async () => {
    const requests = { cached: 0, fresh: 0 };
    let resolveHydration!: (result: boolean) => void;
    const hydration = new Promise<boolean>((resolve) => {
      resolveHydration = resolve;
    });
    const hydrate = vi.fn(() => {
      requests.cached++;
      return hydration;
    });
    const freshRead = vi.fn(async () => {
      requests.fresh++;
    });
    const onBaseline = limitsBaselineReader(hydrate, freshRead);

    const baseline = onBaseline(version(1));
    expect(requests).toEqual({ cached: 1, fresh: 0 });
    resolveHydration(true);
    await baseline;

    expect(hydrate).toHaveBeenCalledTimes(1);
    expect(freshRead).not.toHaveBeenCalled();
    expect(requests).toEqual({ cached: 1, fresh: 0 });

    await onBaseline(version(1));
    expect(freshRead).not.toHaveBeenCalled();
    expect(requests).toEqual({ cached: 1, fresh: 0 });

    await onBaseline(version(2));
    expect(freshRead).toHaveBeenCalledTimes(1);
    expect(requests).toEqual({ cached: 1, fresh: 1 });

    await onBaseline(version(2));
    expect(freshRead).toHaveBeenCalledTimes(1);
    expect(requests).toEqual({ cached: 1, fresh: 1 });
  });

  it("reads once more for the newest version that arrives during a read", async () => {
    const pendingReads: Array<(succeeded: boolean) => void> = [];
    const read = vi.fn(
      () =>
        new Promise<boolean>((resolve) => {
          if (pendingReads.length >= 2)
            throw new Error("version drain exceeded its read bound");
          pendingReads.push(resolve);
        }),
    );
    const onBaseline = limitsBaselineReader(
      async () => true,
      read,
      undefined,
      () => true,
    );

    await onBaseline(version(1));
    const versionTwo = onBaseline(version(2));
    await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(1));
    const versionThree = onBaseline(version(3));
    await Promise.resolve();

    expect(read).toHaveBeenCalledTimes(1);
    pendingReads[0](true);
    await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(2));
    const duplicateVersionThree = onBaseline(version(3));
    pendingReads[1](true);
    await Promise.all([versionTwo, versionThree, duplicateVersionThree]);

    await onBaseline(version(3));
    expect(read).toHaveBeenCalledTimes(2);
  }, 1_000);

  it("reads a queued newer version after the current read rejects", async () => {
    const pendingReads: Array<{
      resolve: (succeeded: boolean) => void;
      reject: (error: Error) => void;
    }> = [];
    const read = vi.fn(
      () =>
        new Promise<boolean>((resolve, reject) => {
          if (pendingReads.length >= 2)
            throw new Error("version drain exceeded its read bound");
          pendingReads.push({ resolve, reject });
        }),
    );
    const onBaseline = limitsBaselineReader(
      async () => true,
      read,
      undefined,
      () => true,
    );

    await onBaseline(version(1));
    const versionTwo = onBaseline(version(2));
    await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(1));
    const versionThree = onBaseline(version(3));

    pendingReads[0].reject(new Error("version 2 read failed"));
    await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(2));
    pendingReads[1].resolve(true);
    await Promise.all([versionTwo, versionThree]);

    await onBaseline(version(3));
    expect(read).toHaveBeenCalledTimes(2);
  }, 1_000);

  it("drains a version received during a failing versionless baseline read", async () => {
    const pendingReads: Array<{
      resolve: (succeeded: boolean) => void;
      reject: (error: Error) => void;
    }> = [];
    const read = vi.fn(
      () =>
        new Promise<boolean>((resolve, reject) => {
          if (pendingReads.length >= 2)
            throw new Error("versionless drain exceeded its read bound");
          pendingReads.push({ resolve, reject });
        }),
    );
    const onBaseline = limitsBaselineReader(
      async () => false,
      read,
      undefined,
      () => true,
    );

    const versionlessBaseline = onBaseline();
    await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(1));
    const versionOne = onBaseline(version(1));
    pendingReads[0].reject(new Error("versionless read failed"));
    await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(2));
    pendingReads[1].resolve(true);
    await Promise.all([versionlessBaseline, versionOne]);

    expect(read).toHaveBeenCalledTimes(2);
  }, 1_000);

  it("keeps a newer version seen at read completion inside the same drain", async () => {
    const pendingReads: Array<(succeeded: boolean) => void> = [];
    const read = vi.fn(
      () =>
        new Promise<boolean>((resolve) => {
          if (pendingReads.length >= 2)
            throw new Error("version drain exceeded its read bound");
          pendingReads.push(resolve);
        }),
    );
    const onBaseline = limitsBaselineReader(
      async () => true,
      read,
      undefined,
      () => true,
    );

    await onBaseline(version(1));
    const versionTwo = onBaseline(version(2));
    await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(1));
    const versionThree = onBaseline(version(3));

    // Enter v3 before allowing the v2 read continuation to resume.
    pendingReads[0](true);
    await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(2));
    pendingReads[1](true);
    await Promise.all([versionTwo, versionThree]);

    expect(read).toHaveBeenCalledTimes(2);
  }, 1_000);

  it("drains a version notified from the successful read continuation", async () => {
    let resolveFirstRead!: (succeeded: boolean) => void;
    const firstRead = new Promise<boolean>((resolve) => {
      resolveFirstRead = resolve;
    });
    const read = vi.fn(() => {
      if (read.mock.calls.length > 2)
        throw new Error("completion-boundary drain exceeded its read bound");
      return read.mock.calls.length === 1 ? firstRead : Promise.resolve(true);
    });
    const onBaseline = limitsBaselineReader(
      async () => true,
      read,
      undefined,
      () => true,
    );

    await onBaseline(version(1));
    const versionTwo = onBaseline(version(2));
    await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(1));
    const versionThree = firstRead.then(() => onBaseline(version(3)));
    resolveFirstRead(true);
    await Promise.all([versionTwo, versionThree]);

    expect(read).toHaveBeenCalledTimes(2);
  }, 1_000);

  it("drains a version notified from the rejected read continuation", async () => {
    let rejectFirstRead!: (error: Error) => void;
    const firstRead = new Promise<boolean>((_resolve, reject) => {
      rejectFirstRead = reject;
    });
    const read = vi.fn(() => {
      if (read.mock.calls.length > 2)
        throw new Error("rejection-boundary drain exceeded its read bound");
      return read.mock.calls.length === 1 ? firstRead : Promise.resolve(true);
    });
    const onBaseline = limitsBaselineReader(
      async () => true,
      read,
      undefined,
      () => true,
    );

    await onBaseline(version(1));
    const versionTwo = onBaseline(version(2));
    await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(1));
    const versionThree = firstRead.catch(() => onBaseline(version(3)));
    rejectFirstRead(new Error("version 2 read failed"));
    const [versionTwoResult, versionThreeResult] = await Promise.allSettled([
      versionTwo,
      versionThree,
    ]);

    expect(versionTwoResult.status).toBe("rejected");
    expect(versionThreeResult.status).toBe("fulfilled");
    expect(read).toHaveBeenCalledTimes(2);
  }, 1_000);

  it("does not accept successful hydration when the snapshot is stale", async () => {
    vi.useFakeTimers();
    try {
      let snapshotAt = Date.now();
      const hydrate = vi.fn(async () => true);
      const read = vi.fn(async () => {});
      const onBaseline = limitsBaselineReader(
        hydrate,
        read,
        undefined,
        () => Date.now() - snapshotAt < 60_000,
      );

      await onBaseline(version(8));
      expect(read).not.toHaveBeenCalled();

      vi.advanceTimersByTime(2_000);
      snapshotAt = Date.now() - 60_001;
      await onBaseline(version(8));

      expect(read).toHaveBeenCalledTimes(1);
    } finally {
      vi.useRealTimers();
    }
  });

  it("does not suppress reconnect after the current limits snapshot errors", async () => {
    let snapshotFresh = true;
    const read = vi.fn(async () => {});
    const onBaseline = limitsBaselineReader(
      async () => true,
      read,
      undefined,
      () => snapshotFresh,
    );

    await onBaseline(version(9));
    await onBaseline(version(9));
    expect(read).not.toHaveBeenCalled();

    snapshotFresh = false;
    await onBaseline(version(9));

    expect(read).toHaveBeenCalledTimes(1);
  });

  it("does not suppress a versioned frame after its fallback snapshot expires", async () => {
    vi.useFakeTimers();
    try {
      const read = vi.fn(async () => {});
      const snapshotAt = Date.now() - 59_000;
      const onBaseline = limitsBaselineReader(
        async () => false,
        read,
        undefined,
        () => Date.now() - snapshotAt < 60_000,
      );

      await onBaseline();
      vi.advanceTimersByTime(2_000);
      await onBaseline(version(10));

      expect(read).toHaveBeenCalledTimes(2);
    } finally {
      vi.useRealTimers();
    }
  });

  it("coalesces an unchanged reconnect while hydration is still pending", async () => {
    let resolveHydration!: (result: boolean) => void;
    const hydration = new Promise<boolean>((resolve) => {
      resolveHydration = resolve;
    });
    const hydrate = vi.fn(() => hydration);
    const read = vi.fn(async () => {});
    const onBaseline = limitsBaselineReader(hydrate, read);

    const firstFrame = onBaseline(version(4));
    const reconnectFrame = onBaseline(version(4));
    expect(hydrate).toHaveBeenCalledTimes(1);
    expect(read).not.toHaveBeenCalled();
    resolveHydration(true);
    await Promise.all([firstFrame, reconnectFrame]);

    expect(read).not.toHaveBeenCalled();
  });

  it("uses the resolved hydration snapshot before the cache ref is rendered", async () => {
    const snapshot = {
      accountKey: "default",
      at: Date.now() / 1000,
      data: {},
    };
    let resolveHydration!: (result: { snapshot: typeof snapshot }) => void;
    const hydration = new Promise<{ snapshot: typeof snapshot }>((resolve) => {
      resolveHydration = resolve;
    });
    const read = vi.fn(async () => {});
    const onBaseline = limitsBaselineReader(
      () => hydration,
      read,
      undefined,
      (candidate) => candidate === snapshot,
    );

    const versionlessReset = onBaseline();
    resolveHydration({ snapshot });
    await versionlessReset;
    await onBaseline(version(1));

    expect(read).not.toHaveBeenCalled();
  });

  it("reads a changed reconnect version that arrives before hydration finishes", async () => {
    let resolveHydration!: (result: boolean) => void;
    const hydration = new Promise<boolean>((resolve) => {
      resolveHydration = resolve;
    });
    const read = vi.fn(async () => {});
    const onBaseline = limitsBaselineReader(() => hydration, read);

    const baseline = onBaseline(version(4));
    const changedReconnect = onBaseline(version(5));
    resolveHydration(true);
    await Promise.all([baseline, changedReconnect]);

    expect(read).toHaveBeenCalledTimes(1);
  });

  it("falls back once after failed hydration and ignores its same-version reconnect", async () => {
    const hydrate = vi.fn(async () => false);
    const read = vi.fn(async () => {});
    const onBaseline = limitsBaselineReader(hydrate, read);

    await onBaseline(version(7));
    await onBaseline(version(7));

    expect(hydrate).toHaveBeenCalledTimes(1);
    expect(read).toHaveBeenCalledTimes(1);
  });

  it("retries an unsuccessful fallback on an unchanged reconnect", async () => {
    const hydrate = vi.fn(async () => false);
    let readCount = 0;
    const read = vi.fn(async () => ++readCount > 1);
    const onBaseline = limitsBaselineReader(hydrate, read);

    await onBaseline(version(7));
    await onBaseline(version(7));
    await onBaseline(version(7));

    expect(hydrate).toHaveBeenCalledTimes(1);
    expect(read).toHaveBeenCalledTimes(2);
  });

  it("retries an HTTP 200 error snapshot on an unchanged reconnect", async () => {
    const hydrate = vi.fn(async () =>
      limitsHydrationSucceeded({
        data: { rateLimits: { primary: { usedPercent: 25 } } },
        error: "provider temporarily unavailable",
      }),
    );
    const errorSnapshot = {
      data: { rateLimits: { primary: { usedPercent: 25 } } },
      error: "provider temporarily unavailable",
    };
    const read = vi
      .fn<() => Promise<boolean>>()
      .mockResolvedValueOnce(limitsReadSucceeded(errorSnapshot))
      .mockResolvedValueOnce(limitsReadSucceeded({ error: null }));
    const onBaseline = limitsBaselineReader(hydrate, read);

    await onBaseline(version(7));
    await onBaseline(version(7));

    expect(read).toHaveBeenCalledTimes(2);
    expect(hydrate).toHaveResolvedWith(false);
    expect(limitsReadSucceeded(errorSnapshot)).toBe(false);
    expect(limitsHydrationSucceeded(errorSnapshot)).toBe(false);
  });

  it("falls back from cached null data, accepts a null read, and handles it while fresh", async () => {
    const cachedNoLimits = {
      accountKey: "default",
      at: 99,
      data: null,
      error: null,
    };
    const hydrate = vi.fn(async () => ({ snapshot: cachedNoLimits }));
    let currentSnapshot: AccountLimitsSnapshot = {
      accountKey: "default",
      at: 20,
      data: { rateLimits: { primary: { usedPercent: 95 } } },
      error: null,
    };
    const read = vi.fn(async () => {
      currentSnapshot = cachedNoLimits;
      return limitsReadSucceeded(currentSnapshot);
    });
    const isFresh = (snapshot = currentSnapshot) =>
      limitsSnapshotIsFresh(snapshot, "default", undefined, 100);
    const onBaseline = limitsBaselineReader(hydrate, read, () => true, isFresh);

    await onBaseline(version(3));
    await onBaseline(version(3));

    expect(limitsHydrationSucceeded(cachedNoLimits)).toBe(false);
    expect(limitsReadSucceeded(cachedNoLimits)).toBe(true);
    expect(isFresh(cachedNoLimits)).toBe(true);
    expect(currentSnapshot).toEqual(cachedNoLimits);
    expect(hydrate).toHaveBeenCalledOnce();
    expect(read).toHaveBeenCalledOnce();
  });

  it("does not fall back if the account disconnects during hydration", async () => {
    let resolveHydration!: (result: boolean) => void;
    const hydration = new Promise<boolean>((resolve) => {
      resolveHydration = resolve;
    });
    const read = vi.fn(async () => {});
    let connected = true;
    const onBaseline = limitsBaselineReader(
      () => hydration,
      read,
      () => connected,
    );

    const baseline = onBaseline(version(1));
    connected = false;
    resolveHydration(false);
    await baseline;

    expect(read).not.toHaveBeenCalled();
  });

  it("does not read after disposal while failed hydration is pending", async () => {
    let resolveHydration!: (result: boolean) => void;
    const hydration = new Promise<boolean>((resolve) => {
      resolveHydration = resolve;
    });
    const read = vi.fn(async () => {});
    let active = true;
    const onBaseline = limitsBaselineReader(
      () => hydration,
      read,
      () => active,
    );

    const baseline = onBaseline(version(1));
    active = false;
    resolveHydration(false);
    await baseline;

    expect(read).not.toHaveBeenCalled();
  });

  it("stops after a failed read without a newer version and retries later", async () => {
    let calls = 0;
    const read = vi.fn(async () => {
      calls += 1;
      if (calls > 2) throw new Error("read loop exceeded its bound");
      if (calls === 1) throw new Error("temporary failure");
      return true;
    });
    const onBaseline = limitsBaselineReader(async () => false, read);

    await expect(onBaseline(version(20))).rejects.toThrow("temporary failure");
    expect(read).toHaveBeenCalledTimes(1);

    await onBaseline(version(20));
    expect(read).toHaveBeenCalledTimes(2);
  }, 1_000);

  it("exits the drain when the account becomes inactive", async () => {
    let resolveHydration!: (result: boolean) => void;
    const hydration = new Promise<boolean>((resolve) => {
      resolveHydration = resolve;
    });
    const read = vi.fn(async () => {
      if (read.mock.calls.length > 1)
        throw new Error("read loop exceeded its bound");
    });
    let active = true;
    const onBaseline = limitsBaselineReader(
      () => hydration,
      read,
      () => active,
    );

    const pending = onBaseline(version(21));
    active = false;
    resolveHydration(false);
    await pending;
    expect(read).not.toHaveBeenCalled();

    active = true;
    await onBaseline(version(21));
    expect(read).toHaveBeenCalledTimes(1);
  }, 1_000);

  it("does not spin when a successful read returns an already stale snapshot", async () => {
    const read = vi.fn(async () => {
      if (read.mock.calls.length > 2)
        throw new Error("stale snapshot loop exceeded its bound");
      return true;
    });
    const onBaseline = limitsBaselineReader(
      async () => false,
      read,
      () => true,
      () => false,
    );

    await onBaseline(version(22));
    expect(read).toHaveBeenCalledTimes(1);

    // A later same-version reconnect can retry the stale result, but each
    // notification must cause at most one read.
    await onBaseline(version(22));
    expect(read).toHaveBeenCalledTimes(2);
  }, 1_000);
});
