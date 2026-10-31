import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { expect, spawnFixture, test, rustUiFixture } from "../playwright.ts";

test("stream session supplies the owner credential and workspace identity", async ({
  page,
}) => {
  const root = await mkdtemp(join(tmpdir(), "studio-bootstrap-"));
  const fixture = spawnFixture(
    process.execPath,
    [rustUiFixture, root, "--workers", "0"],
    { stdio: ["ignore", "pipe", "pipe"] },
  );
  let errors = "";
  fixture.stderr!.on("data", (chunk) => {
    errors += chunk;
  });
  try {
    const port = await new Promise<number>((resolve, reject) => {
      fixture.stdout!.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(new Error(errors)));
    });
    await page.goto(`http://127.0.0.1:${port}`);
    const matches = await page.evaluate(async () => {
      const [session, identity] = await Promise.all([
        fetch("/api/session").then((response) => response.json()),
        fetch("/api/sync/identity").then((response) => response.json()),
      ]);
      const controller = new AbortController();
      const response = await fetch(
        "/api/sync/stream?protocol=4&resources=%5B%5D&tokenRates=false",
        { signal: controller.signal },
      );
      const reader = response.body!.getReader();
      const frame = new TextDecoder().decode((await reader.read()).value);
      controller.abort();
      const data = JSON.parse(
        frame
          .split("\n")
          .find((line) => line.startsWith("data: "))!
          .slice(6),
      );
      return (
        data.token === session.token &&
        data.workspaceId === identity.workspaceId
      );
    });
    expect(matches).toBe(true);
  } finally {
    fixture.kill();
  }
});
