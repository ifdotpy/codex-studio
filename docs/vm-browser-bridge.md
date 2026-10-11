# VM browser bridge

Status: design. Nothing in this file is implemented. The measurements are from 2026-10-11,
against `origin/main` at `07ea99c7`.

## Goal

A command that an agent runs in the Linux VM can sign in through a browser on the Mac.
Studio stays generic: it knows URLs, ports and chats, and it knows no tool. The person
approves every link.

## 1. Measured tool behaviour

### Setup

- Guest: Ubuntu 24.04.5, aarch64, kernel 6.8.0-142. The probes ran with
  `codex_layr_exec.py` as the project owner `studio-p-9127f608616d564f` (UID 1010) in
  `/tmp/studio-bridge-probe`. The folder was removed after the probes.
- The probe environment had `PATH=/usr/local/bin:/usr/bin:/bin`, `HOME`, `USER`, `LOGNAME`,
  `LANG` and `LAYR_ROOT`. It had no `BROWSER`, `DISPLAY` or `TERM`.
- The guest has `/usr/bin/sensible-browser`. It has no `xdg-open`, `x-www-browser`,
  `www-browser`, `kubectl`, `kubelogin`, `aws`, `gh` or `az`.
- Tools: kubelogin v1.36.4 (`kubelogin_linux_arm64.zip`) and AWS CLI 2.37.12
  (`awscli-exe-linux-aarch64.zip`, Python 3.14.6), unpacked into the probe folder.
- Identity provider: a fake OIDC server on the guest loopback. It served discovery,
  `/authorize` (a redirect back with `code=fake-code`), `/token`, and the AWS SSO OIDC routes
  `/client/register`, `/device_authorization` and `/token`. AWS used
  `AWS_ENDPOINT_URL_SSO_OIDC` and the start URL `https://studio-probe-fake.awsapps.com/start`.
  An AWS-owned suffix skips the start URL lookup, so no request went to AWS or to another
  real provider.
- Browser: a shell script, used as `xdg-open`, as `BROWSER`, or as `--browser-command`. It
  logs its arguments. In "follow" mode it acts as the person and runs `curl -L` on the URL.

### kubelogin (`kubectl oidc-login`)

| Check                        | Result                                                                                                                                                                                                                           |
| ---------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Browser launch               | Runs `xdg-open`, then `x-www-browser`, then `www-browser` from `PATH`. It ignores `BROWSER`. `--browser-command CMD` replaces the search.                                                                                        |
| No launcher                  | Prints `error: could not open the browser: exec: "xdg-open,x-www-browser,www-browser": executable file not found in $PATH` and `Please visit the following URL in your browser manually: http://localhost:8000/`. Then it waits. |
| URL it opens                 | `http://localhost:8000/`. This is its own loopback server, not the provider.                                                                                                                                                     |
| Listener                     | `127.0.0.1:8000` only. `[::1]:8000` refuses the connection. The default is `--listen-address 127.0.0.1:8000,127.0.0.1:18000`.                                                                                                    |
| Port 8000 taken in the guest | Binds `127.0.0.1:18000` and opens `http://localhost:18000/`.                                                                                                                                                                     |
| `GET /` on the listener      | `302` to `<authorization_endpoint>?...&redirect_uri=http%3A%2F%2Flocalhost%3A8000&response_type=code&scope=openid&state=...` with PKCE S256. Two GETs return the same `state`.                                                   |
| Provider URL opened directly | The client skipped `/` and went to the provider URL. kubelogin accepted the callback `http://localhost:8000/?code=...&state=...` and sent one token request. (It then failed on the fake ID token. This is expected.)            |
| Callback page                | `200 text/html` with an inline `<script>window.close()</script>`. No `Set-Cookie`.                                                                                                                                               |
| Wait                         | `--authentication-timeout-sec`, default 180 s. With the value 3 it stopped after about 3 s with `context deadline exceeded`.                                                                                                     |
| Device code                  | `--grant-type=device-code` opens `verification_uri_complete` with `xdg-open`. It starts no listener. The provider must offer a device endpoint.                                                                                  |
| Manual paste                 | `--grant-type=authcode-keyboard` prints the authorize URL and reads the code from stdin (`Enter code:`). Without a terminal it fails with `EOF`.                                                                                 |

### AWS CLI v2 (`aws sso login`)

