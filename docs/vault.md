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
