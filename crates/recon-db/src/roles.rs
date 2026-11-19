//! Database roles (D-037). Three of them, and the server is only ever the last:
//!
//! * the cluster's administrator creates roles and databases (`recon db setup`) and runs
//!   migrations as the owner (`recon db migrate`: `SET ROLE recon_owner`);
//! * `recon_owner` owns the database, the schema and every object, and cannot log in
//!   (D-049): whoever acts as it can disable the log's guards, so nobody holds it as a
//!   login;
//! * `recon_app` logs in for the server, inheriting `recon_runtime`'s grants: no
//!   superuser, no ownership, no `TRUNCATE`, no DDL, no `TEMP`, no `BYPASSRLS`.
//!
//! [`check_runtime`] is the server's startup check: it refuses to serve on a connection
//! with any power beyond those grants, and on a schema whose functions do not pin
//! `search_path`.

use sqlx::{AssertSqlSafe, Executor, PgConnection, PgPool, Row};

use crate::DbError;

/// The owner role.
pub const OWNER: &str = "recon_owner";
/// The group role holding the server's grants.
pub const RUNTIME: &str = "recon_runtime";
/// The server's login role.
pub const APP: &str = "recon_app";

/// The `search_path` every function in schema `recon` must pin.
pub const PINNED_SEARCH_PATH: &str = "search_path=pg_catalog, pg_temp";

const SETUP_LOCK: i64 = 7_262_001;

/// True for a database or role name this code is willing to interpolate.
pub fn is_safe_identifier(name: &str) -> bool {
    let b = name.as_bytes();
    matches!(b.first(), Some(c) if c.is_ascii_lowercase() || *c == b'_')
        && b.len() <= 63
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

async fn execute_formatted(
    conn: &mut PgConnection,
    template: &'static str,
    args: &[&str],
) -> Result<(), DbError> {
    // format() quotes identifiers (%I) and literals (%L) on the server, so a password can
    // contain anything without becoming SQL.
    let mut query = sqlx::query_scalar::<_, String>(template);
    for arg in args {
        query = query.bind(*arg);
    }
    let statement = query.fetch_one(&mut *conn).await?;
    conn.execute(AssertSqlSafe(statement)).await?;
    Ok(())
}

/// Creates (or re-asserts) the three roles. Idempotent, serialized by an advisory lock so
/// parallel test processes can call it. Needs a role with `CREATEROLE` (the stack's
/// superuser in development).
pub async fn ensure_roles(admin: &mut PgConnection, app_password: &str) -> Result<(), DbError> {
    sqlx::query("SELECT pg_catalog.pg_advisory_lock($1)")
        .bind(SETUP_LOCK)
        .execute(&mut *admin)
        .await?;
    let result = ensure_roles_locked(admin, app_password).await;
    let unlocked = sqlx::query("SELECT pg_catalog.pg_advisory_unlock($1)")
        .bind(SETUP_LOCK)
        .execute(&mut *admin)
        .await;
    result?;
    unlocked?;
    Ok(())
}

async fn ensure_roles_locked(admin: &mut PgConnection, app_password: &str) -> Result<(), DbError> {
    for role in [OWNER, RUNTIME, APP] {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = $1)",
        )
        .bind(role)
        .fetch_one(&mut *admin)
        .await?;
        if !exists {
            execute_formatted(
                admin,
                "SELECT pg_catalog.format('CREATE ROLE %I', $1::text)",
                &[role],
            )
            .await?;
        }
    }
    let attributes = "NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS";
    // The statement carries a SCRAM verifier, never the password itself (D-057).
    let verifier = scram_verifier(app_password)?;
    execute_formatted(
        admin,
        "SELECT pg_catalog.format('ALTER ROLE %I WITH LOGIN INHERIT %s PASSWORD %L', $1::text, $2::text, $3::text)",
        &[APP, attributes, &verifier],
    )
    .await?;
    for role in [OWNER, RUNTIME] {
        execute_formatted(
            admin,
            "SELECT pg_catalog.format('ALTER ROLE %I WITH NOLOGIN %s PASSWORD NULL', $1::text, $2::text)",
            &[role, attributes],
        )
        .await?;
    }
    // A non-superuser administrator (a managed service's) needs SET on the owner to migrate.
    let superuser: bool =
        sqlx::query_scalar("SELECT rolsuper FROM pg_catalog.pg_roles WHERE rolname = current_user")
            .fetch_one(&mut *admin)
            .await?;
    if !superuser {
        execute_formatted(
            admin,
            "SELECT pg_catalog.format('GRANT %I TO CURRENT_USER WITH INHERIT FALSE, SET TRUE', $1::text)",
            &[OWNER],
        )
        .await?;
    }
    let member: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_auth_members m
                          JOIN pg_catalog.pg_roles g ON g.oid = m.roleid
                          JOIN pg_catalog.pg_roles u ON u.oid = m.member
                         WHERE g.rolname = $1 AND u.rolname = $2)",
    )
    .bind(RUNTIME)
    .bind(APP)
    .fetch_one(&mut *admin)
    .await?;
    if !member {
        execute_formatted(
            admin,
            "SELECT pg_catalog.format('GRANT %I TO %I WITH INHERIT TRUE, SET FALSE', $1::text, $2::text)",
            &[RUNTIME, APP],
        )
        .await?;
    }
    Ok(())
}

