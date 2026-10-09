# layr in the Studio VM

The VM stores projects at `/var/lib/codex-studio/layr` on btrfs.
The root systemd service runs `layr daemon`.
Agents use `/run/layr/layr.sock` as their own Linux users.
The separate root broker accepts connections only from the `studio` user.
It permits specific project, line, provider, and slot operations.
It rejects a repeated request ID with different parameters.
An unfinished request keeps its unknown outcome after a restart.

## Project import

The first VM chat imports the Mac folder through the existing tree upload protocol.
`project.import` creates the layr project and its main line.
The project ID determines a Linux user for all leads of that project.
`project.ensure` returns the project ID, main path, user, home, and current state ID.
Later chats use the existing VM main line.
The Mac folder remains separate for native chats.
Studio does not synchronize those two folders automatically.

An explicit re-import requires a new request ID and the expected main state ID.
It creates an `import-*` line for review.
It does not replace the main line.
Use `layr export` when you need a normal copy outside the VM.

## Mac share

Samba serves each project's `main/` and `states/` folders.
It binds only to the internal VM interface.
It accepts only the Mac gateway address and the private `studio-view` credential.
The Mac credential file has mode 0600 in the VM state directory.
No password appears in a process argument or a status response.

The Mac mounts the share at `~/Studio/<projectId>` without root.
The mount appears in Finder.
The server and the Mac mount both reject writes.
The guest uses read-only bind mounts for the export paths.
The share rejects symbolic links and `.DS_Store` files.
It exposes no layr metadata or machine keys.
VM startup restores the guest exports and the registered Mac mounts.

`/api/desktop` reports the `linuxVm` status on macOS.
It includes the layr version, store size, disk space, guest share state, and Mac mount state.
Linux servers return `linuxVm: null`.

## Source update

The repository contains the clean layr source at `vm/layr`.
Its manifest records the commit, Git tree, and SHA-256 hashes.
The guest installer verifies those hashes before a build.
It uses Rust 1.90.0 and the committed Cargo lock file.

Run this command to select another committed layr revision:

```sh
python3 scripts/update-vm-layr.py --source ~/Projects/layr --revision <commit>
```

The command reads the Git commit through `git archive`.
It does not read or change that repository's work files.

## Checks

Run each command separately:

```sh
python3 -B -m unittest discover -s vm/guest -p test_layr_admin.py -v
PYTHONPATH=scripts python3 -B -m unittest test_codex_linux_vm -q
python3 -B tests/linux-vm-layr-host-contract.py -q
npm run typecheck:runtime
```

Live checks require the existing Studio VM on this Mac.
Back up its disks before guest changes.
Do not restart it while a provider is active.
The live check must include a non-root layr command, a rejected Mac write, and a restart.
