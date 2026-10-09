# Sync browser spec known failures

## Confirmed product defect

- `sync/sync-cross-tab-browser.spec.mjs`: full eight-tab resumed-owner duplicate-stream assertion failed in 4/20 marker-free solo repeats (earlier 2/10), after the unchanged 1,500 ms wait. The two live SSE resource sets were `[transcript(session-one), panel(takeover-unique-panel), state]` and `[transcript(session-one), state]`. In one failing repeat, after a further 10 seconds only one stream remained; a state change yielded one callback in each of eight visible tabs and zero entity pulls. Duplicate notifications were not measured. Separate takeover symptom: 1/10 earlier repeats opened two identical union-query streams; first closed after 13 ms and duplicate opened 14 ms after first, ending with one active stream. Corrected versioned fixture did not eliminate the defect. `27f6ea71` remains a candidate; causality is unproven.