/// Creates database `name` owned by `recon_owner`, with every default `PUBLIC` privilege
/// (`CONNECT`, `TEMP`) revoked and `CONNECT` granted to the runtime role only.
pub async fn create_database(admin: &mut PgConnection, name: &str) -> Result<(), DbError> {
    if !is_safe_identifier(name) {
        return Err(DbError::Inconsistent(format!(
            "unsafe database name {name:?}"
        )));
    }
    execute_formatted(
        admin,
        "SELECT pg_catalog.format('CREATE DATABASE %I OWNER %I', $1::text, $2::text)",
        &[name, OWNER],
    )
    .await?;
    lock_down_database(admin, name).await
}

/// Revokes `PUBLIC`'s default `CONNECT` and `TEMP` on `name` and grants `CONNECT` to the
/// runtime role. Idempotent.
pub async fn lock_down_database(admin: &mut PgConnection, name: &str) -> Result<(), DbError> {
    execute_formatted(
        admin,
        "SELECT pg_catalog.format('REVOKE ALL ON DATABASE %I FROM PUBLIC', $1::text)",
        &[name],
    )
    .await?;
    execute_formatted(
        admin,
        "SELECT pg_catalog.format('GRANT CONNECT ON DATABASE %I TO %I', $1::text, $2::text)",
        &[name, RUNTIME],
    )
    .await
}

/// Drops `name` (`WITH (FORCE)` terminates anything still connected). The name is
/// validated like `create_database`'s; which names may be dropped is the caller's
/// decision — `recon db drop` allows only the `recon_load_`/`recon_test_` scratch
/// prefixes.
pub async fn drop_database(admin: &mut PgConnection, name: &str) -> Result<(), DbError> {
    if !is_safe_identifier(name) {
        return Err(DbError::Inconsistent(format!(
            "unsafe database name {name:?}"
        )));
    }
    execute_formatted(
        admin,
        "SELECT pg_catalog.format('DROP DATABASE IF EXISTS %I WITH (FORCE)', $1::text)",
        &[name],
    )
    .await
}

/// One reason a connection is not a safe runtime connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleProblem(pub String);

