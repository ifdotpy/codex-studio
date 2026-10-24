import { randomUUID } from "../uuid";
import type { DraftJournal } from "./draftJournal";
import type { DraftVersion } from "../sync/drafts";

export type DraftView = {
  drafts: Record<string, string>;
  conflicts: DraftVersion[];
  workspaceId: string;
  revision: number;
  error?: string;
};
type Invoke = <T>(method: string, params: unknown, id?: string) => Promise<T>;
type Edit = { id: string; changes: Record<string, string> };

// TODO(attar-sync): When this renderer moves to Attar, restore synchronous
// core.commit("drafts.edit"/"drafts.dismiss") after the SQLite transaction commits.
// The async write queue below exists for Tauri IPC. Remove it only when the
// synchronous native bridge preserves request IDs, durable receipts, and errors.
// Keep network replication asynchronous.
// The visible text and the durable acknowledgement are distinct. An event with
// matching text is not an acknowledgement of a particular local write.
export class NativeDraftState {
  private committed: DraftView;
  private queue: Edit[] = [];
  private saving = false;
  private failure = "";
  private listeners = new Set<() => void>();
  private snapshot: DraftView & { pending: boolean; localError: string };

  constructor(
    readonly workspace: string,
    private invoke: Invoke,
    private journal?: DraftJournal,
  ) {
    this.committed = {
      drafts: {},
      conflicts: [],
      workspaceId: workspace,
      revision: -1,
    };
    this.snapshot = { ...this.committed, pending: false, localError: "" };
    this.queue = journal?.load() ?? [];
    this.publish();
  }
  getSnapshot = () => this.snapshot;
  subscribe = (listener: () => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };
  private publish() {
    const drafts = { ...this.committed.drafts };
    for (const edit of this.queue) Object.assign(drafts, edit.changes);
    this.snapshot = {
      ...this.committed,
      drafts,
      pending: this.queue.length > 0,
      localError: this.failure,
    };
    for (const listener of this.listeners) listener();
  }
  accept(view: DraftView) {
    if (view.workspaceId !== this.workspace)
      throw new Error(
        "The server workspace changed. Saved drafts remain on this device.",
      );
    if (view.revision < this.committed.revision) return;
    this.committed = view;
    this.publish();
  }
  edit(next: Record<string, string>) {
    const previous = this.snapshot.drafts;
    const changes: Record<string, string> = {};
    for (const key of new Set([
      ...Object.keys(previous),
      ...Object.keys(next),
    ])) {
      if ((previous[key] ?? "") !== (next[key] ?? ""))
        changes[key] = next[key] ?? "";
    }
    if (!Object.keys(changes).length) return;
    // The head may already be committed, so its content is immutable. Only
    // coalesce the unsent tail to bound typing backlog while IPC is delayed.
    if (!this.journal && this.queue.length > 1)
      Object.assign(this.queue[1].changes, changes);
    else this.queue.push({ id: randomUUID(), changes });
    try {
      this.journal?.save(this.queue[this.queue.length - 1]);
    } catch (error) {
      this.failure = error instanceof Error ? error.message : String(error);
    }
    this.publish();
    if (!this.failure) void this.flush();
  }
  async retry() {
    // An uncertain command keeps both its identity and its exact content.
    this.failure = "";
    await this.flush();
  }
  /** Wait for all queued local commits, without replaying an uncertain write. */
  waitCommitted(timeoutMs = 8_000): Promise<void> {
    if (!this.snapshot.pending) return Promise.resolve();
    if (this.failure) return Promise.reject(new Error(this.failure));
    return new Promise((resolve, reject) => {
      const finish = (error?: Error) => {
        clearTimeout(timer);
        unsubscribe();
        if (error) reject(error);
        else resolve();
      };
      const unsubscribe = this.subscribe(() => {
        if (this.snapshot.localError)
          finish(new Error(this.snapshot.localError));
        else if (!this.snapshot.pending) finish();
      });
      const timer = setTimeout(
        () => finish(new Error("Draft changes have not finished saving.")),
        timeoutMs,
      );
    });
  }
  private async flush() {
    if (this.saving) return;
    this.saving = true;
    try {
      while (this.queue.length) {
        const edit = this.queue[0];
        this.journal?.save(edit);
        const view = await this.invoke<DraftView>(
          "drafts.edit",
          {
            workspace: this.workspace,
            changes: edit.changes,
          },
          edit.id,
        );
        if (view.workspaceId !== this.workspace)
          throw new Error("The draft acknowledgement has another workspace.");
        // Drop only the acknowledged command; later edits remain overlaid.
        this.journal?.acknowledge(edit);
        this.queue.shift();
        if (view.revision >= this.committed.revision) this.committed = view;
        this.failure = "";
        this.publish();
      }
    } catch (error) {
      this.failure = error instanceof Error ? error.message : String(error);
      this.publish();
    } finally {
      this.saving = false;
    }
  }
}
