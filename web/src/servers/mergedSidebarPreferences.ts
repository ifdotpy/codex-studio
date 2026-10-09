import type { SidebarBackend } from "../components/sidebar/services";
import type {
  MergedSidebar,
  SidebarOrder,
  SidebarSource,
} from "./mergedSidebar";

const compactPrefix = "codex-project-compact:";
const treePrefix = "codex-project-tree:";
const orderPrefix = "codex-sidebar-order:";
const originalKey = (prefix: string, source: SidebarSource) =>
  prefix + source.sidebar.stateDir;

function wireOrderGroup(
  model: MergedSidebar,
  owner: string,
  group: string,
): string {
  if (group === "projects") return group;
  const parts = JSON.parse(group);
  const path = model.wireProject(owner, parts[1]);
  if (parts[0] === "items" && typeof parts[2] === "string")
    parts[2] = model.requireReference(model.folderReferences, parts[2]).id;
  if (parts[0] === "chats")
    for (let index = 4; index < parts.length; index++) {
      if (parts[index] === "team")
        parts[++index] = model.requireReference(
          model.teamReferences,
          parts[index],
        ).id;
      else if (typeof parts[index] === "string")
        parts[index] = model.requireReference(
          model.folderReferences,
          parts[index],
        ).id;
    }
  parts[1] = path;
  return JSON.stringify(parts);
}

function wireOrderItem(
  model: MergedSidebar,
  owner: string,
  group: string,
  item: string,
): string | undefined {
  if (group === "projects") {
    try {
      return model.wireProject(owner, item);
    } catch {
      return undefined;
    }
  }
  const chat = model.chatReferences.get(item);
  if (chat) return chat.owner === owner ? chat.id : undefined;
  try {
    const parts = JSON.parse(item);
    if (!Array.isArray(parts)) return undefined;
    const reference =
      parts[1] === "folder"
        ? model.folderReferences.get(parts[2])
        : parts[1] === "team"
          ? model.teamReferences.get(parts[2])
          : undefined;
    if (!reference || reference.owner !== owner) return undefined;
    return JSON.stringify([
      model.wireProject(owner, parts[0]),
      parts[1],
      reference.id,
    ]);
  } catch {
    return undefined;
  }
}