/// What the server's connection may do beyond its grants: empty when it is safe.
pub async fn runtime_problems(pool: &PgPool) -> Result<Vec<RoleProblem>, DbError> {
    let row = sqlx::query(
        "SELECT r.rolsuper, r.rolbypassrls, r.rolcreaterole, r.rolcreatedb, r.rolreplication,
                pg_catalog.has_database_privilege(current_user, pg_catalog.current_database(), 'TEMP') AS temp,
                pg_catalog.has_database_privilege(current_user, pg_catalog.current_database(), 'CREATE') AS create_db,
                pg_catalog.has_schema_privilege(current_user, 'recon', 'CREATE') AS create_recon,
                pg_catalog.has_schema_privilege(current_user, 'public', 'CREATE') AS create_public,
                pg_catalog.pg_has_role(current_user, 'recon_owner', 'MEMBER') AS owner_member,
                (SELECT o.rolcanlogin FROM pg_catalog.pg_roles o WHERE o.rolname = 'recon_owner') AS owner_login,
                EXISTS (SELECT 1 FROM pg_catalog.pg_class c
                          JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                         WHERE n.nspname = 'recon'
                           AND pg_catalog.pg_has_role(current_user, c.relowner, 'MEMBER')) AS owns,
                EXISTS (SELECT 1 FROM pg_catalog.pg_class c
                          JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                         WHERE n.nspname = 'recon' AND c.relkind IN ('r', 'p')
                           AND pg_catalog.has_table_privilege(current_user, c.oid, 'TRUNCATE')) AS truncate,
                pg_catalog.has_table_privilege(current_user, 'recon.decision_log', 'UPDATE')
                    OR pg_catalog.has_table_privilege(current_user, 'recon.decision_log', 'DELETE') AS log_writable
           FROM pg_catalog.pg_roles r
          WHERE r.rolname = current_user",
    )
    .fetch_one(pool)
    .await?;
    let checks = [
        ("rolsuper", "is a superuser"),
        ("rolbypassrls", "bypasses row-level security"),
        ("rolcreaterole", "can create roles"),
        ("rolcreatedb", "can create databases"),
        ("rolreplication", "is a replication role"),
        ("temp", "may create temporary tables"),
        ("create_db", "may create schemas in the database"),
        ("create_recon", "may create objects in schema recon"),
        ("create_public", "may create objects in schema public"),
        ("owner_member", "is a member of recon_owner"),
        (
            "owner_login",
            "shares a cluster whose recon_owner can log in (D-049)",
        ),
        ("owns", "owns objects in schema recon"),
        ("truncate", "may TRUNCATE tables"),
        ("log_writable", "may UPDATE or DELETE the decision log"),
    ];
    let mut problems = vec![];
    for (column, problem) in checks {
        if row.try_get::<bool, _>(column)? {
            problems.push(RoleProblem(format!("the server's role {problem}")));
        }
    }
    for name in unpinned_functions(pool).await? {
        problems.push(RoleProblem(format!(
            "function recon.{name} does not pin {PINNED_SEARCH_PATH}"
        )));
    }
    Ok(problems)
}

/// Functions in schema `recon` that do not pin exactly [`PINNED_SEARCH_PATH`].
pub async fn unpinned_functions(pool: &PgPool) -> Result<Vec<String>, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT p.proname::text
           FROM pg_catalog.pg_proc p
           JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
          WHERE n.nspname = 'recon'
            AND NOT coalesce(p.proconfig @> ARRAY[$1::text], false)
          ORDER BY 1",
    )
    .bind(PINNED_SEARCH_PATH)
    .fetch_all(pool)
    .await?)
}

/// The newest migration applied to the database.
pub async fn applied_version(pool: &PgPool) -> Result<Option<i64>, DbError> {
    Ok(
        sqlx::query_scalar("SELECT max(version) FROM public._sqlx_migrations WHERE success")
            .fetch_one(pool)
            .await?,
    )
}

/// The newest migration this build carries.
pub fn expected_version() -> Option<i64> {
    crate::migrator().iter().map(|m| m.version).max()
}

/// The server's startup check: safe role, pinned functions, and exactly this build's
/// schema. Returns every problem found.
pub async fn check_runtime(pool: &PgPool) -> Result<Vec<RoleProblem>, DbError> {
    let mut problems = runtime_problems(pool).await?;
    let applied = applied_version(pool).await?;
    let expected = expected_version();
    if applied != expected {
        problems.push(RoleProblem(format!(
            "schema version {applied:?} applied, this build expects {expected:?}"
        )));
    }
    Ok(problems)
}

