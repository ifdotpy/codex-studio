//! Shared encrypted store format, keys, object transport, and bounded HTTP/1.
#![deny(unsafe_code)]

#[cfg(feature = "native")]
pub mod aws;
pub mod canonical;
pub mod epochs;
pub mod format;
#[cfg(feature = "native")]
pub mod http;
pub mod index;
pub mod keys;
pub mod paths;
#[cfg(feature = "native")]
pub mod s3;
#[cfg(any(feature = "native", feature = "wasm"))]
pub mod zst;

pub mod person;
pub mod public;
pub mod request;
pub mod session;

#[cfg(any(feature = "native", feature = "wasm"))]
pub mod reader;
pub mod record;
