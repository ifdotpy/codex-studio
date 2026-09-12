import { useEffect, useRef, useState } from "react";
import "./file-preview.css";

export default function ImageViewer({
  src,
  alt,
  onError,
}: {
  src: string;
  alt: string;
  onError?: () => void;
}) {
  const viewport = useRef<HTMLDivElement>(null);
  const [natural, setNatural] = useState({ width: 0, height: 0 });
  const [box, setBox] = useState({ width: 0, height: 0 });
  const [zoom, setZoom] = useState<number | null>(null);
  useEffect(() => {
    setNatural({ width: 0, height: 0 });
    setZoom(null);
  }, [src]);
  useEffect(() => {
    const node = viewport.current;
    if (!node) return;
    const observer = new ResizeObserver(() =>
      setBox({ width: node.clientWidth, height: node.clientHeight }),
    );
    observer.observe(node);
    return () => observer.disconnect();
  }, []);
  const fit =
    natural.width && box.width
      ? Math.min(1, box.width / natural.width, box.height / natural.height)
      : 1;
  const scale = zoom ?? fit;
  const change = (factor: number) =>
    setZoom(Math.max(0.05, Math.min(8, scale * factor)));
  return (
    <section className="image-viewer" aria-label="Image viewer">
      <div className="file-preview-toolbar">
        <button
          type="button"
          onClick={() => setZoom(null)}
          aria-pressed={zoom === null}
        >
          Fit
        </button>
        <button
          type="button"
          onClick={() => setZoom(1)}
          aria-pressed={zoom === 1}
        >
          Actual size
        </button>
        <button
          type="button"
          aria-label="Zoom out"
          onClick={() => change(1 / 1.25)}
        >
          −
        </button>
        <output aria-label="Image zoom">{Math.round(scale * 100)}%</output>
        <button type="button" aria-label="Zoom in" onClick={() => change(1.25)}>
          +
        </button>
      </div>
      <div
        ref={viewport}
        className="image-viewer-viewport"
        tabIndex={0}
        aria-label="Image canvas. Use plus and minus to zoom, zero to fit, one for actual size."
        onKeyDown={(event) => {
          if (event.key === "+" || event.key === "=") change(1.25);
          else if (event.key === "-") change(1 / 1.25);
          else if (event.key === "0") setZoom(null);
          else if (event.key === "1") setZoom(1);
          else return;
          event.preventDefault();
        }}
      >
        <img
          src={src}
          alt={alt}
          referrerPolicy="no-referrer"
          draggable={false}
          onLoad={(event) =>
            setNatural({
              width: event.currentTarget.naturalWidth,
              height: event.currentTarget.naturalHeight,
            })
          }
          onError={onError}
          onClick={() => setZoom(zoom === null ? 1 : null)}
          style={
            natural.width
              ? { width: natural.width * scale, height: natural.height * scale }
              : undefined
          }
        />
      </div>
    </section>
  );
}
