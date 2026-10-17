use studio_runtime_services::{
    sync_entity_context::RoomDirectory,
    sync_policy::{ProjectVaultSnapshot, maintain_entity_projections},
};
use studio_runtime_store::RuntimeStore;

/// Run the complete projection rebuild in its own writer transaction, as the
/// sync repair path does. Seeded fixtures use it so lifecycle actions start
/// from current views, like a live state directory.
pub async fn full_rebuild(store: &RuntimeStore) {
    let state_dir = store.root().to_path_buf();
    let reader = store.read().expect("reader");
    let rooms = RoomDirectory::prepare_for_sync(&reader).expect("room directory");
    let vault = ProjectVaultSnapshot::capture_for_sync(&state_dir, &reader).expect("vault");
    drop(reader);
    store
        .transaction(move |tx| {
            maintain_entity_projections(
                tx,
                &state_dir.to_string_lossy(),
                &vault,
                &rooms,
                |_, _, _, _| true,
            )
        })
        .await
        .expect("full projection rebuild");
}
