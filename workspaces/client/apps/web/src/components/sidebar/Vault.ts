import type { components } from "../../generated/api";
import type { ProjectChip } from "./MacSync";
import type { SidebarBackend } from "./services";

/** The project vault of a layr project (docs/vault.md): kept by layr-desk, written by the VM. */
export type ProjectVault = components["schemas"]["ProjectVaultDto"];
export type VaultPull = components["schemas"]["VaultPullResponse"];
export type VaultSetupFacts = components["schemas"]["VaultSetupFactsResponse"];
export type VaultSetupJob = components["schemas"]["VaultSetupResponse"];
export type VaultLogin = components["schemas"]["VaultLoginResponse"];

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

type JobOptions<T> = {
  wait?: (ms: number) => Promise<void>;
  deadline?: number;
  onUpdate?: (job: T) => void;
};

/**
 * Ask for a job of this Mac's server until it ends. The body keeps the same request id, so a
 * repeated request reports the same job and a lost reply never starts a second one.
 */
export async function runJob<T extends { state: string }>(
  backend: Pick<SidebarBackend, "post">,
  url: Parameters<SidebarBackend["post"]>[0],
  body: Record<string, unknown>,
  {
    wait = (ms: number) => new Promise<void>((done) => setTimeout(done, ms)),
    deadline = Date.now() + 60 * 60 * 1000,
    onUpdate,
  }: JobOptions<T> = {},
): Promise<T> {
  for (;;) {
    const job = (await backend.post(url, body, { timeoutMs: 30000 })) as T;
    onUpdate?.(job);
    if (job.state !== "running") return job;
    if (Date.now() > deadline)
      throw new Error(`${url} takes more than an hour`);
    await wait(1500);
  }
}

/** Pull the latest vault state into a new folder below `to` (Downloads when left out). */
export async function pullVault(
  backend: Pick<SidebarBackend, "post">,
  projectId: string,
  to: string | null,
  {
    requestId = crypto.randomUUID() as string,
    ...options
  }: { requestId?: string } & JobOptions<VaultPull> = {},
): Promise<VaultPull> {
  return runJob<VaultPull>(
    backend,
    "/api/vault/pull",
    { projectId, requestId, ...(to ? { to } : {}) },
    options,
  );
}

export function pullText(job: VaultPull): string {
  return job.state === "done"
    ? `Pulled state ${job.seq} (${job.files} files) into ${job.folder}`
    : `The pull failed: ${job.error ?? "unknown error"}`;
}

/**
 * The LAYR chip with the project's vault: a cloud when a vault is set up, marked when its last
 * push failed, and the vault report in the tooltip.
 */
export function chipWithVault(
  chip: ProjectChip | null,
  entries: readonly VaultEntry[],
  now = Date.now(),
): ProjectChip | null {
  const vaults = entries.filter((entry) => entry.vault.state !== "none");
  if (!chip || !vaults.length) return chip;
  const failed = vaults.some((entry) => entry.vault.state === "error");
  const lines = vaults.map(
    (entry) =>
      (entry.server ? `${entry.server}: ` : "") + vaultLabel(entry.vault, now),
  );
  return {
    summary: {
      ...chip.summary,
      title: [chip.summary.title, ...lines].join("\n"),
    },
    attention: chip.attention || failed,
    vault: failed ? "error" : "ok",
  };
}

export type VaultAction =
  | "desk"
  | "unlock"
  | "finder"
  | "pull"
  | "setup"
  | "guide";

/**
 * The vault items of the LAYR menu. Setup, Unlock, Show in Finder and Pull act on this Mac, so
 * they need a project on this Mac's server; Show in Finder needs the desktop app. Another
 * server's project without a vault gets the setup guide.
 */
export function vaultActions(
  vault: ProjectVault,
  { onThisMac, desktop }: { onThisMac: boolean; desktop: boolean },
): VaultAction[] {
  if (vault.state === "none") return onThisMac ? ["setup"] : ["guide"];
  const link = deskLink(vault);
  const here = onThisMac && !!link;
  return [
    ...(link ? (["desk"] as const) : []),
    ...(here && vault.unlocked === false ? (["unlock"] as const) : []),
    ...(here && desktop && vault.disk ? (["finder"] as const) : []),
    ...(here && vault.unlocked !== false ? (["pull"] as const) : []),
  ];
}

/** The AWS profile that the setup dialog chooses first: the gateway's, else the first one. */
export function firstProfile(facts: VaultSetupFacts): string | null {
  const names = facts.profiles.filter((p) => !p.error).map((p) => p.name);
  return facts.gatewayProfile && names.includes(facts.gatewayProfile)
    ? facts.gatewayProfile
    : (names[0] ?? null);
}
