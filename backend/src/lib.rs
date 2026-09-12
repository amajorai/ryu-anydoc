//! AnyDoc's standalone extraction service and Ryu `document.parse` provider.
//!
//! The crate has no dependency on `apps/core`. Core owns the manifest, lifecycle,
//! node authentication, and public-mount policy; this crate owns document bytes,
//! conversion jobs, limits, and the standalone API. The same binary can therefore
//! run behind Core or as a customer-facing service with API keys.

pub mod api;
pub mod auth;
pub mod convert;
pub mod jobs;
pub mod limits;
pub mod paths;
pub mod state;

pub use api::router;
pub use state::AnyDocState;
