//! The layr-desk vault (docs/design.md): keys and slots, the object format, a reader that
//! decrypts states from any store of objects, and the writer that pushes them.

pub mod aws;
pub mod canonical;
pub mod format;
pub mod keys;
pub mod reader;
#[cfg(unix)]
pub mod writer;
pub mod zst;

pub use format::{Entry, Header, Link};
pub use reader::{Reader, Store};
