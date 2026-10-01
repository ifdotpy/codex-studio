import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";

// Claude Code reads skills from the project, the user config directory and
// installed plugins. Studio lists the same roots for the composer.
function configDir() {
  return process.env.CLAUDE_CONFIG_DIR || path.join(os.homedir(), ".claude");
}

function unquote(value) {
  const text = value.trim();
  if (
    text.length >= 2 &&
    ((text.startsWith('"') && text.endsWith('"')) ||
      (text.startsWith("'") && text.endsWith("'")))
  )
    return text.slice(1, -1);
  return text;
}

export function frontmatter(source) {
  const match = /^---\r?\n([\s\S]*?)\r?\n---/.exec(source);
  if (!match) return {};
  const fields = {};
  const lines = match[1].split(/\r?\n/);
  for (let i = 0; i < lines.length; i++) {
    const field = /^([A-Za-z_-]+):\s*(.*)$/.exec(lines[i]);
    if (!field) continue;
    let value = field[2];
    if (/^[>|][-+]?$/.test(value.trim())) {
      const block = [];
      while (i + 1 < lines.length && /^\s+\S/.test(lines[i + 1]))
        block.push(lines[++i].trim());
      value = block.join(" ");
    }
    fields[field[1]] = unquote(value);
  }
  return fields;
}

async function skillsIn(root, prefix, errors) {
  let entries;
  try {
    entries = await fs.readdir(root, { withFileTypes: true });
  } catch (error) {
    if (error.code !== "ENOENT" && error.code !== "ENOTDIR")
      errors.push({ message: `${root}: ${error.message}` });
    return [];
  }
  const result = [];
  for (const entry of entries) {
    if (!entry.isDirectory() && !entry.isSymbolicLink()) continue;
    const file = path.join(root, entry.name, "SKILL.md");
    let source;
    try {
      source = await fs.readFile(file, "utf8");
    } catch (error) {
      if (error.code !== "ENOENT" && error.code !== "ENOTDIR")
        errors.push({ message: `${file}: ${error.message}` });
      continue;
    }
    const fields = frontmatter(source);
    const name = fields.name || entry.name;
    result.push({
      name: prefix ? `${prefix}:${name}` : name,
      description: fields.description || "",
      path: file,
      enabled: true,
    });
  }
  return result.sort((a, b) => a.name.localeCompare(b.name));
}

async function pluginRoots(errors) {
  const file = path.join(configDir(), "plugins", "installed_plugins.json");
  let installed;
  try {
    installed = JSON.parse(await fs.readFile(file, "utf8"));
  } catch (error) {
    if (error.code !== "ENOENT")
      errors.push({ message: `${file}: ${error.message}` });
    return [];
  }
  let enabled = null;
  try {
    const settings = JSON.parse(
      await fs.readFile(path.join(configDir(), "settings.json"), "utf8"),
    );
    if (settings.enabledPlugins && typeof settings.enabledPlugins === "object")
      enabled = settings.enabledPlugins;
  } catch {
    enabled = null;
  }
  const roots = [];
  for (const [key, installs] of Object.entries(installed.plugins || {})) {
    if (enabled && enabled[key] === false) continue;
    const install = Array.isArray(installs) ? installs[0] : installs;
    if (!install?.installPath) continue;
    roots.push({
      root: path.join(install.installPath, "skills"),
      prefix: key.split("@")[0],
    });
  }
  return roots;
}

export async function listSkills(cwds) {
  const plugins = await pluginRoots([]);
  const data = [];
  for (const cwd of cwds || []) {
    const errors = [];
    const roots = [];
    // Project skills from the folder and its parents, nearest first.
    let dir = path.resolve(cwd);
    const home = os.homedir();
    for (;;) {
      roots.push({ root: path.join(dir, ".claude", "skills"), prefix: "" });
      const parent = path.dirname(dir);
      if (parent === dir || dir === home) break;
      dir = parent;
    }
    roots.push({ root: path.join(configDir(), "skills"), prefix: "" });
    roots.push(...plugins);
    const skills = [];
    const seen = new Set();
    for (const { root, prefix } of roots)
      for (const skill of await skillsIn(root, prefix, errors)) {
        if (seen.has(skill.name)) continue;
        seen.add(skill.name);
        skills.push(skill);
      }
    data.push({ cwd, skills, errors });
  }
  return { data };
}
