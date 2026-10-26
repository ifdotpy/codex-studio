import type { Host } from "../../src/native/host.ts";
import { required } from "./performance-fixture.ts";
(async () => {
  const report = async (result: InteractionResult) => {
    const output = document.createElement("output");
    output.id = "benchmark-result";
    output.textContent = JSON.stringify(result);
    document.body.append(output);
    await fetch("/benchmark-result", {
      method: "POST",
      body: JSON.stringify(result),
    });
  };
  const storageWrites: { duplicate: boolean; bytes: number }[] = [];
  const setItem = Storage.prototype.setItem;
  Storage.prototype.setItem = function (key, value) {
    const duplicate = this.getItem(String(key)) === String(value);
    storageWrites.push({ duplicate, bytes: String(value).length * 2 });
    return setItem.call(this, key, value);
  };
  const storageCounts = (start = 0) => ({
    writes: storageWrites.length - start,
    duplicateWrites: storageWrites
      .slice(start)
      .filter((write) => write.duplicate).length,
    bytes: storageWrites
      .slice(start)
      .reduce((sum, write) => sum + write.bytes, 0),
  });
  const calls: string[] = [];
  const preferenceValues = new Map();
  let repeatedPreferenceWrites = 0;
  const invoke = window.__TAURI__?.core.invoke;
  if (invoke && window.__TAURI__)
    window.__TAURI__.core.invoke = (command, args) => {
      if (command === "client_request") {
        const request = args.request;
        if (
          !request ||
          typeof request !== "object" ||
          !("method" in request) ||
          typeof request.method !== "string"
        )
          throw new Error("Invalid native request fixture");
        calls.push(request.method);
        if (request.method === "preferences.raw_set") {
          const params = "params" in request ? request.params : null;
          if (
            !params ||
            typeof params !== "object" ||
            !("key" in params) ||
            !("raw" in params)
          )
            throw new Error("Missing preference parameters");
          const { key, raw } = params;
          if (preferenceValues.get(key) === raw) repeatedPreferenceWrites++;
          preferenceValues.set(key, raw);
        } else if (request.method === "preferences.raw_remove") {
          const params = "params" in request ? request.params : null;
          if (!params || typeof params !== "object" || !("key" in params))
            throw new Error("Missing preference key");
          preferenceValues.delete(params.key);
        }
      }
      return invoke(command, args);
    };
  const counts = (start = 0) =>
    Object.fromEntries(
      [...new Set(calls.slice(start))].map((method) => [
        method,
        calls.slice(start).filter((value) => value === method).length,
      ]),
    );
  const results: { startup: InteractionStartup; actions: InteractionAction[] } =
    { startup: {}, actions: [] };
  const delay = (ms: number) =>
    new Promise((resolve) => setTimeout(resolve, ms));
  const paint = () =>
    new Promise((resolve) =>
      requestAnimationFrame(() => requestAnimationFrame(resolve)),
    );
  const wait = async <T>(
    read: () => T,
    label: string,
  ): Promise<NonNullable<T>> => {
    const deadline = performance.now() + 8000;
    while (performance.now() < deadline) {
      const value = read();
      if (value) return value;
      await delay(10);
    }
    throw new Error(
      `Timed out: ${label}; ${document.body.innerText.slice(-1800)}`,
    );
  };
  const button = (name: string) =>
    [...document.querySelectorAll<HTMLElement>('button,[role="tab"]')].find(
      (node) =>
        (node.getAttribute("aria-label") || node.textContent.trim()) === name,
    );
  const formattedMessage = (matches: (id: string) => boolean) =>
    [...document.querySelectorAll("[data-message] .markdown-block p")].some(
      (node) =>
        matches(
          required(
            node.closest("[data-message]")?.getAttribute("data-message"),
          ),
        ),
    );
  const messages = () =>
    [...document.querySelectorAll("[data-message]")].map((node) =>
      node.getAttribute("data-message"),
    );
  const measure = async (
    name: string,
    action: () => unknown | Promise<unknown>,
    ready: () => unknown,
  ) => {
    const start = performance.now();
    const callStart = calls.length;
    const storageStart = storageWrites.length;
    const repeatedStart = repeatedPreferenceWrites;
    let stopped = false,
      previous = start;
    const frames: number[] = [];
    const sample = (now: number) => {
      frames.push(now - previous);
      previous = now;
      if (!stopped) requestAnimationFrame(sample);
    };
    requestAnimationFrame(sample);
    await action();
    await wait(ready, name);
    const contentMs = performance.now() - start;
    await paint();
    const paintedMs = performance.now() - start;
    await delay(150);
    stopped = true;
    results.actions.push({
      name,
      contentMs,
      paintedMs,
      maxFrameMs: Math.max(0, ...frames),
      framesOver50: frames.filter((ms) => ms > 50).length,
      domNodes: document.querySelectorAll("*").length,
      messages: messages().length,
      calls: counts(callStart),
      storage: storageCounts(storageStart),
      repeatedPreferenceWrites: repeatedPreferenceWrites - repeatedStart,
    });
  };
  try {
    await wait(() => document.querySelector("#message"), "composer");
    results.startup.composerMs = performance.now();
    const initialChat = await wait(
      () =>
        document.querySelector<HTMLElement>('#chat-list [data-chat="long"]'),
      "fixture chat",
    );
    initialChat.click();
    await wait(
      () =>
        document.querySelector('[data-message="msg-359"] .markdown-block p'),
      "formatted content",
    );
    await paint();
    results.startup.contentPaintedMs = performance.now();
    results.startup.calls = counts();
    results.startup.storage = storageCounts();
    results.startup.repeatedPreferenceWrites = repeatedPreferenceWrites;
    const chats = [...document.querySelectorAll("#chat-list .chat-row")];
    if (!chats.every((node) => /conversation/.test(node.textContent)))
      throw new Error("Not the isolated performance fixture");
    if (button("Show old (2)")) required(button("Show old (2)")).click();
    for (const id of ["short", "long", "short", "long"]) {
      await measure(
        `chat-${id}`,
        async () =>
          (
            await wait(
              () =>
                document.querySelector<HTMLElement>(
                  `#chat-list [data-chat="${id}"]`,
                ),
              `chat row ${id}`,
            )
          ).click(),
        () =>
          document
            .querySelector("#conversation-title")
            ?.textContent?.toLowerCase()
            .includes(id) &&
          formattedMessage((m) =>
            id === "short" ? m === "msg-3" : m === "msg-359",
          ),
      );
    }
    for (const tab of [
      "Accounts",
      "Appearance",
      "Federation",
      "Servers",
      "Linux VM",
      "Hotkeys",
    ]) {
      await measure(
        `settings-${tab}`,
        async () => {
          if (!document.querySelector('[role="dialog"]'))
            required(button("Studio settings")).click();
          (await wait(() => button(tab), `settings tab ${tab}`)).click();
        },
        () => {
          const selected = document.querySelector(
            '[role="dialog"] [role="tab"][aria-selected="true"]',
          );
          const panel =
            selected &&
            document.getElementById(
              selected.getAttribute("aria-controls") ?? "",
            );
          return (
            selected?.textContent?.trim() === tab &&
            panel?.getClientRects().length
          );
        },
      );
    }
    required(
      document.querySelector<HTMLElement>(
        '[role="dialog"] button[aria-label="Close"]',
      ),
    ).click();
    await wait(
      () => !document.querySelector('[role="dialog"]'),
      "close settings",
    );
    const closeDialog = async () => {
      (
        await wait(
          () =>
            document.querySelector<HTMLElement>(
              '[role="dialog"] button[aria-label="Close"]',
            ),
          "close dialog button",
        )
      ).click();
      await wait(
        () => !document.querySelector('[role="dialog"]'),
        "dialog closed",
      );
    };
    await measure(
      "chat-settings",
      () => required(button("Chat settings")).click(),
      () => document.querySelector('[role="dialog"] .chat-settings-panel'),
    );
    await closeDialog();
    await measure(
      "team-messages",
      () => required(button("Messages")).click(),
      () => document.querySelector('[role="dialog"] .workspace-message-body'),
    );
    await closeDialog();
    for (const section of [
      "Changes",
      "Plan",
      "Checkpoints",
      "Tools",
      "Profiles",
      "Rules",
      "Search",
    ]) {
      await measure(
        `workspace-${section}`,
        async () => {
          if (!document.querySelector('[role="dialog"]')) {
            required(button("Chat actions")).click();
            (
              await wait(
                () =>
                  document.querySelector<HTMLElement>(
                    '[data-workspace-section="changes"]',
                  ),
                "changes menu",
              )
            ).click();
          }
          (
            await wait(
              () =>
                [
                  ...document.querySelectorAll<HTMLElement>(
                    ".workspace-nav button",
                  ),
                ].find((node) => node.textContent.trim() === section),
              `workspace ${section}`,
            )
          ).click();
        },
        () =>
          document
            .querySelector('.workspace-nav [aria-current="page"]')
            ?.textContent.trim() === section &&
          !document.querySelector(".workspace-load") &&
          document.querySelector(".workspace-content"),
      );
    }
    await closeDialog();
    const scroller = required(document.querySelector("#messages"));
    scroller.dispatchEvent(
      new WheelEvent("wheel", { deltaY: -10000, bubbles: true }),
    );
    scroller.scrollTop = 0;
    await paint();
    await delay(250);
    await measure(
      "older-page",
      async () => {
        required(button("Earlier messages")).click();
        await delay(50);
        scroller.dispatchEvent(
          new WheelEvent("wheel", { deltaY: -10000, bubbles: true }),
        );
        scroller.scrollTop = 0;
      },
      () => formattedMessage((m) => Number(m.slice(4)) < 240),
    );
    await measure(
      "scroll-history",
      async () => {
        for (let i = 0; i < 12; i++) {
          scroller.dispatchEvent(
            new WheelEvent("wheel", { deltaY: 180, bubbles: true }),
          );
          scroller.scrollTop = Math.min(
            scroller.scrollHeight,
            scroller.scrollTop + 180,
          );
          await paint();
        }
      },
      () => true,
    );
    await report({
      passed: true,
      mode: "renderer-performance",
      ...results,
    });
  } catch (error) {
    await report({
      passed: false,
      mode: "renderer-performance",
      calls: counts(),
      error: String(error),
      stack: error instanceof Error ? error.stack : undefined,
      storage: storageCounts(),
      ...results,
    });
  }
})();

export type InteractionStartup = {
  composerMs?: number;
  contentPaintedMs?: number;
  calls?: Record<string, number>;
  storage?: { writes: number; duplicateWrites: number; bytes: number };
  repeatedPreferenceWrites?: number;
};
export type InteractionAction = {
  name: string;
  contentMs: number;
  paintedMs: number;
  maxFrameMs: number;
  framesOver50: number;
  domNodes: number;
  messages: number;
  calls: Record<string, number>;
  storage: NonNullable<InteractionStartup["storage"]>;
  repeatedPreferenceWrites: number;
};
export type InteractionResult = {
  passed: boolean;
  mode: "renderer-performance";
  startup: InteractionStartup;
  actions: InteractionAction[];
  calls?: Record<string, number>;
  error?: string;
  stack?: string | undefined;
  storage?: NonNullable<InteractionStartup["storage"]>;
};
export type InteractionVariant = { label: string; dist: string };
declare global {
  interface Window {
    __TAURI__?: Host;
  }
}
