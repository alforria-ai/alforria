//! Route plumbing: protocol, endpoint, auth, framing, client, executor.

pub mod auth;
pub mod client;
pub mod endpoint;
pub mod executor;
pub mod framing;
pub mod protocol;

pub use auth::{Auth, AuthInput, Credential, Headers};
pub use endpoint::{Endpoint, EndpointInput, EndpointPart, EndpointPatch};
pub use framing::Framing;
pub use protocol::Protocol;
