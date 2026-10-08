# Sync browser spec changes

- `errors/native-error-ui.spec.mjs`: expand the account and feature warning Details disclosures before checking their JSON payloads. Class 2, stale expectation after `b3e73f2e` wrapped warning details in `<details>`.
- `errors/native-error-ui.spec.mjs`: expand the retrying phase's Details disclosure before checking its diagnostic text and JSON. Class 2, `AgentPhase.tsx` renders reconnect details inside a collapsed `<details>` disclosure.
- `errors/native-error-ui.spec.mjs`: send the second retry error's `message` and JSON diagnostic as strings, matching `AgentNativeStatus` / `NativeProviderError` in `scripts/studio_api/sync/models.py`; keep asserting the parsed structured diagnostic. Class 1, the fixture event previously violated the sync entity schema.
- `errors/native-error-ui.spec.mjs`: open the second retry Details disclosure only if closed. Class 2, the rendered disclosure remains open when the retrying status updates in place; an unconditional second click closes it.
- `errors/native-error-ui.spec.mjs`: configure the fixture's capacity retry deadlines to 60 seconds so the scheduled retry remains visible through the UI assertions and cancellation. Class 1, the production default 10-second timer advanced to `starting` before the test could inspect it.
- `errors/native-error-ui.spec.mjs`: assert the account sign-in recovery title alongside the capacity retry state. Class 2, `c461c430` added auth recovery UI that intentionally takes precedence over the previous error title while the capacity retry and transcript warning remain visible.
