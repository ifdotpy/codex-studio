import { useServerApi } from "../servers/ServerServicesContext";
import { useEffect, useState } from "react";
import { Button } from "@mantine/core";
import { errorText, type GetResult } from "../api";
import {
  devicePushId,
  enableSignInPush,
  pushSupported,
  rememberDevicePushId,
} from "../push/signInPush";

type PushState = GetResult<"/api/web-push">;

/** Sign-in alerts on the phone through Web Push (docs/web-push.md). */
export function SignInPushSettings({ active }: { active: boolean }) {
  const { get, post } = useServerApi();
  const [state, setState] = useState<PushState | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const load = () =>
    get("/api/web-push").then(
      (value) => {
        setState(value);
        setError("");
      },
      (failure) => setError(errorText(failure)),
    );
  useEffect(() => {
    if (active) void load();
  }, [active]);
  if (!active) return null;
  const thisDevice = state ? devicePushId(state.publicKey) : null;
  const subscribed =
    !!thisDevice &&
    !!state?.subscriptions.some((device) => device.id === thisDevice);
  const run = (work: () => Promise<unknown>) => {
    setBusy(true);
    setError("");
    void work()
      .then(load, (failure) => setError(errorText(failure)))
      .finally(() => setBusy(false));
  };
  const enable = (publicKey: string) =>
    run(async () => {
      const registration = await navigator.serviceWorker.ready;
      const old = devicePushId(publicKey);
      if (old) await post("/api/web-push/remove", { id: old });
      const row = await enableSignInPush({
        registration,
        requestPermission: () => Notification.requestPermission(),
        publicKey,
        userAgent: navigator.userAgent,
        save: (body) => post("/api/web-push/subscriptions", body),
      });
      rememberDevicePushId(publicKey, row.id);
    });
  const stop = (publicKey: string, id: string) =>
    run(async () => {
      await post("/api/web-push/remove", { id });
      if (id !== devicePushId(publicKey)) return;
      rememberDevicePushId(publicKey, null);
      const registration = await navigator.serviceWorker.getRegistration("/");
      await (await registration?.pushManager.getSubscription())?.unsubscribe();
    });
  return (
    <section className="settings-group" aria-label="Phone sign-in alerts">
      <h2>Sign-in alerts on the phone</h2>
      <p>
        When a sign-in card waits for you, Studio sends one alert to each device
        below, and one reminder before the card closes. The alert shows only the
        sign-in host and the chat name.
      </p>
      {!pushSupported() && (
        <p>
          This window cannot get push alerts. On an iPhone, add Studio to the
          Home Screen and open it from there.
        </p>
      )}
      {state && !state.ready && (
        <p role="status">
          Studio sends push alerts only when remote access has an https address.
        </p>
      )}
      {state && pushSupported() && (
        <Button
          mt="md"
          loading={busy}
          disabled={!state.ready || subscribed}
          onClick={() => enable(state.publicKey)}
        >
          {subscribed
            ? "This device gets alerts"
            : "Send alerts to this device"}
        </Button>
      )}
      {state && state.subscriptions.length > 0 && (
        <ul aria-label="Devices with sign-in alerts">
          {state.subscriptions.map((device) => (
            <li key={device.id}>
              {device.id === thisDevice
                ? `${device.device} (this device)`
                : device.device}
              , since {new Date(device.created * 1000).toLocaleDateString()}{" "}
              <Button
                size="compact-sm"
                variant="subtle"
                disabled={busy}
                onClick={() => stop(state.publicKey, device.id)}
              >
                Stop alerts
              </Button>
            </li>
          ))}
        </ul>
      )}
      {error && <p role="alert">{error}</p>}
    </section>
  );
}
