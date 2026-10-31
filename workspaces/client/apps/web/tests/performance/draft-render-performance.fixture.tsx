import type { useSyncedDrafts } from "../../src/sync/drafts.ts";
import type { syncDatabase } from "../../src/sync/client.ts";
import type PromptComposer from "../../src/components/prompt-composer/PromptComposer.tsx";
import type React from "react";
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
  }
  const { useSyncedDrafts } = await importWithRetry(
    () => import("../../src/sync/drafts.ts"),
  );
  try {
    window.PromptComposer = (
      await importWithRetry(
        () => import("../../src/components/prompt-composer/PromptComposer.tsx"),
      )
    ).default;
  } catch {
    window.PromptComposer = null;
  }
  const { db } = await (
    await importWithRetry(() => import("../../src/sync/client.ts"))
  ).syncDatabase();
  window.db = db;
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
  await db.drafts.hydrate(docs);
  const r = await importWithRetry(() => import("react")),
    d = await importWithRetry(() => import("react-dom/client"));
  const React = r.default || r,
    { createRoot } = d.default || d;
  const node = document.createElement("div");
  document.body.append(node);
  window.commits = 0;
  window.renders = 0;
  window.composerCommits = 0;
  window.sidebarRenders = 0;
  window.transcriptRenders = 0;
  createRoot(node).render(
    React.createElement(
      React.Profiler,
      { id: "draft", onRender: () => window.commits++ },
      React.createElement(function Harness() {
        window.renders++;
        window.draft = useSyncedDrafts();
        return React.createElement(
          React.Fragment,
          null,
          React.createElement(function SidebarFixture() {
            window.sidebarRenders++;
            return React.createElement("aside");
          }),
          React.createElement(function TranscriptFixture() {
            window.transcriptRenders++;
            return React.createElement("main");
          }),
          typeof window.draft.getDraft === "function" && window.PromptComposer
            ? React.createElement(window.PromptComposer, {
                session: "chat-0",
                getDraft: window.draft.getDraft,
                subscribeDraft: window.draft.subscribeDraft,
                onSubmit: (event: React.FormEvent) => event.preventDefault(),
                children: (value: string) =>
                  React.createElement(
                    React.Profiler,
                    {
                      id: "prompt-composer",
                      onRender: () => window.composerCommits++,
                    },
                    React.createElement("span", null, value),
                  ),
              })
            : React.createElement(
                React.Profiler,
                {
                  id: "prompt-composer",
                  onRender: () => window.composerCommits++,
                },
                React.createElement(
                  "span",
                  null,
                  window.draft.drafts["chat-0"],
                ),
              ),
        );
      }),
    ),
  );
}
export async function mountOtherDraftFixture() {
  const { useSyncedDrafts } = await import("../../src/sync/drafts.ts");
  const r = await import("react");
  const d = await import("react-dom/client");
  const React = r.default || r;
  const { createRoot } = d.default || d;
  const node = document.createElement("div");
  document.body.appendChild(node);
  createRoot(node).render(
    React.createElement(function Harness() {
      window.draft = useSyncedDrafts();
      return null;
    }),
  );
}
declare global {
  interface Window {
    draft: ReturnType<typeof useSyncedDrafts>;
    db: Awaited<ReturnType<typeof syncDatabase>>["db"];
    PromptComposer: typeof PromptComposer | null;
    commits: number;
    renders: number;
    composerCommits: number;
    sidebarRenders: number;
    transcriptRenders: number;
  }
}