| Check          | Result                                                                                                                                                                                                                                                                                                                                           |
| -------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Browser launch | Python `webbrowser.open_new_tab`. `BROWSER` works. `xdg-open` on `PATH` is not used without `DISPLAY` or `WAYLAND_DISPLAY`. With `DISPLAY=:0` it is used.                                                                                                                                                                                        |
| No launcher    | Prints "Attempting to open your default browser..." and the URL. Then it waits. A `BROWSER` command that exits 1 gives the same result.                                                                                                                                                                                                          |
| URL it opens   | The provider authorize URL: `<sso-oidc endpoint>/authorize?response_type=code&client_id=...&redirect_uri=http%3A%2F%2F127.0.0.1%3A<port>%2Foauth%2Fcallback&state=<uuid>&code_challenge_method=S256&scopes=sso%3Aaccount%3Aaccess&code_challenge=...`. With the real service the endpoint is `https://oidc.<sso_region>.amazonaws.com` (source). |
| Listener       | `0.0.0.0:<random port>` (`HTTPServer(('', 0))`). This is every guest interface, not only the loopback. `--redirect-port 8000` binds `0.0.0.0:8000`.                                                                                                                                                                                              |
| Full fake flow | `BROWSER` shim in follow mode: "Successfully logged into Start URL", and token files in `~/.aws/sso/cache`.                                                                                                                                                                                                                                      |
| Callback page  | `200 text/html` "Amazon Web Services Sign-in". No `Set-Cookie`. The handler accepts a GET on any path.                                                                                                                                                                                                                                           |
| Wait           | 600 s in total (`AuthCodeFetcher._OVERALL_TIMEOUT`, source).                                                                                                                                                                                                                                                                                     |
| Device code    | `--use-device-code` opens `verificationUriComplete` with `BROWSER` after the first `authorization_pending` reply. It starts no listener.                                                                                                                                                                                                         |
| Print only     | `--no-browser` prints "Browser will not be automatically opened." and the URL. It still waits for the loopback callback.                                                                                                                                                                                                                         |

### gh and az (source only, not run)

- `gh auth login --web` uses the device flow for github.com. The launcher is `GH_BROWSER`,
  then the `browser` setting, then `BROWSER`, then `xdg-open`, `x-www-browser`, `www-browser`
  and `wslview`. Without a terminal it only prints "Open this URL to continue in your web
  browser: https://github.com/login/device". It never needs a loopback forward.
- `az login` on Linux uses the device code when Python `webbrowser.get()` finds no browser.
  This is true for every agent today. With `BROWSER` set, az uses the MSAL browser flow: a
  listener on `127.0.0.1:<random port>` (`0.0.0.0` inside Docker), the redirect
  `http://localhost:<port>`, and no timeout from az. So `BROWSER` moves az from the device code
  to a loopback redirect.

### Mac loopback rules (measured with Python sockets on this Mac)

- `localhost` resolves to `127.0.0.1` and `::1`.
- When another process holds `0.0.0.0:P` or `[::]:P`, a bind of `127.0.0.1:P` without
  `SO_REUSEADDR` fails with `EADDRINUSE`. With `SO_REUSEADDR` it succeeds and takes the
  connections from the other process.
- When another process holds `127.0.0.1:P`, a bind of `[::1]:P` succeeds. Then a browser that
  tries `::1` first reaches a different process than a browser that tries `127.0.0.1`.
- In the guest, `/proc/net/tcp` shows the owner of each listener, for example
  `0100007F:1F40 ... uid 1010` for kubelogin. The file is world-readable, so the guest service
  can check the owner without root.

### Consequences for the design

1. The shim must be `BROWSER` (AWS, az, gh) and `xdg-open` (kubelogin, gh).
2. The URL can be the provider (AWS) or the tool's own loopback page (kubelogin). For a
   loopback URL, the guest can read the first redirect and give the person the provider URL.
3. The forward port comes from `redirect_uri`. It is fixed (kubelogin 8000 or 18000) or random
   (AWS, az). Studio cannot change it, because the provider checks it and the token request
   repeats it.
4. The tools wait 180 s (kubelogin) or 600 s (AWS). The card must expire when the tool stops.
5. A Mac forward must hold the port on every address the browser can use. It must never use
   `SO_REUSEADDR`.

## 2. Design

### Flow

