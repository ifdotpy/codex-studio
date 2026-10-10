# Checkpoint 4 G3 known failures

No new product failures were confirmed in the six repaired rows.

The existing message delivery pointer-overlap defect remains and is documented in `workspaces/client/apps/web/tests/KNOWN-FAILURES.md`: marker-removed repeat-3 failed 3/3 at the unchanged pointer hit assertion, reporting `RemQuovte message` instead of `Quote message`. The normal spec marks only that assertion as expected-failing.

The native policy notice was present with its policy-specific title. The `queue-mutation-deadline-browser` startup timeout was a fixture omission (the initial sync resource route did not match the query-bearing URL); it passed repeat-3 after the fixture correction. The UX recovery and queue failures attributed to PR #34 were refuted: UX recovery passes after its fixture/state selection is corrected, and the queue timeout occurs before a mutation is sent.
