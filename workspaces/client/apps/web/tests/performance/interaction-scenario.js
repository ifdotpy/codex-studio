(async () => {
  const report = async (result) => {
    const output = document.createElement("output");
    output.id = "benchmark-result";
    output.textContent = JSON.stringify(result);
    document.body.append(output);
    await fetch("/benchmark-result", {
      method: "POST",
      body: JSON.stringify(result),
    });
  };
  const storageWrites = [];
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
  const calls = [];
  const preferenceValues = new Map();
  let repeatedPreferenceWrites = 0;
  const invoke = window.__TAURI__?.core.invoke;
  if (invoke)
    window.__TAURI__.core.invoke = (command, args) => {
      if (command === "client_request") {
        calls.push(args.request.method);
        if (args.request.method === "preferences.raw_set") {
          const { key, raw } = args.request.params;
          if (preferenceValues.get(key) === raw) repeatedPreferenceWrites++;
          preferenceValues.set(key, raw);
        } else if (args.request.method === "preferences.raw_remove") {
          preferenceValues.delete(args.request.params.key);
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
  const results = { startup: {}, actions: [] };
  const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const paint = () =>
    new Promise((resolve) =>
      requestAnimationFrame(() => requestAnimationFrame(resolve)),
    );
  const wait = async (read, label) => {
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
  const button = (name) =>
    [...document.querySelectorAll('button,[role="tab"]')].find(
      (node) =>
        (node.getAttribute("aria-label") || node.textContent.trim()) === name,
    );
  const formattedMessage = (matches) =>
    [...document.querySelectorAll("[data-message] .markdown-block p")].some(
      (node) =>
        matches(node.closest("[data-message]").getAttribute("data-message")),
    );
  const messages = () =>
    [...document.querySelectorAll("[data-message]")].map((node) =>
      node.getAttribute("data-message"),
    );
  const measure = async (name, action, ready) => {
    const start = performance.now();
    const callStart = calls.length;
    const storageStart = storageWrites.length;
    const repeatedStart = repeatedPreferenceWrites;
    let stopped = false,
      previous = start;
    const frames = [];
    const sample = (now) => {
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
      () => document.querySelector('#chat-list [data-chat="long"]'),
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
    if (button("Show old (2)")) button("Show old (2)").click();
    for (const id of ["short", "long", "short", "long"]) {
      await measure(
        `chat-${id}`,
        async () =>
          (
            await wait(
              () => document.querySelector(`#chat-list [data-chat="${id}"]`),
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
            button("Studio settings").click();
          (await wait(() => button(tab), `settings tab ${tab}`)).click();
        },
        () => {
          const selected = document.querySelector(
            '[role="dialog"] [role="tab"][aria-selected="true"]',
          );
          const panel =
            selected &&
            document.getElementById(selected.getAttribute("aria-controls"));
          return (
            selected?.textContent?.trim() === tab &&
            panel?.getClientRects().length
          );
        },
      );
    }
    document
      .querySelector('[role="dialog"] button[aria-label="Close"]')
      .click();
    await wait(
      () => !document.querySelector('[role="dialog"]'),
      "close settings",
    );
    const closeDialog = async () => {
      (
        await wait(
          () =>
            document.querySelector(
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
      () => button("Chat settings").click(),
      () => document.querySelector('[role="dialog"] .chat-settings-panel'),
    );
    await closeDialog();
    await measure(
      "team-messages",
      () => button("Messages").click(),
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
            button("Chat actions").click();
            (
              await wait(
                () =>
                  document.querySelector('[data-workspace-section="changes"]'),
                "changes menu",
              )
            ).click();
          }
          (
            await wait(
              () =>
                [...document.querySelectorAll(".workspace-nav button")].find(
                  (node) => node.textContent.trim() === section,
                ),
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
    const scroller = document.querySelector("#messages");
    scroller.dispatchEvent(
      new WheelEvent("wheel", { deltaY: -10000, bubbles: true }),
    );
    scroller.scrollTop = 0;
    await paint();
    await delay(250);
    await measure(
      "older-page",
      async () => {
        button("Earlier messages").click();
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
      stack: error?.stack,
      storage: storageCounts(),
      ...results,
    });
  }
})();
