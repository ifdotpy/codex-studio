import type { components } from "../generated/api";
import {
  ACCOUNT_GROUP_PATTERN,
  ACCOUNT_GROUPS_MAX,
  DEFAULT_CLAUDE_LEAD_EFFORT,
  DEFAULT_CLAUDE_LEAD_MODEL,
} from "../generated/sharedValues";

/** Legacy and preference fixtures may omit fields other than the account id. */
export type PreferenceAccount = Pick<components["schemas"]["Account"], "id"> &
  Partial<
    Pick<
      components["schemas"]["Account"],
      | "provider"
      | "group"
      | "status"
      | "canAttemptNativeProof"
      | "disconnected"
      | "deleted"
      | "duplicateOf"
    >
  >;

/** The project fields that the default choice reads. */
export type PreferenceProject = Pick<
  components["schemas"]["ProjectEntityDto"],
  "accountKey" | "accountKeys" | "accountGroups"
>;

export type NewChatDefault = {
  accountKey: string;
  /** Set only when the request names no model and the account is a Claude account. */
  model: string | null;
  effort: string | null;
};

const groupPattern = new RegExp(ACCOUNT_GROUP_PATTERN);

/** Read an account order typed as names separated by commas or spaces. */
export function parseGroups(text: string): {
  groups: string[];
  error: string;
} {
  const groups = [
    ...new Set(
      text
        .split(/[\s,]+/)
        .map((name) => name.trim().toLowerCase())
        .filter(Boolean),
    ),
  ];
  if (groups.length > ACCOUNT_GROUPS_MAX)
    return {
      groups,
      error: `Use at most ${ACCOUNT_GROUPS_MAX} groups.`,
    };
  const invalid = groups.find((name) => !groupPattern.test(name));
  return {
    groups,
    error: invalid
      ? `"${invalid}" is not a group name. Use 1 to 32 lowercase letters, digits, hyphens or underscores.`
      : "",
  };
}

/** The order of a project: its own non-empty order, else the default order. */
export const effectiveGroups = (
  project: PreferenceProject | null | undefined,
  defaultGroups: string[] | null | undefined,
) =>
  project?.accountGroups?.length ? project.accountGroups : defaultGroups || [];

const rank = (group: string | null | undefined, groups: string[]) => {
  const index = group ? groups.indexOf(group) : -1;
  return index < 0 ? groups.length : index;
};

const providerOf = (account: PreferenceAccount) => account.provider || "codex";

/** The provider of a model id; `null` for an unknown model. */
export function modelProvider(model: string): "codex" | "claude" | null {
  if (model.startsWith("gpt-")) return "codex";
  if (
    model.startsWith("claude-") ||
    ["default", "sonnet", "opus", "haiku"].includes(model)
  )
    return "claude";
  return null;
}

export const canStart = (account: PreferenceAccount) =>
  !account.disconnected &&
  !account.deleted &&
  !account.duplicateOf &&
  (account.status === "ready" || account.canAttemptNativeProof === true);

/**
 * The default account of a new chat. The cases in
 * `workspaces/client/contracts/account-preference.json` hold this function to the
 * Rust owner (runtime-services `account_preference::new_chat_default`).
 */
export function newChatDefault(
  accounts: PreferenceAccount[],
  defaultAccountKey: string,
  defaultGroups: string[] | null | undefined,
  project: PreferenceProject | null | undefined,
  model: string | null | undefined,
): NewChatDefault {
  const groups = effectiveGroups(project, defaultGroups);
  const allowed = project?.accountKeys || [];
  const candidates = accounts
    .map((account, index) => ({ account, index }))
    .filter(
      ({ account }) =>
        canStart(account) && (!allowed.length || allowed.includes(account.id)),
    )
    .sort(
      (a, b) =>
        rank(a.account.group, groups) - rank(b.account.group, groups) ||
        Number(a.account.id !== project?.accountKey) -
          Number(b.account.id !== project?.accountKey) ||
        Number(a.account.id !== defaultAccountKey) -
          Number(b.account.id !== defaultAccountKey) ||
        a.index - b.index,
    )
    .map(({ account }) => account);
  const wanted = model
    ? modelProvider(model)
    : candidates.some((account) => providerOf(account) === "claude")
      ? "claude"
      : null;
  const chosen =
    candidates.find((account) => !wanted || providerOf(account) === wanted)
      ?.id ||
    project?.accountKey ||
    defaultAccountKey;
  const claude = accounts.some(
    (account) => account.id === chosen && providerOf(account) === "claude",
  );
  const defaults = !model && claude;
  return {
    accountKey: chosen,
    model: defaults ? DEFAULT_CLAUDE_LEAD_MODEL : null,
    effort: defaults ? DEFAULT_CLAUDE_LEAD_EFFORT : null,
  };
}

/** The model a new chat starts with when the person chose no model. */
export function defaultChatModel(
  provider: string | null | undefined,
  models: Array<{ model: string; resolvedModel?: string; isDefault?: boolean }>,
): string | undefined {
  if (
    provider === "claude" &&
    models.some(
      (row) =>
        row.model === DEFAULT_CLAUDE_LEAD_MODEL ||
        row.resolvedModel === DEFAULT_CLAUDE_LEAD_MODEL,
    )
  )
    return DEFAULT_CLAUDE_LEAD_MODEL;
  return (
    models.find((row) => row.model === "gpt-6-astra")?.model ||
    models.find((row) => row.isDefault)?.model ||
    models[0]?.model
  );
}
