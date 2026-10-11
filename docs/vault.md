# Project vault

The vault is its own project: [ifdotpy/layr-desk](https://github.com/ifdotpy/layr-desk) holds the
format, the keys, the gateway, the Desk site, the AWS templates and the commands. Studio keeps
only the integration:

- The VM writer (`workspaces/runtime/apps/vm-guest/vault_writer.py`, `vault_s3.py` and
  `vault_reader.py`) pushes the main head of each configured project every 10 minutes. It stays
  here until layr-desk has its Rust writer.
- The root broker methods `vault.configure`, `vault.push` and `vault.status` (`vault_admin.py`)
  keep the writer configuration in `layr-admin/vault/<project>.json`, readable by root only.
- `codex_vault.py configure|push|status PROJECT_FOLDER` calls them from the Mac.

To give the writer of a project its key, run the layr-desk deploy command with a writer command:

```sh
vault-deploy --project <prefix> --profile <aws profile> \
  --writer "python3 <studio>/workspaces/runtime/apps/server/src/codex_vault.py configure <project folder>"
```

The key goes through the pipe and is deleted again if the command fails.

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

The server asks the VM for `vault.status` of every imported project every 2 minutes, outside
the runtime lock, and keeps the report in `layr-projects/<id>/vault.json`. The sidebar reads
only that record; the time of a check stays out of it, so a quiet vault does not change the
project entity. The Desk site comes from the writer configuration (`site`) or from
`codex_vault.py site PROJECT_FOLDER https://desk.example.com/` and stays in
`layr-projects/<id>/vault-site.json`. Studio finds `layr-desk` through `STUDIO_LAYR_DESK_BIN`,
`PATH` or `~/.cargo/bin`.
