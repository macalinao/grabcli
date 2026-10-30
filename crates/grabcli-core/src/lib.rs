//! Shared core for grabcli: the parts every Grab service reuses — config,
//! session, auth/hydra, the allowlisted HTTP client, the read-only response
//! cache, money and output helpers.
//!
//! Service crates (e.g. `grabfood`) depend on this, register their URL
//! allowlist, and make calls through [`ctx::Context`].

pub mod cache;
pub mod cdp;
pub mod config;
pub mod ctx;
pub mod error;
pub mod http;
pub mod hydra;
pub mod jwt;
pub mod location;
pub mod login;
pub mod money;
pub mod output;
pub mod paths;
pub mod places;
pub mod session;
pub mod sso;

pub use cache::{Cache, CacheKey, CacheKind, CacheMode, Fetched};
pub use config::Config;
pub use ctx::Context;
pub use error::{Error, ErrorKind, Result};
pub use http::{Allowlist, Client, Hydra, Request};
pub use location::Location;
pub use money::Money;
pub use output::{Format, Output};
pub use session::Session;
