import { API_SCHEMA_HASH } from "../src/generated/apiSchema.ts";

export const cacheFixture = (value: unknown) =>
  JSON.stringify({ schemaHash: API_SCHEMA_HASH, value });
export const readCacheFixture = (raw: string) => JSON.parse(raw).value;
export const readCachedStorage = (key: string) =>
  JSON.parse(localStorage.getItem(key) || "null")?.value;
export const hasCachedValue = ({
  key,
  value,
}: {
  key: string;
  value: unknown;
}) => JSON.parse(localStorage.getItem(key) || "null")?.value === value;