1. The tool runs the shim with the URL.
2. The shim sends the URL to the guest service on a Unix socket. The kernel gives the peer UID.
3. The guest service checks the URL, finds the chat and queues a request. The shim exits 0.
4. The Studio server long-polls the guest service and creates a `browser/open` request card in
   that chat.
5. The person approves. The server binds the Mac loopback port, tells the guest to arm the
   callback, and returns the URL to the renderer.
6. The renderer opens the URL with Electron `openExternal`.
7. The provider sends the browser to `http://127.0.0.1:<port>/...` or `localhost`. The server
   relays this one GET to the tool's listener in the guest and returns a cleaned response.
8. The forward closes after the first callback with `code` or `error`, after a timeout, or
   when the tool's listener goes away.

### (a) Guest shim and socket

- `/usr/local/bin/studio-browser` is a Python script that uses only the standard library. It
  takes one URL argument. It connects to `/run/codex-studio-browser/open.sock` and sends one
  JSON line `{"v":1,"url":URL,"session":$LAYR_SESSION}`. It reads one reply and prints one
  status line to stderr. It exits 0 when the request is queued and 1 otherwise. It does not
  wait for the person: the tool keeps its own timeout, and AWS starts its callback loop only
  after the launcher returns.
- `/usr/local/bin/xdg-open` is a symbolic link to the shim. `/usr/local/bin` comes before
  `/usr/bin` in the agent `PATH`, so the link also wins if `xdg-utils` is installed later.
- `agent_env` in `layr_agents.py` adds `BROWSER=/usr/local/bin/studio-browser`.
  `LAUNCH_ENV_NAMES` adds `BROWSER`, so a project secret cannot replace it.
- The guest service (user `studio`) listens on the socket. Its unit gets
  `RuntimeDirectory=codex-studio-browser` with mode 0755. The socket has mode 0666. The access
  control is `SO_PEERCRED`, as in `layr_admin.py`.
- Identity: the peer UID is the fact, and `session` is only a hint. The service reads
  `agent.context(session)` from the root broker and requires `uid == peer UID`. A worker UID
  belongs to one agent. A lead UID (`studio-p-*`) belongs to a project, and all leads of that
  project share it, so the hint selects the chat among them. A missing or wrong hint is
  refused. No token or secret is in the agent's reach.
- Limits per UID: 2 open connections, a 16 KB line, a 5 s read deadline, 1 pending request,
  and 6 requests in 10 minutes.

### URL policy

The guest checks the URL. The server checks it again before it makes the card, and Electron
keeps its own `http` and `https` rule.

- At most 8 KB, no control characters, no user or password part.
- `https` with a DNS host is accepted. An `https` URL with an IP literal in a private,
  link-local or loopback range is refused.
- `http` is accepted only for the hosts `localhost`, `127.0.0.1` and `[::1]`. Then the guest
  resolves the first hop. It checks that the listener on that port belongs to the peer UID,
  sends one `GET` without cookies (5 s limit, 64 KB limit), and requires a `3xx` reply whose
  `Location` passes the `https` rule. That `Location` becomes the URL to open. In the first
  slice, a loopback page that does not redirect is refused.
- Every other scheme is refused: `file`, `javascript`, `data`, `ftp`, `smb`, application
  schemes and custom schemes.
- Forward detection: when the final URL has a `redirect_uri` query value with the host
  `localhost`, `127.0.0.1` or `[::1]` and a port from 1024 to 65535, the request needs a
  forward on that port and path. A loopback `redirect_uri` without a port is refused. Other
  `redirect_uri` values need no forward (device code pages, for example).
- A test installation can allow `http` providers on the guest loopback with a guest service
  command line flag. Agents cannot set it.

### (b) Approval card

- The server polls `browser.wait` and writes one request for each queued item:
  `method: "browser/open"`, `agent: <chat agent ID>`, and the request ID `uuid5(bridgeId)`.
  A lost reply never makes a second card.
- `Requests.tsx` shows it as an approval card, next to `monitor/approve`:
  - "Open a sign-in page in your browser", with the provider host in large text.
  - The chat name (the existing card header) and the project.
  - When present: `client_id`, `scope`, and "The sign-in result goes to this chat's Linux user
    on port 8000."
  - The full URL, collapsed.
  - The buttons "Open in browser" and "Decline".
