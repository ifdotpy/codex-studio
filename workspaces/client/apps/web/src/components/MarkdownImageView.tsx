import type { ReactNode } from "react";
export default function MarkdownImageView({
  image,
  error,
  alt,
  remoteHost,
  onPreview,
  onError,
  onRetry,
  onLoad,
  children,
}: {
  image: string;
  error: string;
  alt: string;
  remoteHost: string;
  onPreview(): void;
  onError(): void;
  onRetry(): void;
  onLoad(): void;
  children: ReactNode;
}) {
  return (
    <span className="markdown-image">
      {image && !error ? (
        <button
          type="button"
          className="image-preview-button"
          aria-label={`Preview ${alt || "image"}`}
          onClick={(event) => {
            event.preventDefault();
            event.stopPropagation();
            onPreview();
          }}
        >
          <img
            src={image}
            alt={alt}
            loading="lazy"
            referrerPolicy="no-referrer"
            onError={onError}
          />
        </button>
      ) : (
        <span className="markdown-image-placeholder">
          <span>{alt || "Image"}</span>
          {error ? (
            <>
              <span role="alert">{error}</span>
              <button type="button" onClick={onRetry}>
                Retry image
              </button>
            </>
          ) : remoteHost ? (
            <>
              <span>External image. Load it to contact {remoteHost}.</span>
              <button type="button" onClick={onLoad}>
                Load image
              </button>
            </>
          ) : (
            <span role="status">Load image…</span>
          )}
        </span>
      )}
      {children}
    </span>
  );
}
