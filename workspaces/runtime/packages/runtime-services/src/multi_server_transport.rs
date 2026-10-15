//! Stable transport surface for server-to-server features.
//!
//! Outbound exchanges are signed and journaled by `exchange_multi_server` or
//! the production `HttpsPairedTransport`. Inbound handlers verify signed
//! requests and bind owner observations through the injected `OwnerLookup`.
//! Dependent services can use `PairedTransport` to inject an in-process fake
//! peer without network access.

pub use crate::multi_server_auth::{
    AccessError, OwnerObservation, SignedRequest, VerifiedPrincipal,
};
pub use crate::multi_server_orchestration::{
    ExchangeOutcome, HttpsPairedTransport, OrchestrationDriver, OrchestrationEffects,
    PairedTransport,
};
pub use crate::multi_server_pairing::{MultiServerBoundary, OwnerLookup};
pub use crate::remote_transport::{
    MultiServerExchangeInput, RemoteIdentity, exchange_multi_server, load_multi_server_identity,
    multi_server_signed_headers, verify_multi_server_response,
};