- Accept: the server binds the forward, calls `browser.answer`, and returns
  `{"status":"answered","openUrl":URL}`. The renderer then calls
  `window.codexDesktop.openExternal(openUrl)`. Today `openExternal` uses `activate: false`. This
  action should bring the browser to the front.
- Decline: the server calls `browser.answer` with `decline` and queues a message to the chat:
  "The person declined to open HOST."
- Expiry: the guest marks a request `stale` when its loopback listener goes away (it checks
  every 5 s) or after 10 minutes. The server then closes the card.

### (c) Loopback forward

- Owner: the Studio server process on the Mac. It already owns the guest channel. The Swift
  helper does not change.
- Transport: each callback is one guest JSON-RPC call, `browser.callback`, on the existing
  path `guest.sock` to vsock 4050. No raw TCP stream crosses the VM boundary. The guest
  service sends the GET on the guest loopback. The 2 MB frame limit holds a 256 KB body.
- Bind, before the guest lease is armed: `127.0.0.1:P` for `127.0.0.1`, `[::1]:P` for
  `[::1]`, and both for `localhost`. Never `SO_REUSEADDR`. If one bind fails, nothing opens.
  The card shows "Port P is in use on this Mac" and stays pending. The person can free the
  port and try again, or decline.
- Relay rules:
  - Only `GET`, and only the `redirect_uri` path. Every other request gets a Studio 404 page
    and never reaches the guest.
  - Send only `Host`, `User-Agent`, `Accept` and `Accept-Language` to the guest. Drop
    `Cookie`, `Authorization`, `Referer` and `Origin`.
  - Send the tool's status and body (at most 256 KB) to the browser. `Content-Type` is
    `text/html` or `text/plain` only. Drop `Set-Cookie` and `Location`. Add
    `Content-Security-Policy: sandbox; default-src 'none'; style-src 'unsafe-inline'; img-src data:`,
    `Cache-Control: no-store`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`
    and `Connection: close`. The tool's page cannot run a script at a Mac loopback origin.
  - The server never sends `browser.callback` again after a lost reply, because the tool can
    already have used the code. The browser gets "Result unknown. Check the chat."
- Guest side: `browser.callback` checks the lease (request, port, path and expiry). It checks
  again in `/proc/net/tcp` that the listener on `127.0.0.1:P`, `[::1]:P` or the wildcard
  belongs to the requester's UID. Then it connects to that address, sends the cleaned request,
  and returns the status, the content type and the body.
- Close: after the first relayed callback with `code` or `error` in the query, after 10
  minutes, after 5 relays, on decline, when the guest reports `stale`, or at server shutdown.
  A backend restart drops open forwards. The server then marks those cards closed and calls
  `browser.close`.
- Collisions:
  - A Mac program holds the port: nothing opens, as above. kubelogin can run again with
    `--listen-address 127.0.0.1:18000` when the provider has that redirect registered. AWS can
    use `--redirect-port`.
  - Two agents: the guest kernel lets only one UID listen on a guest port, so kubelogin moves
    to 18000, and AWS and az use random ports. On the Mac, the server keeps one forward per
    port. A second approval for a busy port waits until the first forward closes. The UID check
    stops an old lease from sending a code to another agent's listener.
- The long poll `browser.wait` holds one of the 16 guest operation slots.

### (d) Security

An agent can make the Mac do only this:

- Show an approval card in its own chat, within the limits above.
- After the person approves: open one `https` URL in the default browser, and receive one
  cleaned `GET` on one loopback port for at most 10 minutes.

An agent cannot:

- Open a scheme other than `https`. `http` works only through the loopback rule. No `file`,
  `javascript`, application or custom schemes.
- Open a Mac loopback URL. The only loopback traffic is the forward, for the approved port
  and path only.
- Bind a Mac port without an approval, bind a port below 1024, or take a port that a Mac
  program holds.
- Read the cookies of other `localhost` or `127.0.0.1` services on the Mac. Cookies are not
  separated by port, so the relay drops them. It also cannot set cookies.
- Run JavaScript at a loopback origin. The browser treats a loopback page as local, so its
  requests to other Mac loopback services skip the private network checks that apply to
  internet pages. The CSP sandbox stops this.
- Use the guest service to reach another agent's listener or another guest service (the owner
  UID check), or the network (the guest service connects only to the guest loopback).
- Act for another chat. The kernel peer UID decides. A lead can select only chats of its own
  project, and these chats already share one Linux user.
- Get a token from the socket. There is none. A hand-written client gets the same checks as
  the shim.

Remaining risks:

- Consent phishing. An agent can ask the person to sign in at any `https` provider with an
  OAuth client that the agent controls, and then receive a code for the person's account. The
  card shows the host, `client_id` and `scope`, and it says where the result goes. The token
  stays in that agent's Linux user. A provider allow list per project is an open decision.
- Tokens are per Linux user. A worker must sign in again in its own home. Other agents cannot
  read them (home mode 0700).
- AWS listens on `0.0.0.0` in the guest. The forward uses only the loopback, but the guest
  network can reach that port while the tool waits. This is AWS behaviour, not an exposure
  that the bridge adds.
- A short race between the owner check and the connect. An agent that wins it must hold the
  port and know the `state` value, or the tool rejects the code.

## 3. Changes, protocol and tests

### Files

| Area    | File                                                                               | Change                                                                                                                             |
| ------- | ---------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| Guest   | `workspaces/runtime/apps/vm-guest/browser_bridge.py` (new)                         | Socket server, UID and chat check, URL policy, first hop, queue on disk, lease, callback relay, `/proc/net/tcp` owner check.       |
| Guest   | `workspaces/runtime/apps/vm-guest/studio-browser` (new)                            | The shim.                                                                                                                          |
| Guest   | `service.py`                                                                       | Start the socket. Add `browser.wait` to `READ_METHODS`, and `browser.answer`, `browser.callback` and `browser.close` to `METHODS`. |
| Guest   | `layr_agents.py`                                                                   | `BROWSER` in `agent_env` and `LAUNCH_ENV_NAMES`.                                                                                   |
| Guest   | `install.sh`, `codex-studio-guest.service`                                         | Install the shim and the `xdg-open` link. `RuntimeDirectory=codex-studio-browser` and `RuntimeDirectoryMode=0755`.                 |
| Guest   | `test_browser_bridge.py` (new), `README.md`                                        | Unit checks and the contract text.                                                                                                 |
| Server  | `workspaces/runtime/apps/server/src/codex_browser_bridge.py` (new)                 | Poll thread, request records, Mac binds, HTTP relay, close rules.                                                                  |
| Server  | `codex_runtime.py`                                                                 | `browser/open` branch in `answer()`. Start and stop the bridge with the VM.                                                        |
| Web     | `workspaces/client/apps/web/src/components/questions/Requests.tsx`                 | The card. Open `openUrl` after a successful answer.                                                                                |
| Desktop | `workspaces/client/apps/desktop/main.cjs`                                          | Bring the browser to the front for this open.                                                                                      |
| Helper  | `workspaces/client/apps/desktop/native/linux-vm/main.swift`                        | No change. A helper change needs a VM restart, and a VM restart stops every agent.                                                 |
| Docs    | `docs/linux-vm-workspaces.md`, `.agents/skills/codex-workspace/references/layr.md` | The method table. One generic paragraph for agents: run the sign-in command; the person approves the link.                         |

### Protocol

Shim to guest service (Unix socket, one JSON line in each direction):

```json
{"v": 1, "url": "http://localhost:8000/", "session": "<LAYR_SESSION>"}
{"ok": true, "id": "<bridgeId>", "host": "accounts.example.com"}
{"ok": false, "code": "invalid_url", "message": "Only https links and loopback redirects can open"}
```

The error codes are `invalid_url`, `unknown_agent`, `limit` and `unavailable`.

Server to guest (the existing JSON-RPC on vsock 4050):

| Method             | Params                                         | Result                                                                                                                                            | Retry after a lost reply                        |
| ------------------ | ---------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------- |
| `browser.wait`     | `after` (sequence), `waitSeconds` (at most 20) | `items` (`seq`, `id`, `agentId`, `projectId`, `uid`, `url`, `host`, `redirect` with `host`, `port`, `path` or null, `state`, `expiresAt`), `next` | Yes                                             |
| `browser.answer`   | `id`, `decision` (`approve` or `decline`)      | `state`, `lease` (`port`, `path`, `expiresAt`); error `stale` when the listener went away                                                         | Yes. The same decision returns the same result. |
| `browser.callback` | `id`, `target` (path and query), `headers`     | `status`, `contentType`, `body` (base64), `done`                                                                                                  | No                                              |
| `browser.close`    | `id`, `reason`                                 | `state`                                                                                                                                           | Yes                                             |

Server to renderer: the request record above. `/api/answer` returns `openUrl` on accept.

### Tests

- Guest unit tests (`test_browser_bridge.py`):
  - URL table. Accept an `https` provider, and kubelogin's loopback URL with a fake first hop.
    Refuse `file:`, `javascript:`, `data:`, a custom scheme, a user part, `http` to another
    host, a loopback URL to another port, a loopback `redirect_uri` without a port, a port
    below 1024, and 8 KB plus one byte.
  - Identity: a valid hint, the hint of another UID, no hint, a lead hint from another project.
  - Limits: a second pending request, the rate limit, a slow client.
  - Owner check with fixture `/proc/net/tcp` text: own UID, other UID, wildcard listener, no
    listener.
  - Lease: two `browser.answer` calls give one lease. `browser.callback` after `done` is
    refused.
- Server tests:
  - The same `bridgeId` after a lost reply gives one card.
  - A Mac port held on `0.0.0.0`, `[::]`, only `127.0.0.1`, or only `::1`: nothing opens and
    the card stays pending.
  - Relay: drops `Cookie` and `Authorization`, drops `Set-Cookie` and `Location`, adds the CSP
    sandbox, refuses `POST` and other paths, closes after the first `code`, closes at the
    timeout, and never sends a callback again after a lost reply.
- Web test: the card shows the host, the chat and the port. Accept calls `openExternal` with
  `openUrl`. Decline does not.
- End to end (opt-in, on an isolated VM, like `test_layr_e2e.py`): the fake OIDC server from
  these probes runs in the guest, with pinned kubelogin and AWS CLI builds. A scripted HTTP
  client on the Mac takes the place of the browser, so no browser window opens and no
  application becomes active.
  - kubelogin: the card shows the fake provider host after the first hop. The client follows
    the approved URL through the forward. The fake records one token request. With an
    RSA-signed fake ID token, `get-token` exits 0 and prints an `ExecCredential`.
  - AWS: "Successfully logged into Start URL" and a token file.
  - Negative: another agent user listens on the port, so the callback is refused. A Mac
    process holds the port, so nothing opens.
  - A fake run does not prove Gardener or AWS. One manual sign-in with the person on each
    provider completes the check.

## 4. First slice

Goal: `kubectl oidc-login` for Gardener and `aws sso login`, on the Mac that runs the VM.

1. The shim, the `xdg-open` link, and `BROWSER` in `agent_env`.
2. The guest socket, the URL policy with the loopback first hop, the queue, the four
   `browser.*` methods and the owner check.
3. The server poll thread, the `browser/open` card, the strict Mac binds, the HTTP relay, the
   close rules and the decline message.
4. The card in `Requests.tsx` with host, chat and port, and the Electron open.
5. The guest unit tests, the server tests and the fake end-to-end run for both tools.

Not in the first slice: provider allow lists, remembered approvals, loopback pages without a
redirect, remote clients, and a vsock port in the Swift helper.

Rollout: a guest upgrade and a backend restart. The guest service restart keeps providers.
There is no VM restart. A running provider keeps its old environment. The `xdg-open` link
works at once for kubelogin. AWS needs `BROWSER` in a new provider, or
`BROWSER=studio-browser aws sso login` in the command.

## Open decisions

1. A provider allow list per project (for example `accounts.sap.com`, `*.awsapps.com`,
   `oidc.*.amazonaws.com`), or one approval for each link as in this design.
2. Remote clients: a paired server window opens the browser on another Mac, and the redirect
   cannot reach this Mac. Refuse forwards there, or move the forward listener to the client.
3. Bring the browser to the front for this open, or keep the current `activate: false`.
4. Loopback pages without a redirect (a tool that serves its own start page): refuse them, or
   open them through the forward with the CSP sandbox.
5. Whether the Gardener provider offers the device code. If it does, the agent can use
   `--grant-type=device-code`, and the card needs no forward.
6. Tokens: one sign-in for each Linux user (this design), or a token store per project.
7. Check that the shim's Unix socket connect works in the Codex sandbox when network access is
   off. The sign-in commands need network, so they already run outside that sandbox.
