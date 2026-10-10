import { realpathSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";

export function expandHome(value) {
  if (value === "~") return homedir();
  return value.startsWith("~/") ? join(homedir(), value.slice(2)) : value;
}

export function outsideClaude(path) {
  const absolute = resolve(expandHome(path));
  let ancestor = absolute;
  while (true) {
    try {
      const real = realpathSync(ancestor);
      if (real.split("/").includes(".claude")) {
        throw new Error(`Codex agent data must be outside .claude: ${absolute}`);
      }
      // A missing suffix can itself contain .claude.
      if (absolute.slice(ancestor.length).split("/").includes(".claude")) {
        throw new Error(`Codex agent data must be outside .claude: ${absolute}`);
      }
      return absolute;
    } catch (error) {
      if (error.code !== "ENOENT") throw error;
      const parent = dirname(ancestor);
      if (parent === ancestor) throw error;
      ancestor = parent;
    }
  }
}

export function codexHome() {
  return outsideClaude(process.env.CODEX_HOME || join(homedir(), ".codex"));
}
