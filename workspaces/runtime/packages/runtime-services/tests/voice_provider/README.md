# Voice provider mutation witnesses

`voice_provider_corpus.rs` checks the policy's provider failure transition and
the callback adapter's stop decision. `mutations.json` seeds the defects that
must be caught by those named witnesses. Run the isolated shared mutation
runner with `--manifest workspaces/runtime/packages/runtime-services/tests/voice_provider/mutations.json`
and `--filter binary(/^voice_provider_corpus$/)`.
