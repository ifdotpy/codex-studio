#!/usr/bin/env node
// copyText falls back to the copy command when the Clipboard API is denied.
import assert from "node:assert/strict";

import { test } from "vitest";
const originalNavigator = Object.getOwnPropertyDescriptor(
  globalThis,
  "navigator",
);
const originalDocument = Object.getOwnPropertyDescriptor(
  globalThis,
  "document",
);

test("copyText uses the clipboard API, fallback command and restores selection", async () => {
  try {
    const log = [];
    let rejectApi = true;
    let commandResult = true;
    const area = () => ({
      value: "",
      style: {},
      attrs: {},
      setAttribute(k, v) {
        this.attrs[k] = v;
      },
      select() {
        log.push(["select", this.value]);
      },
      setSelectionRange() {},
      remove() {
        log.push(["remove"]);
      },
    });
    const saved = { id: "range" };
    // Node 24 defines a read-only navigator; replace it for this test.
    Object.defineProperty(globalThis, "navigator", {
      configurable: true,
      value: {
        clipboard: {
          writeText: async (text) => {
            log.push(["api", text]);
            if (rejectApi) throw new Error("denied");
          },
        },
      },
    });
    globalThis.document = {
      activeElement: { focus: () => log.push(["focus"]) },
      getSelection: () => ({
        rangeCount: 1,
        getRangeAt: () => saved,
        removeAllRanges: () => log.push(["clear"]),
        addRange: (r) => log.push(["restore", r.id]),
      }),
      createElement: () => area(),
      body: { appendChild: (el) => log.push(["append", el.value]) },
      execCommand: (name) => {
        log.push(["command", name]);
        return commandResult;
      },
    };
    const { copyText } = await import("./clipboard.ts");

    await copyText("secret code");
    assert.deepEqual(
      log.map((e) => e[0]),
      [
        "api",
        "append",
        "select",
        "command",
        "remove",
        "clear",
        "restore",
        "focus",
      ],
    );
    assert.deepEqual(
      log.find((e) => e[0] === "select"),
      ["select", "secret code"],
    );

    log.length = 0;
    rejectApi = false;
    await copyText("direct");
    assert.deepEqual(log, [["api", "direct"]]);

    log.length = 0;
    rejectApi = true;
    commandResult = false;
    await assert.rejects(copyText("blocked"), /blocked clipboard access/);
    assert.ok(
      log.some((e) => e[0] === "remove"),
      "the hidden field is removed after a failure",
    );
    console.log(
      "PASS: Clipboard API, copy-command fallback, cleanup and failure",
    );
  } finally {
    if (originalNavigator)
      Object.defineProperty(globalThis, "navigator", originalNavigator);
    else delete globalThis.navigator;
    if (originalDocument)
      Object.defineProperty(globalThis, "document", originalDocument);
    else delete globalThis.document;
  }
});
