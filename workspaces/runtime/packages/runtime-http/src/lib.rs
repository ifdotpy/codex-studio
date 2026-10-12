//! Axum HTTP edge for the Studio runtime.

pub mod error;
mod json_body;
pub mod middleware;
pub mod router;
pub mod routes;
pub mod schema;
mod validation;

pub use router::{HttpState, router};
pub use schema::{API_SCHEMA_HASH_HEADER, API_SCHEMA_MISMATCH_HEADER, api_schema_hash, operations};

#[allow(clippy::all)]
pub mod generated {
    include!(concat!(env!("OUT_DIR"), "/generated/mod.rs"));
}

#[cfg(test)]
mod tests;
