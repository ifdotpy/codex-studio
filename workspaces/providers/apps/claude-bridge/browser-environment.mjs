import { readFileSync } from "node:fs";
import path from "node:path";

// Studio's launcher for browser links of a session (docs/vm-browser-bridge.md, "Native chats"
// and section 6.4). The server writes studio-browser.json next to the bridge state; the bridge
// process keeps its own environment, so its launch identity does not change.
export function browserEnvironment(root, sessionId, environment = process.env) {
  let value;
  try {
    value = JSON.parse(
      readFileSync(path.join(root, "studio-browser.json"), "utf8"),
    );
  } catch {
    return {};
  }
  if (
    typeof value?.BROWSER !== "string" ||
    typeof value?.STUDIO_BROWSER_SOCKET !== "string"
  )
    return {};
  const result = {
    BROWSER: value.BROWSER,
    STUDIO_BROWSER_SOCKET: value.STUDIO_BROWSER_SOCKET,
    STUDIO_BROWSER_SESSION: sessionId,
  };
  // Tools that search PATH for xdg-open find Studio's launcher links first.
  if (
    typeof value.STUDIO_BROWSER_PATH === "string" &&
    path.isAbsolute(value.STUDIO_BROWSER_PATH)
  ) {
    const rest = (environment.PATH || "")
      .split(path.delimiter)
      .filter((entry) => entry && entry !== value.STUDIO_BROWSER_PATH);
    result.PATH = [value.STUDIO_BROWSER_PATH, ...rest].join(path.delimiter);
  }
  return result;
}
