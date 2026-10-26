// The paper key of a new project vault (layr-desk docs/design.md, "Set up a vault for a project")
// shows in Terminal, the person's own terminal: the words go from layr-desk to the screen only,
// never through the Studio server, its terminals (which keep their output) or an agent.
const fs = require("node:fs/promises");
const path = require("node:path");
const { randomUUID } = require("node:crypto");

const PREFIX = /^[A-Za-z0-9][A-Za-z0-9_-]{0,43}$/;

/** The layr-desk that the app carries; a development run uses STUDIO_LAYR_DESK_BIN or PATH. */
function layrDesk({ packaged, resources, env }) {
  if (packaged) return path.join(resources, "bin", "layr-desk");
  return env.STUDIO_LAYR_DESK_BIN || "layr-desk";
}

const quote = (text) => `'${text.replaceAll("'", `'\\''`)}'`;

/**
 * A Terminal script in `folder` that removes itself and runs `layr-desk keys` for the vault
 * `prefix`; layr-desk clears the terminal after the person presses Enter.
 */
async function vaultKeysScript(prefix, { folder, program }) {
  if (
    typeof prefix !== "string" ||
    !PREFIX.test(prefix) ||
    ["api", "assets"].includes(prefix)
  )
    throw new Error("Invalid vault name.");
  const file = path.join(
    folder,
    `studio-vault-keys-${prefix}-${randomUUID()}.command`,
  );
  await fs.writeFile(
    file,
    `#!/bin/sh\nrm -f "$0"\nexec ${quote(program)} keys --project ${quote(prefix)} --clear\n`,
    { mode: 0o700, flag: "wx" },
  );
  return file;
}

module.exports = { layrDesk, vaultKeysScript };
