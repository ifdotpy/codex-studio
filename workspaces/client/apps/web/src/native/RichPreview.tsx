import { useCallback, useRef } from "react";
import RichPreviewView from "../components/RichPreviewView";
import DocumentPreview from "./DocumentPreview";
import { useNativePreview } from "./usePreview";

export default function RichPreview({
  kind,
  source,
}: {
  kind: "html" | "mermaid";
  source: string;
}) {
  const owner = useNativePreview();
  const rendered = useRef<{ source: string; svg?: string }>({ source });
  const load = useCallback(
    (page: number) => owner.then((port) => port.document(kind, source, page)),
    [owner, kind, source],
  );
  const accept = useCallback(
    (value: { svg?: string }) => {
      rendered.current = { source, svg: value.svg };
    },
    [source],
  );
  return (
    <RichPreviewView
      kind={kind}
      source={source}
      copy={(text) => owner.then((port) => port.copy(text))}
      save={(mode) => {
        const svg =
          rendered.current.source === source ? rendered.current.svg : undefined;
        const diagram = kind === "mermaid" && mode === "preview" && !!svg;
        return owner.then((port) =>
          port.saveText(
            kind === "html"
              ? "preview.html"
              : diagram
                ? "diagram.svg"
                : "diagram.mmd",
            diagram ? svg! : source,
          ),
        );
      }}
      preview={
        <DocumentPreview
          key={JSON.stringify([kind, source])}
          title={
            kind === "html" ? "HTML content preview" : "Mermaid diagram preview"
          }
          load={load}
          accept={accept}
        />
      }
    />
  );
}
