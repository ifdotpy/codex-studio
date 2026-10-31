//! Hold obligations whose sending commit may have been lost during an unclean stop.
use studio_runtime_store::remote_outbox::RemoteOutbox;
use studio_runtime_store::{RuntimeStore, SchedulerRepository, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Provider(#[from] crate::account_worker::WorkerError),
    #[error(transparent)]
    Callback(#[from] crate::provider_callbacks::CallbackError),
    #[error(transparent)]
    Voice(#[from] crate::voice::provider::VoiceProviderError),
    #[error(transparent)]
    Native(#[from] crate::native_actions::NativeError),
    #[error(transparent)]
    Title(#[from] crate::title_generation::TitleError),
}

pub async fn hold(store: &RuntimeStore) -> Result<(), RecoveryError> {
    store
        .transaction_with(|tx| {
            SchedulerRepository::hold_unclean_steers(tx)?;
            SchedulerRepository::hold_unclean_starts(tx)?;
            crate::account_worker::retained::hold_unclean_provider_calls(tx)?;
            crate::provider_callbacks::storage::hold_unclean_responses(tx)?;
            crate::voice::hold_unclean_effects(tx)?;
            crate::native_actions::hold_unclean_actions(tx)?;
            crate::title_generation::hold_unclean_jobs(tx)?;
            RemoteOutbox::hold_unclean(tx).map_err(StoreError::from)?;
            crate::multi_server_orchestration::hold_unclean_server_requests(tx).map_err(
                |error| match error {
                    crate::multi_server_auth::AccessError::Store(error) => error,
                    other @ (crate::multi_server_auth::AccessError::Refused { .. }
                    | crate::multi_server_auth::AccessError::Stored { .. }
                    | crate::multi_server_auth::AccessError::Sqlite(_)) => {
                        StoreError::SchemaMismatch(other.to_string())
                    }
                },
            )?;
            Ok::<_, RecoveryError>(())
        })
        .await
}
