// Mermaid and HTML previews under the document policy of the native host.
// No server state, provider or live model request.
import assert from "node:assert/strict";
import { randomInt } from "node:crypto";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test, createFixtureViteServer } from "../../tests/playwright.mjs";

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
  const policy = security.csp
    .split(";")
    .map((part) => part.trim())
    .filter(Boolean)
    .map((part) =>
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
const diagram = `flowchart LR
  subgraph N["Plain mode (account: false)"]
    direction LR
    U1["User account in the shared hub<br/>Simple cluster"] -->|"Cluster operator<br/>applies any manifest it receives"| P1["Provider cluster<br/>namespace taken from the kubeconfig file<br/>any resource type"]
    P1 --> H1["Hosting provider<br/>creates the cluster"]
  end

  subgraph A["Account mode (account: true) - used on staging"]
    direction LR
    U2["User account in the shared hub<br/>Simple cluster<br/>+ resource next to it"] -->|"sync-agent"| P2["Provider cluster<br/>copy of the resource"]
    P2 --> H2["Hosting provider<br/>creates the cluster"]
    C["Code check:<br/>only one resource kind<br/>from one API group"] -.-> U2
  end`;
const html =
  '<style>.card{background:rgb(12, 85, 60);color:rgb(255, 255, 255)}</style><div class="card">Styled HTML preview</div>';
const labels = (pattern) =>
  [...diagram.matchAll(pattern)].map((match) => match[1].split("<br/>"));
const nodeLabels = labels(/(?<!subgraph )\b\w+\["([^"]+)"\]/g);
const clusterLabels = labels(/subgraph \w+\["([^"]+)"\]/g).flat();
const edgeLabels = labels(/\|"([^"]+)"\|/g);

const channel = (value) => {
  const c = value / 255;
  return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
};
const luminance = (rgb) => {
  const [r, g, b] = rgb
    .match(/\d+(\.\d+)?/g)
    .slice(0, 3)
    .map(Number);
  return 0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b);
};
const contrast = (a, b) => {
  const [high, low] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (high + 0.05) / (low + 0.05);
};

test("Mermaid and HTML previews stay readable under the native host policy", async ({
  browser,
}) => {
  test.setTimeout(120_000);
  const { policy, nonce } = await nativeDocumentPolicy();
  const entry = `
    import { createElement as e } from "react";
    import { createRoot } from "react-dom/client";
    import { MantineProvider } from "@mantine/core";
    import "/src/browser-reset.css";
    import "@mantine/core/styles.css";
    import "/src/style.css";
    import RichPreview from "/src/components/RichPreview.tsx";
    const scheme = new URLSearchParams(location.search).get("scheme");
    createRoot(document.getElementById("root")).render(
      e(MantineProvider, { forceColorScheme: scheme },
        e("main", { style: { width: 1200 } },
          e(RichPreview, { kind: "mermaid", source: ${JSON.stringify(diagram)} }),
          e(RichPreview, { kind: "html", source: ${JSON.stringify(html)} }))));
  `;
  const server = await createFixtureViteServer({
    configFile: false,
    root: webRoot,
    logLevel: "error",
    server: { host: "127.0.0.1", port: 0, hmr: false, ws: false },
    optimizeDeps: { include: ["mermaid"] },
    plugins: [
      {
        name: "rich-preview-policy-check",
        resolveId(id) {
          if (id === "/rich-preview-policy-entry")
            return "\0rich-preview-policy-entry";
        },
        load(id) {
          if (id === "\0rich-preview-policy-entry") return entry;
        },
        configureServer(server) {
          server.middlewares.use((request, response, next) => {
            if (!request.url.startsWith("/rich-preview-policy-check"))
              return next();
            response.setHeader("Content-Type", "text/html");
            response.setHeader("Content-Security-Policy", policy);
            // Vite gives its development style elements this nonce. The
            // packaged renderer links the same CSS as 'self' stylesheets.
            response.end(`<!doctype html><meta charset="utf-8">
              <meta property="csp-nonce" nonce="${nonce}">
              <title>Rich preview policy check</title>
              <div id="root"></div>
              <script type="module" src="/rich-preview-policy-entry"></script>`);
          });
        },
      },
    ],
  });
  try {
    await server.listen();
    const origin = server.resolvedUrls.local[0].replace(/\/$/, "");
    for (const scheme of ["light", "dark"]) {
      const page = await browser.newPage({
        viewport: { width: 1280, height: 1000 },
      });
      const errors = [];
      const styleViolations = [];
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
          const color = (element) => getComputedStyle(element).color;
          const fill = (element) => getComputedStyle(element).fill;
          const lines = (element) =>
            element.innerText
              .split("\n")
              .map((line) => line.trim())
              .filter(Boolean);
          // A label fits when its text box stays inside its foreignObject.
          const fits = (label) => {
            const box = label.closest("foreignObject").getBoundingClientRect();
            const text = label.getBoundingClientRect();
            return text.width <= box.width + 1 && text.height <= box.height + 1;
          };
          const shape = (group, selector) => {
            const label = group.querySelector(
              `${selector} foreignObject > div`,
            );
            return {
              fill: fill(group.querySelector("rect, polygon, path")),
              color: color(label.querySelector(".nodeLabel, .edgeLabel")),
              lines: lines(label),
              fits: fits(label),
            };
          };
          const view = svg.ownerDocument.defaultView;
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
