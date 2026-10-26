# Provider replay fixtures

These hand-authored JSON-RPC frames are consumed by
[`connection.rs`](../../connection.rs). The session witness checks event frames
through the codec and session dispatcher; dedicated witnesses exercise RPC reply
templates, missing replies, and orphaned responses. The Claude bridge owns its
SDK-level fixtures separately.

The fixtures require no backend or paid model request. The provider session
checks do not establish the historical runtime outcomes described in fixture
metadata such as `expectedStatus` or `assert`.
