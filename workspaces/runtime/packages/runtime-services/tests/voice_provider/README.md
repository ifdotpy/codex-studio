# Voice provider mutation witnesses

`voice_provider_corpus.rs` exercises provider admission ordering, receipt
ownership and identity, audio validation and file recovery, and each callback
actor transcript lifecycle. `mutations.json` names a test witness for every
seeded defect. Run the shared mutation runner with the manifest and a filter
that includes the voice corpus plus the dispatch and receipt tests and the
interrupted-audio test.
