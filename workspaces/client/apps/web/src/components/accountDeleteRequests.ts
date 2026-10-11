import { serverLocalStorage } from "../servers/storage";

export type AccountDeleteTarget = {
  id: string;
  provider?: string | null;
  email?: string | null;
  label: string;
};

export type ExternalAccountIdentity = {
  provider: "codex" | "claude";
  email: string | null;
  label: string;
};

type Storage = Pick<globalThis.Storage, "getItem" | "setItem">;

const DELETE_REQUESTS_KEY = "codex-studio-account-delete-requests-v1";

function deleteRequestsStorageKey(scope?: string) {
  return `${DELETE_REQUESTS_KEY}:${scope || "default"}`;
}

function storedDeleteRequest(
  scope: string | undefined,
  accountKey: string,
  storage: Storage,
) {
  try {
    const requests = JSON.parse(
      storage.getItem(deleteRequestsStorageKey(scope)) || "{}",
    );
    return typeof requests[accountKey] === "string"
      ? (requests[accountKey] as string)
      : "";
  } catch {
    return "";
  }
}

function saveDeleteRequest(
  scope: string | undefined,
  accountKey: string,
  requestId: string | null,
  storage: Storage,
) {
  try {
    const current = JSON.parse(
      storage.getItem(deleteRequestsStorageKey(scope)) || "{}",
    );
    const requests =
      current && typeof current === "object" && !Array.isArray(current)
        ? current
        : {};
    if (requestId) requests[accountKey] = requestId;
    else delete requests[accountKey];
    storage.setItem(deleteRequestsStorageKey(scope), JSON.stringify(requests));
  } catch {
    // The live dialog still retains its exact request identity when storage is unavailable.
  }
}

export function accountForExternalCommand<T extends AccountDeleteTarget>(
  accounts: T[],
  identity: ExternalAccountIdentity,
) {
  return accounts.find(
    (candidate) =>
      (candidate.provider === "claude" ? "claude" : "codex") ===
        identity.provider &&
      (identity.email
        ? candidate.email?.toLocaleLowerCase() ===
          identity.email.toLocaleLowerCase()
        : candidate.label === identity.label),
  );
}

export function getOrCreateAccountDeleteRequest(
  scope: string | undefined,
  accountKey: string,
  storage: Storage = serverLocalStorage,
  createRequestId: () => string = () => crypto.randomUUID(),
) {
  const requestId =
    storedDeleteRequest(scope, accountKey, storage) || createRequestId();
  saveDeleteRequest(scope, accountKey, requestId, storage);
  return requestId;
}

export async function confirmAccountDeleteRequest<T>(
  scope: string | undefined,
  accountKey: string,
  requestId: string,
  send: (requestId: string) => Promise<T>,
  storage: Storage = serverLocalStorage,
) {
  const result = await send(requestId);
  saveDeleteRequest(scope, accountKey, null, storage);
  return result;
}
