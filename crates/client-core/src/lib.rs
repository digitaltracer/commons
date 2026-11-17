//! The headless Commons client core (decision 2).
//!
//! The UI sends commands and renders view models; it never touches storage or the network.
//! One [`Account`] runs per signed-in server account, each with its own SQLite cache, so the
//! client can be signed in to several Commons servers at once (decision 3).

pub mod account;
pub mod apply;
pub mod core;
pub mod platform;
pub mod store;
pub mod transport;
pub mod views;
pub mod window;

pub use account::{Account, AccountConfig};
pub use core::Core;
