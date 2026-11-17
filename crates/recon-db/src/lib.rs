//! PostgreSQL storage for Accounting Reconciliation (ARCHITECTURE: `recon-db`).
//!
//! PostgreSQL is the only authority. Every query is scoped by firm (D-020) and every rule
//! that must hold for raw SQL too is enforced by the schema (`migrations/0001_spine.sql`):
//! composite firm keys, the append-only hash-chained decision log, imports that become
//! visible only whole. The server connects as a runtime role with nothing beyond its
//! grants ([`roles`], D-037).
//!
//! Queries use runtime-checked `sqlx::query` with explicit binds, so building needs no
//! database; the tests in `tests/` prove them against a real PostgreSQL.

pub mod decisions;
pub mod error;
pub mod imports;
pub mod pool;
pub mod replay;
pub mod roles;
pub mod tenancy;
#[cfg(feature = "test-support")]
pub mod testing;

pub use error::DbError;
pub use pool::{PoolSettings, connect_lazy};

/// The embedded migrations. Their table is `public._sqlx_migrations`, named explicitly so
/// the owner's `search_path` cannot move it.
pub fn migrator() -> sqlx::migrate::Migrator {
    let mut migrator = sqlx::migrate!("./migrations");
    migrator.dangerous_set_table_name("public._sqlx_migrations");
    migrator
}
