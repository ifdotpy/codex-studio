// The built site behind the real gateway with a fake S3 that holds the writer's fixture
// (workspaces/runtime/apps/vault/test/fixture) for two vaults, in headless Chromium with a
// virtual authenticator that has PRF: the paper key unlocks, a passkey is added and then unlocks
// alone, the same passkey opens the second vault, files download and show in Quick Look with
// the writer's content, and a passkey is removed.
//
//   pnpm --filter studio-vault-web run build && node tests/site.e2e.mjs
import { chromium } from "playwright";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync, readdirSync, statSync } from "node:fs";
import http from "node:http";
import { join, relative } from "node:path";
import { Readable } from "node:stream";
import { makeGateway } from "../../../../runtime/apps/vault/gateway/server.mjs";
import { makeVault } from "../../../../runtime/apps/vault/src/service.mjs";
import {
  generateIdentity,
  recipientOf,
  identityFromPaper,
  sealSlot,
  wordsOfIdentity,
} from "../../../../runtime/apps/vault/src/keys.mjs";

const dist = new URL("../dist/", import.meta.url).pathname;
const fixture = new URL(
  "../../../../runtime/apps/vault/test/fixture/",
  import.meta.url,
).pathname;
const vaultIdentity = readFileSync(
  join(fixture, "identity.txt"),
  "utf8",
).trim();
const expected = JSON.parse(
  readFileSync(join(fixture, "expected.json"), "utf8"),
);
const paper = await generateIdentity();

