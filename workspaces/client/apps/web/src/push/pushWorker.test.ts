/// <reference lib="webworker" />
import assert from "node:assert/strict";
import { expect, test } from "vitest";
import vm from "node:vm";
import { readFile } from "node:fs/promises";

async function worker({
  windows = [],
}: {
  windows?: (Pick<WindowClient, "url"> & { focus(): Promise<unknown> })[];
} = {}) {
  const handlers: Record<
    string,
    (event: {
      waitUntil(value: Promise<unknown>): void;
      data?: { json(): unknown };
      notification?: Pick<Notification, "close">;
    }) => void
  > = {};
  const shown: (NotificationOptions & { title: string })[] = [];
  const opened: string[] = [];
  vm.runInNewContext(
    await readFile(
      new URL("../../public/studio-sw.js", import.meta.url),
      "utf8",
    ),
    {
      self: {
        location: { origin: "https://studio.example" },
        addEventListener: (name: string, handler: (typeof handlers)[string]) =>
          (handlers[name] = handler),
        registration: {
          showNotification: async (
            title: string,
            options: NotificationOptions,
          ) => shown.push({ title, ...options }),
        },
        clients: {
          matchAll: async () => windows,
          openWindow: async (url: string) => opened.push(url),
        },
      },
      URL,
    },
  );
  const fire = async (
    name: string,
    event: {
      data?: { json(): unknown };
      notification?: Pick<Notification, "close">;
    },
  ) => {
    let work: Promise<unknown> | undefined;
    assert.ok(handlers[name]);
    handlers[name]({ ...event, waitUntil: (value) => (work = value) });
    await work;
  };
  return { fire, shown, opened };
}

test("a sign-in push shows its text with the card tag and nothing else", async () => {
  const { fire, shown } = await worker();
  await fire("push", {
    data: {
      json: () => ({
        title: "Sign-in needed",
        body: "id.example.com for Infra",
        tag: "studio-sign-in:r1",
        url: "https://evil.example",
      }),
    },
  });
  expect(shown).toEqual([
    {
      title: "Sign-in needed",
      body: "id.example.com for Infra",
      tag: "studio-sign-in:r1",
      icon: "/apple-touch-icon.png",
    },
  ]);
});

test("a push without readable text still shows a notification", async () => {
  const { fire, shown } = await worker();
  await fire("push", {
    data: {
      json: () => {
        throw new SyntaxError("bad");
      },
    },
  });
  expect(shown[0]).toMatchObject({
    title: "Sign-in needed",
    body: "Open Studio to sign in.",
  });
});

test("a click on the notification opens Studio or focuses its window", async () => {
  let focused = 0;

  const open = await worker({
    windows: [{ url: "https://studio.example/", focus: async () => focused++ }],
  });
  const close = () => {};
  await open.fire("notificationclick", { notification: { close } });
  expect([focused, open.opened]).toEqual([1, []]);
  const none = await worker();
  await none.fire("notificationclick", { notification: { close } });
  expect(none.opened).toEqual(["/"]);
});
