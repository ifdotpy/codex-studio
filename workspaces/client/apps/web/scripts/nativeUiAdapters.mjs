/** Exact view-only scheduling owners. These exceptions permit no I/O. */
export const nativeUiAdapters = [
  {
    path: "src/components/ui/useToast.ts",
    reason: "Hide the visible toast after its display period.",
  },
  {
    path: "src/sync/frameBatch.ts",
    reason: "Coalesce view updates into one animation frame.",
  },
  {
    path: "src/components/ui/focusAfterRender.ts",
    reason: "Focus the current visible element after the view changes.",
  },
];
