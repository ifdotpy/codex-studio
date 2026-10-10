import { test, expect } from "../playwright.mjs";
import { detectTextClipping } from "./text-clipping.mjs";

test("text audit detects clipping and covered controls", async ({ page }) => {
  await page.setContent(`
    <style>
      body { font: 16px Arial; }
      .cut { height: 8px; overflow: hidden; }
      .narrow { width: 20px; white-space: nowrap; overflow: hidden; }
      .ellipsis { text-overflow: ellipsis; }
      .cover { position: absolute; inset: 0; background: white; }
      .trim { height: 10px; display: flex; width: max-content; line-height: 1; overflow: hidden; text-box-trim: trim-both; text-box-edge: cap alphabetic; }
      .sr { position: absolute; width: 1px; height: 1px; clip: rect(0,0,0,0); overflow: hidden; }
    </style>
    <div class="trim"><span id="descender">Change gy</span></div>
    <div id="vertical" class="cut">Descenders gy</div>
    <div id="horizontal" class="narrow">Long horizontal text</div>
    <div id="ellipsis" class="narrow ellipsis">Hidden full text</div>
    <div id="title" class="narrow ellipsis" title="Available full text">Available full text</div>
    <div style="position: relative"><button id="covered">Covered button</button><div class="cover"></div></div>
    <div class="sr-only" style="width:1px;overflow:hidden"><span id="hidden-heading">Conversations</span></div>
    <div style="height:8px;overflow:hidden"><div style="height:8px;overflow:auto"><div id="scroll-text">Scrollable descenders gy</div></div></div>
    <div style="position:relative"><button id="popup-covered">Popup covered</button><div role="listbox" class="cover"></div></div>
    <div id="sticky-scroll" style="height:60px;overflow:auto"><header class="mantine-Modal-header" style="position:sticky;top:0;height:20px;background:white;z-index:1">Header</header><label id="sticky-covered">Scrollable label</label><div style="height:100px"></div></div>
    <span id="screen-reader" class="sr">Accessible help</span>
  `);
  await page.locator("#sticky-scroll").evaluate((el) => {
    el.scrollTop = 20;
  });
  const rows = await page.evaluate(detectTextClipping, {
    scene: "calibration",
    profile: "fixture",
  });
  for (const [selector, reason] of [
    ["#vertical", "vertical-text-clipping"],
    ["#descender", "vertical-text-clipping"],
    ["#horizontal", "horizontal-clipping-without-ellipsis"],
    ["#ellipsis", "ellipsis-without-full-text"],
    ["#covered", "covered-control-text"],
  ]) {
    expect(
      rows.some((row) => row.selector === selector && row.reason === reason),
      `${selector}: ${reason}; ${JSON.stringify(rows)}`,
    ).toBe(true);
  }
  expect(
    rows.some(
      (row) =>
        row.selector === "#title" &&
        row.reason === "ellipsis-without-full-text",
    ),
  ).toBe(false);
  expect(rows.some((row) => row.selector === "#screen-reader")).toBe(false);
  expect(rows.some((row) => row.selector === "#hidden-heading")).toBe(false);
  expect(
    rows.some(
      (row) =>
        row.selector === "#scroll-text" &&
        row.reason === "vertical-text-clipping",
    ),
  ).toBe(false);
  expect(
    rows.some((row) => row.selector === "#popup-covered" && row.intentional),
  ).toBe(true);
  expect(
    rows.some((row) => row.selector === "#sticky-covered" && row.intentional),
  ).toBe(true);
});
