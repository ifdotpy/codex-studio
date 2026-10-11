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
