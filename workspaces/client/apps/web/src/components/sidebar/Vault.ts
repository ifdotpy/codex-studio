import type { components } from "../../generated/api";
import type { SidebarBackend } from "./services";

/** The project vault of a layr project (docs/vault.md): kept by layr-desk, written by the VM. */
export type ProjectVault = components["schemas"]["ProjectVaultDto"];
export type VaultPull = components["schemas"]["VaultPullResponse"];

/** One server's report of a project's vault; actions go to that server. */
export type VaultEntry = {
  vault: ProjectVault;
  ownerId?: string;
  server?: string;
};

export const VAULT_SETUP_URL =
  "https://github.com/ifdotpy/layr-desk/blob/main/docs/design.md#set-up-a-vault-for-a-project";

function ago(seconds: number): string {
  if (seconds < 90) return "just now";
  const minutes = Math.round(seconds / 60);
  if (minutes < 90) return `${minutes} min ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 36) return `${hours} h ago`;
  return `${Math.round(hours / 24)} days ago`;
}

/** The menu label of the vault: whether it is set up and when the VM last saved a change. */
export function vaultLabel(vault: ProjectVault, now = Date.now()): string {
  if (vault.state === "none") return "Vault: not set up";
  if (vault.state === "error") return "Vault: the last push failed";
  return vault.lastPush
    ? `Vault: saved ${ago(now / 1000 - vault.lastPush)}`
    : "Vault: no state yet";
}

/** The Desk page of the vault, or null when its site is not known. */
export function deskLink(vault: ProjectVault): string | null {
  if (!vault.site || !vault.prefix || !vault.site.startsWith("https://"))
    return null;
  return `${vault.site.replace(/\/*$/, "/")}${encodeURIComponent(vault.prefix)}/`;
}

/**
 * Pull the latest vault state into a new folder below `to` (Downloads when left out). The
 * request id stays the same for the whole pull: a repeated request reports the same pull,
 * so a lost reply never starts a second one.
 */
export async function pullVault(
  backend: Pick<SidebarBackend, "post">,
  projectId: string,
  to: string | null,
  {
    requestId = crypto.randomUUID() as string,
    wait = (ms: number) => new Promise<void>((done) => setTimeout(done, ms)),
    deadline = Date.now() + 60 * 60 * 1000,
  }: {
    requestId?: string;
    wait?: (ms: number) => Promise<void>;
    deadline?: number;
  } = {},
): Promise<VaultPull> {
  const body = { projectId, requestId, ...(to ? { to } : {}) };
  for (;;) {
    const job = (await backend.post("/api/vault/pull", body, {
      timeoutMs: 30000,
    })) as VaultPull;
    if (job.state !== "running") return job;
    if (Date.now() > deadline)
      throw new Error(`The pull into ${job.folder} takes more than an hour`);
    await wait(1500);
  }
}

export function pullText(job: VaultPull): string {
  return job.state === "done"
    ? `Pulled state ${job.seq} (${job.files} files) into ${job.folder}`
    : `The pull failed: ${job.error ?? "unknown error"}`;
}