/// SCRAM-SHA-256 iterations (PostgreSQL's default).
const SCRAM_ITERATIONS: u32 = 4_096;

/// The SCRAM-SHA-256 verifier PostgreSQL stores for `password`, with a random salt, so the
/// password never appears in statement text a log or `pg_stat_statements` could record
/// (D-057). Only printable ASCII passwords are accepted: `SASLprep` leaves them unchanged.
pub fn scram_verifier(password: &str) -> Result<String, DbError> {
    if password.is_empty() || !password.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(DbError::Inconsistent(
            "a role password must be printable ASCII".into(),
        ));
    }
    let mut salt = [0_u8; 16];
    getrandom::fill(&mut salt)
        .map_err(|e| DbError::Inconsistent(format!("no randomness for a salt: {e}")))?;
    Ok(scram_verifier_with(password, &salt, SCRAM_ITERATIONS))
}

/// [`scram_verifier`] with a given salt and iteration count (RFC 5802 / RFC 7677).
pub fn scram_verifier_with(password: &str, salt: &[u8], iterations: u32) -> String {
    use sha2::{Digest as _, Sha256};
    let mac = hmac_sha256;
    // PBKDF2-HMAC-SHA256 with one 32-byte block.
    let mut block = salt.to_vec();
    block.extend_from_slice(&1_u32.to_be_bytes());
    let mut u = mac(password.as_bytes(), &block);
    let mut salted = u;
    for _ in 1..iterations {
        u = mac(password.as_bytes(), &u);
        for (s, x) in salted.iter_mut().zip(u) {
            *s ^= x;
        }
    }
    let client_key = mac(&salted, b"Client Key");
    let stored_key: [u8; 32] = Sha256::digest(client_key).into();
    let server_key = mac(&salted, b"Server Key");
    format!(
        "SCRAM-SHA-256${iterations}:{}${}:{}",
        base64(salt),
        base64(&stored_key),
        base64(&server_key)
    )
}

#[expect(
    clippy::expect_used,
    reason = "invariant: HMAC accepts a key of any length"
)]
fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    use hmac::{Hmac, KeyInit as _, Mac as _};
    let mut m =
        Hmac::<sha2::Sha256>::new_from_slice(key).expect("invariant: HMAC accepts any key length");
    m.update(data);
    m.finalize().into_bytes().into()
}

/// Standard base64 with padding.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3).saturating_mul(4));
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18_u32, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(
                    ALPHABET[usize::try_from((n >> shift) & 63).unwrap_or(0)],
                ));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{base64, scram_verifier, scram_verifier_with};

    #[test]
    fn the_verifier_matches_an_independent_computation() {
        // Python hashlib: pbkdf2_hmac("sha256", password, bytes(range(16)), 4096) and RFC 5802.
        assert_eq!(
            scram_verifier_with("recon-test-password", &(0..16).collect::<Vec<u8>>(), 4_096),
            "SCRAM-SHA-256$4096:AAECAwQFBgcICQoLDA0ODw==$b4Kx4XlVdxddIef1OVA/heBriEX8reGWugRoYTbWF1U=:l+s6amJ+YZJsnI3pYeLDwxS7E/x9M0/lAzKIpB9SOlM="
        );
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
    }

    #[test]
    fn the_password_never_appears_and_salts_differ() {
        let a = scram_verifier("recon-app-dev").unwrap_or_default();
        let b = scram_verifier("recon-app-dev").unwrap_or_default();
        assert!(
            a.starts_with("SCRAM-SHA-256$4096:") && !a.contains("recon-app-dev"),
            "{a}"
        );
        assert_ne!(a, b, "each verifier has its own salt");
        assert!(scram_verifier("not ascii é").is_err());
    }
}
