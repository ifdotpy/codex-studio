import { Loader } from "@mantine/core";
import { useEffect, useState } from "react";
import ImageViewer from "../components/ImageViewer";
import ErrorDescription from "../components/ErrorDescription";
import type { NativePage } from "./preview";

export default function DocumentPreview({
  load,
  title,
  accept,
}: {
  load(page: number): Promise<NativePage & { svg?: string }>;
  title: string;
  accept?(result: NativePage & { svg?: string }): void;
}) {
  const [index, setIndex] = useState(0);
  const [result, setResult] = useState<NativePage | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [busy, setBusy] = useState(true);
  useEffect(() => {
    let active = true;
    setBusy(true);
    setError(null);
    void load(index)
      .then(
        (value) => {
          if (active) {
            setResult(value);
            accept?.(value);
          }
        },
        (error: unknown) => {
          if (active) setError(error);
        },
      )
      .finally(() => {
        if (active) setBusy(false);
      });
    return () => {
      active = false;
    };
  }, [load, index, accept]);
  return (
    <div className="file-preview-document">
      {error != null && (
        <p>
          <ErrorDescription value={error} />
        </p>
      )}
      {busy && <Loader size="sm" aria-label="Render preview" />}
      {result && (
        <>
          <ImageViewer src={result.url} alt={title} />
          {result.pages > 1 && (
            <div className="file-preview-toolbar">
              <button
                type="button"
                disabled={busy || index === 0}
                onClick={() => setIndex(index - 1)}
              >
                Previous page
              </button>
              <span>
                Page {index + 1} of {result.pages}
              </span>
              <button
                type="button"
                disabled={busy || index + 1 >= result.pages}
                onClick={() => setIndex(index + 1)}
              >
                Next page
              </button>
            </div>
          )}
        </>
      )}
    </div>
  );
}
