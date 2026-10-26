import { fixtureOrigin } from "../../tests/misc-fixture.ts";
// Mermaid and HTML previews under the document policy of the native host.
// No server state, provider or live model request.
import assert from "node:assert/strict";
import { randomInt } from "node:crypto";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test, createFixtureViteServer } from "../../tests/playwright.ts";

const webRoot = fileURLToPath(new URL("../..", import.meta.url));

// The native host serves app.security.csp of tauri.conf.json. Unless style-src
// is excluded from dangerousDisableAssetCspModification, Tauri marks each
// inline <style> of index.html with a nonce and appends 'nonce-N' to style-src
// (tauri-codegen inject_nonce_token, tauri manager replace_csp_nonce). The
// tooling check native-style-policy.test.mjs guards that configuration.
async function nativeDocumentPolicy() {
  const { security } = JSON.parse(
    await readFile(
      new URL("../../../native/src-tauri/tauri.conf.json", import.meta.url),
      "utf8",
    ),
  ).app;
  const index = await readFile(join(webRoot, "index.html"), "utf8");
  const disabled = security.dangerousDisableAssetCspModification ?? false;
  const styleNonce =
    (Array.isArray(disabled) ? !disabled.includes("style-src") : !disabled) &&
    /<style[\s>]/i.test(index);
  const nonce = String(randomInt(1, 2 ** 47));
  if (typeof security.csp !== "string") throw new Error("Missing native CSP");
  const policy = security.csp
    .split(";")
    .map((part: string) => part.trim())
    .filter(Boolean)
    .map((part: string) =>
      styleNonce && part.startsWith("style-src ")
        ? `${part} 'nonce-${nonce}'`
        : part,
    )
    .join("; ");
  return { policy, nonce };
}

// The shape of a chat diagram that rendered as black boxes: two subgraphs
// with punctuated titles, quoted labels and edge labels with <br/>, wrapped
// lines and a dotted edge.
import { diagram } from "./RichPreview.browser.data.ts";
const labels = (pattern: RegExp) =>
  [...diagram.matchAll(pattern)].map((match) =>
    (match[1] ?? "").split("<br/>"),
  );
const nodeLabels = labels(/(?<!subgraph )\b\w+\["([^"]+)"\]/g);
const clusterLabels = labels(/subgraph \w+\["([^"]+)"\]/g).flat();
const edgeLabels = labels(/\|"([^"]+)"\|/g);

const channel = (value: number) => {
  const c = value / 255;
  return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
};
const luminance = (rgb: string) => {
  const [r = 0, g = 0, b = 0] = (rgb.match(/\d+(\.\d+)?/g) ?? [])
    .slice(0, 3)
    .map(Number);
  return 0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b);
};
const contrast = (a: string, b: string) => {
  const [high = 0, low = 0] = [luminance(a), luminance(b)].sort(
    (x, y) => y - x,
  );
  return (high + 0.05) / (low + 0.05);
};

