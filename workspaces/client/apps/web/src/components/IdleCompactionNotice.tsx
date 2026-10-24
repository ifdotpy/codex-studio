import "./native-notice.css";

/** The server adds this notice only after the native compact boundary arrives. */
export function IdleCompactionNotice({ text }: { text?: string | null }) {
  return (
    <div className="native-notice" role="status">
      {text}
    </div>
  );
}
