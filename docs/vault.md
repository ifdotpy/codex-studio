# Project vault: encrypted copies in AWS

The vault keeps the latest state of a Studio project, and its history, in an S3 bucket
that the user owns. Only the user can read it: the data is encrypted in the VM with the
public key of the vault, and only the user's passkeys and paper key open the private key.
The Mac and the VM are working copies; the vault is the copy that stays.

## What it gives

- Desk, a Finder-like web page on the gateway (one small server in the user's tailnet), shows
  the latest state of every vault and any earlier one, previews files, and downloads them. The
  browser decrypts them. Outside the tailnet the page does not exist.
- `vault-restore` rebuilds any state into a folder with the paper key.
- Each project can use its own AWS account and bucket (one per client).

## Keys

| Key                                                    | Where                                             | Can                   |
| ------------------------------------------------------ | ------------------------------------------------- | --------------------- |
| Vault identity (`age` X25519)                          | nowhere in plain form; inside each slot           | decrypt everything    |
| Vault recipient (`age1…`)                              | VM vault configuration, `vault.json`, gateway     | encrypt new data      |
| Paper key (`age` X25519 identity, `AGE-SECRET-KEY-1…`) | on paper, two copies                              | open the paper slot   |
| Passkey (WebAuthn PRF)                                 | the user's passkey provider                       | open its passkey slot |
| Writer AWS key                                         | VM, root only (`layr-admin/vault/<project>.json`) | put new objects only  |

- A slot is the vault identity encrypted with `age` to the recipient of one opener:
  `slots/paper.age` for the paper key, `slots/passkey-<credential id>.age` for each passkey.
  The PRF output of a passkey (32 bytes, salt `layr-vault/v1/<project>`) is the X25519 secret
  of its recipient, so one passkey of the gateway host can open several vaults with different
  secrets. Any number of slots can exist; removing a slot object revokes that opener.
- The paper key is 24 words of the English BIP39 list: the 32 bytes of an X25519 secret with an
  8-bit checksum, as crypto wallets write their keys. The site and the Mac tools also take the
  same key in its `age` form (`AGE-SECRET-KEY-1…`), the bech32 encoding of those 32 bytes with
  the prefix `age-secret-key-`.
- Without this software: any BIP39 tool turns the words into the 32 bytes (the entropy, not a
  BIP39 seed), bech32 turns them into the `age` form, and the `age` tool decrypts every object:
  `age -d -i paper.txt slots/paper.age > vault.txt`, then `age -d -i vault.txt <object>`.
- The VM has the recipient only, so agents and a stolen VM disk cannot read the vault.
- The gateway proves that a caller holds the vault identity: it returns a token encrypted to
  the vault recipient, and accepts the decrypted token (an HMAC with a time and the project) for
  10 minutes. There is no user database; the signing key lives in the gateway's memory.

## Objects

Bucket prefix `<project>/`:

- `vault.json`: `{"format": 1, "recipient": "age1…", "created": <epoch>}` (plain).
- `slots/*.age`: see above (open to every tailnet device through the gateway; each is encrypted).
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

## AWS (`studio-vault/cloud/`)

One stack per project, in the project's AWS account (`template.yaml`):

- Bucket with versioning; public access blocked; no CORS, no public path.
- Writer user: `s3:PutObject` on `<project>/states/*` with `If-None-Match` required,
  `s3:ListBucket` on `<project>/states/`; nothing else.
- Reader role `studio-vault-reader-<project>`: read states and slots, put and delete
  `slots/passkey-*`. It trusts the gateway role by ARN (`aws:PrincipalArn`), so the gateway can
  be in another account and its role can be made again.
- What stays out of CloudFormation: `vault.json`, the paper slot, and the writer's access key,
  which would otherwise sit in the stack outputs; IAM Roles Anywhere is to replace that key.

One gateway for all projects, in the user's account (`vault-gateway.mjs`):

- `dns.yaml`: the Route 53 zone of the domain; the registrar names its servers.
- `bootstrap.yaml`: the artifacts bucket: `gateway/releases/<sha256>.tgz`, `gateway/release.json`,
  `gateway/gateway.json` (host and zone) and `gateway/projects/<project>.json` (bucket, region,
  reader role, recipient), which `vault-deploy.mjs` writes.
- `gateway.yaml`: an IPv6-only VPC and one `t4g.nano` (Debian 13, standard CPU credits, IMDSv2,
  8 GB encrypted disk). No public IPv4 address; the security group lets in only WireGuard (UDP
  41641). Its role reads `gateway/*`, assumes `studio-vault-reader-*`, reads the tailnet auth key
  in SSM, and changes only the host's `A` and `_acme-challenge` records.

## Gateway

`gateway/` in `studio-vault`: `server.mjs` (site and API), `main.mjs` (TLS on the tailnet
address only), `sync.py` (runs as root at first boot and every 5 minutes). `sync.py` joins the
tailnet with the auth key from SSM (`tag:vault`, Tailscale SSH on), installs the release that
`release.json` names, writes `/etc/vault-gateway/config.json` from `gateway/projects/`, points the
host's public `A` record at the tailnet address (100.x, which only the tailnet can reach), gets a
Let's Encrypt certificate by DNS challenge, and keeps `vault-gateway.service` running
(`DynamicUser`, read-only system). Unattended upgrades, Tailscale's included, reboot at 04:30
when needed.

API, per vault `<p>`: `GET /api/projects`, `GET /api/p/<p>/slots`, `GET /api/p/<p>/challenge`,
and with `Authorization: Bearer <token>`: `GET /api/p/<p>/states`, `GET /api/p/<p>/states/<name>`,
`PUT|DELETE /api/p/<p>/slots/passkey-<id>`. Every other path is the site; `/<p>/` is the page of
one vault. The host header must be the configured host.

Tailnet policy: `tag:vault` belongs to the admins; access rules take `autogroup:member` as the
source, so the gateway answers but cannot reach the user's devices; one SSH rule lets members
reach `root@tag:vault`.

Cost in eu-central-1: about $3.50 a month for the instance, $0.80 for the disk and $0.50 for the
zone. From a network without IPv6, the tailnet relays the traffic through a Tailscale DERP server.

## Measurements behind the format

On the codex-agents history (1,095 states), one run each: btrfs send streams with zstd took
100.8 MB; content-defined chunks 92 to 116 MB in 17,000 to 21,000 objects; file deltas against
the previous version with tree deltas, one object per state, 25.7 MB (zstd -3) and 23.9 MB
(zstd -19). `git gc --aggressive` took 23.9 MB.

## Mac view

`vault-view` serves the latest state read-only over WebDAV on localhost, and Finder mounts it
(Go > Connect to Server). Files download and decrypt when they are opened and stay in a local
cache. It uses the same reader as the site; the Mac only reads the vault.

## Set up the gateway, once

1. Buy the domain; `vault-gateway.mjs` prints the Route 53 name servers for the registrar.
2. Tailnet policy as above; then an auth key: not reusable, not ephemeral, pre-approved, tag
   `tag:vault`. The user stores it in SSM `/studio-vault/gateway/tailscale-auth-key`
   (`SecureString`) from their own terminal.
3. `node workspaces/runtime/apps/vault/bin/vault-gateway.mjs --host <desk.example.com> --profile
<profile>`. It builds the site and the server, uploads the release, and makes the stacks. A
   later run with the same arguments installs a new release within 5 minutes; `--new-image`
   replaces the instance, which then needs a new auth key.

## Set up a vault for a project

1. The user, in a terminal of their own (never through an agent, so the paper key stays out of
   transcripts): `node workspaces/runtime/apps/vault/bin/vault-keys.mjs <prefix>`. It shows the
   paper key once; write it down twice. `~/.config/studio-vault/<prefix>/` keeps only the
   recipient and the encrypted paper slot.
2. `aws sso login --profile <profile>`, then, with the Studio Python first on `PATH`:
   `node workspaces/runtime/apps/vault/bin/vault-deploy.mjs --project <prefix> --profile <profile>
--region eu-central-1 --folder <project folder>`. The profile names the vault's account, which
   may be a client's. It makes the stack, uploads the paper slot and `vault.json`, registers the
   vault with the gateway, and pipes a write-only key to the VM (`codex_vault.py configure`).
3. The root broker pushes every configured project every 10 minutes; `codex_vault.py push` pushes
   at once, and `codex_vault.py status` shows the last push and the last error.
4. On `https://<host>/<prefix>/`, unlock with the paper key and add a passkey, or use an existing
   passkey of another vault on the same host.
5. Mac: `vault-view.mjs --project <prefix> --mount` mounts the latest state in Finder;
   `vault-restore.mjs --project <prefix> --out <folder> [--seq <n>]` rebuilds a state. Both read
   S3 directly with the AWS profile, so they work without the gateway and the tailnet.

`tests/site.e2e.mjs` in `studio-vault-web` checks the built site behind the real gateway code
in headless Chromium (paper key, columns and list views, Quick Look, download, passkeys for two
vaults with a virtual PRF authenticator); `tests/live.e2e.mjs` checks a deployed gateway.

## Not in the first slice

A company key slot, Object Lock defaults, garbage collection, Node.js 22 on the gateway (Debian 13
ships 20; AWS SDK releases after January 2027 need 22).
