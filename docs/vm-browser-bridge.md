# VM browser bridge

Status: the first slice is implemented (sections 2 to 4). The measurements are from
2026-10-11, against `origin/main` at `07ea99c7`.

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
   callback, and returns the URL to the window.
6. The window opens the URL with the desktop action `openSignIn`.
7. The provider sends the browser to `http://127.0.0.1:<port>/...` or `localhost`. The server
   relays this one GET to the tool's listener in the guest and returns a cleaned response.
8. The forward closes after the first callback with `code` or `error`, after a timeout, or
   when the tool's listener goes away.

### (a) Guest shim and socket

- `studio_browser.py` (installed as `/usr/local/bin/studio-browser`) is a Python script that
  uses only the standard library. It takes one URL argument. It connects to
  `/run/codex-studio-browser/open.sock` and sends one JSON line
  `{"v":1,"url":URL,"session":$LAYR_SESSION}`. It reads one reply and prints one status line to
  stderr. It exits 0 when the request is queued and 1 otherwise. It does not wait for the
  person: the tool keeps its own timeout, and AWS starts its callback loop only after the
  launcher returns.
- `/usr/local/bin/xdg-open` is a symbolic link to the shim. `/usr/local/bin` comes before
  `/usr/bin` in the agent `PATH`, so the link also wins if `xdg-utils` is installed later.
- `agent_env` in `layr_agents.py` adds `BROWSER=/usr/local/bin/studio-browser`.
  `LAUNCH_ENV_NAMES` adds `BROWSER`, so a project secret cannot replace it.
- The guest service (user `studio`) listens on the socket (`browser_bridge.py`). Its unit gets
  `RuntimeDirectory=codex-studio-browser` with mode 0755. The socket has mode 0666. The access
  control is the kernel peer UID (`SO_PEERCRED`), as in `layr_admin.py`.
- Identity: the peer UID is the fact, and `session` is only a hint. The service reads
  `agent.context(session)` from the root broker and requires `uid == peer UID`. A worker UID
  belongs to one agent. A lead UID (`studio-p-*`) belongs to a project, and all leads of that
  project share it, so the hint selects the chat among them. A missing or wrong hint is
  refused. No token or secret is in the agent's reach.
- Limits: 2 open connections for each UID, a 16 KB line, a 5 s read deadline, and 6 links in
  10 minutes for each UID. The newest link of a chat replaces its pending link.
- The queue is a private JSON file in the guest state (`browser/requests.json`), so it
  survives a guest service restart. Each item has a sequence number that changes with its
  state: `pending`, `approved`, `opened`, `declined`, `replaced`, `stale`, `expired` or
  `closed`.

### URL policy

`browser_policy.py` is the one rule set. The guest checks the URL. The server imports the same
file from the guest folder and checks the URL again before it makes the card. Electron keeps
its own `http` and `https` rule.

- At most 8 KB, no control characters or spaces, no user or password part.
- `https` with a DNS host is accepted. An `https` URL to `localhost`, `*.localhost` or an IP
  literal in a private, link-local, loopback, multicast or reserved range is refused.
- `http` is accepted only for the hosts `localhost`, `127.0.0.1` and `[::1]` with a port from
  1024 to 65535. Then the guest resolves the first hop. It checks that the listener on that
  port belongs to the peer UID, sends one `GET` without cookies (5 s limit, 64 KB limit), and
  requires a `3xx` reply whose `Location` passes the `https` rule. That `Location` becomes the
  URL to open. A loopback page that does not redirect is refused.
- Every other scheme is refused: `file`, `javascript`, `data`, `ftp`, `smb`, application
  schemes and custom schemes.
- Forward detection: when the final URL has a `redirect_uri` query value with the host
  `localhost`, `127.0.0.1` or `[::1]`, the request needs a forward on that port and path. The
  redirect must use `http` and a port from 1024 to 65535. Other `redirect_uri` values need no
  forward (device code pages, for example).
- The guest service flag `--test-http-provider HOST:PORT` lets a test provider use `http`.
  Agents cannot set it.

### (b) Approval card

