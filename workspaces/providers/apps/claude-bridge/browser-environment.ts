import { readFileSync, renameSync, writeFileSync } from "node:fs";
import path from "node:path";

// The file that Claude Code runs ahead of each Bash command of a session.
export const PATH_SCRIPT = "studio-browser-path.sh";

function shellQuote(text: string) {
  return "'" + text.replaceAll("'", "'\"'\"'") + "'";
}

// Write the script only when its text changes: sessions start often.
function writePathScript(file: string, folder: string) {
  const text = `PATH=${shellQuote(folder)}:"$PATH"; export PATH\n`;
  try {
    if (readFileSync(file, "utf8") === text) return;
  } catch {
    // A missing or unreadable script is written below.
  }
  const temporary = `${file}.${process.pid}.tmp`;
  writeFileSync(temporary, text, { mode: 0o600 });
  renameSync(temporary, file);
}

// Studio's launcher for browser links of a session (docs/vm-browser-bridge.md, "Native chats"
// and section 6.4). The server writes studio-browser.json next to the bridge state; the bridge
// process keeps its own environment, so its launch identity does not change.
export function browserEnvironment(
  root: string,
  sessionId: string,
  environment = process.env,
): NodeJS.ProcessEnv {
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
  const result: NodeJS.ProcessEnv = {
    BROWSER: value.BROWSER,
    STUDIO_BROWSER_SOCKET: value.STUDIO_BROWSER_SOCKET,
    STUDIO_BROWSER_SESSION: sessionId,
  };
  // Tools that search PATH for xdg-open (or `open` on macOS) find Studio's launcher links first.
  if (
    typeof value.STUDIO_BROWSER_PATH === "string" &&
    path.isAbsolute(value.STUDIO_BROWSER_PATH)
  ) {
    const rest = (environment.PATH || "")
      .split(path.delimiter)
      .filter((entry) => entry && entry !== value.STUDIO_BROWSER_PATH);
    result.PATH = [value.STUDIO_BROWSER_PATH, ...rest].join(path.delimiter);
    // A login shell's startup (macOS path_helper, a Linux /etc/profile) or Claude Code's shell
    // snapshot can move the folder after /usr/bin, where the system `open` wins. Claude Code
    // runs CLAUDE_ENV_FILE after that startup, ahead of each Bash command. A person's own
    // CLAUDE_ENV_FILE stays in place.
    if (!environment.CLAUDE_ENV_FILE) {
      const script = path.join(root, PATH_SCRIPT);
      try {
        writePathScript(script, value.STUDIO_BROWSER_PATH);
        result.CLAUDE_ENV_FILE = script;
      } catch {
        // Without the script the session keeps the PATH order above.
      }
    }
  }
  return result;
}
