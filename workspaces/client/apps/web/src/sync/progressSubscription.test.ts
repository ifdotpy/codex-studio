import { expect, it, vi } from "vitest";
import { subscribeProgress } from "./progressSubscription";
import type {
  ResourceConnectionState,
  ResourceVersion,
} from "./resourceEvents";

it("uses transient values while idle and performs one recovery read after interruption", async () => {
  let changed!: (version?: ResourceVersion) => void;
  let connection!: (state: ResourceConnectionState) => void;
  const remove = vi.fn();
  const events = {
    watchResourceChanges: vi.fn((_resource, accept) => {
      changed = accept;
      return remove;
    }),
    watchResourceConnection: vi.fn((accept) => {
      connection = accept;
      accept("live");
      return remove;
    }),
  };
  const accept = vi.fn();
  const recover = vi.fn().mockResolvedValue(undefined);
  const resource = { kind: "auth-progress", requestId: "one" } as const;
  const stop = subscribeProgress(events, resource, accept, recover, vi.fn());
  for (let revision = 1; revision < 10; revision++) {
    changed({ epoch: "one", revision });
    changed({
      epoch: "one",
      revision,
      progress: {
        resource,
        value: { requestId: "one", accountKey: "account", status: "pending" },
      },
    });
  }
  expect(accept).toHaveBeenCalledTimes(9);
  expect(recover).not.toHaveBeenCalled();
  connection("degraded");
  connection("live");
  connection("live");
  expect(recover).toHaveBeenCalledTimes(1);
  await Promise.resolve();
  stop();
  expect(remove).toHaveBeenCalledTimes(2);
  changed({ epoch: "one", revision: 11, progress: { resource, value: null } });
  expect(recover).toHaveBeenCalledTimes(1);
});

it("recovers a missing retained request once without reading on every hint", () => {
  let changed!: (version?: ResourceVersion) => void;
  const recover = vi.fn().mockReturnValue(new Promise(() => {}));
  const resource = { kind: "auth-progress", requestId: "one" } as const;
  const stop = subscribeProgress(
    {
      watchResourceChanges: (_resource, accept) => {
        changed = accept;
        return () => {};
      },
      watchResourceConnection: () => () => {},
    },
    resource,
    vi.fn(),
    recover,
    vi.fn(),
  );
  changed({ epoch: "one", revision: 1, progress: { resource, value: null } });
  changed({ epoch: "one", revision: 2, progress: { resource, value: null } });
  expect(recover).toHaveBeenCalledTimes(1);
  stop();
});
