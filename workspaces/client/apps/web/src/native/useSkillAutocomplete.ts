import { useMemo } from "react";
import { useServerApi, useServerOwner } from "./servicesContext";
import {
  normalizeSkillCatalog,
  useSkillAutocompleteWithReader,
  type SkillAutocompleteOptions,
  type SkillCatalogReader,
} from "../components/skillAutocompleteCore";
export type { SkillItem } from "../components/skillAutocompleteCore";

export function useSkillAutocomplete(options: SkillAutocompleteOptions) {
  const { get } = useServerApi();
  const owner = useServerOwner();
  const ownerKey = JSON.stringify([
    owner.id,
    owner.origin,
    owner.credentialId ?? null,
    owner.workspaceId,
  ]);
  const read = useMemo<SkillCatalogReader>(
    () => (_scope, agentId) =>
      get("/api/skills", {
        query: { agent: agentId },
        workspaceId: owner.workspaceId,
      }).then(normalizeSkillCatalog),
    [get, ownerKey, owner.workspaceId],
  );
  return useSkillAutocompleteWithReader(options, read, ownerKey);
}
