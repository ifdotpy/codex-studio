//! Process supervisor protocol values shared by the runtime and the supervisor.

/// Largest encoded supervisor transport envelope, in bytes (32 MiB), including its JSON
/// escaping. Native history pages and command results can exceed the former 1 MiB cap.
pub const MAX_SUPERVISOR_FRAME_BYTES: usize = 33554432;
/// Open-receipt field that names one journal handle instance: the supervisor journal's
/// random identity plus the handle row's creation time. Event sequences and saved
/// applied-event cursors are meaningful only inside one journal epoch.
pub const SUPERVISOR_JOURNAL_EPOCH_FIELD: &str = "journalEpoch";
/// `supervisor_state` key of the random identity created with each supervisor journal.
pub const SUPERVISOR_JOURNAL_ID_KEY: &str = "journalId";
