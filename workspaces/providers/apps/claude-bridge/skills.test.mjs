import assert from "node:assert/strict";
import { test } from "vitest";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";

test("skill discovery and frontmatter parsing", async () => {
  const root = await fs.mkdtemp(
    path.join(os.tmpdir(), "claude-skills-contract-"),
  );
  const config = path.join(root, "config");
  const previousConfigDir = process.env.CLAUDE_CONFIG_DIR;
  process.env.CLAUDE_CONFIG_DIR = config;
  const { listSkills, frontmatter } = await import("./skills.mjs");
  const skill = async (dir, name, body) => {
    await fs.mkdir(path.join(dir, name), { recursive: true });
    await fs.writeFile(path.join(dir, name, "SKILL.md"), body);
  };
  try {
    const project = path.join(root, "repo", "app");
    await skill(
      path.join(root, "repo", ".claude", "skills"),
      "deploy",
      "---\nname: deploy\ndescription: Project deploy steps\n---\nBody",
    );
    await skill(
      path.join(config, "skills"),
      "deploy",
      "---\nname: deploy\ndescription: User copy\n---\n",
    );
    await skill(
      path.join(config, "skills"),
      "review",
      "---\nname: review\ndescription: >\n  Folded line one\n  line two\n---\n",
    );
    const plugin = path.join(root, "plugins", "warp");
    await skill(
      path.join(plugin, "skills"),
      "notify",
      "---\nname: notify\ndescription: 'Quoted'\n---\n",
    );
    await fs.mkdir(path.join(config, "plugins"), { recursive: true });
    await fs.writeFile(
      path.join(config, "plugins", "installed_plugins.json"),
      JSON.stringify({
        version: 2,
        plugins: { "warp@market": [{ installPath: plugin }] },
      }),
    );
    await fs.writeFile(
      path.join(config, "settings.json"),
      JSON.stringify({ enabledPlugins: { "warp@market": true } }),
    );
    const { data } = await listSkills([project]);
    assert.equal(data.length, 1);
    assert.equal(data[0].cwd, project);
    const byName = Object.fromEntries(data[0].skills.map((s) => [s.name, s]));
    assert.equal(
      byName.deploy.description,
      "Project deploy steps",
      "Project skills take precedence",
    );
    assert.equal(byName.review.description, "Folded line one line two");
    assert.equal(byName["warp:notify"].description, "Quoted");
    assert.ok(
      data[0].skills.every((s) => s.enabled && s.path.endsWith("SKILL.md")),
    );
    assert.deepEqual(data[0].errors, []);
    assert.deepEqual(frontmatter("no header"), {});
    console.log(
      "PASS claude skills: project, user and plugin roots; precedence; folded and quoted descriptions",
    );
  } finally {
    if (previousConfigDir === undefined) delete process.env.CLAUDE_CONFIG_DIR;
    else process.env.CLAUDE_CONFIG_DIR = previousConfigDir;
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("plugin skills use each project's install scope and local enable override", async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "claude-skill-scope-"));
  const previous = process.env.CLAUDE_CONFIG_DIR;
  const config = path.join(root, "config");
  process.env.CLAUDE_CONFIG_DIR = config;
  const { listSkills } = await import("./skills.mjs");
  const a = path.join(root, "a"),
    b = path.join(root, "b");
  const write = async (file, value) => {
    await fs.mkdir(path.dirname(file), { recursive: true });
    await fs.writeFile(
      file,
      typeof value === "string" ? value : JSON.stringify(value),
    );
  };
  try {
    for (const dir of [a, b]) {
      await write(path.join(dir, ".claude/settings.json"), {
        enabledPlugins: { "tool@market": true },
      });
      await write(
        path.join(dir, "plugin/skills/check/SKILL.md"),
        `---\nname: check\ndescription: ${dir}\n---\n`,
      );
    }
    await write(path.join(config, "settings.json"), {
      enabledPlugins: { "tool@market": true },
    });
    await write(path.join(a, ".claude/settings.local.json"), {
      enabledPlugins: { "tool@market": false },
    });
    await write(path.join(config, "plugins/installed_plugins.json"), {
      version: 2,
      plugins: {
        "tool@market": [
          {
            scope: "project",
            projectPath: b,
            installPath: path.join(b, "plugin"),
          },
          {
            scope: "project",
            projectPath: a,
            installPath: path.join(a, "plugin"),
          },
        ],
        "other@market": [
          {
            scope: "project",
            projectPath: b,
            installPath: path.join(b, "plugin"),
          },
        ],
      },
    });
    const { data } = await listSkills([a, b]);
    assert.equal(
      data[0].skills.find((s) => s.name === "tool:check").description,
      a,
    );
    assert.equal(
      data[0].skills.find((s) => s.name === "tool:check").enabled,
      false,
    );
    assert.equal(
      data[0].skills.some((s) => s.name === "other:check"),
      false,
    );
    assert.equal(
      data[1].skills.find((s) => s.name === "tool:check").enabled,
      true,
    );
    assert.equal(
      data[1].skills.find((s) => s.name === "other:check").enabled,
      false,
    );
  } finally {
    if (previous === undefined) delete process.env.CLAUDE_CONFIG_DIR;
    else process.env.CLAUDE_CONFIG_DIR = previous;
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("plugin scope follows Git worktrees and reports malformed local policy", async () => {
  const { execFile } = await import("node:child_process");
  const { promisify } = await import("node:util");
  const exec = promisify(execFile);
  const root = await fs.mkdtemp(
    path.join(os.tmpdir(), "claude-worktree-skills-"),
  );
  const previous = process.env.CLAUDE_CONFIG_DIR;
  const config = path.join(root, "config"),
    repo = path.join(root, "repo"),
    worktree = path.join(root, "worktree");
  process.env.CLAUDE_CONFIG_DIR = config;
  try {
    await fs.mkdir(repo);
    await exec("git", ["init", repo]);
    await exec("git", [
      "-C",
      repo,
      "-c",
      "user.name=Test",
      "-c",
      "user.email=test@example.test",
      "-c",
      "commit.gpgsign=false",
      "commit",
      "--allow-empty",
      "-m",
      "Fixture",
    ]);
    await exec("git", ["-C", repo, "worktree", "add", "--detach", worktree]);
    await fs.mkdir(path.join(config, "plugins"), { recursive: true });
    await fs.mkdir(path.join(root, "plugin/skills/check"), { recursive: true });
    await fs.writeFile(
      path.join(root, "plugin/skills/check/SKILL.md"),
      "---\nname: check\n---\n",
    );
    await fs.writeFile(
      path.join(config, "settings.json"),
      JSON.stringify({
        enabledPlugins: { "tool@market": true, "global@market": true },
      }),
    );
    await fs.writeFile(
      path.join(config, "plugins/installed_plugins.json"),
      JSON.stringify({
        plugins: {
          "tool@market": [
            {
              scope: "local",
              projectPath: repo,
              installPath: path.join(root, "plugin"),
            },
          ],
          "global@market": [
            { scope: "user", installPath: path.join(root, "plugin") },
          ],
        },
      }),
    );
    const { listSkills } = await import("./skills.mjs");
    const result = await listSkills([worktree]);
    assert.deepEqual(
      result.data[0].skills.map((s) => [s.name, s.enabled]),
      [
        ["tool:check", true],
        ["global:check", true],
      ],
    );
    await fs.mkdir(path.join(worktree, ".claude"));
    await fs.writeFile(
      path.join(worktree, ".claude/settings.local.json"),
      "{broken",
    );
    const broken = (await listSkills([worktree])).data[0];
    assert.ok(
      broken.errors.some((e) => e.message.includes("settings.local.json")),
    );
    assert.ok(broken.skills.every((s) => s.enabled === false));
    await fs.writeFile(
      path.join(worktree, ".claude/settings.local.json"),
      "null",
    );
    const invalid = (await listSkills([worktree])).data[0];
    assert.ok(
      invalid.errors.some((e) => e.message.includes("settings.local.json")),
    );
    assert.ok(invalid.skills.every((s) => s.enabled === false));
    await fs.writeFile(
      path.join(worktree, ".claude/settings.local.json"),
      JSON.stringify({ enabledPlugins: { "tool@market": false } }),
    );
    const nested = path.join(worktree, "nested");
    await fs.mkdir(path.join(nested, ".claude"), { recursive: true });
    await fs.writeFile(
      path.join(nested, ".claude/settings.json"),
      JSON.stringify({ enabledPlugins: { "tool@market": true } }),
    );
    const nestedSkills = (await listSkills([nested])).data[0].skills;
    assert.equal(
      nestedSkills.find((s) => s.name === "tool:check").enabled,
      false,
    );
  } finally {
    if (previous === undefined) delete process.env.CLAUDE_CONFIG_DIR;
    else process.env.CLAUDE_CONFIG_DIR = previous;
    await fs.rm(root, { recursive: true, force: true });
  }
});
