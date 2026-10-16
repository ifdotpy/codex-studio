# Terminal differential corpus and mutation witnesses

`terminal_corpus.rs` launches `terminal_oracle.py`, which imports
`turn_lifecycle/oracle_guard.py` before the application and invokes the real
Python `TerminalManager` read, paging, dimensions, restart, and publication
functions. The Python manager writes the SQLite state first. Rust opens those
same files and compares complete return values and all five terminal tables.
Run fast cases with `cargo nextest run -p studio-runtime-services -E
binary(terminal_corpus)`; add `STUDIO_RUNTIME_CORPUS_FULL=1` for the >1 MiB
history case.

## Corpus cases

| Family                            |                            Deterministic cases | Compared evidence                                                                  |
| --------------------------------- | ---------------------------------------------: | ---------------------------------------------------------------------------------- |
| Listing, rows, closure visibility |                         3 seeded terminal rows | Complete listing, Python-written JSON record, output text, all terminal table rows |
| Output paging and offsets         | 4 output offsets; 3 history offset/limit pairs | Complete Python and Rust response objects, including status and truncation         |
| Dimension validation              |                                 8 input shapes | Defaults, min/max, bool, and each low/high boundary                                |
| Restart persistence               |                               1 running record | Python restart output and persisted rows                                           |
| Sync publication                  |         1 visible output event; 2 in full tier | Resource kind and terminal ID                                                      |
| Bounded output                    |    3 full-tier reads over 1,048,580 characters | Retained suffix, first page, and final page                                        |

Live action tests separately exercise input byte delivery, PTY resize, child
environment and cwd, background process groups, identity verification, and
close/reap. The Python/Rust HTTP contract exercises complete create/input/
resize/rename/close responses, receipt replay, invalid sizes, invalid owner,
and unchanged stored rows after rejected requests.

## Mutation table

The manifest is `terminal_pty_mutations.json`. Every mutant must fail a test;
build failures are invalid detections.

| Mutation                         | Witness                                                                                                |
| -------------------------------- | ------------------------------------------------------------------------------------------------------ |
| Discard PTY output               | `printf_output_is_delivered_losslessly`                                                                |
| Discard PTY input bytes          | `cat_accepts_input_resizes_and_closes_through_the_session_port`                                        |
| Ignore resize ioctl              | `resize_changes_the_window_size_reported_by_the_child`                                                 |
| Signal only leader process group | `close_kills_background_jobs_in_other_process_groups`                                                  |
| Inherit server environment       | `child_environment_does_not_inherit_server_only_variables`                                             |
| Reduce output bound              | `driver_truncates_retained_output_but_keeps_archived_cursor_history` and full-corpus suffix assertions |
| Normalize create replay identity | `create_receipt_refuses_a_retry_with_changed_submitted_content`                                        |
| Shorten history page             | Python differential history pages                                                                      |
| Skip restart cleanup             | `restart_reconciles_only_after_cleaning_the_persisted_process_identity`                                |
