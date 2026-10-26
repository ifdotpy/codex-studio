import { get } from "../api";
import {
  normalizeSkillCatalog,
  useSkillAutocompleteWithReader,
  type SkillCatalog,
  type SkillAutocompleteOptions,
} from "./skillAutocompleteCore";
export type { SkillItem } from "./skillAutocompleteCore";

interface CacheEntry {
  expiresAt: number;
  value?: SkillCatalog;
  pending?: Promise<SkillCatalog>;
  failed?: boolean;
}

const CACHE_TTL_MS = 2 * 60 * 1000;
const CACHE_MAX_ENTRIES = 48;
const skillCatalogCache = new Map<string, CacheEntry>();

function catalogFor(key: string, agentId: string): Promise<SkillCatalog> {
  const existing = skillCatalogCache.get(key);
  if (existing?.value && existing.expiresAt > Date.now())
    return Promise.resolve(existing.value);
  if (existing?.pending) return existing.pending;
  if (existing?.failed && existing.expiresAt > Date.now())
    return Promise.reject(new Error("Skill catalog unavailable"));

  const entry: CacheEntry = { expiresAt: Date.now() + CACHE_TTL_MS };
  const pending = get("/api/skills", { query: { agent: agentId } })
    .then((result) => {
      const value = normalizeSkillCatalog(result);
      entry.value = value;
      entry.expiresAt = Date.now() + CACHE_TTL_MS;
      entry.pending = undefined;
      return value;
    })
    .catch((error: unknown) => {
      entry.pending = undefined;
      entry.failed = true;
      entry.expiresAt = Date.now() + CACHE_TTL_MS;
      throw error;
    });
  entry.pending = pending;
  skillCatalogCache.delete(key);
  skillCatalogCache.set(key, entry);
  while (skillCatalogCache.size > CACHE_MAX_ENTRIES)
    skillCatalogCache.delete(skillCatalogCache.keys().next().value!);
  return pending;
}

export function useSkillAutocomplete(options: SkillAutocompleteOptions) {
  return useSkillAutocompleteWithReader(options, catalogFor, "browser");
}
