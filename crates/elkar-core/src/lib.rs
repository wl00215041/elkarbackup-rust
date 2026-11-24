//! Domain model and pure business rules for ElkarBackup.
//!
//! This crate performs no I/O: everything here is deterministic and
//! unit-testable. Persistence lives in `elkar-storage`, execution in
//! `elkar-engine`.

#[macro_use]
mod macros;

pub mod client;
pub mod error;
pub mod ids;
pub mod job;
pub mod location;
pub mod log;
pub mod policy;
pub mod queue;
pub mod rsnapshot;
pub mod schedule;
pub mod script;
pub mod user;

pub use error::{ValidationError, ValidationErrors};
