//! The parts of sqlx the server uses, taken from `sqlx-core` and `sqlx-postgres` directly.
//! The `sqlx` facade crate would put `sqlx-sqlite` in the lockfile, and its `libsqlite3-sys`
//! links key conflicts with the patched SQLite the client core needs.
pub use sqlx_core::error::{Error, Result};
pub use sqlx_core::executor::Executor;
pub use sqlx_core::query::query;
pub use sqlx_core::query_as::query_as;
pub use sqlx_core::query_scalar::query_scalar;
pub use sqlx_core::row::Row;
pub use sqlx_postgres::{PgConnection, PgPool, Postgres};

pub mod postgres {
    pub use sqlx_postgres::*;
}

pub mod migrate {
    pub use sqlx_core::migrate::*;
}
