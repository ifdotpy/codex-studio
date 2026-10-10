# Provider replay fixtures

These transcripts are hand-authored from the public JSON-RPC frame shapes in
the runtime and Claude bridge contract fixtures. They use no live backend or
paid model request. `provider-replay-runner.py` replays their provider events
through `AppServer` and the real `Runtime` in a temporary state directory.

Each JSON file records its source and the protocol events that it supplies.
The replay subprocess returns local setup replies and injects the recorded
notification frames after `turn/start`. Fixture paths and machine credentials
do not enter the checked-in frames.
