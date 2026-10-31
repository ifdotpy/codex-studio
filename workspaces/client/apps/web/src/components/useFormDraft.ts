import { useRef, useState, type SetStateAction } from "react";
import type { Json } from "../types";

// These profile and rule forms contain no credential fields.
export function useFormDraft(key: string) {
  const scopes = useRef(new Map<string, Json | null>());
  const [, render] = useState(0);
  const update = (change: SetStateAction<Json | null>) => {
    const next =
      typeof change === "function"
        ? change(scopes.current.get(key) || null)
        : change;
    scopes.current.set(key, next);
    render((version) => version + 1);
  };
  return [scopes.current.get(key) || null, update] as const;
}
