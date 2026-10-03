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