// The same writer objects serve two vaults, "proj" and "other".
const objects = new Map();
const walk = (dir) =>
  readdirSync(dir).forEach((n) => {
    if (statSync(join(dir, n)).isDirectory()) return walk(join(dir, n));
    const key = relative(join(fixture, "objects"), join(dir, n));
    objects.set(key, readFileSync(join(dir, n)));
    objects.set(key.replace(/^proj\//, "other/"), readFileSync(join(dir, n)));
  });
walk(join(fixture, "objects"));
const paperSlot = Buffer.from(
  await sealSlot(vaultIdentity, await recipientOf(paper)),
);
objects.set("proj/slots/paper.age", paperSlot);
objects.set("other/slots/paper.age", paperSlot);

const s3 = {
  async send(command) {
    const input = command.input;
    switch (command.constructor.name) {
      case "ListObjectsV2Command":
        return {
          Contents: [...objects.keys()]
            .filter((k) => k.startsWith(input.Prefix))
            .map((Key) => ({ Key })),
        };
      case "GetObjectCommand": {
        const data = objects.get(input.Key);
        if (!data)
          throw Object.assign(new Error("missing"), { name: "NoSuchKey" });
        return {
          ContentLength: data.length,
          Body: Object.assign(Readable.from([data]), {
            transformToByteArray: async () => data,
          }),
        };
      }
      case "PutObjectCommand":
        if (input.IfNoneMatch === "*" && objects.has(input.Key))
          throw Object.assign(new Error("exists"), {
            name: "PreconditionFailed",
          });
        objects.set(input.Key, Buffer.from(input.Body));
        return {};
      case "DeleteObjectCommand":
        objects.delete(input.Key);
        return {};
    }
    throw new Error(command.constructor.name);
  },
};
const recipient = await recipientOf(vaultIdentity);
const secret = Buffer.alloc(32, 9);
const vaults = Object.fromEntries(
  ["proj", "other"].map((prefix) => [
    prefix,
    makeVault({ s3, bucket: "b", prefix, recipient, secret }),
  ]),
);
const server = http.createServer(
  makeGateway({
    config: () => ({ projects: { proj: {}, other: {} } }),
    vaultFor: (name) => vaults[name] ?? null,
    site: dist,
  }),
);
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const port = server.address().port;
const sha = (data) => createHash("sha256").update(data).digest("hex");

const browser = await chromium.launch();
try {
  const context = await browser.newContext({ acceptDownloads: true });
  const page = await context.newPage();
  const cdp = await context.newCDPSession(page);
  await cdp.send("WebAuthn.enable");
  await cdp.send("WebAuthn.addVirtualAuthenticator", {
    options: {
      protocol: "ctap2",
      transport: "internal",
      hasResidentKey: true,
      hasUserVerification: true,
      isUserVerified: true,
      hasPrf: true,
      automaticPresenceSimulation: true,
    },
  });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  const site = `http://localhost:${port}`;

  // Two vaults: the root page asks for one.
  await page.goto(`${site}/`);
  await page.waitForSelector("#empty-text:has-text('Choose a vault')");
  await page.click("#vaults a:has-text('proj')");
  await page.waitForSelector("#unlock:not([hidden])");
  assert.equal(
    await page.isHidden("#with-passkey"),
    true,
    "no passkey slot yet",
  );

  // The paper key is 24 words. Two words in the wrong order fail the checksum, and the page
  // says so; numbered words, some cut to their first four letters, unlock the latest state.
  const words = wordsOfIdentity(paper).split(" ");
  const swapped = words
    .map((_, i) => {
      const copy = [...words];
      [copy[i], copy[i + 1]] = [copy[i + 1], copy[i]];
      return copy.join(" ");
    })
    .find((text) => {
      try {
        identityFromPaper(text);
        return false;
      } catch {
        return true;
      }
    });
  await page.fill("#paper", swapped);
  await page.click("#paper-form button");
  await page.waitForFunction(() =>
    document.querySelector("#status").textContent.includes("checksum"),
  );
  await page.fill(
    "#paper",
    words
      .map((w, i) => `${i + 1}. ${i % 2 ? w.slice(0, 4).toUpperCase() : w}`)
      .join("\n"),
  );
  await page.click("#paper-form button");
  await page.waitForSelector("#columns:not([hidden])");
  assert.equal(await page.inputValue("#state"), "4");

  // Column view: a folder opens the next column, a file shows its preview with Download.
  await page.click("#columns .item:has-text('src')");
  await page.click("#columns .item:has-text('app.py')");
  await page.waitForSelector(".column.preview h3:has-text('app.py')");
  const download = page.waitForEvent("download");
  await page.click(".column.preview button:has-text('Download')");
  assert.equal(
    sha(readFileSync(await (await download).path())),
    expected.states[3]["src/app.py"][0],
  );
  assert.equal(await page.textContent("#crumbs"), "projsrcapp.py");

  // Quick Look shows the text; Escape closes it.
  await page.keyboard.press(" ");
  await page.waitForSelector("#ql-body pre");
  assert.equal(
    sha(Buffer.from(await page.textContent("#ql-body pre"))),
    expected.states[3]["src/app.py"][0],
  );
  await page.keyboard.press("Escape");
  await page.waitForSelector("#quicklook-panel", { state: "hidden" });

  // List view and an earlier state.
  await page.click("#view-list");
  await page.waitForSelector("#list:not([hidden])");
  await page.selectOption("#state", "2");
  await page.click("#crumbs button:first-child");
  await page.waitForSelector("#list-rows .item:has-text('empty.txt')");

  // Search finds a file in any folder.
  await page.fill("#filter", "tool");
  await page.waitForSelector("#list-rows .item:has-text('bin/tool.bin')");
  await page.fill("#filter", "");

  // A passkey on this device, then a fresh tab that opens with the passkey alone.
  await page.click("#passkeys");
  await page.fill("#label", "test device");
  await page.click("#add-form button");
  await page.waitForSelector("#passkey-list li button");
  assert.ok(
    [...objects.keys()].some((k) => k.startsWith("proj/slots/passkey-")),
  );
  await page.click("#close-passkeys");
  await page.reload();
  await page.waitForSelector("#with-passkey:not([hidden])");
  await page.click("#with-passkey");
  await page.waitForSelector("#list:not([hidden])");
  assert.equal(await page.inputValue("#state"), "4");

  // The same passkey opens the second vault after one paper unlock there.
  await page.click("#vaults a:has-text('other')");
  await page.waitForSelector("#unlock:not([hidden])");
  assert.equal(await page.isHidden("#with-passkey"), true);
  await page.fill("#paper", words.join(" "));
  await page.click("#paper-form button");
  await page.waitForSelector("#list:not([hidden])");
  await page.click("#passkeys");
  await page.click("#use-existing");
  await page.waitForSelector("#passkey-list li button");
  const ids = (prefix) =>
    [...objects.keys()]
      .filter((k) => k.startsWith(`${prefix}/slots/passkey-`))
      .map((k) => k.slice(prefix.length + 7));
  assert.deepEqual(ids("other"), ids("proj"));
  // One credential, two vaults: the slots differ, because the PRF salt is per vault.
  assert.notDeepEqual(
    objects.get(`other/slots/${ids("other")[0]}`),
    objects.get(`proj/slots/${ids("proj")[0]}`),
  );
  await page.click("#close-passkeys");
  await page.reload();
  await page.click("#with-passkey");
  await page.waitForSelector("#list:not([hidden])");

  // Removing the passkey removes its slot; the paper slot stays.
  await page.click("#passkeys");
  await page.click("#passkey-list li button");
  await page.waitForFunction(
    () => !document.querySelector("#passkey-list li button"),
  );
  assert.equal(
    [...objects.keys()].filter((k) => k.startsWith("other/slots/")).join(),
    "other/slots/paper.age",
  );
  assert.deepEqual(errors, []);
  console.log("site e2e: passed");
} finally {
  await browser.close();
  server.close();
}
