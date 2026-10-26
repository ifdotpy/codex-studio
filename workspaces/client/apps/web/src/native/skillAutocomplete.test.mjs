import { setImmediate } from "node:timers/promises";
import { expect, it, vi } from "vitest";
const h = vi.hoisted(() => ({
  slots: [],
  cursor: 0,
  pending: [],
  owner: null,
  api: null,
}));
vi.mock("./servicesContext", () => ({
  useServerOwner: () => h.owner,
  useServerApi: () => h.api,
}));
vi.mock("react", () => {
  const same = (left, right) =>
    left &&
    right &&
    left.length === right.length &&
    left.every((value, index) => Object.is(value, right[index]));
  const memo = (read, deps) => {
    const index = h.cursor++;
    if (!h.slots[index] || !same(h.slots[index].deps, deps))
      h.slots[index] = { deps, value: read() };
    return h.slots[index].value;
  };
  return {
    useState(initial) {
      const index = h.cursor++;
      if (!(index in h.slots))
        h.slots[index] = typeof initial === "function" ? initial() : initial;
      return [
        h.slots[index],
        (value) => {
          h.slots[index] =
            typeof value === "function" ? value(h.slots[index]) : value;
        },
      ];
    },
    useRef(current) {
      const index = h.cursor++;
      return (h.slots[index] ??= { current });
    },
    useMemo: memo,
    useCallback: (callback, deps) => memo(() => callback, deps),
    useEffect(effect, deps) {
      const index = h.cursor++;
      const previous = h.slots[index];
      if (!previous || !same(previous.deps, deps)) {
        const slot = { deps };
        h.slots[index] = slot;
        h.pending.push(() => {
          previous?.cleanup?.();
          slot.cleanup = effect();
        });
      }
    },
  };
});
import { useSkillAutocomplete } from "./useSkillAutocomplete";

function deferred() {
  let resolve;
  const promise = new Promise((done) => {
    resolve = done;
  });
  return { promise, resolve };
}
function render(options) {
  h.cursor = 0;
  return useSkillAutocomplete(options);
}
function effects() {
  for (const effect of h.pending.splice(0)) effect();
}

it("the real native skill hook never shows a late catalog from another owner", async () => {
  const left = deferred(),
    right = deferred();
  const leftGet = vi.fn(() => left.promise),
    rightGet = vi.fn(() => right.promise);
  const options = {
    enabled: true,
    agentId: "same-agent",
    workspace: "same",
    account: "same",
    cwd: "/project",
    provider: "claude",
    draft: "$skill",
    input: { current: { value: "$skill", selectionStart: 6, selectionEnd: 6 } },
    insert: vi.fn(),
  };
  h.owner = {
    id: "left",
    origin: "https://left.example.test",
    credentialId: "left",
    workspaceId: "a".repeat(32),
  };
  h.api = { get: leftGet };
  render(options);
  effects();
  render(options);
  effects();
  expect(leftGet).toHaveBeenCalledOnce();
  h.owner = {
    id: "right",
    origin: "https://right.example.test",
    credentialId: "right",
    workspaceId: "b".repeat(32),
  };
  h.api = { get: rightGet };
  render(options);
  effects();
  render(options);
  effects();
  expect(rightGet).toHaveBeenCalledOnce();
  right.resolve({ skills: [{ name: "skill-right" }], errors: [] });
  await setImmediate();
  expect(render(options).matches.map((item) => item.name)).toEqual([
    "skill-right",
  ]);
  left.resolve({ skills: [{ name: "skill-left" }], errors: [] });
  await setImmediate();
  expect(render(options).matches.map((item) => item.name)).toEqual([
    "skill-right",
  ]);
  expect(rightGet.mock.calls[0][1].workspaceId).toBe(h.owner.workspaceId);
});