- The server polls `browser.wait` (`codex_browser_bridge.py`) and writes one request for each
  new `pending` item: `method: "browser/open"`, `agent: <chat agent ID>`, and the request ID
  `uuid5(bridgeId)`. A lost reply or a second poll never makes a second card. The server
  refuses a link of an unknown, deleted or native chat, a link of another project, and a link
  that fails the URL policy, and closes it in the guest.
- `Requests.tsx` shows it as an approval card:
  - "Open a sign-in page in your browser", with the provider host in large text.
  - The chat name (the card header), and the `client_id` and `scope` when present.
  - "The sign-in result goes to this chat's Linux user (USER) on port P of this Mac.", or
    "This page needs no forward." for a link without a loopback redirect.
  - The full URL, collapsed. The card has no direct link.
  - The buttons "Decline" and "Open in browser".
- Accept: the server binds the forward, calls `browser.answer`, and returns
  `{"status":"answered","openUrl":URL}`. The window then calls
  `window.codexDesktop.openSignIn(openUrl)`. Electron opens the link with `activate: true`, so
  the browser comes to the front; `openExternal` keeps `activate: false` for other links. The
  open follows the server answer, so the usual 1.2 second click window can pass. `openSignIn`
  therefore requires a click or key press of the person in the last 30 seconds. A Studio page
  in a plain browser opens a new tab.
- A remote window: only a window on the Mac that runs the server can accept. A request with a
  paired-server signature, with Tailscale Serve headers, or from another address gets "Open
  this chat in the window of the Mac that runs its server to sign in, or use the tool's
  device-code mode." Nothing binds and nothing opens. Decline works from every window.
- Decline: the server calls `browser.answer` with `decline` and queues a message to the chat:
  "The person declined to open HOST in the browser. ..."
- Expiry: the guest marks a request `stale` when its loopback listener goes away (it checks
  every 5 s), `expired` after 10 minutes, and `replaced` by a newer link of the chat. The
  server then marks a pending card `expired`. A backend restart and a stopped chat also
  expire pending cards (the existing request rules).

### (c) Loopback forward

- Owner: the Studio server process on the Mac. It already owns the guest channel. The Swift
  helper does not change.
- Transport: each callback is one guest JSON-RPC call, `browser.callback`, on the existing
  path `guest.sock` to vsock 4050. No raw TCP stream crosses the VM boundary. The guest
  service sends the GET on the guest loopback. The 2 MB frame limit holds a 256 KB body.
- Bind, before the guest lease is armed: `127.0.0.1:P` for `127.0.0.1`, `[::1]:P` for
  `[::1]`, and both for `localhost`. Never `SO_REUSEADDR`. If one bind fails, nothing stays
  bound and nothing opens. The card stays pending with "Port P is in use on this Mac. Free it
  and approve again, or decline."
