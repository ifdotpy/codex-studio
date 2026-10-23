import { describe, expect, it } from "vitest";
import {
  applicationServerKey,
  deviceName,
  enableSignInPush,
  pushSupported,
  subscriptionBody,
} from "./signInPush";

describe("sign-in alerts through Web Push", () => {
  it("turns the server key into the bytes that PushManager needs", () => {
    const key = applicationServerKey("BAEC_-8");
    expect([...key]).toEqual([4, 1, 2, 255, 239]);
  });

  it("names the device without its user agent", () => {
    expect(
      deviceName("Mozilla/5.0 (iPhone; CPU iPhone OS 26_0 like Mac OS X)"),
    ).toBe("iPhone");
    expect(deviceName("Mozilla/5.0 (Linux; Android 16)")).toBe("Android phone");
    expect(deviceName("Mozilla/5.0 (Macintosh; Intel Mac OS X 15_0)")).toBe(
      "Mac",
    );
    expect(deviceName("Other")).toBe("This device");
  });

  it("sends only the endpoint, the two keys and the device name", () => {
    expect(
      subscriptionBody(
        {
          endpoint: "https://web.push.apple.com/abc",
          keys: { p256dh: "BAEC", auth: "AAEC", extra: "no" },
        },
        "iPhone",
      ),
    ).toEqual({
      endpoint: "https://web.push.apple.com/abc",
      p256dh: "BAEC",
      auth: "AAEC",
      device: "iPhone",
    });
    expect(() => subscriptionBody({ endpoint: "x" }, "iPhone")).toThrow(
      "no push subscription keys",
    );
  });

  it("is off in the desktop app and without the Push API", () => {
    const base = {
      isSecureContext: true,
      navigator: { serviceWorker: {} },
      PushManager: class {},
      Notification: class {},
    };
    expect(pushSupported(base as never)).toBe(true);
    expect(pushSupported({ ...base, codexDesktop: {} } as never)).toBe(false);
    expect(pushSupported({ ...base, isSecureContext: false } as never)).toBe(
      false,
    );
    const { PushManager: _, ...noPush } = base;
    expect(pushSupported(noPush as never)).toBe(false);
  });
});

describe("turning on sign-in push for this device", () => {
  function registration(old: { unsubscribe: () => Promise<boolean> } | null) {
    const calls: string[] = [];
    const created = {
      toJSON: () => ({
        endpoint: "https://web.push.apple.com/new",
        keys: { p256dh: "BAEC", auth: "AAEC" },
      }),
      unsubscribe: async () => (calls.push("unsubscribe new"), true),
    };
    return {
      calls,
      value: {
        pushManager: {
          getSubscription: async () => old,
          subscribe: async (options: {
            userVisibleOnly: boolean;
            applicationServerKey: Uint8Array;
          }) => {
            calls.push(
              `subscribe ${options.userVisibleOnly} ${options.applicationServerKey.length}`,
            );
            return created;
          },
        },
      },
    };
  }

  it("asks for permission, replaces an old subscription and saves the new one", async () => {
    const old = {
      unsubscribe: async () => (calls.push("unsubscribe old"), true),
    };
    const { calls, value } = registration(old);
    const saved: unknown[] = [];
    const row = await enableSignInPush({
      registration: value,
      requestPermission: async () => "granted",
      publicKey: "BAEC_-8",
      userAgent: "(iPhone)",
      save: async (body) => (saved.push(body), { id: "s1" }),
    });
    expect(row).toEqual({ id: "s1" });
    expect(calls).toEqual(["unsubscribe old", "subscribe true 5"]);
    expect(saved).toEqual([
      {
        endpoint: "https://web.push.apple.com/new",
        p256dh: "BAEC",
        auth: "AAEC",
        device: "iPhone",
      },
    ]);
  });

  it("stops when the person blocks notifications", async () => {
    const { calls, value } = registration(null);
    await expect(
      enableSignInPush({
        registration: value,
        requestPermission: async () => "denied",
        publicKey: "BAEC",
        userAgent: "",
        save: async () => ({ id: "s1" }),
      }),
    ).rejects.toThrow("Notifications are blocked");
    expect(calls).toEqual([]);
  });

  it("drops the browser subscription when the server refuses it", async () => {
    const { calls, value } = registration(null);
    await expect(
      enableSignInPush({
        registration: value,
        requestPermission: async () => "granted",
        publicKey: "BAEC",
        userAgent: "",
        save: async () => {
          throw new Error("refused");
        },
      }),
    ).rejects.toThrow("refused");
    expect(calls).toEqual(["subscribe true 3", "unsubscribe new"]);
  });
});
