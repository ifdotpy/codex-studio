import type { components } from "../../generated/api";
import type { SidebarBackend } from "./services";

/** The secret names of a layr project (docs/project-secrets.md). No response has a value. */
export type ProjectSecretsReply =
  components["schemas"]["ProjectSecretsResponse"];

/** The name rule of the VM (vm-guest project_secrets.py). The VM also refuses the names that
 * Studio or a provider sets, and tells why. */
export const SECRET_NAME = /^[A-Z_][A-Z0-9_]{0,63}$/;
export const MAX_VALUE_BYTES = 16 * 1024;

export function nameProblem(name: string): string | null {
  if (!name) return "Enter a name";
  if (SECRET_NAME.test(name)) return null;
  if (name.length > 64) return "Use at most 64 characters";
  if (/^[0-9]/.test(name)) return "Start with a letter or _";
  return "Use only A to Z, 0 to 9 and _";
}

export function valueProblem(value: string): string | null {
  if (!value) return "Enter a value";
  if (value.includes("\0")) return "The value must not contain a NUL character";
  if (new TextEncoder().encode(value).length > MAX_VALUE_BYTES)
    return "The value is longer than 16 KiB";
  return null;
}

/** The change that waits for its answer. The same change keeps its request ID until the
 * server answers, so a retry after a lost reply never applies it twice. */
export type PendingChange = { key: string; requestId: string };

export function changeRequest(
  pending: PendingChange | null,
  key: string,
  make: () => string = () => crypto.randomUUID(),
): PendingChange {
  return pending?.key === key ? pending : { key, requestId: make() };
}

/** True when the server answered the request, also with an error: the next try is a new
 * request. A network failure or a timeout has no answer, and the change keeps its ID. */
export function answered(error: unknown): boolean {
  return (
    typeof error === "object" &&
    error !== null &&
    typeof (error as { status?: unknown }).status === "number"
  );
}

type Poster = Pick<SidebarBackend, "post">;

async function names(reply: Promise<unknown>): Promise<string[]> {
  return [...((await reply) as ProjectSecretsReply).names];
}

export function listSecrets(backend: Poster, projectId: string) {
  return names(
    backend.post(
      "/api/project-secrets/list",
      { projectId },
      { timeoutMs: 30000 },
    ),
  );
}

export function setSecret(
  backend: Poster,
  projectId: string,
  name: string,
  value: string,
  requestId: string,
) {
  return names(
    backend.post(
      "/api/project-secrets/set",
      { projectId, name, value, requestId },
      { timeoutMs: 30000 },
    ),
  );
}

export function removeSecret(
  backend: Poster,
  projectId: string,
  name: string,
  requestId: string,
) {
  return names(
    backend.post(
      "/api/project-secrets/remove",
      { projectId, name, requestId },
      { timeoutMs: 30000 },
    ),
  );
}
