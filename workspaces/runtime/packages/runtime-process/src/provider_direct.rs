//! Backend-owned direct provider launch over the bounded stdio adapter.
//! Unlike supervisor attachments, these children are intentionally terminated
//! and reaped when the backend account registry shuts down.
//! Registered difference (`workspace-state/image-execution`): image-workspace
//! agents are refused before provider launch, so direct mode uses the unwrapped
//! command. When the image backend moves to Rust, it must supply this prefix natively.
//!
//! A direct child is the account's persistent provider session: it has no
//! lifetime deadline or total output budget, and each stdout frame is bounded by
//! the provider codec's frame limit. Its stderr goes to the state directory's
//! app-server log when the backend supplies one.
use crate::{
    Generation, ProviderHandle, ProviderLaunchSpec,
    child_priority::ChildPriority,
    operational_log::OperationalLog,
    provider_seam::{AdapterError, OpenFuture, OpenOptions, ProviderTaskFactory, Shutdown},
    provider_stdio::StdioProviderTaskFactory,
};

pub struct DirectProviderTaskFactory {
    priority: ChildPriority,
    stderr_log: Option<OperationalLog>,
}

impl DirectProviderTaskFactory {
    #[must_use]
    pub fn new(priority: ChildPriority, stderr_log: Option<OperationalLog>) -> Self {
        Self {
            priority,
            stderr_log,
        }
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
            stderr_log: self.stderr_log.clone(),
        })
    }
}

pub struct PreparedDirectProviderTask {
    handle: ProviderHandle,
    generation: Generation,
    launch: ProviderLaunchSpec,
    clock: crate::provider_seam::SessionClock,
    priority: ChildPriority,
    stderr_log: Option<OperationalLog>,
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
                deadline: None,
                max_frame_bytes: studio_runtime_providers::MAX_FRAME_BYTES,
                max_output_bytes: None,
                stderr_log: self.stderr_log.clone(),
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
