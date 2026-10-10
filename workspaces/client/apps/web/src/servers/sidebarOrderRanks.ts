/** Pack render identities into bounded rank records instead of duplicate backend IDs. */
export function encodeSidebarRanks(items: readonly string[]): string[] {
  const records: string[] = [];
  let parts: string[] = [];
  let length = 2;
  for (const item of items) {
    const part = JSON.stringify(item);
    if (part.length + 2 > 4096)
      throw new Error("The sidebar identity exceeds the server limit.");
    if (length + part.length + (parts.length ? 1 : 0) > 4096) {
      records.push("[" + parts.join(",") + "]");
      parts = [];
      length = 2;
    }
    length += part.length + (parts.length ? 1 : 0);
    parts.push(part);
  }
  if (parts.length) records.push("[" + parts.join(",") + "]");
  return records;
}
export function decodeSidebarRanks(records: readonly string[]): string[] {
  return records.flatMap((record) => {
    const items: unknown = JSON.parse(record);
    if (!Array.isArray(items) || items.some((item) => typeof item !== "string"))
      throw new Error("Invalid sidebar ranks.");
    return items as string[];
  });
}
