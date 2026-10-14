//! Backend-owned direct provider launch over the bounded stdio adapter.
//! Unlike supervisor attachments, these children are intentionally terminated
//! and reaped when the backend account registry shuts down.
//! Registered difference (`workspace-state/image-execution`): image-workspace
//! agents are refused before provider launch, so direct mode uses the unwrapped
//! command. When the image backend moves to Rust, it must supply this prefix natively.
use crate::{
    Generation, ProviderHandle, ProviderLaunchSpec,
    child_priority::ChildPriority,
    provider_seam::{AdapterError, OpenFuture, OpenOptions, ProviderTaskFactory, Shutdown},
    provider_stdio::StdioProviderTaskFactory,
};
use tokio::time::Instant;

const DIRECT_LIFETIME: std::time::Duration = std::time::Duration::from_secs(86_400);
const MAX_FRAME_BYTES: usize = 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

pub struct DirectProviderTaskFactory {
    priority: ChildPriority,
}

impl DirectProviderTaskFactory {
    #[must_use]
    pub fn new(priority: ChildPriority) -> Self {
        Self { priority }
    }

    /// Resolve a direct launch before the account worker activates its provider
    /// session, matching the supervisor factory's prepare/open boundary.
    pub fn prepare(
        &self,
        handle: ProviderHandle,
        options: OpenOptions,
        shutdown: &Shutdown,
    ) -> Result<PreparedDirectProviderTask, AdapterError> {
        if *shutdown.subscribe().borrow()
            || options.launch.command.is_empty()
            || options.launch.command.iter().any(|arg| arg.contains('\0'))
            || !options.launch.retained_response_ids.is_empty()
            || options
                .launch
                .cwd
                .as_ref()
                .is_some_and(|cwd| !cwd.is_absolute())
            || options.launch.handled_cursor != 0
        {
            return Err(AdapterError::InvalidConfiguration);
        }
        let Some(generation) = options.expected_generation else {
            return Err(AdapterError::InvalidConfiguration);
        };
        if generation.0 == 0 {
            return Err(AdapterError::InvalidConfiguration);
        }
        Ok(PreparedDirectProviderTask {
            handle,
            generation,
            launch: options.launch,
            clock: options.clock,
            priority: self.priority,
        })
    }
}

pub struct PreparedDirectProviderTask {
    handle: ProviderHandle,
    generation: Generation,
    launch: ProviderLaunchSpec,
    clock: crate::provider_seam::SessionClock,
    priority: ChildPriority,
}

impl PreparedDirectProviderTask {
    #[must_use]
    pub fn generation(&self) -> Generation {
        self.generation
    }
}

impl ProviderTaskFactory for PreparedDirectProviderTask {
    fn open(
        &self,
        handle: ProviderHandle,
        options: OpenOptions,
        shutdown: Shutdown,
    ) -> OpenFuture<'_> {
        Box::pin(async move {
            if handle != self.handle || options.expected_generation != Some(self.generation) {
                return Err(AdapterError::InvalidConfiguration);
            }
            if *shutdown.subscribe().borrow() {
                return Err(AdapterError::Cancelled);
            }
            let factory = StdioProviderTaskFactory {
                deadline: Instant::now() + DIRECT_LIFETIME,
                max_frame_bytes: MAX_FRAME_BYTES,
                max_output_bytes: MAX_OUTPUT_BYTES,
            };
            factory
                .open_with_priority(
                    handle,
                    OpenOptions {
                        clock: self.clock.clone(),
                        launch: self.launch.clone(),
                        expected_generation: Some(self.generation),
                    },
                    shutdown,
                    self.priority,
                )
                .await
        })
    }
}
