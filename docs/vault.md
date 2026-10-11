# Project vault

The vault is its own project: [ifdotpy/layr-desk](https://github.com/ifdotpy/layr-desk) holds the
format, the keys, the gateway, the Desk site, the AWS templates and the commands. Studio keeps
only the integration:

- The VM writer is `layr-desk push` (layr-desk's `docs/design.md`, "Writer"). Studio vendors
  the layr-desk source at one commit into `workspaces/runtime/apps/layr-desk`, and VM
  provisioning builds it next to layr (`install-layr.sh`, see "Writer in the VM" below).
- The root broker methods `vault.configure`, `vault.push` and `vault.status`
  (`workspaces/runtime/apps/vm-guest/vault_admin.py`) keep the writer configuration in
  `layr-admin/vault/<project>/config.json`, readable by root only. The broker pushes the main
  head of each configured project every 10 minutes.
- `codex_vault.py configure|push|status PROJECT_FOLDER` calls them from the Mac.

## Set up a vault

"Set up vault…" in the LAYR menu of a project on this Mac opens `VaultSetup.tsx`. It runs the
steps of layr-desk's `docs/design.md` ("Set up a vault for a project") with the layr-desk that
the app carries (`Resources/workspace/bin/layr-desk`):

1. Paper key: the desktop app writes a one-time `.command` script and opens it in Terminal
   (`desktop/vault-keys.cjs`), which runs `layr-desk keys --project <prefix> --clear`. The words
   go from layr-desk to Terminal only. Studio's own terminals are not used, because they keep
   their output in SQLite. The dialog then waits for `~/.config/studio-vault/<prefix>/recipient.txt`.
2. AWS account: `POST /api/vault/setup/facts` lists the AWS profiles (`layr-desk aws-profiles`)
   and the gateway's Desk site (`~/.config/studio-vault/gateway.json`). A profile without a
   usable IAM Identity Center sign-in gets `POST /api/vault/aws-login`, which runs
   `layr-desk aws-login --json` and shows the AWS page and its code.
3. Vault: `POST /api/vault/setup` runs, once per request id:

   ```sh
   layr-desk deploy --project <prefix> --profile <aws profile> --region <region> \
     --writer "<studio python> <scripts>/codex_vault.py configure <project folder>"
   ```

   The writer's access key and signing key go through the pipe; a key that the command does
   not take is deleted again. Exit code 3 means that the profile needs a sign-in. On success
   the server checks the vault at once, so the LAYR chip gets its cloud.

The vault name comes from the folder name (lower case, at most 44 characters) and can change
until the paper key is made. A server on another Mac offers the setup guide instead.

## Writer in the VM

`vault.configure` checks and keeps `bucket`, `region`, `prefix`, `recipient`, `accessKeyId`,
`secretAccessKey`, the optional `signingKey` (the base64 seed of an Ed25519 key) and, for tests
only, a local `endpoint`. A new bucket, prefix, recipient, endpoint or signing key removes the
writer state, so the next push is full. A new access key keeps it.

A push runs, as root:

```sh
layr-desk push --project <project> --state-dir layr-admin/vault/<project>/writer \
  --layr /usr/local/bin/layr --config -
```

The configuration goes on standard input, never in the arguments or in another file. The
writer state (`writer.json`, `tree.json`, the export folders `a` and `b`) has the format of the
first Python writer, so `layr-desk push` continues a state folder that it made. One push may
take 6 hours at most. `status.json` beside the configuration keeps the outcome:

- `lastAttempt`: the start of the last push.
- `lastPush`: the start of the last push that sent a new state. A push without a new state does
  not change it.
- `lastError`: `null` after a push without an error. Otherwise the cause on one line, for
  example `layr-desk push: S3 put ...: HTTP 403 AccessDenied Access Denied` or
  `layr-desk is not installed`.

`vault.status` returns the bucket, region, prefix, recipient, the last record (`writer.json`)
and `status.json`. It never returns the access key or the signing key.

To vendor another layr-desk commit, run:

```sh
python3 workspaces/runtime/apps/server/src/update-vm-layr.py --name layr-desk \
  --source ~/Projects/layr-desk --revision <commit>
```

The command takes `Cargo.toml`, `Cargo.lock`, `LICENSE`, `crates/desk-core` and
`crates/layr-desk` of that commit through `git archive`, and records each file's SHA-256 in
`manifest.json`. Provisioning copies the source to `/opt/codex-studio/vm/layr-desk`.
`install-layr.sh` checks the files against the manifest, builds them with the Rust toolchain
and the crates.io crates of the lock file (as for layr), and installs
`/usr/local/bin/layr-desk`. It builds again only when the source identity in
`/var/lib/codex-studio/layr-desk-build` changes. The binary has no service: the next push uses
a new binary.

The end-to-end check runs as root on Linux with btrfs, real layr and a `layr-desk` built from
the vendored source, against a local fake S3:

```sh
LAYR_E2E_ROOT=<btrfs store> LAYR_E2E_LAYR=<layr> LAYR_E2E_DESK=<layr-desk> \
  python3 -B -m unittest -v test_vault_e2e
```

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
