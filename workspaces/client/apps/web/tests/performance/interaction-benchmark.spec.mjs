import { test, expect, spawnFixture } from "../playwright.mjs";
import { mkdtemp, writeFile, readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

test(
  "built client interaction measurements",
  { tag: "@performance" },
  async ({ browser }) => {
    test.setTimeout(300000);
    const baseline = process.env.STUDIO_BENCHMARK_BASELINE;
    const output = process.env.STUDIO_BENCHMARK_OUTPUT;
    test.skip(
      !output || (!baseline && !process.env.STUDIO_BENCHMARK_VARIANTS),
      "Use just benchmark-client-interactions to supply the explicit baseline and output.",
    );
    const script = fileURLToPath(
      new URL("./interaction-fixture.mjs", import.meta.url),
    );
    const current = fileURLToPath(new URL("../../dist", import.meta.url));
    const results = [];
    const variants = process.env.STUDIO_BENCHMARK_VARIANTS
      ? JSON.parse(
          await readFile(process.env.STUDIO_BENCHMARK_VARIANTS, "utf8"),
        )
      : [
          { label: "baseline", dist: baseline },
          { label: "changed", dist: current },
        ];
    // Rotate order between fresh-context samples to reduce warm-up bias.
    const samples = Number(process.env.STUDIO_BENCHMARK_SAMPLES || 3);
    if (!Number.isInteger(samples) || samples < 1 || samples > 9)
      throw new Error("Benchmark samples must be an integer between 1 and 9.");
    const runs = Array.from({ length: samples }, (_, sample) =>
      variants.map((_, index) => ({
        ...variants[(index + sample) % variants.length],
        sample,
      })),
    ).flat();
    for (const { label, dist, sample } of runs) {
      const scratch = await mkdtemp(join(tmpdir(), "studio-interactions-"));
      const child = spawnFixture(
        process.execPath,
        [script, scratch, "0", dist],
        { stdio: ["ignore", "pipe", "pipe"] },
      );
      let log = "";
      const exited = new Promise((resolve) => child.once("exit", resolve));
      try {
        const origin = await new Promise((resolve, reject) => {
          let text = "";
          child.stdout.on("data", (chunk) => {
            text += chunk;
            const match = text.match(/http:\/\/127\.0\.0\.1:\d+/);
            if (match) resolve(match[0]);
          });
          child.stderr.on("data", (chunk) => (log += chunk));
          child.once("exit", (code) =>
            reject(new Error(`Fixture exited ${code}: ${log}`)),
          );
        });
        const context = await browser.newContext({
          viewport: { width: 1200, height: 800 },
          serviceWorkers: "block",
        });
        try {
          const page = await context.newPage();
          const errors = [];
          page.on("pageerror", (error) => errors.push(error.message));
          await page.goto(origin + "/?benchmark=" + label);
          const result = page.locator("#benchmark-result");
          await result.waitFor({ state: "attached", timeout: 25000 });
          const value = JSON.parse(await result.textContent());
          await writeFile(
            output + ".latest.json",
            JSON.stringify(value, null, 2),
          );
          expect(value.passed, value.stack || value.error).toBe(true);
          expect(errors).toEqual([]);
          results.push({ label, sample, ...value });
          await writeFile(output, JSON.stringify(results, null, 2));
        } finally {
          await context.close();
        }
      } finally {
        child.kill("SIGTERM");
        await exited;
      }
    }
  },
);
