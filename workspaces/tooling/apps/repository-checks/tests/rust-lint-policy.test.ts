import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import test from "node:test";

const rootUrl = new URL("../../../../../", import.meta.url);
const root = fileURLToPath(rootUrl);
const read = (file: string) => readFileSync(new URL(file, rootUrl), "utf8");

function section(document: string, name: string): string {
  const header = `[${name}]`;
  const start = document.indexOf(header);
  assert.notEqual(start, -1, `missing ${header}`);
  return document.slice(start + header.length).split(/^\[/m)[0] ?? "";
}

function assignments(document: string): Map<string, string> {
  const values = new Map<string, string>();
  for (const line of document.split("\n")) {
    const match = /^([\w-]+)\s*=\s*(.+?)\s*(?:#.*)?$/.exec(line);
    if (match) {
      const [, key, value] = match;
      assert.ok(key && value, `invalid assignment: ${line}`);
      assert.ok(!values.has(key), `duplicate setting: ${key}`);
      values.set(key, value.trim());
    }
  }
  return values;
}

function assertConfigParity(owner: string, consumer: string, name: string) {
  const expected = assignments(owner);
  const actual = assignments(consumer);
  for (const [key, value] of expected) {
    assert.equal(
      actual.get(key),
      value,
      `${name}: ${key} is owned by root clippy.toml`,
    );
  }
}

test("every workspace crate inherits the root Cargo lint policy", () => {
  const manifest = read("Cargo.toml");
  const members = /\bmembers\s*=\s*\[([\s\S]*?)\]/.exec(
    section(manifest, "workspace"),
  );
  assert.ok(members?.[1], "Cargo.toml owns explicit workspace members");
  const paths = [...members[1].matchAll(/"([^"]+)"/g)];
  assert.ok(paths.length > 0, "workspace has members");
  for (const [, member] of paths) {
    assert.ok(member, "member path exists");
    const path = `${member}/Cargo.toml`;
    const crate = read(path);
    assert.equal(
      assignments(section(crate, "lints")).get("workspace"),
      "true",
      path,
    );
    assert.doesNotMatch(
      crate,
      /^\[lints\./m,
      `${path} must not replace root lint settings`,
    );
  }
});

test("Cargo.toml keeps the selected Rust and Clippy rules at deny level", () => {
  const manifest = read("Cargo.toml");
  const rust = assignments(section(manifest, "workspace.lints.rust"));
  const clippy = assignments(section(manifest, "workspace.lints.clippy"));
  for (const name of ["unsafe_code", "unused_must_use"]) {
    assert.equal(rust.get(name), '"deny"', `Cargo.toml: ${name}`);
  }
  for (const name of [
    "let_underscore_future",
    "let_underscore_must_use",
    "unwrap_used",
    "await_holding_lock",
    "await_holding_refcell_ref",
    "large_futures",
    "wildcard_enum_match_arm",
    "match_wildcard_for_single_variants",
  ]) {
    assert.equal(clippy.get(name), '"deny"', `Cargo.toml: ${name}`);
  }
  assert.match(clippy.get("all") ?? "", /\blevel\s*=\s*"deny"/);
});

test("nested Clippy configs preserve every root-owned setting", () => {
  const owner = read("clippy.toml");
  const settings = assignments(owner);
  assert.equal(settings.get("allow-unwrap-in-tests"), "true");
  assert.match(settings.get("future-size-threshold") ?? "", /^[1-9]\d*$/);
  const paths = execFileSync(
    "git",
    ["ls-files", "--", "**/clippy.toml", "**/.clippy.toml"],
    {
      cwd: root,
      encoding: "utf8",
    },
  )
    .trim()
    .split("\n")
    .filter(Boolean);
  for (const path of paths) assertConfigParity(owner, read(path), path);
});

test("the parity witness rejects a missing or changed consumer setting", () => {
  const owner = read("clippy.toml");
  for (const [key, value] of assignments(owner)) {
    const missing = owner
      .split("\n")
      .filter((line) => !line.startsWith(`${key} =`))
      .join("\n");
    assert.throws(() => assertConfigParity(owner, missing, "missing fixture"), {
      name: "AssertionError",
    });
    const changed = owner.replace(`${key} = ${value}`, `${key} = "changed"`);
    assert.throws(() => assertConfigParity(owner, changed, "changed fixture"), {
      name: "AssertionError",
    });
  }
});