- Relay rules:
  - Only `GET`, and only the `redirect_uri` path. Every other request gets a Studio page
    (404 or 405) and never reaches the guest.
  - Send only `User-Agent`, `Accept` and `Accept-Language` to the guest. The guest sets
    `Host` from the redirect. `Cookie`, `Authorization`, `Referer` and `Origin` never pass.
  - Send the tool's status and body (at most 256 KB) to the browser. `Content-Type` is
    `text/html` or `text/plain` only. A `3xx` reply becomes a Studio page. Studio writes every
    header, so no `Set-Cookie` or `Location` of the tool passes. It adds
    `Content-Security-Policy: sandbox; default-src 'none'; style-src 'unsafe-inline'; img-src data:`,
    `Cache-Control: no-store`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`
    and `Connection: close`. The tool's page cannot run a script at a Mac loopback origin.
  - After a lost reply, the server never sends `browser.callback` again, because the tool can
    already have used the code. The browser gets "The sign-in result is unknown. Check the
    chat in Studio." The guest also keeps a receipt for each request ID.
- Guest side: `browser.callback` checks the lease (request, port, path and expiry) and the
  relay count. It checks again in `/proc/net/tcp` that every listener on the port that a
  loopback connection can reach (`127.0.0.1`, `::1` or a wildcard) belongs to the requester's
  UID. Then it sends the cleaned GET with `http.client` and returns the status, the content
  type and the body. A query with `code` or `error` closes the request.
- Close: after the first relayed callback with `code` or `error`, after 10 minutes, after 5
  relays, on decline, when the guest reports a final state, or at server shutdown. The
  backend process ends its forwards when it stops, and the guest lease ends at its deadline
  or when the tool stops listening.
- Collisions:
  - A Mac program holds the port: nothing opens, as above. kubelogin can run again with
    `--listen-address 127.0.0.1:18000` when the provider has that redirect registered. AWS can
    use `--redirect-port`.
  - Two agents: the guest kernel lets only one UID listen on a guest port, so kubelogin moves
    to 18000, and AWS and az use random ports. On the Mac, the server keeps one forward per
    port. A second approval for a busy port gets "Another sign-in uses port P on this Mac.
    Wait until it ends, then approve again." The UID check stops an old lease from sending a
    code to another agent's listener.
- A link without a loopback redirect (a device code page) needs no forward. Its approval
  moves the guest request to `opened`.
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
- Open a link from a remote window. Only a window on the Mac that runs the server accepts.

Remaining risks:

- Consent phishing. An agent can ask the person to sign in at any `https` provider with an
  OAuth client that the agent controls, and then receive a code for the person's account. The
  card shows the host, `client_id` and `scope`, and it says where the result goes. The token
  stays in that agent's Linux user. Each link needs its own approval; there is no provider
  allow list.
- Tokens are per Linux user. A worker must sign in again in its own home. Other agents cannot
  read them (home mode 0700).
- AWS listens on `0.0.0.0` in the guest. The forward uses only the loopback, but the guest
  network can reach that port while the tool waits. This is AWS behaviour, not an exposure
  that the bridge adds.
- A short race between the owner check and the connect. An agent that wins it must hold the
  port and know the `state` value, or the tool rejects the code.

## 3. Changes, protocol and tests

### Files

| Area    | File                                                                                                       | Change                                                                                                                      |
| ------- | ---------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------- |
| Guest   | `workspaces/runtime/apps/vm-guest/browser_policy.py` (new)                                                 | The URL rules. The server imports the same file.                                                                            |
| Guest   | `workspaces/runtime/apps/vm-guest/browser_bridge.py` (new)                                                 | Socket, peer UID and chat check, first hop, queue, lease, callback relay, `/proc/net/tcp` owner check.                      |
| Guest   | `workspaces/runtime/apps/vm-guest/studio_browser.py` (new)                                                 | The shim.                                                                                                                   |
| Guest   | `service.py`                                                                                               | Starts the socket. `browser.wait` is a read method; `browser.answer`, `browser.callback` and `browser.close` keep receipts. |
| Guest   | `layr_agents.py`                                                                                           | `BROWSER` in `agent_env` and `LAUNCH_ENV_NAMES`.                                                                            |
| Guest   | `install.sh`, `codex-studio-guest.service`                                                                 | Install the shim and the `xdg-open` link. `RuntimeDirectory=codex-studio-browser`, mode 0755.                               |
| Guest   | `test_browser_bridge.py`, `test_browser_fakes.py` (new), `test_project_secrets.py`                         | Guest checks and the shared fakes.                                                                                          |
| Server  | `workspaces/runtime/apps/server/src/codex_browser_bridge.py` (new)                                         | Poll thread, cards, answer, strict binds, HTTP relay, close rules.                                                          |
| Server  | `codex_runtime.py`, `studio_api/work/router.py`, `studio_api/work/models.py`                               | Tick and stop of the bridge; `answer(..., local_window=)`; `openUrl` in the answer result.                                  |
| Server  | `test_codex_browser_bridge.py` (new)                                                                       | Server checks and the fake end-to-end flows.                                                                                |
| Web     | `Requests.tsx`, `requestData.ts`, `openSignIn.ts`, `request-questions.css`, `desktop.ts`, `generated/api*` | The card, the open after the answer.                                                                                        |
| Web     | `tests/questions/browser-open-approval-ui.spec.mjs`, `openSignIn.test.ts` (new), `requestData.test.ts`     | Card and open checks.                                                                                                       |
| Desktop | `main.cjs`, `preload.cjs`, `bridge.d.ts`, `test.mjs`                                                       | The `openSignIn` action.                                                                                                    |
| Helper  | `workspaces/client/apps/desktop/native/linux-vm/main.swift`                                                | No change.                                                                                                                  |
| Docs    | `docs/linux-vm-workspaces.md`, `.agents/skills/codex-workspace/references/layr.md`                         | The method table, and one generic paragraph for agents.                                                                     |

### Protocol

Shim to guest service (Unix socket, one JSON line in each direction):

```json
{"v": 1, "url": "http://localhost:8000/", "session": "<LAYR_SESSION>"}
{"ok": true, "id": "<bridgeId>", "host": "accounts.example.com"}
{"ok": false, "code": "invalid_url", "message": "Only https sign-in links and loopback sign-in pages can open"}
```

The error codes are `invalid_request`, `invalid_url`, `unknown_agent`, `forbidden`, `stale`,
`unavailable`, `limit` and `internal`.

Server to guest (the existing JSON-RPC on vsock 4050):

| Method             | Params                                         | Result                                                                                                                                                      | Retry after a lost reply                       |
| ------------------ | ---------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------- |
| `browser.wait`     | `after` (sequence), `waitSeconds` (at most 20) | `items` (`seq`, `id`, `agentId`, `projectId`, `uid`, `linuxUser`, `url`, `startUrl`, `host`, `redirect`, `clientId`, `scope`, `state`, `expiresAt`), `next` | Yes                                            |
| `browser.answer`   | `id`, `decision` (`approve` or `decline`)      | `state`, `lease` (`host`, `port`, `path`, `expiresAt`) or null; error `stale` when the tool stopped                                                         | Yes. The same request ID replays the receipt.  |
| `browser.callback` | `id`, `target` (path and query), `headers`     | `status`, `contentType`, `body` (base64), `done`                                                                                                            | No. A new request ID for each browser request. |
| `browser.close`    | `id`, `reason`                                 | `state`                                                                                                                                                     | Yes                                            |

Server to window: the request record above. `/api/answer` returns `openUrl` for an accepted
`browser/open` card, from a local window only.

### Tests

Run them one at a time:

```sh
python3 -B -m unittest discover -s workspaces/runtime/apps/vm-guest -p test_browser_bridge.py -v
python3 -B workspaces/runtime/apps/server/src/test-server.py --filter test_codex_browser_bridge
pnpm --filter codex-agents-web exec vitest run --config vitest.unit.config.ts src/components/questions
pnpm --filter codex-agents-web exec playwright test --config playwright.config.ts --project=client tests/questions/browser-open-approval-ui.spec.mjs
```

What each check proves:

- `test_browser_bridge.py` (guest, macOS and Linux):
  - `Policy`: the accepted link shapes of kubelogin, AWS and device pages, and 28 refused
    links (schemes, user parts, `http` to a remote host, private and loopback `https`, bad
    loopback redirects, length and control characters).
  - `AgentEnvironment`: every agent process gets `BROWSER`, and no project secret can take
    the name.
  - `Listeners`: `/proc/net/tcp` and `tcp6` rows give the owner UID; any other UID on a
    reachable address refuses the port. On Linux, the real table names the test process.
  - `Bridge`, with the real shim run through `xdg-open` or `$BROWSER` against a real socket:
    the kubelogin first hop yields the provider page without a code being used; the AWS link
    queues as is; the lease is idempotent; only the redirect path passes; the relay drops
    `Cookie` and `Authorization`; a second callback after the code is refused, so the
    provider gets one token request; missing, unknown and other-UID sessions are refused; a
    port of another UID and a page without a redirect are refused; the rate limit and the
    replacement work; the long poll wakes on a new link; a stopped tool makes the request
    `stale`; decline, device links (`opened`), expiry and a restart of the queue work.
- `test_codex_browser_bridge.py` (server):
  - `Flow`: the whole path with the real guest bridge, the real shim, a fake provider and a
    test HTTP client as the browser. kubelogin style: `xdg-open`, port 8000 when it is free,
    the tool's own start page. AWS style: `$BROWSER`, a random port, the provider link. Each
    flow ends with one code at the tool, one token request at the provider, no Mac cookie at
    the tool, the CSP sandbox and no `Set-Cookie` or `Location` at the browser, the forward
    closed, and the card answered. Also: a remote window cannot accept and nothing binds;
    decline queues the chat message; a stopped tool expires the card; a card appears once
    and follows the guest state.
  - `Refusals`: six bad items get no card and are closed in the guest; one good item gets one
    card.
  - `Relay`: `POST`, other paths and path tricks never reach the tool; only the allowed
    headers pass; the content type is reduced to text; the forward closes after 5 relays; a
    lost relay reply closes the forward with a 502 page.
  - `Binds`: a port held on `0.0.0.0`, `[::]`, only `127.0.0.1` or only `::1` refuses the
    forward and leaves nothing bound; `localhost` binds both addresses.
  - `Wiring`: `Runtime.answer` sends `browser/open` to the bridge with the window flag; only a
    direct local request is a local window; the VM payload carries the three guest files, the
    shim installation and the socket folder.
- `openSignIn.test.ts` (web): the desktop uses `openSignIn`, an older desktop falls back to
  `openExternal`, a plain browser opens a tab without an opener, and other schemes never open.
- `requestData.test.ts` and `browser-open-approval-ui.spec.mjs` (web): the card shows the
  host, the client, the access, the port and the Linux user; it has no direct link; nothing
  opens before the approval; accept opens only the returned link with `openSignIn`; a server
  refusal (the remote window message) opens nothing and shows the error; decline sends one
  decline.
- `test.mjs` (desktop, hidden Electron): `openSignIn` refuses non-http schemes and opens with
  `activate: true`, while `openExternal` keeps `activate: false`. In a source checkout this
  test cannot start at `origin/main`: its `resources` path is the `apps` folder, so it reports
  "Desktop assets are missing".

What only a live run can prove:

- A real provider: Gardener's identity provider with `kubectl oidc-login`, and AWS IAM
  Identity Center with `aws sso login`. Each needs one manual sign-in with the person.
- A real browser on the Mac: Safari or Chrome with a `localhost` redirect, `::1` and
  `127.0.0.1` behaviour, and the browser coming to the front.
- The installed VM: the `RuntimeDirectory`, the socket mode, `SO_PEERCRED` across agent users,
  the agent `PATH` and `BROWSER` in a new provider, and the provider sandbox (see below).
- The installed desktop app with the `openSignIn` action.

## 4. First slice and rollout

The first slice covers the shim, the guest socket and methods, the server poll, card, answer
and forward, the web card and the desktop action. It makes `kubectl oidc-login` and
`aws sso login` work from a chat on the Mac that runs the VM, as far as fakes can show.

Rollout:

- The guest files reach the VM through the existing guest upgrade
  (`upgrade-linux-vm-layr.py`). It restarts the VM only when no provider is active. The Swift
  helper does not change.
- The server and the web need a backend update. The API schema hash changes (`openUrl`), so
  the renderer and the backend update together. `openSignIn` needs a desktop app update.
  Without it, the window falls back to `openExternal` (browser in the background).
- A running provider keeps its old environment. The `xdg-open` link works at once for
  kubelogin. AWS needs `BROWSER` in a new provider, or `BROWSER=studio-browser aws sso login`
  in the command.

## Decisions

1. One approval for each link. There is no provider allow list.
2. A remote window refuses to open with "Open this chat in the window of the Mac that runs its
   server to sign in, or use the tool's device-code mode." Nothing is forwarded.
3. Only this action brings the browser to the front (`openSignIn`); `openExternal` keeps its
   behaviour.
4. A loopback start page without a redirect is refused.
5. Tokens stay with the Linux user. There is no token store.

## Sandbox note

The Codex workspace sandbox without network access can block the shim's Unix socket
connect. This was not measured, and the sandbox does not change. The sign-in commands need
network, so they already run where network is allowed, and the shim works there.
