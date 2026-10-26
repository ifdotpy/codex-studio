import { useCallback, useEffect, useRef, useState } from "react";

// This delay controls only the time that the UI shows a toast.
const TOAST_DISPLAY_MS = 5000;

export function useToast() {
  const [toast, setToast] = useState("");
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const notify = useCallback((text: string) => {
    setToast(text);
    if (timer.current !== null) clearTimeout(timer.current);
    timer.current = setTimeout(() => {
      timer.current = null;
      setToast("");
    }, TOAST_DISPLAY_MS);
  }, []);
  useEffect(
    () => () => {
      if (timer.current !== null) clearTimeout(timer.current);
    },
    [],
  );
  return [toast, setToast, notify] as const;
}
