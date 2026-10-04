import assert from "node:assert/strict";
import test from "node:test";
import { renderStreamValidators } from "./stream-validators.mjs";

function document() {
  // These are compiler fixtures, not a second description of the wire protocol.
  return {
    components: {
      schemas: {
        ResourceRef: { $ref: "#/components/schemas/FixtureLabel" },
        FixtureLabel: {
          type: "object",
          required: ["label"],
          additionalProperties: false,
          properties: { label: { type: "string", minLength: 2 } },
        },
        ResourceChangeEvent: {
          type: "array",
          items: { $ref: "#/components/schemas/ResourceRef" },
        },
        ResourceHeartbeatEvent: { type: "integer", minimum: 0 },
        ResourceTokenRatesEvent: { type: "boolean" },
      },
    },
  };
}

test("standalone guards follow references without coercing or changing input", async () => {
  const output = await renderStreamValidators(document());
  const module = await import(
    `data:text/javascript;base64,${Buffer.from(output["stream-validators.js"]).toString("base64")}`
  );
  assert.equal(module.isResourceRef({ label: "ab" }), true);
  assert.equal(module.isResourceRef({ label: "a" }), false);
  assert.equal(module.isResourceRef({ label: 12 }), false);
  assert.equal(module.isResourceRef({ label: "ab", extra: true }), false);
  assert.equal(module.isResourceChangeEvent([{ label: "ab" }]), true);
  assert.equal(module.isResourceChangeEvent([{ label: false }]), false);
  assert.equal(module.isResourceHeartbeatEvent(1), true);
  assert.equal(module.isResourceHeartbeatEvent("1"), false);
  assert.equal(module.isResourceHeartbeatEvent(-1), false);
  assert.equal(module.isResourceTokenRatesEvent(true), true);
  assert.equal(module.isResourceTokenRatesEvent("true"), false);
  const input = Object.freeze({ label: "a", extra: true });
  assert.equal(module.isResourceRef(input), false);
  assert.deepEqual(input, { label: "a", extra: true });
  assert.match(
    output["stream-validators.d.ts"],
    /import type \{ components \} from "\.\/api"/,
  );
  assert.doesNotMatch(output["stream-validators.d.ts"], /label/);
  assert.doesNotMatch(output["stream-validators.js"], /new Function\(/);
});

test("missing authoritative schema fails generation instead of weakening validation", async () => {
  const source = document();
  delete source.components.schemas.ResourceRef;
  await assert.rejects(
    renderStreamValidators(source),
    /Missing stream schema: ResourceRef/,
  );
});

test("schema changes change validation and output is reproducible", async () => {
  const original = document();
  assert.deepEqual(
    await renderStreamValidators(original),
    await renderStreamValidators(original),
  );
  original.components.schemas.FixtureLabel.properties.label.minLength = 3;
  const output = await renderStreamValidators(original);
  const module = await import(
    `data:text/javascript;base64,${Buffer.from(output["stream-validators.js"]).toString("base64")}`
  );
  assert.equal(module.isResourceRef({ label: "ab" }), false);
  assert.equal(module.isResourceRef({ label: "abc" }), true);
});