test("Mermaid and HTML previews stay readable under the native host policy", async ({
  browser,
}) => {
  test.setTimeout(120_000);
  const { policy, nonce } = await nativeDocumentPolicy();
  const server = await createFixtureViteServer({
    configFile: false,
    root: webRoot,
    logLevel: "error",
    server: { host: "127.0.0.1", port: 0, hmr: false, ws: false },
    optimizeDeps: { include: ["mermaid"] },
    plugins: [
      {
        name: "rich-preview-policy-check",
        configureServer(server) {
          server.middlewares.use((request, response, next) => {
            if (!request.url?.startsWith("/rich-preview-policy-check"))
              return next();
            response.setHeader("Content-Type", "text/html");
            response.setHeader("Content-Security-Policy", policy);
            // Vite gives its development style elements this nonce. The
            // packaged renderer links the same CSS as 'self' stylesheets.
            response.end(`<!doctype html><meta charset="utf-8">
              <meta property="csp-nonce" nonce="${nonce}">
              <title>Rich preview policy check</title>
              <div id="root"></div>
              <script type="module" src="/src/components/RichPreview.browser.fixture.tsx"></script>`);
          });
        },
      },
    ],
  });
  try {
    await server.listen();
    const origin = fixtureOrigin(server);
    for (const scheme of ["light", "dark"] as const) {
      const page = await browser.newPage({
        viewport: { width: 1280, height: 1000 },
      });
      const errors: string[] = [];
      const styleViolations: string[] = [];
      page.on("pageerror", (error) => errors.push(error.message));
      page.on("console", (message) => {
        if (/Content Security Policy.*style-src/s.test(message.text()))
          styleViolations.push(message.text());
      });
      try {
        await page.goto(`${origin}/rich-preview-policy-check?scheme=${scheme}`);
        const card = page.locator('[data-preview-kind="mermaid"]');
        const frame = card.frameLocator("iframe");
        await frame.locator("svg .node").first().waitFor({ state: "attached" });
        const report = await frame.locator("svg").evaluate((svg) => {
          const color = (element: Element | null) => {
            if (!element) throw new Error("Missing preview color element");
            return getComputedStyle(element).color;
          };
          const fill = (element: Element | null) => {
            if (!element) throw new Error("Missing preview fill element");
            return getComputedStyle(element).fill;
          };
          const lines = (element: Element) =>
            (element instanceof HTMLElement
              ? element.innerText
              : (element.textContent ?? "")
            )
              .split("\n")
              .map((line) => line.trim())
              .filter(Boolean);
          // A label fits when its text box stays inside its foreignObject.
          const fits = (label: Element) => {
            const parent = label.closest("foreignObject");
            if (!parent) throw new Error("Missing preview foreign object");
            const box = parent.getBoundingClientRect();
            const text = label.getBoundingClientRect();
            return text.width <= box.width + 1 && text.height <= box.height + 1;
          };
          const shape = (group: Element, selector: string) => {
            const label = group.querySelector(
              `${selector} foreignObject > div`,
            );
            if (!label) throw new Error("Missing preview label");
            return {
              fill: fill(group.querySelector("rect, polygon, path")),
              color: color(label.querySelector(".nodeLabel, .edgeLabel")),
              lines: lines(label),
              fits: fits(label),
            };
          };
          const view = svg.ownerDocument.defaultView;
          if (!view) throw new Error("Missing preview window");
          return {
            // The frame is tall enough to show the whole diagram.
            overflow:
              svg.ownerDocument.documentElement.scrollHeight - view.innerHeight,
            clusters: [...svg.querySelectorAll(".cluster")].map((group) =>
              shape(group, ".cluster-label"),
            ),
            nodes: [...svg.querySelectorAll(".node")].map((group) =>
              shape(group, ".label"),
            ),
            edges: [...svg.querySelectorAll("g.edgeLabel foreignObject > div")]
              .map(lines)
              .filter((label) => label.length),
          };
        });
        await card.screenshot({
          path: test.info().outputPath(`mermaid-${scheme}.png`),
        });
        assert.deepEqual(
          report.clusters.map((cluster) => cluster.lines.join(" ")).sort(),
          [...clusterLabels].sort(),
          `${scheme}: every subgraph title is rendered`,
        );
        assert.deepEqual(
          report.nodes.map((node) => node.lines).sort(),
          [...nodeLabels].sort(),
          `${scheme}: every node label line is rendered`,
        );
        assert.deepEqual(
          [...report.edges].sort(),
          [...edgeLabels].sort(),
          `${scheme}: every edge label line is rendered`,
        );
        for (const shape of [...report.clusters, ...report.nodes]) {
          assert.notEqual(
            shape.fill,
            "rgb(0, 0, 0)",
            `${scheme}: no black fill`,
          );
          assert.ok(
            contrast(shape.fill, shape.color) >= 4.5,
            `${scheme}: readable label ${shape.lines.join(" ")} (${shape.color} on ${shape.fill})`,
          );
          assert.ok(
            shape.fits,
            `${scheme}: ${shape.lines.join(" ")} fits its box`,
          );
        }
        assert.ok(
          report.overflow <= 1,
          `${scheme}: the frame shows the whole diagram (${report.overflow}px hidden)`,
        );
        const htmlFrame = page
          .locator('[data-preview-kind="html"]')
          .frameLocator("iframe");
        await htmlFrame.getByText("Styled HTML preview").waitFor();
        assert.equal(
          await htmlFrame
            .locator(".card")
            .evaluate((element) => getComputedStyle(element).backgroundColor),
          "rgb(12, 85, 60)",
          `${scheme}: HTML preview styles apply`,
        );
        assert.deepEqual(styleViolations, [], `${scheme}: no blocked styles`);
        assert.deepEqual(errors, [], `${scheme}: no page errors`);
      } finally {
        await page.close();
      }
    }
  } finally {
    await server.close();
  }
});
