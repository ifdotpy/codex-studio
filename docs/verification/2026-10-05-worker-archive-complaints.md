# Worker archive complaints, item 16

Source starts at `bfdcd25b`. The complaint file is
`~/Library/Caches/chrompile/lead-progress/studio-complaints-20261005.md`.

## Missing folders

The lead requested `archive_finished` at 10:48:52.812 UTC on October 5.
It completed at 10:53:29.597 UTC. The receipt is
`claude-local:e2187ecb-97f4-4539-933d-5a0609eb23db:9eb78dd9-6d3c-42ac-8c7f-cae80d2f75ed`.

Nine worktree folders were already absent when Studio checked them. That path
saved archive refs and pruned Git metadata. It did not delete their folders.
The saved worker branches equal their archive refs. The earlier deletion owner
and time remain unknown. These observations do not prove that Studio deleted them.

Earlier receipts narrow the observation time. Receipt `905b78cc` shows eight
missing folders on October 4 at 07:35 UTC. The ninth worker is outside that page.
Receipt `ab07bf45` shows all nine missing at 20:24 UTC. This predates the update.
These are read-only observations. Separate archive calls keep the workers because of assigned tasks.

The follow-up checks 1227 lead requests and 1466 tool items from 48 hours.
It also reads one exact rollout. None contains a root-folder removal command for these
nine workers. Other actors and earlier actions remain outside this search.
This is a search limit, not proof that no deletion occurred.

Private evidence is in `/private/tmp/studio-complaints16-evidence-20261005`.
It contains the caller, receipt, saved agents, filesystem checks, Git refs, and
cleanup entrypoints. No active worker was stopped for this investigation.

## Partial Git removal

Worker `ab237303-1741-5785-a1e3-ce2186d7f336` has a different outcome.
Git removal failed, but its registration and Git directory disappeared.
The worktree folder and original `.git` file remained. Its branch and archive
ref both point to `a9b2fbe98ed4c84ed7df562795b22c69be75c1d6`.
The remaining folder measured 293818368 bytes during the read-only check.

Studio previously discarded the cleanup record after this error. It then marked
cleanup complete. It also discarded Git stderr, which prevents identification
of the old filesystem error.

Commit `938bf5e0` preserves the cleanup record, stderr, errno or return code,
actor, time, path, and saved head. A new archive call can repair the exact Git
metadata without changing files. Only Git removes the folder. Changed files,
foreign registration, active work, or a changed original Git link prevent removal.
A missing original Git link leaves the folder for inspection.

The older complaint about worker `fd4ca809` also has a confirmed partial removal
receipt on October 5 at 09:05 UTC. Its folder remains; its Git registration is
absent. This does not prove the reported disappearance near 04:00 UTC.

Private regressions reproduce the old archived record, missing registration,
matching refs, original Git link, ignored cache, and missing tracked files.
They also preserve changed files and reject a FIFO in place of `.git`.

## Main reachability and task decisions

Commit `a3a607f4` fetches main through the current origin into a private ref.
It preserves local main, configured remote refs, and `FETCH_HEAD`. It checks
remote and repository identity before archive. A failed fetch keeps the worker.
The result supplies an exact `mainEvidence` commit and source.

Private real-Git tests cover stale local main, the fresh remote commit, no-remote
repositories, failed fetch, changed refs, and chained `insteadOf` URL rules.
An exact retry of the old acceptance receipt repeats only the archive check.
It creates no second task decision or result. The error for `accept` and `reject`
now names the required `result` field. Old rejection records remain `not_applied`.

## Obsolete native inspection

Commit `665463bb` retires an exact blocked inspection that never submitted an
unsubscribe request. An unloaded reset uses a neutral phase. It does not claim
native closure. A confirmed resume removes only that old inspection error.
Unknown or submitted requests, changed identities, Stop, and current errors remain.

The current worker `9356d4af` already has a released record with no error.
The source regressions separately reproduce the old stale display.

## Archive with open tasks

Commit `938bf5e0` adds explicit `unassign_work=true` to `archive` and
`archive_finished`. It commits the worker archive and task release together.
Tasks return to ready with no owner. Results and decisions remain.
The default still blocks archive for assigned work. Native activity, monitors,
inputs, and other unfinished operations remain blockers.

Verification uses private repositories, private SQLite state, and native server
fixtures. It does not prove a live model request. Source and live publication
are separate checks.
