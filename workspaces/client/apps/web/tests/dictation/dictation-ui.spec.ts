import { fixtureOrigin } from "../misc-fixture.ts";
import type {} from "./dictation-ui.fixture.tsx";
import { createFixtureViteServer, test } from "../playwright.ts";
// Isolated real React and IndexedDB. Audio capture and native speech are mocks.
import assert from "node:assert/strict";
import { resolve } from "node:path";
test("Dictation ui", async ({
  browser: testBrowser,
  page: runnerPage,
  context: runnerContext,
}, testInfo) => {
  test.setTimeout(180_000);
  const root = resolve(import.meta.dirname, "../../");
  const server = await createFixtureViteServer({
    configFile: false,
    root,
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "dictation-fixture",
        configureServer(server) {
          server.middlewares.use("/dictation-test", (_request, response) => {
            response.setHeader("Content-Type", "text/html");
            response.end(
              '<html><body><div id="root"></div><script type="module" src="/tests/dictation/dictation-ui.fixture.tsx"></script></body></html>',
            );
          });
        },
      },
    ],
  });
  await server.listen();
  try {
    const page = runnerPage;
    await page.setViewportSize({ width: 700, height: 850 });
    const errors: string[] = [];
    page.on("pageerror", (e) => {
      errors.push(e.message);
      console.error(e.message);
    });
    page.on("console", (message) => {
      if (message.type() === "error") console.error(message.text());
    });
    const origin = fixtureOrigin(server) + "/";
    await page.goto(origin + "dictation-test");
    await page.getByRole("button", { name: "Dictation", exact: true }).click();
    await page.getByRole("button", { name: "Record", exact: true }).click();
    await page.getByRole("button", { name: /Stop ·/ }).waitFor();
    await page.getByRole("button", { name: /Stop ·/ }).waitFor();
    await page.evaluate(() => window.dictationFixture.emitAudio());
    await page.getByRole("button", { name: /Stop ·/ }).click();
    await page
      .getByText("Recognition unavailable; recording preserved.")
      .waitFor();
    assert.equal(
      await page.getByRole("textbox", { name: "Draft" }).inputValue(),
      "Original draft",
    );
    await page.locator("audio").waitFor();
    assert.equal(await page.locator("audio").count(), 1);
    assert.equal(
      await page.evaluate(() => window.dictationFixture.audioBytes),
      32044,
    );
    await page.reload();
    await page.getByRole("button", { name: "Dictation", exact: true }).click();
    await page
      .getByText("Recognition unavailable; recording preserved.")
      .waitFor();
    await page.locator("audio").waitFor();
    assert.equal(await page.locator("audio").count(), 1);
    await page.evaluate(() => (window.dictationFixture.retryOK = true));
    await page.getByRole("button", { name: "Retry transcription" }).click();
    await page.waitForFunction(
      () =>
        document.querySelector<HTMLTextAreaElement>(
          'textarea[aria-label="Review dictated text"]',
        )?.value === "Recovered spoken instruction",
    );
    assert.equal(
      await page.getByRole("textbox", { name: "Draft" }).inputValue(),
      "Original draft",
    );
    await page.setViewportSize({ width: 320, height: 760 });
    await page.screenshot({
      path: testInfo.outputPath("studio-dictation-mobile.png"),
    });
    await page.setViewportSize({ width: 700, height: 850 });
    await page.getByRole("button", { name: "Insert into message" }).click();
    assert.equal(
      await page.getByRole("textbox", { name: "Draft" }).inputValue(),
      "Original draft\nRecovered spoken instruction",
    );
    await page.getByRole("button", { name: "Switch chat" }).click();
    await page.getByRole("button", { name: "Dictation", exact: true }).click();
    assert.equal(await page.locator("audio").count(), 0);
    await page.getByRole("button", { name: "Record", exact: true }).click();
    await page.getByRole("button", { name: /Stop ·/ }).waitFor();
    await page.getByRole("button", { name: /Stop ·/ }).waitFor();
    await page.evaluate(() => window.dictationFixture.emitAudio());
    await page.getByRole("button", { name: "Switch chat" }).click();
    await page.waitForFunction(
      () => window.dictationFixture.captureStopped >= 1,
    );
    await page.getByRole("button", { name: "Dictation", exact: true }).click();
    await page
      .getByRole("textbox", { name: "Review dictated text" })
      .fill("Corrected spoken instruction");
    await page.getByRole("button", { name: "Delete recording" }).click();
    await page.waitForFunction(
      () => document.querySelectorAll("audio").length === 0,
    );
    await page.getByRole("button", { name: "Undo delete" }).click();
    await page.waitForFunction(
      () => document.querySelectorAll("audio").length === 1,
    );
    assert.equal(
      await page
        .getByRole("textbox", { name: "Review dictated text" })
        .inputValue(),
      "Corrected spoken instruction",
      "Delete and Undo preserve reviewed text",
    );
    await page.getByRole("button", { name: "Delete recording" }).click();
    await page.waitForFunction(
      () => document.querySelectorAll("audio").length === 0,
    );
    // Reload during capture simulates a renderer loss. Periodic chunks survive.
    await page.getByRole("button", { name: "Record", exact: true }).click();
    await page.getByRole("button", { name: /Stop ·/ }).waitFor();
    await page.evaluate(() => window.dictationFixture.emitAudio());
    await page.waitForFunction(async () => {
      const { listRecordings } = await import("/src/dictation/storage.ts");
      return (await listRecordings("first")).some(
        (row) => row.samples === 16000,
      );
    });
    await page.reload();
    await page.getByRole("button", { name: "Dictation", exact: true }).click();
    await page.getByText(/Recovered$/).waitFor();
    await page.locator("audio").waitFor();
    assert.equal(await page.locator("audio").count(), 1);
    // The original component can finish after this recording was deleted elsewhere.
    await page.evaluate(
      () => (window.dictationFixture.deferTranscription = true),
    );
    await page.getByRole("button", { name: "Transcribe", exact: true }).click();
    await page.waitForFunction(
      () => !!window.dictationFixture.finishTranscription,
    );
    await page.getByRole("button", { name: "Switch chat" }).click();
    await page.getByRole("button", { name: "Switch chat" }).click();
    await page.getByRole("button", { name: "Dictation", exact: true }).click();
    await page.getByRole("button", { name: "Delete recording" }).click();
    await page.waitForFunction(
      () => document.querySelectorAll("audio").length === 0,
    );
    await page.evaluate(() =>
      window.dictationFixture.finishTranscription?.({
        text: "Late response",
        provider: "fixture",
        onDevice: true,
      }),
    );
    await page.waitForTimeout(100);
    assert.equal(
      await page.evaluate(async () => {
        const store = await import("/src/dictation/storage.ts");
        return (await store.listRecordings("first")).length;
      }),
      0,
    );
    // A newer attempt owns the result, even when the older attempt finishes last.
    assert.equal(
      await page.evaluate(async () => {
        const store = await import("/src/dictation/storage.ts");
        const row: import("../../src/dictation/storage.ts").Recording = {
          id: "attempt-race",
          chatId: "first",
          created: Date.now(),
          sampleRate: 16000,
          samples: 1,
          state: "ready",
        };
        await store.saveRecording(row);
        await store.beginTranscription(row, "old");
        await store.beginTranscription(row, "new");
        await store.updateRecording(
          { ...row, transcriptionAttempt: "new", transcript: "New result" },
          "new",
        );
        await store.updateRecording(
          { ...row, transcriptionAttempt: "old", transcript: "Old result" },
          "old",
        );
        const result = (await store.listRecordings("first"))[0]?.transcript;
        await store.deleteRecording(row.id);
        return result;
      }),
      "New result",
    );
    await page.getByRole("button", { name: "Record", exact: true }).click();
    await page.getByRole("button", { name: /Stop ·/ }).waitFor();
    await page.evaluate(() => {
      window.dictationFixture.emitAudio();
      window.dictationFixture.failStop = true;
    });
    await page.getByRole("button", { name: /Stop ·/ }).click();
    await page
      .getByText("Final chunk not confirmed. Earlier audio saved.", {
        exact: true,
      })
      .first()
      .waitFor();
    await page.locator("audio").waitFor();
    assert.equal(
      await page
        .getByRole("button", { name: "Record", exact: true })
        .isEnabled(),
      true,
    );
    assert.equal(await page.locator("audio").count(), 1);
    await page.reload();
    await page.getByRole("button", { name: "Dictation", exact: true }).click();
    await page
      .getByText("Final chunk not confirmed. Earlier audio saved.", {
        exact: true,
      })
      .waitFor();
    await page.locator("audio").waitFor();
    await page.evaluate(() => {
      window.dictationFixture.deferTranscription = true;
      window.dictationFixture.finishTranscription = null;
    });
    await page.getByRole("button", { name: "Retry transcription" }).click();
    await page.waitForFunction(
      () => !!window.dictationFixture.finishTranscription,
    );
    await page.evaluate(() =>
      window.dictationFixture.speechProgress?.({
        id: window.dictationFixture.attemptId,
        completed: 1,
        total: 3,
      }),
    );
    await page
      .getByText("Transcribing… 1 of 3 parts", { exact: true })
      .waitFor();
    await page.getByRole("button", { name: "Cancel transcription" }).click();
    await page
      .getByText("Transcription cancelled. Your recording is saved.", {
        exact: true,
      })
      .waitFor();
    await page.locator("audio").waitFor();
    assert.equal(await page.locator("audio").count(), 1);
    assert.deepEqual(errors, []);
    console.log(
      "PASS: recording chunks, WAV, failure, reload, retry, explicit draft insertion, chat isolation, unmount stop, deletion. No microphone or recognition calls.",
    );
  } finally {
    await Promise.all(
      testBrowser
        .contexts()
        .filter((ownedContext) => ownedContext !== runnerContext)
        .map((ownedContext) => ownedContext.close()),
    );
    await server.close();
  }
});
