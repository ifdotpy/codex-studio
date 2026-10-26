import type { LocalDraftRecord } from "../../src/sync/draftStorage.ts";
import type { useSyncedDrafts } from "../../src/sync/drafts.ts";
import type { syncDatabase } from "../../src/sync/client.ts";
import type PromptComposer from "../../src/components/prompt-composer/PromptComposer.tsx";
import React from "react";
import { createRoot } from "react-dom/client";
export async function mountDraftFixture({
  count,
  workspaceId,
}: {
  count: number;
  workspaceId: string;
}) {
  const importWithRetry = async <T,>(load: () => Promise<T>): Promise<T> => {
    for (let attempt = 0; ; attempt++) {
      try {
        return await load();
      } catch (error) {
        if (
          attempt === 11 ||
          !String(error).includes("Failed to fetch dynamically imported module")
        )
          throw error;
        await new Promise((resolve) => setTimeout(resolve, 250));
      }
    }
  };
  localStorage.setItem("codex-sync-workspace", JSON.stringify(workspaceId));
  const drafts: Record<string, string> = {};
  for (let i = 0; i < count; i++) {
    const session = "chat-" + i;
    const text = "x".repeat(2000);
    drafts[session] = text;
    localStorage.setItem(
      `codex-chat-draft:${workspaceId}:${session}`,
      JSON.stringify({
        version: 1,
        workspace: workspaceId,
        session,
        text,
        deleted: false,
        source: "local",
        updated: 1,
      } satisfies LocalDraftRecord),
    );
  }
  const { useSyncedDrafts } = await importWithRetry(
    () => import("../../src/sync/drafts.ts"),
  );
  try {
    window.draftPerformancePromptComposer = (
      await importWithRetry(
        () => import("../../src/components/prompt-composer/PromptComposer.tsx"),
      )
    ).default;
  } catch {
    window.draftPerformancePromptComposer = null;
  }
  const { db } = await (
    await importWithRetry(() => import("../../src/sync/client.ts"))
  ).syncDatabase();
  window.draftPerformanceDb = db;
  const docs = Object.entries(drafts).map(([session, text]) => ({
    id: "fixture:" + session,
    seq: 0,
    payload: JSON.stringify({
      id: "fixture:" + session,
      device: "fixture",
      session,
      text,
      updated: 1,
    }),
  }));
  await db.drafts.bulkInsert(docs);
  const r = await importWithRetry(() => import("react")),
    d = await importWithRetry(() => import("react-dom/client"));
  const React = r.default || r,
    { createRoot } = d.default || d;
  const node = document.createElement("div");
  document.body.append(node);
  window.draftPerformanceCommits = 0;
  window.draftPerformanceRenders = 0;
  window.draftPerformanceComposerCommits = 0;
  window.draftPerformanceSidebarRenders = 0;
  window.draftPerformanceTranscriptRenders = 0;
  createRoot(node).render(
    React.createElement(
      React.Profiler,
      { id: "draft", onRender: () => window.draftPerformanceCommits++ },
      React.createElement(function Harness() {
        window.draftPerformanceRenders++;
        window.draftPerformanceDraft = useSyncedDrafts();
        return React.createElement(
          React.Fragment,
          null,
          React.createElement(function SidebarFixture() {
            window.draftPerformanceSidebarRenders++;
            return React.createElement("aside");
          }),
          React.createElement(function TranscriptFixture() {
            window.draftPerformanceTranscriptRenders++;
            return React.createElement("main");
          }),
          typeof window.draftPerformanceDraft.getDraft === "function" &&
            window.draftPerformancePromptComposer
            ? React.createElement(window.draftPerformancePromptComposer, {
                session: "chat-0",
                getDraft: window.draftPerformanceDraft.getDraft,
                subscribeDraft: window.draftPerformanceDraft.subscribeDraft,
                onSubmit: (event: React.FormEvent) => event.preventDefault(),
                children: (value: string) =>
                  React.createElement(
                    React.Profiler,
                    {
                      id: "prompt-composer",
                      onRender: () => window.draftPerformanceComposerCommits++,
                    },
                    React.createElement("span", null, value),
                  ),
              })
            : React.createElement(
                React.Profiler,
                {
                  id: "prompt-composer",
                  onRender: () => window.draftPerformanceComposerCommits++,
                },
                React.createElement(
                  "span",
                  null,
                  window.draftPerformanceDraft.drafts["chat-0"],
                ),
              ),
        );
      }),
    ),
  );
}
export async function mountOtherDraftFixture() {
  const { useSyncedDrafts } = await import("../../src/sync/drafts.ts");
  const node = document.createElement("div");
  document.body.appendChild(node);
  createRoot(node).render(
    React.createElement(function Harness() {
      window.draftPerformanceDraft = useSyncedDrafts();
      return null;
    }),
  );
}
declare global {
  interface Window {
    draftPerformanceDraft: ReturnType<typeof useSyncedDrafts>;
    draftPerformanceDb: Awaited<ReturnType<typeof syncDatabase>>["db"];
    draftPerformancePromptComposer: typeof PromptComposer | null;
    draftPerformanceCommits: number;
    draftPerformanceRenders: number;
    draftPerformanceComposerCommits: number;
    draftPerformanceSidebarRenders: number;
    draftPerformanceTranscriptRenders: number;
  }
}
