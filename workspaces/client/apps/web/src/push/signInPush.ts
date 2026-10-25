/**
 * Sign-in alerts on this device through Web Push (docs/web-push.md). The home-screen web
 * app subscribes with the server's VAPID key; the server sends one encrypted push when a
 * sign-in card waits for the person. The subscription keys go only to the chat's server.
 */

/** The local storage key of this device's subscription ID, per server key. */
const PUSH_SUBSCRIPTION_KEY = "studio-sign-in-push:";

/** The server's subscription ID of this device, if this device subscribed. */
export function devicePushId(publicKey: string): string | null {
  try {
    return localStorage.getItem(PUSH_SUBSCRIPTION_KEY + publicKey);
  } catch {
    return null;
  }
}

export function rememberDevicePushId(publicKey: string, id: string | null) {
  try {
    if (id) localStorage.setItem(PUSH_SUBSCRIPTION_KEY + publicKey, id);
    else localStorage.removeItem(PUSH_SUBSCRIPTION_KEY + publicKey);
  } catch {
    // Without storage the device list still shows and stops every device.
  }
}

export function pushSupported(scope: typeof globalThis = globalThis) {
  const window = scope as typeof globalThis & {
    codexDesktop?: unknown;
    PushManager?: unknown;
  };
  return (
    !window.codexDesktop &&
    !!window.isSecureContext &&
    "serviceWorker" in (window.navigator ?? {}) &&
    "PushManager" in window &&
    "Notification" in window
  );
}

/** The VAPID public key (base64url) as `applicationServerKey`. */
export function applicationServerKey(
  base64url: string,
): Uint8Array<ArrayBuffer> {
  const base64 = base64url.replace(/-/g, "+").replace(/_/g, "/");
  const padded = base64 + "=".repeat((4 - (base64.length % 4)) % 4);
  const binary = atob(padded);
  return Uint8Array.from(binary, (char) => char.charCodeAt(0));
}

/** A short name of this device for the device list. */
export function deviceName(userAgent: string): string {
  if (/iPhone/.test(userAgent)) return "Phone";
  if (/iPad/.test(userAgent)) return "Tablet";
  if (/Android/.test(userAgent)) return "Phone";
  if (/Macintosh/.test(userAgent)) return "Computer";
  return "This device";
}

/** The body of `POST /api/web-push/subscriptions` from `PushSubscription.toJSON()`. */
export function subscriptionBody(
  subscription: { endpoint?: string; keys?: Record<string, string> },
  device: string,
) {
  const endpoint = subscription.endpoint;
  const p256dh = subscription.keys?.p256dh;
  const auth = subscription.keys?.auth;
  if (!endpoint || !p256dh || !auth)
    throw new Error("The browser gave no push subscription keys.");
  return { endpoint, p256dh, auth, device };
}

type BrowserSubscription = {
  toJSON(): { endpoint?: string; keys?: Record<string, string> };
  unsubscribe(): Promise<boolean>;
};

/** The parts of `ServiceWorkerRegistration` that push needs. */
export type PushRegistration = {
  pushManager: {
    getSubscription(): Promise<Pick<BrowserSubscription, "unsubscribe"> | null>;
    subscribe(options: {
      userVisibleOnly: boolean;
      applicationServerKey: Uint8Array<ArrayBuffer>;
    }): Promise<BrowserSubscription>;
  };
};

/**
 * Subscribe this device and save the subscription on the server. An older browser
 * subscription (for example of an older server key) is replaced. When the server refuses
 * the new subscription, the browser subscription is dropped again.
 */
export async function enableSignInPush<Row>({
  registration,
  requestPermission,
  publicKey,
  userAgent,
  save,
}: {
  registration: PushRegistration;
  requestPermission: () => Promise<NotificationPermission>;
  publicKey: string;
  userAgent: string;
  save: (body: ReturnType<typeof subscriptionBody>) => Promise<Row>;
}): Promise<Row> {
  if ((await requestPermission()) !== "granted")
    throw new Error(
      "Notifications are blocked for Studio. Allow them in the device settings, then try again.",
    );
  await (await registration.pushManager.getSubscription())?.unsubscribe();
  const subscription = await registration.pushManager.subscribe({
    userVisibleOnly: true,
    applicationServerKey: applicationServerKey(publicKey),
  });
  try {
    return await save(
      subscriptionBody(subscription.toJSON(), deviceName(userAgent)),
    );
  } catch (failure) {
    await subscription.unsubscribe().catch(() => false);
    throw failure;
  }
}
