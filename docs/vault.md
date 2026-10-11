# Project vault: encrypted copies in AWS

The vault keeps the latest state of a Studio project, and its history, in an S3 bucket
that the user owns. Only the user can read it: the data is encrypted in the VM with the
public key of the vault, and only the user's passkeys and paper key open the private key.
The Mac and the VM are working copies; the vault is the copy that stays.

## What it gives

- A web page (static site and one Lambda function in the same AWS account) shows the latest
  state of the project and any earlier one, and downloads files. The browser decrypts them.
- `vault-restore` rebuilds any state into a folder with the paper key.
- Each project can use its own AWS account and bucket (one per client).

## Keys

| Key                                                    | Where                                             | Can                   |
| ------------------------------------------------------ | ------------------------------------------------- | --------------------- |
| Vault identity (`age` X25519)                          | nowhere in plain form; inside each slot           | decrypt everything    |
| Vault recipient (`age1…`)                              | VM vault configuration, `vault.json`, Lambda      | encrypt new data      |
| Paper key (`age` X25519 identity, `AGE-SECRET-KEY-1…`) | on paper, two copies                              | open the paper slot   |
| Passkey (WebAuthn PRF)                                 | the user's passkey provider                       | open its passkey slot |
| Writer AWS key                                         | VM, root only (`layr-admin/vault/<project>.json`) | put new objects only  |

- A slot is the vault identity encrypted with `age` to the recipient of one opener:
  `slots/paper.age` for the paper key, `slots/passkey-<credential id>.age` for each passkey.
  The PRF output of a passkey (32 bytes, salt `layr-vault/v1`) is the X25519 secret of its
  recipient. Any number of slots can exist; removing a slot object revokes that opener.
- The paper key and the `age` tool decrypt every object without this software:
  `age -d -i paper.txt slots/paper.age > vault.txt`, then `age -d -i vault.txt <object>`.
- The VM has the recipient only, so agents and a stolen VM disk cannot read the vault.
- The Lambda proves that a caller holds the vault identity: it returns a token encrypted to
  the vault recipient, and accepts the decrypted token (an HMAC with a time) for 10 minutes.
  There is no user database.

## Objects

Bucket prefix `<project>/`:

- `vault.json`: `{"format": 1, "recipient": "age1…", "created": <epoch>}` (plain).
- `slots/*.age`: see above (public through the site; each is encrypted).
- `states/<seq>.<attempt>.tree.age`: the tree of one archived state (the commit point).
- `states/<seq>.<attempt>.<part>.data.age`: data frames of that state.

`seq` is a 10-digit counter per project; `attempt` is 8 random hex digits, so a retried push
never collides with the parts of a failed one. Every put uses `If-None-Match: *`; objects
are never changed or deleted by the writer. The latest state is the highest `seq` that has a
tree object; every reference names its `seq` and `attempt`, so a second tree for one `seq`
(after a lost reply) is harmless.

### Tree object

Plaintext: one header line of JSON, a newline, then one zstd frame.

```json
{"v": 1, "seq": 12, "state": "<layr state id>", "time": 1791700000,
 "tree": "full" | "delta", "base": [11, "0a9b8c7d"], "parts": 2, "attempt": "1f2e3d4c"}
```

The frame holds the canonical tree JSON: `{"files": {"<path>": [mode, size, sha256, ref]}}`
with sorted keys. `tree: "delta"` means the frame is compressed with the canonical tree of
`base` (`[seq, attempt]` of the previous push) as its zstd prefix (`zstd --patch-from`). A full tree is written at
least every 32 seq.

`ref` is `[seq, attempt, part, offset, length, kind, base_ref]`: the bytes at `offset` and
`length` of the decrypted data part are one zstd frame of the file content. `kind` is
`"full"`, or `"delta"`, compressed with the content of the `base_ref` version as its prefix.
A delta chain is at most 16 versions long. Mode is octal text (`"100644"`, `"100755"`,
`"120000"` for a symbolic link, whose content is its target).

### Data part

The concatenated frames. A part holds up to 16 MiB of frames; a larger frame has its own part.

## Writer (VM)

`vault.push` in the root broker, for one project:

1. Read the main head state. Without changes since the last push, stop.
2. The base folder `layr-admin/vault/<project>/base` is a `layr export` of the last pushed
   state. `layr diff --name-status <last> <head>` lists the changed paths; the first push
   takes every file.
3. For a changed file: its new content from `layr show <head>:<path>`; a delta against the base
   folder when the previous version exists and the delta is smaller; else a full frame.
4. Encrypt each object with `age -r <recipient>` and put the parts, then the tree.
5. Update the base folder with `layr export <head>` and record the pushed state.

The writer reads its last `seq` from `ListObjectsV2` (`states/`, names only) when its record is
missing, and then starts with a full tree and full files.

## Reader

