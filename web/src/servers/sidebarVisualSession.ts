import type { SidebarBackend } from "../components/sidebar/services";
import type { MergedSidebar } from "./mergedSidebar";
import type { ServerSidebarSnapshot } from "./sidebarSnapshot";

type Override = {
  value: boolean | undefined;
  sources: Map<string, ServerSidebarSnapshot>;
};

/** Offline view changes stay in memory until a source snapshot or explicit edit. */
export function createSidebarVisualSession() {
  const values = new Map<string, Map<string, Override>>();
  return (
    model: MergedSidebar,
    backend: SidebarBackend,
    failed: (failure: unknown) => void,
  ): SidebarBackend => {
    const kindFor = (key: string) =>
      key === `codex-project-compact:${model.data.stateDir}`
        ? "compact"
        : key === `codex-project-tree:${model.data.stateDir}`
          ? "collapsed"
          : null;
    const read = <T>(key: string, fallback: T): T => {
      const base = backend.saved(key, fallback);
      const overrides = values.get(key);
      if (!overrides || !base || typeof base !== "object") return base;
      const result = { ...base } as Record<string, unknown>;
      for (const [field, override] of overrides) {
        if (
          [...override.sources].some(
            ([owner, snapshot]) =>
              model.sourceById.get(owner)?.sidebar !== snapshot,
          )
        ) {
          overrides.delete(field);
          continue;
        }
        if (override.value === undefined) delete result[field];
        else result[field] = override.value;
      }
      return result as T;
    };
    const owners = (kind: "compact" | "collapsed", field: string) => {
      const result = new Map<string, ServerSidebarSnapshot>();
      for (const source of model.sources) {
        const keys = new Set(Object.keys(source.sidebar[kind]));
        for (const project of source.sidebar.projects) {
          const path = project.path || "";
          keys.add(path);
          if (kind === "collapsed") {
            for (const folder of project.folders || [])
              keys.add(JSON.stringify([path, "folder", folder.id]));
            for (const team of [
              ...(project.peerTeams || []),
              ...source.sidebar.peerTeams.filter(
                (team) => team.projectPath === path,
              ),
            ])
              keys.add(JSON.stringify([path, "team", team.id]));
          }
        }
        if (
          [...keys].some(
            (key) =>
              (kind === "compact"
                ? model.projectKey(source.id, key)
                : model.mapTreeKey(source.id, key)) === field,
          )
        )
          result.set(source.id, source.sidebar);
      }
      return result;
    };
    return {
      ...backend,
      saved: read,
      editPreference(key, previous, next) {
        const kind = kindFor(key);
        const prior = previous as Record<string, boolean>;
        const value = next as Record<string, boolean>;
        const changed = kind
          ? [...new Set([...Object.keys(prior), ...Object.keys(value)])].filter(
              (field) => prior[field] !== value[field],
            )
          : [];
        try {
          const accepted = backend.editPreference(key, previous, next);
          for (const field of changed) values.get(key)?.delete(field);
          return read(key, accepted);
        } catch (failure) {
          if (
            kind &&
            failure instanceof Error &&
            failure.message === "The sidebar server is offline."
          ) {
            const overrides = values.get(key) || new Map<string, Override>();
            for (const field of changed)
              overrides.set(field, {
                value: value[field],
                sources: owners(kind, field),
              });
            values.set(key, overrides);
            return read(key, previous);
          }
          failed(failure);
          return previous;
        }
      },
    };
  };
}
