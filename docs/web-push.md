# Sign-in alerts on the phone (Web Push)

A sign-in card waits for the person (docs/vm-browser-bridge.md, section 6.2). Studio windows
show a desktop alert for it. A phone that is locked or closed has no Studio window, so the
server also sends a standard Web Push alert to the devices that turned it on.

## What the person does

1. Open Studio on the phone from its remote access address (https). On an iPhone, add Studio
   to the Home Screen first and open it from there: iOS gives push only to Home Screen apps.
2. Open Settings, then "Phone alerts", and select "Send alerts to this device". The browser
   asks for notification permission.
3. The device list shows every subscribed device. "Stop alerts" ends the pushes of one device,
   from any device.

## What the server sends

- One alert when a card is still pending 10 s after its creation, and one reminder 5 minutes
  before the card closes. The times are `SIGN_IN_ALERT_GRACE_SECONDS` and
  `SIGN_IN_REMINDER_SECONDS` (owner `runtime-schema/src/shared_values.rs`); the desktop alerts
  use the same values. A phone sign-in that waits for its pasted result gets no reminder. A
  deferred card and a card that closed give nothing.
- The payload is `{"title", "body", "tag"}`: "Sign-in needed" and "<host> for <chat>", or the
  reminder text, and the tag `studio-sign-in:<card id>`. It holds no link, token, code or
  sign-in address. The service worker (`public/studio-sw.js`) shows only these three fields,
  and a click opens or focuses Studio.
- The payload is encrypted for the device (RFC 8291, `aes128gcm`). The push service sees only
  the encrypted bytes.
- The server pushes only when the remote access address is https (`remote-access.json`). That
  address is the VAPID contact (`sub`).

## Keys and records

The server keeps its files in `<state>/web-push`, folder mode 0700 and file mode 0600:

- `vapid.pkcs8`: the server's VAPID signing key (RFC 8292, ES256). The server makes it on
  first use. It never goes into the repository, a log or an API answer; the API returns only
  the public key.
- `subscriptions.json`: the endpoint, the device keys, the device name and the creation time
  of each subscription. The API returns only the ID, the device name and the time.
- `sent.json`: which alerts each open card already got, so a restart sends no second alert.
  A card that is no longer pending leaves the record.

A push service that answers 404 or 410 ended the subscription; the server removes it. Other
failures are logged with the device name only, because the endpoint is a capability. When the
browser changes its subscription, the device leaves the list at the next push. Turn the alerts
on again on that device.

## API

- `GET /api/web-push`: the public key, `ready` (an https remote access address exists) and the
  device list.
- `POST /api/web-push/subscriptions`: subscribe with `endpoint`, `p256dh`, `auth` and `device`.
  The endpoint must be https, and the keys must be a P-256 point and 16 bytes.
- `POST /api/web-push/remove`: stop the pushes of one device by its `id`.

## Tests

`runtime-services/src/web_push_tests.rs` decrypts a push with the device key as a browser
does, and sends to a local fake push endpoint on the loopback address. No test sends a real
push. `web/src/push/` tests the subscription steps and the service worker handlers.
