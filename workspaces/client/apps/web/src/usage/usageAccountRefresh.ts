export type UsageAccountConnection = {
  id: string;
  disconnected?: boolean | null;
};

/** Changes only when one of the participating accounts connects/disconnects. */
export function usageAccountConnectionKey(
  usageAccountKeys: string,
  accounts: UsageAccountConnection[],
): string {
  return usageAccountKeys
    .split("\n")
    .filter(Boolean)
    .map((key) => {
      const account = accounts.find((item) => item.id === key);
      return `${key}:${account?.disconnected ? "disconnected" : "connected"}`;
    })
    .join("\n");
}
