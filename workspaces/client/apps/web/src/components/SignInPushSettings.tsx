import { useServerApi } from "../servers/ServerServicesContext";
import { useEffect, useState } from "react";
import { Button } from "@mantine/core";
import { errorText, type GetResult } from "../api";
import {
  endpointFingerprint,
  enableSignInPush,
  pushSupported,
  stopSignInPush,
} from "../push/signInPush";

type PushState = GetResult<"/api/web-push">;

/** Sign-in alerts on the phone through Web Push (docs/web-push.md). */
export function SignInPushSettings({ active }: { active: boolean }) {
  const { get, post } = useServerApi();
  const [state, setState] = useState<PushState | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [fingerprint, setFingerprint] = useState<string | null>();
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
  useEffect(() => {
    if (!active || !pushSupported()) return;
    setFingerprint(undefined);
    let mounted = true;
    void navigator.serviceWorker
      .getRegistration("/")
      .then(async (registration) => {
        const subscription = await registration?.pushManager.getSubscription();
        const value = subscription
          ? await endpointFingerprint(subscription.endpoint)
          : null;
        if (mounted) setFingerprint(value);
      })
      .catch((failure: unknown) => {
        if (mounted) setError(errorText(failure));
      });
    return () => {
      mounted = false;
    };
  }, [active]);
  if (!active) return null;
  const thisDevice = state?.subscriptions.find(
    (device) => device.endpointFingerprint === fingerprint,
  )?.id;
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
      const old = thisDevice;
      if (old) {
        await stopSignInPush(
          await registration.pushManager.getSubscription(),
          () => post("/api/web-push/remove", { id: old }),
        );
      }
      await enableSignInPush({
        registration,
        requestPermission: () => Notification.requestPermission(),
        publicKey,
        userAgent: navigator.userAgent,
        save: (body) => post("/api/web-push/subscriptions", body),
      });
      const subscription = await registration.pushManager.getSubscription();
      setFingerprint(
        subscription ? await endpointFingerprint(subscription.endpoint) : null,
      );
    });
  const stop = (id: string) =>
    run(async () => {
      const registration =
        id === thisDevice
          ? await navigator.serviceWorker.getRegistration("/")
          : undefined;
      await stopSignInPush(
        (await registration?.pushManager.getSubscription()) ?? null,
        () => post("/api/web-push/remove", { id }),
      );
      if (id === thisDevice) setFingerprint(null);
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
          This window cannot get push alerts. Try adding Studio to your home
          screen and opening it from there.
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
          disabled={!state.ready || subscribed || fingerprint === undefined}
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
                disabled={
                  busy || (pushSupported() && fingerprint === undefined)
                }
                onClick={() => stop(device.id)}
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
