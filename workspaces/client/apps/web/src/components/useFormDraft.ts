import { useRef, useState, type SetStateAction } from "react";
import { saved } from "../api";
import { writeLocalDraft } from "../sync/localDraft";
import type { Json } from "../types";

// These profile and rule forms contain no credential fields.
export function useFormDraft(key: string, notify: (message: string) => void) {
  const scopes = useRef(new Map<string, Json | null>());
  const [, render] = useState(0);
  if (!scopes.current.has(key)) scopes.current.set(key, saved(key, null));
  const update = (change: SetStateAction<Json | null>) => {
    const next =
      typeof change === "function"
        ? change(scopes.current.get(key) || null)
        : change;
    scopes.current.set(key, next);
    const error = writeLocalDraft(key, next);
    if (error) notify(error);
    render((version) => version + 1);
  };
  return [scopes.current.get(key) || null, update] as const;
}
