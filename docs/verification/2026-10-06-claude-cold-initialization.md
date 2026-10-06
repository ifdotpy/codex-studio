# Claude cold initialization, 2026-10-06

CATIA Review repeatedly failed before Claude accepted its input.
The error was `Claude preparation timed out before input was submitted`.
The source before this change was `cf5d07c1`.

## Diagnosis

- The existing bridge aborted a no-input account probe after 22.369 seconds with the 20-second limit.
- The same bridge completed the extended probe after 33.268 seconds with the correct account.
- A fresh process completed the same profile and history probe after 674 milliseconds.
- Both processes used the same software development kit (SDK) and executable content.
- A central processing unit (CPU) probe took 3.590 seconds in the old bridge and 0.096 seconds in the ordinary process.
- A temporary Interactive LaunchAgent completed the same CPU-only probe after 0.0577 seconds.
- The live recovery and supervisor jobs reported `spawn type = daemon (3)`.
- Their saved plists already contained `ProcessType = Interactive`.

The confirmed failure is a healthy cold initialization that exceeds the old deadline.
The process comparisons indicate that the inherited launchd policy causes the CPU delay.
They do not identify the exact kernel limit.
[Apple describes the resource limits for Standard jobs and the Interactive exception.](https://github.com/apple-oss-distributions/launchd/blob/main/man/launchd.plist.5)

## Source change

- Cold metadata probes have a 60-second limit.
- A new native query shares one 60-second limit for metadata and account initialization.
- Controls on a retained query keep their shared 20-second limit.
- Input remains withheld until the account check succeeds.
- An exact repeated input keeps its original receipt and turn.
- Bridge version 17 becomes current only after the existing idle-process checks permit replacement.

## Checks

- The new cold initialization case fails against `cf5d07c1` before the fix.
- The receipt contract passes all 7 cases.
- The controls contract passes all 21 cases.
- The account admission contract passes all 9 cases.
- The input recovery contract passes all 27 cases.
- The bridge contract passes 42 distinct cases across the full run and the targeted version-assertion rerun.
- A separate review confirms the shared deadline, late-answer rejection, exact repeat, and background-task guards.
- Node syntax, Oxlint, and Oxfmt checks pass for the bridge.
- The pre-commit fixtures confirm checks against staged content.

## Live result

- CATIA agent: `4c360fa0-9be1-497b-ae50-b2e06a9b666b`.
- Recovery input: `studio-catia-resume-after-preparation-fix-20261006`, delivered once.
- Restored turn: `376ad2db-6d4f-4ef7-89bf-6d3d97699c18`, completed without error.
- Claude confirmed context recovery and then ran Bash and Studio tools.
- The Studio status call completed in 840.65 milliseconds.
- Claude sent work to an existing subagent and a message to a peer lead.
- The agent then entered `waiting` after its completed answer.

All three live Claude bridges kept their processes and active generators.
Their process IDs are 16714, 16753, and 15254.
A guarded compatibility patch changed four ordinary functions in its exact loaded source.
The patch extended account initialization and preserved exact input admission checks.
The first response was lost; source readback confirmed application without a second mutation.
All three source readbacks have hash `dc50c4f761878d0c404f01ae6d58b29f42e04d3cd9f6d9be6551a30dcff62b02`.
Each inspector closed after readback.

The installed bridge and controls match the reviewed source hashes.
The backend applied `claude-cold-initialization-20261006` without a restart.
Its receipt records PID 6414 and manifest hash `866370a03e43d3ecd48bff46b89256cc21a83244ca1a6b3c82afd1676a25cafb`.
The temporary patch and manifest moved to the state backup after confirmed application.
The app signature check passes.

## Remaining limit

The existing launchd jobs still use their old resource policy.
The saved Interactive policy applies at the next safe service registration.
This change does not replace the live supervisor or interrupt its agent pipes.
Old failed input receipts remain unchanged and are not replayed automatically.