/** Read original server keys. Write only fields changed by a visible control. */
export function mergedSidebarPreferences(
  model: MergedSidebar,
  backends: ReadonlyMap<string, SidebarBackend>,
): SidebarBackend {
  const backendFor = (id: string) => {
    const backend = backends.get(id);
    if (!backend) throw new Error("The sidebar server is unavailable.");
    return backend;
  };
  for (const source of model.sources) backendFor(source.id);
  if (
    model.data.stateDir !== "studio-combined-sidebar" &&
    model.sources.length === 1
  )
    return backendFor(model.sources[0].id);
  const coordinator = backends.has("local") ? "local" : model.sources[0]?.id;
  if (!coordinator) throw new Error("The sidebar has no server.");
  const base = backendFor(coordinator);
  const scope = model.data.stateDir;
  const compactKey = compactPrefix + scope;
  const treeKey = treePrefix + scope;
  const orderKey = orderPrefix + scope;
  let order: SidebarOrder = model.order;
  const visualEdits = new Map<string, Record<string, boolean>>();
  const prefixFor = (key: string) =>
    key === compactKey
      ? compactPrefix
      : key === treeKey
        ? treePrefix
        : undefined;
  const readVisual = (kind: "compact" | "collapsed") => {
    const prefix = kind === "compact" ? compactPrefix : treePrefix;
    return new Map(
      model.sources.map((source) => [
        source.id,
        visualEdits.get(source.id + ":" + originalKey(prefix, source)) ||
          source.sidebar[kind],
      ]),
    );
  };
  const read = (key: string) => {
    if (key === orderKey) return order;
    if (key === compactKey)
      return model.mergeVisual("compact", readVisual("compact"));
    if (key === treeKey)
      return model.mergeVisual("collapsed", readVisual("collapsed"));
    return undefined;
  };
  const transientKey = (key: string) => "studio-combined-ui:" + key;
  const storage: SidebarBackend["storage"] = {
    getItem(key) {
      const value = read(key);
      return value === undefined
        ? base.storage.getItem(transientKey(key))
        : JSON.stringify(value);
    },
    setItem(key, value) {
      if (prefixFor(key))
        throw new Error("Use preference edits to save sidebar state.");
      if (key === orderKey) {
        order = JSON.parse(value);
        return;
      }
      base.storage.setItem(transientKey(key), value);
    },
    removeItem(key) {
      if (prefixFor(key))
        throw new Error("Use preference edits to save sidebar state.");
      if (key === orderKey) {
        order = {};
        return;
      }
      base.storage.removeItem(transientKey(key));
    },
  };
  const edits = (
    key: string,
    previous: Record<string, unknown>,
    next: Record<string, unknown>,
  ) => {
    const prefix = prefixFor(key);
    if (!prefix) throw new Error("The sidebar preference key is unavailable.");
    const kind = prefix === compactPrefix ? "compact" : "collapsed";
    const changed = new Set(
      [...Object.keys(previous), ...Object.keys(next)].filter(
        (name) => JSON.stringify(previous[name]) !== JSON.stringify(next[name]),
      ),
    );
    const values = readVisual(kind);
    const updates: {
      source: SidebarSource;
      previous: Record<string, boolean>;
      next: Record<string, boolean>;
    }[] = [];
    for (const source of model.sources) {
      const prior = values.get(source.id)!;
      const value = { ...prior };
      const names = new Set(Object.keys(prior));
      for (const group of model.groups.values())
        for (const member of group.members) {
          if (member.source.id !== source.id) continue;
          const path = member.project.path || "";
          names.add(path);
          if (kind === "collapsed") {
            for (const folder of member.project.folders || [])
              names.add(JSON.stringify([path, "folder", folder.id]));
            for (const team of [
              ...(member.project.peerTeams || []),
              ...source.sidebar.peerTeams.filter(
                (team) => team.projectPath === path,
              ),
            ])
              names.add(JSON.stringify([path, "team", team.id]));
          }
        }
      for (const name of names) {
        const mapped =
          kind === "compact"
            ? model.projectKey(source.id, name)
            : model.mapTreeKey(source.id, name);
        if (!changed.has(mapped)) continue;
        if (mapped in next) {
          if (typeof next[mapped] !== "boolean")
            throw new Error("Invalid sidebar preference value.");
          value[name] = next[mapped];
        } else delete value[name];
      }
      if (JSON.stringify(prior) !== JSON.stringify(value))
        updates.push({ source, previous: prior, next: value });
    }
    // Validate all owners before touching any saved value.
    if (updates.some(({ source }) => !source.online))
      throw new Error("The sidebar server is offline.");
    for (const update of updates) {
      const accepted = backendFor(update.source.id).editPreference(
        originalKey(prefix, update.source),
        update.previous,
        update.next,
      );
      visualEdits.set(
        update.source.id + ":" + originalKey(prefix, update.source),
        accepted,
      );
    }
    return read(key);
  };
  return {
    ...base,
    storage,
    saved<T>(key: string, fallback: T): T {
      const value = read(key);
      if (value !== undefined) return value as T;
      try {
        const raw = storage.getItem(key);
        return raw === null ? fallback : JSON.parse(raw);
      } catch {
        return fallback;
      }
    },
    save(key, value) {
      storage.setItem(key, JSON.stringify(value));
    },
    editPreference(key, previous, next) {
      return edits(key, previous, next) as typeof next;
    },
    subscribePreference(key, update) {
      const prefix = prefixFor(key);
      if (!prefix) return () => {};
      const stops = model.sources.map((source) =>
        backendFor(source.id).subscribePreference(
          originalKey(prefix, source),
          () => {
            const kind = prefix === compactPrefix ? "compact" : "collapsed";
            visualEdits.set(
              source.id + ":" + originalKey(prefix, source),
              backendFor(source.id).saved(
                originalKey(prefix, source),
                source.sidebar[kind],
              ),
            );
            update();
          },
        ),
      );
      return () => stops.forEach((stop) => stop());
    },
  };
}

/** Convert one explicit reorder to its owner's original groups and revision. */
export function sidebarOrderWrite(
  model: MergedSidebar,
  owner: string,
  visible: SidebarOrder,
) {
  const source = model.sourceById.get(owner);
  if (!source) throw new Error("The sidebar server is unavailable.");
  if (!source.online) throw new Error("The sidebar server is offline.");
  const original =
    source.sidebar.sidebarOrder?.groups || source.sidebar.savedOrder;
  const groups = { ...original };
  for (const [group, items] of Object.entries(original)) {
    try {
      if (JSON.parse(group)?.[0] === "combined-sidebar") continue;
    } catch {
      /* Native keys may be plain strings. */
    }
    const projected = model.mapOrderGroup(owner, group);
    const replacement = visible[projected];
    if (!replacement) continue;
    const wireById = new Map(
      items.map((id) => [model.mapOrderItem(owner, group, id), id]),
    );
    // Keep unknown and currently hidden values. They are still saved data.
    groups[group] = [
      ...replacement.flatMap((id) => {
        const wire =
          wireById.get(id) ?? wireOrderItem(model, owner, projected, id);
        return wire === undefined ? [] : [wire];
      }),
      ...items.filter(
        (id) => !replacement.includes(model.mapOrderItem(owner, group, id)),
      ),
    ];
  }
  for (const [group, items] of Object.entries(visible)) {
    const before = model.order[group] || [];
    if (JSON.stringify(before) === JSON.stringify(items)) continue;
    if (model.orderOwner(group) !== owner) continue;
    const nativeGroup = wireOrderGroup(model, owner, group);
    if (!(nativeGroup in groups))
      groups[nativeGroup] = items.flatMap((item) => {
        const wire = wireOrderItem(model, owner, group, item);
        return wire === undefined ? [] : [wire];
      });
    groups[JSON.stringify(["combined-sidebar", group])] = [...items];
  }
  return {
    expected_revision: source.sidebar.sidebarOrder?.revision || 0,
    groups,
  };
}
