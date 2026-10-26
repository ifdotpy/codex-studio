import { readFileSync } from "node:fs";
import { assert, expect, test } from "vitest";
import { theme } from "../theme";

const read = (path: string) =>
  readFileSync(new URL(path, import.meta.url), "utf8");
const stateSource = read("./state.css");
const surfaceSource = read("../studio-theme.css");
const kindDeclaration = read(
  "../components/chat-status/chatStatusModel.ts",
).match(/export type ChatIndicatorKind =([\s\S]*?);/)![1];
assert(kindDeclaration !== undefined);
const kinds = [...kindDeclaration.matchAll(/"(\w+)"/g)].map(
  (match) => match[1],
);

function variables(scheme: "light" | "dark") {
  const values: Record<string, string> = {};
  for (const [palette, shades] of Object.entries(theme.colors ?? {})) {
    if (!shades) continue;
    shades.forEach((shade, index) => {
      values[`--mantine-color-${palette}-${index}`] = shade;
    });
  }
  for (const source of [surfaceSource, stateSource]) {
    for (const [, selector, body] of source.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
      assert(selector !== undefined && body !== undefined);
      if (
        !selector.includes(":root") &&
        !selector.includes(`[data-mantine-color-scheme="${scheme}"]`) &&
        !selector.trim().endsWith("[data-mantine-color-scheme]")
      )
        continue;
      for (const [, name, value] of body.matchAll(/(--[\w-]+):\s*([^;]+);/g)) {
        assert(name !== undefined && value !== undefined);
        values[name] = value.trim();
      }
    }
  }
  const resolve = (name: string): string => {
    expect(values[name], `${name} must have a named token owner`).toBeDefined();
    const value = values[name];
    assert(value !== undefined);
    return value.replace(/var\((--[\w-]+)\)/g, (_, token) => resolve(token));
  };
  return resolve;
}
const channels = (hex: string) =>
  [1, 3, 5].map((offset) => parseInt(hex.slice(offset, offset + 2), 16));
function luminance(rgb: number[]) {
  return rgb
    .map((value) => value / 255)
    .map((value) =>
      value <= 0.04045 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4,
    )
    .reduce((sum, value, index) => {
      const weight = [0.2126, 0.7152, 0.0722][index];
      if (weight === undefined)
        throw new Error("Expected three color channels.");
      return sum + value * weight;
    }, 0);
}
function contrast(foreground: number[], background: number[]) {
  const [low, high] = [luminance(foreground), luminance(background)].sort(
    (a, b) => a - b,
  );
  assert(high !== undefined && low !== undefined);
  return (high + 0.05) / (low + 0.05);
}

test("every ChatIndicatorKind has a rail and soft-color contract", () => {
  expect(kinds).toHaveLength(6);
  for (const kind of kinds) {
    expect(stateSource.includes(`--studio-state-${kind}:`), kind).toBe(true);
    expect(stateSource.includes(`--studio-state-${kind}-soft:`), kind).toBe(
      true,
    );
    expect(stateSource.includes(`[data-studio-state="${kind}"]`), kind).toBe(
      true,
    );
  }
});

for (const scheme of ["light", "dark"] as const) {
  test(`${scheme} text and state colors meet AA on both page and panel`, () => {
    const resolve = variables(scheme);
    for (const surface of ["--studio-page", "--surface"]) {
      const background = channels(resolve(surface));
      for (const name of [
        "--text",
        "--muted",
        ...kinds.map((kind) => `--studio-state-${kind}`),
      ]) {
        expect(
          contrast(channels(resolve(name)), background),
          `${name} on ${surface}`,
        ).toBeGreaterThanOrEqual(4.5);
      }
    }
  });
  test(`${scheme} state labels remain AA on their soft backgrounds`, () => {
    const resolve = variables(scheme);
    for (const surface of ["--studio-page", "--surface"]) {
      const background = channels(resolve(surface));
      for (const kind of kinds) {
        const foreground = channels(resolve(`--studio-state-${kind}`));
        const soft = resolve(`--studio-state-${kind}-soft`);
        const alpha =
          soft === "transparent"
            ? 0
            : Number(soft.match(/([\d.]+)%/)![1]) / 100;
        const tinted = background.map((channel, index) => {
          const frontChannel = foreground[index];
          assert(frontChannel !== undefined);
          return channel * (1 - alpha) + frontChannel * alpha;
        });
        expect(
          contrast(foreground, tinted),
          `${kind} on ${surface} with soft tint`,
        ).toBeGreaterThanOrEqual(4.5);
      }
    }
  });
}
