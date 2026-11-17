//! Types shared across the wire (server ↔ core) and the IPC boundary (core ↔ UI).
//! Nothing here may depend on storage, network or OS crates: the UI links it.

pub mod api;
pub mod events;
pub mod markdown;
pub mod model;
pub mod view;
pub mod window;

/// Version of the `/v1` wire protocol this build speaks.
pub const PROTOCOL_VERSION: u32 = 1;
/// Client build version, compared against the server's `min_client_version`.
pub const CLIENT_VERSION: &str = "0.1.0";
