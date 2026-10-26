import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
const exec = promisify(execFile);

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

async function skillsIn(root, prefix, errors, enabled = true) {
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
      enabled,
    });
  }
  return result.sort((a, b) => a.name.localeCompare(b.name));
}

async function readSettings(file, errors) {
  try {
    const value = JSON.parse(await fs.readFile(file, "utf8"));
    if (!value || typeof value !== "object" || Array.isArray(value))
      throw new Error("Settings must contain an object");
    if (
      value.enabledPlugins != null &&
      (typeof value.enabledPlugins !== "object" ||
        Array.isArray(value.enabledPlugins))
    )
      throw new Error("enabledPlugins must contain an object");
    return value;
  } catch (error) {
    if (error.code !== "ENOENT")
      errors.push({ message: `${file}: ${error.message}` });
    return {};
  }
}

async function projectIdentity(cwd) {
  try {
    const { stdout } = await exec(
      "git",
      [
        "-C",
        cwd,
        "rev-parse",
        "--path-format=absolute",
        "--show-toplevel",
        "--git-common-dir",
      ],
      { timeout: 5000 },
    );
    const [root, common] = stdout.trim().split("\n");
    return { root, common: await fs.realpath(common) };
  } catch {
    return { root: path.resolve(cwd), common: null };
  }
}

function managedDirectory() {
  if (process.platform === "darwin")
    return "/Library/Application Support/ClaudeCode";
  if (process.platform === "win32")
    return path.join(
      process.env.ProgramFiles || "C:\\Program Files",
      "ClaudeCode",
    );
  return "/etc/claude-code";
}

async function pluginRoots(cwd, errors) {
  const installed = await readSettings(
    path.join(configDir(), "plugins", "installed_plugins.json"),
    errors,
  );
  const project = await projectIdentity(cwd);
  const files = [path.join(configDir(), "settings.json")];
  for (const name of ["settings.json", "settings.local.json"])
    for (const dir of new Set([project.root, path.resolve(cwd)]))
      files.push(path.join(dir, ".claude", name));
  const managed = managedDirectory();
  files.push(path.join(managed, "managed-settings.json"));
  try {
    const names = await fs.readdir(path.join(managed, "managed-settings.d"));
    files.push(
      ...names
        .filter((name) => name.endsWith(".json") && !name.startsWith("."))
        .sort()
        .map((name) => path.join(managed, "managed-settings.d", name)),
    );
  } catch (error) {
    if (error.code !== "ENOENT")
      errors.push({ message: `${managed}: ${error.message}` });
  }
  const enabled = {};
  let validSettings = errors.length === 0;
  for (const file of files) {
    const before = errors.length;
    const settings = await readSettings(file, errors);
    validSettings &&= before === errors.length;
    Object.assign(enabled, settings.enabledPlugins || {});
  }
  const roots = [];
  for (const [key, installs] of Object.entries(installed.plugins || {})) {
    // Legacy registries contain one user installation without an explicit scope.
    const candidates = Array.isArray(installs) ? installs : [installs];
    for (const install of candidates) {
      if (!install?.installPath) continue;
      let matches =
        install.scope === "user" ||
        install.scope === "managed" ||
        (!install.scope && !install.projectPath);
      if (!matches && typeof install.projectPath === "string") {
        matches = path.resolve(install.projectPath) === path.resolve(cwd);
        if (!matches && project.common) {
          const other = await projectIdentity(install.projectPath);
          matches = other.common === project.common;
        }
      }
      if (!matches) continue;
      roots.push({
        root: path.join(install.installPath, "skills"),
        prefix: key.split("@")[0],
        enabled: validSettings && enabled[key] === true,
      });
      break;
    }
  }
  return roots;
}

export async function listSkills(cwds) {
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
    roots.push(...(await pluginRoots(cwd, errors)));
    const skills = [];
    const seen = new Set();
    for (const { root, prefix, enabled } of roots)
      for (const skill of await skillsIn(root, prefix, errors, enabled)) {
        if (seen.has(skill.name)) continue;
        seen.add(skill.name);
        skills.push(skill);
      }
    data.push({ cwd, skills, errors });
  }
  return { data };
}
