//! Provider-owned stop policy; identity and wire profiles belong to runtime-schema.

pub use studio_runtime_schema::provider_kind::{
    ProviderCapability, ProviderKind, RequestIdScheme, UnknownProvider, WireProfile,
};

/// The thread request that ends command processes which outlive
/// `turn/interrupt`, sent after a stop interrupts the agent's turn.
///
/// The Codex app-server keeps a thread's unified exec sessions (background
/// terminals) running after an interrupt; `thread/backgroundTerminals/clean`
/// ends them. Studio command monitors use `command/exec`, which this request
/// does not touch. The Claude bridge stops its background tasks inside
/// `turn/interrupt`, so Claude needs no second request.
#[must_use]
pub const fn stop_terminals_method(provider: ProviderKind) -> Option<crate::Method> {
    match provider {
        ProviderKind::Codex => Some(crate::Method::ThreadBackgroundTerminalsClean),
        ProviderKind::Claude => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_cleanup_preserves_provider_policy() {
        assert_eq!(
            stop_terminals_method(ProviderKind::Codex),
            Some(crate::Method::ThreadBackgroundTerminalsClean)
        );
        assert_eq!(stop_terminals_method(ProviderKind::Claude), None);
    }
}