The browser and `vault-restore` share one implementation (`workspaces/runtime/apps/vault`):
`age-encryption` (typage) and `@bokuweb/zstd-wasm` (`decompressUsingDict` takes the base as a
raw dictionary). Reading the latest tree fetches the tree objects back to the last full tree;
reading a file fetches the parts of its delta chain.

## AWS (one stack per project, `vault-cloud/template.yaml`)

- Bucket with versioning; public access blocked.
- Writer user: `s3:PutObject` on `<project>/states/*` with `If-None-Match` required,
  `s3:ListBucket` on `<project>/states/`; nothing else.
- Lambda with a function URL: `GET /slots`, `GET /challenge`, `POST /presign`, `PUT /slot`.
- CloudFront: the site and `slots/*`.

## Measurements behind the format

On the codex-agents history (1,095 states), one run each: btrfs send streams with zstd took
100.8 MB; content-defined chunks 92 to 116 MB in 17,000 to 21,000 objects; file deltas against
the previous version with tree deltas, one object per state, 25.7 MB (zstd -3) and 23.9 MB
(zstd -19). `git gc --aggressive` took 23.9 MB.

## Mac view

`vault-view` serves the latest state read-only over WebDAV on localhost, and Finder mounts it
(Go > Connect to Server). Files download and decrypt when they are opened and stay in a local
cache. It uses the same reader as the site; the Mac only reads the vault.

## Set up a vault for a project

1. The user, in a terminal of their own (never through an agent, so the paper key stays out of
   transcripts): `node workspaces/runtime/apps/vault/bin/vault-keys.mjs <prefix>`. It shows the
   paper key once; write it down twice. `~/.config/studio-vault/<prefix>/` keeps only the
   recipient and the encrypted paper slot.
2. `aws sso login --profile <profile>`, `pnpm --filter studio-vault-web run build`, then, with the
   Studio Python first on `PATH`: `node workspaces/runtime/apps/vault/bin/vault-deploy.mjs
--project <prefix> --profile <profile> --region eu-central-1 --folder <project folder>`. It
   makes the stack, uploads the site, the paper slot and `vault.json`, and pipes a write-only key
   to the VM (`codex_vault.py configure`). It prints the site address.
3. The root broker pushes every configured project every 10 minutes; `codex_vault.py push` pushes
   at once, and `codex_vault.py status` shows the last push and the last error.
4. On the site, unlock with the paper key and add a passkey for each device.
5. Mac: `vault-view.mjs --project <prefix> --mount` mounts the latest state in Finder;
   `vault-restore.mjs --project <prefix> --out <folder> [--seq <n>]` rebuilds a state.

`tests/live.e2e.mjs` in `studio-vault-web` checks a deployed test vault end to end in headless
Chromium (paper key, download, passkey enrollment and unlock with a virtual PRF authenticator).

## Not in the first slice

A company key slot, Object Lock defaults, a custom domain, garbage collection.

## Sidebar

The project menu of an imported layr project shows its vault:

- A label: when the VM last saved a change (`Vault: saved 5 min ago`), `the last push failed`
  with the reason as its tooltip, or `not set up`.
- **Open in Desk** opens `<site><prefix>/` in the browser.
- **Pull…** writes the latest vault state into a new folder `<prefix> <date time>` below a
  folder that the user picks (Downloads without the desktop app). Only the Mac that holds the
  project's folder shows it. The server runs `layr-desk pull`; the vault key must be kept on
  this Mac once with `layr-desk unlock`, otherwise the pull reports that command. One request
  id covers the whole pull (`POST /api/vault/pull` reports the same pull again), so a lost
  reply never starts a second one.
- **Set up vault…** opens the layr-desk setup steps when the project has no vault.

- **Unlock on this Mac…** asks for the paper words of a vault whose key this Mac does not keep
  yet. The words go to this Mac's server and to `layr-desk unlock --paper-file -` on stdin;
  no reply, log or error repeats them. The Keychain then keeps the vault key.
- **Show in Finder** opens the vault's folder on the Desk disk (`~/Desk/<prefix>`).

Studio carries `layr-desk` (`Resources/workspace/bin`, built from the vendored source in
`workspaces/runtime/apps/layr-desk` by the package script), so nothing is installed by hand.
When a vault has a Desk site, the server keeps the Desk disk agent (`layr-desk service`) on
this Mac for that site with the app's own binary, and asks `layr-desk status` which vaults
this Mac keeps a key for. A `layr-desk` outside the app (a development build on `PATH`) only
reports; it never replaces the agent.

The server asks the VM for `vault.status` of every imported project every 2 minutes, outside
the runtime lock, and keeps the report in `layr-projects/<id>/vault.json`. The sidebar reads
only that record; the time of a check stays out of it, so a quiet vault does not change the
project entity. The Desk site comes from the writer configuration (`site`) or from
`codex_vault.py site PROJECT_FOLDER https://desk.example.com/` and stays in
`layr-projects/<id>/vault-site.json`. Studio finds `layr-desk` through `STUDIO_LAYR_DESK_BIN`,
`PATH` or `~/.cargo/bin`.
