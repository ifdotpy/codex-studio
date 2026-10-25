import type { SyncEntityPayload, components } from "../../generated/api";
type ReviewTask = components["schemas"]["WorkItem"];

type History = Extract<
  SyncEntityPayload,
  { collection: "workResult" | "workDecision" | "workRelease" }
>;
type Assignment = {
  sources: SyncEntityPayload[];
  historyInput: History[];
  history: History[];
  task: ReviewTask;
};

/** Rebuild canonical review tasks from scoped metadata and immutable history rows.
 * Unchanged tasks retain their identity when another assignment changes. */
export function createTaskAssignmentSelector() {
  let previous = new Map<string, Assignment>();
  let result: ReviewTask[] = [];
  return (
    entities: readonly SyncEntityPayload[],
    rootId: string,
  ): ReviewTask[] => {
    const metadata: Extract<SyncEntityPayload, { collection: "workDetail" }>[] =
      [];
    const histories = new Map<string, History[]>();
    for (const entity of entities) {
      if (entity.collection === "workDetail" && entity.value.rootId === rootId)
        metadata.push(entity);
      if (
        (entity.collection === "workResult" ||
          entity.collection === "workDecision" ||
          entity.collection === "workRelease") &&
        entity.value.rootId === rootId
      ) {
        const history = histories.get(entity.value.taskId) ?? [];
        history.push(entity);
        histories.set(entity.value.taskId, history);
      }
    }
    const accepted = new Set(
      metadata
        .filter((entity) => entity.value.status === "accepted")
        .map((entity) => entity.id),
    );
    const next = new Map<string, Assignment>();
    const tasks = metadata.map((entity) => {
      const prior = previous.get(entity.id);
      const historyInput = histories.get(entity.id) ?? [];
      const historyUnchanged =
        prior &&
        prior.historyInput.length === historyInput.length &&
        historyInput.every((row, index) => row === prior.historyInput[index]);
      const history = historyUnchanged
        ? prior.history
        : [...historyInput].sort(
            (left, right) =>
              left.value.index - right.value.index ||
              left.collection.localeCompare(right.collection),
          );
      const sources = [entity, ...history];
      const task = entity.value;
      const blockedBy =
        task.dependencies?.filter((id) => !accepted.has(id)) ?? [];
      const displayStatus =
        ["ready", "running"].includes(task.status) && blockedBy.length > 0
          ? "blocked"
          : task.status;
      if (
        prior &&
        prior.task.displayStatus === displayStatus &&
        prior.task.blockedBy?.length === blockedBy.length &&
        blockedBy.every((id, index) => id === prior.task.blockedBy?.[index]) &&
        prior.sources.length === sources.length &&
        sources.every((row, index) => row === prior.sources[index])
      ) {
        next.set(entity.id, prior);
        return prior.task;
      }
      const value: ReviewTask = {
        ...task,
        displayStatus,
        blockedBy,
        results: historyUnchanged
          ? (prior.task.results ?? [])
          : history.flatMap((row) =>
              row.collection === "workResult" ? [row.value.value] : [],
            ),
        decisions: historyUnchanged
          ? (prior.task.decisions ?? [])
          : history.flatMap((row) =>
              row.collection === "workDecision" ? [row.value.value] : [],
            ),
        releases: historyUnchanged
          ? (prior.task.releases ?? [])
          : history.flatMap((row) =>
              row.collection === "workRelease" ? [row.value.value] : [],
            ),
      };
      next.set(entity.id, { sources, historyInput, history, task: value });
      return value;
    });
    previous = next;
    if (
      tasks.length !== result.length ||
      tasks.some((task, index) => task !== result[index])
    )
      result = tasks;
    return result;
  };
}

/** The existing public work subscription consumes one canonical WorkList. */
export function createWorkDataSelector() {
  const select = createTaskAssignmentSelector();
  let previous: components["schemas"]["WorkList"] | undefined;
  return (entities: readonly SyncEntityPayload[], rootId: string) => {
    const items = select(entities, rootId);
    if (!previous || previous.rootId !== rootId || previous.items !== items)
      previous = { rootId, items, tasks: items };
    return previous;
  };
}
