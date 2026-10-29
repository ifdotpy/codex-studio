# Request result migration inventory

This inventory is for reviewers of the incremental request-results migration. It
describes the settlement sites on `81d416f9a`, before the read-reply change is
merged. A reference candidate still needs a revision and a typed response built
from its owning row. A row existing by itself does not prove that it contains
every field of today's reply. Until that proof and the caller conversion exist,
retain the typed body and the existing replay behavior.

`Body` below means that the current exact result has no surviving owning row or
contains operation-specific data not yet owned by one. It is a temporary exception,
not permission to repeat an external effect. A rejected result must retain its
meaning; an uncertain external effect remains an outbox or entity state.
The receipt status describes the command: applied means its outcome was recorded,
even when a retained body reports an unknown external effect; rejected means the
command was refused before any effect.

| Settlement family and source                                                                                                   | Owning row?                          | Proposed result     | Reason or remaining proof                                                                                     |
| ------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------ | ------------------- | ------------------------------------------------------------------------------------------------------------- |
| Annotation (`event_write::annotate`)                                                                                           | Yes: annotation                      | Reference           | The annotation is saved in the same transaction; preserve event effects separately.                           |
| Work writes and accepted archive (`work_write`)                                                                                | Yes: work item                       | Reference candidate | The archive path also carries an accepted operation and must retain its exact revision.                       |
| Agent rename, configure, conversation and permissions (`agents_create`, `agents_create/operations`)                            | Yes: agent                           | Reference candidate | Current replies also carry sync deltas; rebuild rows from the current projection.                             |
| Complaint response (`questions_write`)                                                                                         | Yes: complaint                       | Reference           | The complaint is written before settlement.                                                                   |
| Project peer team, order and location (`projects/operations`, `project_locations`, `runtime-http/routes/projects`)             | Yes: project or sidebar order        | Reference candidate | Identify the owning row for each location result, including removed locations.                                |
| Radio room management and creation (`radio_runtime`)                                                                           | Yes: room                            | Reference candidate | Return the referenced room projection.                                                                        |
| Managed and group messages (`coordination`, `federation_delivery`, `provider_callbacks/tools/services/messages::send_to_user`) | Yes: message or event                | Reference candidate | The reference must identify the actual created message or event.                                              |
| Federation receive (`federation_effects::receive_locked`)                                                                      | Sometimes: inbox                     | Body until mapped   | Duplicate/refused decisions and their reply are not necessarily represented by an inbox row.                  |
| Federation acknowledgement (`federation_effects::acknowledge_outbox`)                                                          | No surviving outbox row              | Body                | Settlement deletes the outbox row and the reply contains acknowledgement data.                                |
| Managed broadcast and send (`provider_callbacks/tools/services/messages::send_as_actor`)                                       | Multiple messages and refusals       | Body until mapped   | Per-recipient results include refusals not represented by one message row.                                    |
| Park/event management (`provider_callbacks/tools/services/manage::park_action`)                                                | Multiple agents or events            | Body until mapped   | The result contains the woken set and operation metadata, not a single owning row.                            |
| Default task gate (`task_gate::set_default`)                                                                                   | Settings row                         | Reference candidate | Specify the settings row revision and typed response.                                                         |
| Queue mutations (`queue_write::action`)                                                                                        | Several queue rows                   | Body until mapped   | The reply's queue revision describes the whole ordered snapshot.                                              |
| Project deletion (`project_deletion`)                                                                                          | Deleted rows                         | Body until mapped   | The reply includes deleted rooms; the project row may no longer exist.                                        |
| Peer conversion and VM-to-host (`peer_conversion`, `vm_to_host`)                                                               | Multiple agents                      | Body until mapped   | The reply contains a moved-agent set and revision.                                                            |
| Rule actions (`managed_status_watch::rules`)                                                                                   | Rule or rule set                     | Body until mapped   | List and mutation replies have different shapes; deletion removes the row.                                    |
| Title job (`title_generation::settle_job`)                                                                                     | Job/agent state                      | Body until mapped   | Prove the stored owner retains the public status and error through replay.                                    |
| Tool result (`tool_requests::settle_result`, `project_locations::save_tool`)                                                   | Tool request                         | Body until mapped   | Mutation results can include output and per-target outcomes. Read replies are removed by the preceding slice. |
| Provider callback response (`provider_callbacks/storage::delivery_in`)                                                         | Callback owner; outbox removed       | Body                | The exact provider response frame is not retained by the owner.                                               |
| Native actions, input and cancellation (`native_actions`)                                                                      | Outbox removed on settlement         | Body                | Provider output and delivery certainty are operation results.                                                 |
| Terminal action (`runtime-store/repositories/terminals::settle_action`)                                                        | Outbox removed on settlement         | Body                | The action response and failure are needed after the transient owner is deleted.                              |
| Monitor input (`monitor_http_input::put_receipt`, `settle`)                                                                    | Outbox removed after terminal status | Body                | The typed input receipt carries delivery result and error.                                                    |
| MCP reconnect (`runtime-http/routes/mcp_reconnect`)                                                                            | Outbox removed for confirmed result  | Body                | The provider response is not an entity row.                                                                   |
| VM bootstrap (`linux_vm_provider::vm_bootstrap_settled`)                                                                       | Agent and transient outbox           | Body until mapped   | Completion data and the exact admitted operation must survive outbox cleanup.                                 |
| Remote exchange (`multi_server_orchestration::settle_delivery`, `multi_server_delivery::settle_remote`, `remote_transport`)    | Transient transport rows             | Body                | Exact remote response, error and principal data are external results.                                         |
| Remote fetch (`multi_server_tools::fetch`)                                                                                     | Fetch row deleted                    | Body                | The reply survives deletion of the fetch row.                                                                 |
| Pairing (`multi_server_pairing::save_receipt`)                                                                                 | Client may exist                     | Body until mapped   | Pairing reply contains operation-specific information not proven to survive on the client row.                |
| Agent recovery (`agent_management_recovery::save`)                                                                             | Agent                                | Body until mapped   | Recovery reply records the specific operation and its outcome.                                                |
| Execution attempt (`execution_history::save_receipt`)                                                                          | Attempt                              | Body until mapped   | Verify that the attempt retains every exact completion field.                                                 |
| Provider late result and retained completion (`account_worker/store`, `account_worker/retained/storage`)                       | Provider/outbox state                | Body                | These are external outcomes and acknowledgement data.                                                         |
| Notification submission (`notification_dispatch/submission`)                                                                   | Outbox/attempt                       | Body until mapped   | Preserve delivery evidence when the pending owner is retired.                                                 |
| Explicit abandonment (`request_results::settle_abandoned`; account and voice effects)                                          | Outbox deleted                       | Body                | Preserve the authorized unknown outcome without retrying the effect.                                          |

`event_write::settle_request` and `RequestResults::settle` are shared adapters,
not additional families. The remaining direct calls in migration and repository
tests are fixtures for those adapters. Historical rows that cannot be converted
must retain their original typed body through the existing retention window;
conversion must never infer a reference from an unrelated row.

The migration must preserve request identity and signature, authorize before
replay, reject changed content, and apply the owner mutation and receipt in one
transaction. Tests must cover old-shape upgrade, first apply, lost-response
replay, a conflicting signature, and a referenced row changing after settlement.
