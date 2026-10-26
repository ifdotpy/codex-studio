import type { Message, TranscriptPageData } from "../types";
import type { NativeServerContext } from "./client";

export type NativeMessageView = {
  viewId: string;
  generation: number;
  revision: number;
  loaded: boolean;
  items: Message[];
  notice: string;
  before: string | number | null;
  after: string | null;
  historyVersion: string;
  historical: boolean;
  pageLoading: boolean;
  liveAgent: TranscriptPageData["agent"] | null;
  connection: string;
  found?: boolean;
};
export type NativeMessageSelection = {
  id: string | null;
  kind: "agent" | "room" | "legacy";
  managed: boolean;
  enabled: boolean;
  syncWorkspaceId?: string;
};
type Command = "older" | "newer" | "latest" | "ensure" | "anchor" | "refresh";

/** UI selection and lifecycle only. Rust owns every page, cursor and merge. */
export function createNativeMessageView(
  context: Promise<
    Pick<NativeServerContext, "workspaceId" | "request" | "subscribe">
  >,
  accept: (view: NativeMessageView) => void,
  report: (error: unknown) => void,
) {
  let stopped = false;
  let generation = 0;
  let revision = -1;
  let navigation = 0;
  let opened:
    | Promise<{ owner: Awaited<typeof context>; viewId: string }>
    | undefined;
  let unsubscribe: (() => Promise<void>) | undefined;
  let selected = Promise.resolve();
  function receive(value: unknown, viewId: string) {
    if (stopped) return undefined;
    if (!value || typeof value !== "object")
      throw new Error("The native conversation view is invalid.");
    const view = value as NativeMessageView;
    if (
      view.viewId !== viewId ||
      !Number.isSafeInteger(view.generation) ||
      !Number.isSafeInteger(view.revision) ||
      view.revision < 0
    )
      throw new Error("The native conversation view has a different identity.");
    if (view.generation !== generation) return undefined;
    if (view.revision < revision) return undefined;
    if (
      !Array.isArray(view.items) ||
      typeof view.loaded !== "boolean" ||
      typeof view.notice !== "string" ||
      typeof view.historyVersion !== "string" ||
      typeof view.historical !== "boolean" ||
      typeof view.pageLoading !== "boolean" ||
      typeof view.connection !== "string"
    )
      throw new Error("The native conversation view is invalid.");
    revision = view.revision;
    accept(view);
    return view;
  }
  async function open(owner: Awaited<typeof context>) {
    const value = await owner.request<{ viewId: string }>(
      "transcript_view.open",
      {},
    );
    if (
      !value ||
      typeof value.viewId !== "string" ||
      !value.viewId ||
      value.viewId.length > 128
    )
      throw new Error("The native conversation view identity is invalid.");
    const viewId = value.viewId;
    try {
      if (!stopped) unsubscribe = await owner.subscribe<NativeMessageView>(
        "transcript-view",
        { viewId },
        (event) => {
          if (stopped) return;
          if (event.error) {
            report(new Error(event.error.message));
            return;
          }
          try {
            receive(event.value, viewId);
          } catch (error) {
            report(error);
          }
        },
      );
    } catch (error) {
      await owner.request("transcript_view.close", { viewId }).catch(() => {});
      throw error;
    }
    return { owner, viewId };
  }
  function select(selection: NativeMessageSelection): Promise<void> {
    const request = ++generation;
    selected = (async () => {
      const owner = await context;
      if (stopped || request !== generation) return;
      if (
        selection.syncWorkspaceId &&
        selection.syncWorkspaceId !== owner.workspaceId
      )
        throw new Error("The conversation belongs to another workspace.");
      const view = await (opened ??= open(owner));
      if (stopped || request !== generation) return;
      const { syncWorkspaceId: _workspace, ...source } = selection;
      const result = await owner.request("transcript_view.select", {
        viewId: view.viewId,
        generation: request,
        workspace: owner.workspaceId,
        ...source,
      });
      if (!stopped && request === generation) receive(result, view.viewId);
    })();
    return selected;
  }
  async function command(
    method: Command,
    params: Record<string, unknown> = {},
  ) {
    const request = generation;
    const operation = method === "anchor" ? navigation : ++navigation;
    await selected;
    if (stopped || !opened || request !== generation) return undefined;
    const { owner, viewId } = await opened;
    if (stopped || request !== generation) return undefined;
    let result: unknown;
    try {
      result = await owner.request(`transcript_view.${method}`, {
        ...params,
        viewId,
        generation: request,
      });
    } catch (error) {
      if (stopped || request !== generation || operation !== navigation)
        return undefined;
      throw error;
    }
    if (!stopped && request === generation) return receive(result, viewId);
    return undefined;
  }
  async function close() {
    if (stopped) return;
    stopped = true;
    if (!opened) return;
    const { owner, viewId } = await opened;
    try { await unsubscribe?.(); }
    finally { await owner.request("transcript_view.close", { viewId }); }
  }
  return { select, command, close };
}
