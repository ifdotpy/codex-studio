# Isolated Mac sync round

This procedure checks the real guest boundary. Unit fixtures do not prove this boundary.
No live VM or user project is part of this procedure.

## Preparation

1. Use a disposable VM with a separate disk, guest socket, and runtime state directory.
2. Use the guest code and server binary from the same commit.
3. Create a scratch project with synthetic files only.
4. Keep the scratch Mac folder outside this checkout and outside every user project.
5. Record the canonical paths of the scratch folder, runtime state, guest socket, and VM disk.
6. Confirm that the test guest uses complete immutable snapshots in `<project>/states/<state>`.
7. Confirm that no backend owns the scratch runtime state.
8. Import the scratch folder through the normal Studio project import.
9. Pause sync before the first edit.

Suggested files: `mac.txt`, `vm.txt`, `same.txt`, `delete-mac.txt`, `delete-vm.txt`,
`readme.md`, `café.txt`, and `sub/keep.txt`. Use different short content in each file.
Keep the file count small. No synthetic load is necessary.

## Preview

1. Record the Mac file content, file names, sync record bytes, and VM main state.
2. Change `mac.txt` on the Mac.
3. Change `vm.txt` in VM main.
4. Delete `delete-mac.txt` on the Mac.
5. Delete `delete-vm.txt` in VM main.
6. Change `same.txt` differently on each side.
7. Rename `readme.md` to `README.md` in VM main.
8. Change the Unicode form of the `café.txt` name in VM main.
9. Run the preview with explicit scratch paths:

   ```sh
   CODEX_LINUX_VM_STATE_DIR=/absolute/scratch/runtime/linux-vm \
     studio-runtime-server mac-sync plan SCRATCH_PROJECT \
       --state /absolute/scratch/runtime
   ```

10. Confirm the upload, VM write, deletion, rename, and conflict lists.
11. Confirm that the preview does not change the recorded files, records, main state, or guest line list.
12. If the preview reports `pending` or `recovery`, keep sync paused until its operation has a proven result.

The command does not start a backend. It does not acquire the server database lease.
The existing round lock protects the preview when that lock is present.

## Real round

1. Start exactly one backend against the disposable runtime state.
2. Resume only the scratch project through the normal Mac folder sync control.
3. Wait for one round to complete.
4. Pause the scratch project.
5. Confirm both directions against the preview and the recorded original content.
6. Confirm that the conflict keeps both versions available and contains no conflict markers.
7. Confirm the case and Unicode names and their content on the Mac.
8. Resume the scratch project for one quiet round.
9. Confirm that the quiet round sends no deletion for either renamed file.
10. Pause the scratch project.

## Failure boundaries

1. Make `sub` unreadable in the scratch Mac folder.
2. Resume sync for one round.
3. Confirm a visible error and no deletion in VM main.
4. Restore the mode of `sub`.
5. Interrupt only the test guest response after the broker accepts a scratch request.
6. Confirm that `pending.json` keeps the original operation and payload.
7. Change the scratch file again while its response is absent.
8. Restore the test response channel.
9. Confirm that the retry uses the same operation and preserves the newer Mac content.
10. Record the exact server commit, guest commit, layr version, requests, receipts, and final file hashes.

## Concurrent editor save

Use only a synthetic scratch file. The deterministic source tests also cover the exact final-check boundary.

1. Open the agreed scratch file for writing and keep its descriptor open.
2. Change the same file in VM main.
3. Resume the scratch project for one round, then pause it.
4. Confirm that the project path contains the VM content.
5. Confirm that `status.json` names a recovery directory with the original Mac content.
6. Write different scratch content through the original descriptor and synchronize that descriptor.
7. Close the descriptor.
8. Resume the scratch project for one round, then pause it.
9. Confirm a retained-data error and no later guest apply.
10. Confirm that the recovery directory's `original` contains the late editor content.
11. Confirm that the current project file still contains the VM content.
12. Follow the retained-data review procedure in `docs/vm-mac-sync.md` for this scratch project.

Do not delete recovery directories to make this check pass.
No automatic expiry removes an inode that an editor can still use.
Test native atomic exchange and no-replace support on the scratch filesystem before any user project.
Repeat this check with a Git linked worktree whose metadata directory is outside the scratch project.
An empty event set and an unrelated file event must both stop before another guest apply.

Do not reset receipts to make a retry succeed. A permanent uncertain result must remain visible.
Do not apply this procedure to the live VM or an existing user project.
