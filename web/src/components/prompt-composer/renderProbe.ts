export type PromptComposerRenderProbe = (
  component: "app" | "sidebar" | "conversation",
) => void;

declare global {
  interface Window {
    /** Optional hook used by isolated browser checks to count real UI renders. */
    __studioPromptComposerRenderProbe?: PromptComposerRenderProbe;
  }
}

export function reportPromptComposerRender(
  component: Parameters<PromptComposerRenderProbe>[0],
) {
  if (typeof window !== "undefined")
    window.__studioPromptComposerRenderProbe?.(component);
}
