import { layoutQueries } from "./layout/layout-contract.generated";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { beforeAll, expect, test } from "vitest";
import { preprocessCSS, resolveConfig } from "vite";

const styles = new Map();

beforeAll(async () => {
  // Use Vite's CSS parser, including the media rule that owns each declaration.
  for (const file of [
    "studio-theme.css",
    "style.css",
    "studio-preferences.css",
    "components/message-queue.css",
    "components/accounts.css",
    "components/agents/subagent-concurrency-control.css",
    "components/questions/sign-in-card.css",
  ]) {
    const declarations = [];
    const config = await resolveConfig(
      {
        configFile: false,
        css: {
          postcss: {
            plugins: [
              {
                postcssPlugin: "touch-input-contract",
                Once(root) {
                  root.walkDecls((declaration) => {
                    const rule = declaration.parent;
                    declarations.push({
                      property: declaration.prop,
                      value: declaration.value,
                      important: Boolean(declaration.important),
                      selectors: rule.selectors ?? [],
                      media:
                        rule.parent.type === "atrule"
                          ? rule.parent.params
                          : null,
                    });
                  });
                },
              },
            ],
          },
        },
      },
      "serve",
    );
    await preprocessCSS(
      await readFile(new URL(file, import.meta.url), "utf8"),
      fileURLToPath(new URL(file, import.meta.url)),
      config,
    );
    styles.set(file, declarations);
  }
});

function touchFontRules(file) {
  return styles
    .get(file)
    .filter(
      ({ property, media }) =>
        property === "font-size" &&
        media
          ?.split(",")
          .some((condition) => condition.trim() === "(any-pointer: coarse)"),
    );
}

test("the touch input font floor has one owner in CSS pixels", () => {
  expect(
    styles
      .get("studio-theme.css")
      .filter(
        ({ property }) => property === "--studio-touch-input-min-font-size",
      ),
  ).toMatchObject([{ value: "16px", selectors: [":root"] }]);
});

test("wide touch screens protect search, composer, and form controls", () => {
  const rules = touchFontRules("style.css");
  const selectors = rules.flatMap((rule) => rule.selectors);
  expect(selectors).toEqual(
    expect.arrayContaining([
      "#chat-search",
      "#composer textarea",
      ":root input",
      ":root textarea",
      ":root select",
    ]),
  );
  for (const rule of rules) {
    expect(rule.value).toContain("var(--studio-touch-input-min-font-size)");
    expect(rule.important).toBe(true);
    // Preserve the existing narrow-screen protection for mouse users.
    expect(rule.media).toContain(layoutQueries.navigationCompact);
  }
});

test("wide touch screens preserve larger composer and sidebar preferences", () => {
  const rules = touchFontRules("studio-preferences.css");
  for (const preference of [
    "--studio-main-font-size",
    "--studio-sidebar-font-size",
  ]) {
    const rule = rules.find(({ value }) =>
      value.includes(`var(${preference})`),
    );
    expect(rule, preference).toBeDefined();
    expect(rule.important).toBe(true);
    expect(rule.value.replace(/\s+/g, "")).toContain(
      "max(var(--studio-touch-input-min-font-size),",
    );
  }
});

test("component input font floors consume the shared owner", () => {
  for (const [file, selector] of [
    ["components/message-queue.css", ".message-queue-editor textarea"],
    ["components/accounts.css", ".account-name-form input"],
    [
      "components/agents/subagent-concurrency-control.css",
      ".agent-mode-limit-label input",
    ],
    ["components/questions/sign-in-card.css", ".sign-in-card-paste input"],
  ]) {
    const rules = styles
      .get(file)
      .filter(
        ({ property, selectors }) =>
          property === "font-size" && selectors.includes(selector),
      );
    expect(rules, file).toHaveLength(1);
    expect(rules[0].value).toContain("var(--studio-touch-input-min-font-size)");
  }
});

test("the viewport permits user zoom", async () => {
  const html = await readFile(
    new URL("../index.html", import.meta.url),
    "utf8",
  );
  const viewport = html.match(/name="viewport"\s+content="([^"]+)"/)[1];
  expect(viewport).not.toMatch(/maximum-scale|user-scalable\s*=\s*no/);
});
