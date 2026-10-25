import { beforeEach, expect, it } from "vitest";
import {
  accountForExternalCommand,
  confirmAccountDeleteRequest,
  getOrCreateAccountDeleteRequest,
} from "./accountDeleteRequests";

const values = new Map<string, string>();
const storage = {
  getItem: (key: string) => values.get(key) ?? null,
  setItem: (key: string, value: string) => values.set(key, value),
};

beforeEach(() => values.clear());

it("resolves an exact account key without selecting a duplicate or a missing replacement", () => {
  const accounts = ["first", "second"].map((id) => ({
    id,
    provider: "claude",
    email: "same@example.test",
    label: "Work",
  }));
  const identity = {
    provider: "claude" as const,
    email: "same@example.test",
    label: "Work",
    accountKey: "second",
  };
  expect(accountForExternalCommand(accounts, identity)?.id).toBe("second");
  expect(
    accountForExternalCommand(accounts, { ...identity, accountKey: "removed" }),
  ).toBeUndefined();
  expect(
    accountForExternalCommand(accounts, { ...identity, provider: "codex" }),
  ).toBeUndefined();
});

it("resolves an external account action to the account's stored delete identity", () => {
  const accounts = [
    {
      id: "other-account",
      provider: "codex",
      email: "other@example.invalid",
      label: "Work",
    },
    {
      id: "target-account",
      provider: "codex",
      email: "target@example.invalid",
      label: "Work",
    },
  ];
  const account = accountForExternalCommand(accounts, {
    provider: "codex",
    email: "TARGET@example.invalid",
    label: "Work",
  });
  expect(account?.id).toBe("target-account");

  values.set(
    "codex-studio-account-delete-requests-v1:settings-state",
    JSON.stringify({ "target-account": "stored-request" }),
  );
  expect(
    getOrCreateAccountDeleteRequest(
      "settings-state",
      account!.id,
      storage,
      () => "unexpected-new-request",
    ),
  ).toBe("stored-request");
});

it("stores a new identity before sending and retains it after an uncertain failure", async () => {
  const requestId = getOrCreateAccountDeleteRequest(
    "settings-state",
    "target-account",
    storage,
    () => "new-request",
  );
  await expect(
    confirmAccountDeleteRequest(
      "settings-state",
      "target-account",
      requestId,
      async (sentId) => {
        expect(sentId).toBe("new-request");
        expect(
          JSON.parse(
            values.get(
              "codex-studio-account-delete-requests-v1:settings-state",
            )!,
          )["target-account"],
        ).toBe("new-request");
        throw new Error("Response was lost");
      },
      storage,
    ),
  ).rejects.toThrow("Response was lost");
  expect(
    getOrCreateAccountDeleteRequest(
      "settings-state",
      "target-account",
      storage,
      () => "must-not-replace-request",
    ),
  ).toBe("new-request");
});

it("clears the stored identity only after the server confirms deletion", async () => {
  const requestId = getOrCreateAccountDeleteRequest(
    "settings-state",
    "target-account",
    storage,
    () => "confirmed-request",
  );
  await expect(
    confirmAccountDeleteRequest(
      "settings-state",
      "target-account",
      requestId,
      async () => ({ ok: true }),
      storage,
    ),
  ).resolves.toEqual({ ok: true });
  expect(
    JSON.parse(
      values.get("codex-studio-account-delete-requests-v1:settings-state")!,
    ),
  ).toEqual({});
});

it("keeps same-account deletion identities and acknowledgements within their owner storage", async () => {
  const otherValues = new Map<string, string>();
  const otherStorage = {
    getItem: (key: string) => otherValues.get(key) ?? null,
    setItem: (key: string, value: string) => otherValues.set(key, value),
  };
  const a = getOrCreateAccountDeleteRequest(
    "same",
    "account",
    storage,
    () => "request-a",
  );
  const b = getOrCreateAccountDeleteRequest(
    "same",
    "account",
    otherStorage,
    () => "request-b",
  );
  await expect(
    confirmAccountDeleteRequest(
      "same",
      "account",
      a,
      async () => {
        throw new Error("lost reply");
      },
      storage,
    ),
  ).rejects.toThrow("lost reply");
  await confirmAccountDeleteRequest(
    "same",
    "account",
    b,
    async (id) => {
      expect(id).toBe("request-b");
    },
    otherStorage,
  );
  expect(
    getOrCreateAccountDeleteRequest(
      "same",
      "account",
      storage,
      () => "wrong-retry",
    ),
  ).toBe("request-a");
  expect(
    getOrCreateAccountDeleteRequest(
      "same",
      "account",
      otherStorage,
      () => "next-b",
    ),
  ).toBe("next-b");
});
