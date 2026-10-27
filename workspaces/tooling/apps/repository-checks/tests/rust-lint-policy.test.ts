import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { CARGO_BUILD_JOBS } from "../gate-config.ts";

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
    "rest_pat_in_fully_bound_structs",
    "fn_params_excessive_bools",
    "match_bool",
    "large_enum_variant",
    "trivially_copy_pass_by_ref",
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

test("native Clippy rejects dispatch patterns that hide contract additions", () => {
  const directory = mkdtempSync(path.join(tmpdir(), "studio-lint-contract-"));
  try {
    const manifest = read("Cargo.toml");
    writeFileSync(
      path.join(directory, "Cargo.toml"),
      `[package]
name = "studio-lint-contract-witness"
version = "0.0.0"
edition.workspace = true
[lib]
path = "lib.rs"
[workspace]
[dependencies]
serde_json = "1"
[lints]
workspace = true
[workspace.package]
${section(manifest, "workspace.package")}
[workspace.lints.rust]
${section(manifest, "workspace.lints.rust")}
[workspace.lints.clippy]
${section(manifest, "workspace.lints.clippy")}`,
    );
    writeFileSync(path.join(directory, "clippy.toml"), read("clippy.toml"));
    writeFileSync(
      path.join(directory, "rust-toolchain.toml"),
      read("rust-toolchain.toml"),
    );
    const source = `
use serde_json::Value as Json;
#[deny(clippy::disallowed_types)]
pub fn typed_owner() {}
#[allow(dead_code)]
fn opaque_boundary(_: Json) {}
pub enum State { Ready, Running, Settled }
pub fn terminal(state: State) -> bool {
    match state { State::Settled => true, State::Ready | State::Running => false }
}
pub struct Receipt { pub request_id: u64 }
pub fn request_id(receipt: Receipt) -> u64 {
    let Receipt { request_id } = receipt;
    request_id
}
`;
    const check = (text: string) => {
      writeFileSync(path.join(directory, "lib.rs"), text);
      const result = spawnSync(
        "cargo",
        ["clippy", "--offline", "-j", String(CARGO_BUILD_JOBS)],
        {
          cwd: directory,
          encoding: "utf8",
          timeout: 30_000,
          // Keep fixture outputs isolated from the prepared workspace target.
          env: {
            ...process.env,
            CARGO_TARGET_DIR: path.join(directory, "target"),
            CLIPPY_CONF_DIR: directory,
          },
        },
      );
      assert.ifError(result.error);
      return result;
    };
    const baseline = check(source);
    assert.equal(baseline.status, 0, baseline.stdout + baseline.stderr);
    for (const [name, text, diagnostic] of [
      [
        "excessive boolean parameters",
        source +
          "\npub fn flags(a: bool, b: bool, c: bool, d: bool) -> bool { a && b && c && d }\n",
        /#fn_params_excessive_bools/,
      ],
      [
        "match on a boolean",
        source +
          "\npub fn bool_match(flag: bool) -> u8 { match flag { true => 1, false => 2 } }\n",
        /#match_bool/,
      ],
      [
        "large enum variant",
        source + "\npub enum Large { Small(u8), Big([u8; 512]) }\n",
        /#large_enum_variant/,
      ],
      [
        "small Copy value passed by reference",
        source +
          "\nfn by_ref(value: &u8) -> u8 { *value }\npub fn call_by_ref() -> u8 { by_ref(&1) }\n",
        /#trivially_copy_pass_by_ref/,
      ],
      [
        "qualified JSON value in typed owner",
        source.replace(
          "pub fn typed_owner() {}",
          "pub fn typed_owner() { let _: Option<serde_json::Value> = None; }",
        ),
        /disallowed type `serde_json::Value`/,
      ],
      [
        "aliased JSON value in typed owner",
        source.replace(
          "pub fn typed_owner() {}",
          "pub fn typed_owner() { let _: Option<Json> = None; }",
        ),
        /disallowed type `serde_json::Value`/,
      ],
      [
        "wildcard enum arm",
        source.replace("State::Ready | State::Running", "_"),
        /#wildcard_enum_match_arm/,
      ],
      [
        "single remaining variant",
        source.replace(
          "State::Settled => true, State::Ready | State::Running => false",
          "State::Ready | State::Running => false, _ => true",
        ),
        /#match_wildcard_for_single_variants/,
      ],
      [
        "redundant struct rest",
        source.replace(
          "Receipt { request_id } =",
          "Receipt { request_id, .. } =",
        ),
        /#rest_pat_in_fully_bound_structs/,
      ],
      [
        "new enum variant",
        source.replace(
          "Ready, Running, Settled",
          "Ready, Running, Settled, Uncertain",
        ),
        /E0004/,
      ],
      [
        "new struct field",
        source.replace(
          "pub request_id: u64",
          "pub request_id: u64, pub sequence: u64",
        ),
        /E0027/,
      ],
    ] as const) {
      const result = check(text);
      assert.notEqual(
        result.status,
        0,
        `${name} must fail under root Cargo.toml policy`,
      );
      assert.match(result.stderr, diagnostic, name);
    }
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("closed provider parsing opts into the Value restriction", () => {
  assert.match(read("clippy.toml"), /disallowed-types\s*=.*serde_json::Value/);
  assert.equal(
    assignments(section(read("Cargo.toml"), "workspace.lints.clippy")).get(
      "disallowed_types",
    ),
    '"allow"',
  );
  assert.match(
    read("workspaces/runtime/packages/runtime-schema/src/provider_kind.rs"),
    /#\[deny\(clippy::disallowed_types\)\]\s*impl FromStr for ProviderKind/,
  );
});
