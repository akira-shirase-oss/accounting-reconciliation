//! Sign-in and sessions (D-021): users and their credentials, TOTP enrolment, recovery
//! codes, the pending bridge between password and TOTP, sessions, the sign-in back-off,
//! client assignment and the read access log. Only digests of bearer secrets are stored
//! (session tokens, pending tokens, recovery codes).

use std::time::Duration;

use recon_domain::{ClientId, FirmId, Role, UserId};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::DbError;

/// A session's idle lifetime (D-021).
pub const IDLE_TTL: Duration = Duration::from_mins(30);
/// Its absolute lifetime from sign-in (D-021).
pub const ABSOLUTE_TTL: Duration = Duration::from_hours(12);
/// How long a password-verified sign-in waits for its TOTP answer.
pub const PENDING_TTL: Duration = Duration::from_mins(5);
/// The TOTP tries one pending sign-in allows before it is destroyed.
pub const PENDING_MAX_ATTEMPTS: i32 = 3;

/// The back-off `failures` consecutive failures earns (D-021: growing, per account and
/// per address). `None` — try now. Bounded: the list ends at 15 minutes.
pub fn lock_seconds(failures: i64) -> Option<i64> {
    match failures {
        ..=2 => None,
        3 => Some(2),
        4 => Some(5),
        5 | 6 => Some(15),
        7..=9 => Some(60),
        _ => Some(900),
    }
}

/// A user row.
#[derive(Debug)]
pub struct UserRow {
    /// The user's firm.
    pub firm: FirmId,
    /// The user's id.
    pub id: UserId,
    /// Email (stored, already normalized lowercase at write).
    pub email: String,
    /// Display name.
    pub display_name: String,
    /// The role (D-018).
    pub role: Role,
    /// The argon2id PHC string; `None` — cannot sign in.
    pub password_hash: Option<String>,
    /// The 160-bit TOTP secret; `None` — never generated.
    pub totp_secret: Option<Vec<u8>>,
    /// True once enrolment succeeded (secret and timestamp move together — the CHECK).
    pub totp_enrolled: bool,
    /// True when the user is disabled: no sign-in, sessions checked live are refused.
    pub disabled: bool,
}

fn user(row: &PgRow) -> Result<UserRow, DbError> {
    let role: String = row.try_get("role")?;
    Ok(UserRow {
        firm: FirmId::from_uuid(row.try_get("firm_id")?),
        id: UserId::from_uuid(row.try_get("id")?),
        email: row.try_get("email")?,
        display_name: row.try_get("display_name")?,
        role: Role::from_code(&role)
            .ok_or_else(|| DbError::Inconsistent(format!("role {role}")))?,
        password_hash: row.try_get("password_hash")?,
        totp_secret: row.try_get("totp_secret")?,
        totp_enrolled: row.try_get::<bool, _>("totp_enrolled")?,
        disabled: row.try_get::<bool, _>("disabled")?,
    })
}

const USER_COLUMNS: &str = "firm_id, id, email, display_name, role, password_hash, totp_secret,
    (totp_enrolled_at IS NOT NULL) AS totp_enrolled, (disabled_at IS NOT NULL) AS disabled";

/// Creates a user; the email is normalized to lowercase (the CHECK requires it).
pub async fn create_user(
    pool: &PgPool,
    firm: FirmId,
    email: &str,
    display_name: &str,
    role: Role,
    password_hash: Option<&str>,
) -> Result<UserId, DbError> {
    let id = UserId::from_uuid(Uuid::now_v7());
    sqlx::query(
        "INSERT INTO recon.users (firm_id, id, email, display_name, role, password_hash)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(firm.as_uuid())
    .bind(id.as_uuid())
    .bind(email.to_lowercase())
    .bind(display_name)
    .bind(role.code())
    .bind(password_hash)
    .execute(pool)
    .await?;
    Ok(id)
}

/// The firm user with `email` (normalized), if any.
pub async fn user_by_email(
    pool: &PgPool,
    firm: FirmId,
    email: &str,
) -> Result<Option<UserRow>, DbError> {
    let sql = format!("SELECT {USER_COLUMNS} FROM recon.users WHERE firm_id = $1 AND email = $2");
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(firm.as_uuid())
        .bind(email.to_lowercase())
        .fetch_optional(pool)
        .await?;
    row.map(|r| user(&r)).transpose()
}

/// The firm user with `id`, if any.
pub async fn user_by_id(
    pool: &PgPool,
    firm: FirmId,
    id: UserId,
) -> Result<Option<UserRow>, DbError> {
    let sql = format!("SELECT {USER_COLUMNS} FROM recon.users WHERE firm_id = $1 AND id = $2");
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(firm.as_uuid())
        .bind(id.as_uuid())
        .fetch_optional(pool)
        .await?;
    row.map(|r| user(&r)).transpose()
}

/// Disables a user (their live sessions are refused on next use — `session()` joins and
/// rejects disabled users).
pub async fn disable_user(pool: &PgPool, firm: FirmId, user: UserId) -> Result<bool, DbError> {
    let n = sqlx::query(
        "UPDATE recon.users SET disabled_at = now()
         WHERE firm_id = $1 AND id = $2 AND disabled_at IS NULL",
    )
    .bind(firm.as_uuid())
    .bind(user.as_uuid())
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n == 1)
}

/// Stores a newly generated TOTP secret before enrolment completes.
pub async fn set_totp_secret(
    pool: &PgPool,
    firm: FirmId,
    user: UserId,
    secret: &[u8],
) -> Result<(), DbError> {
    let n = sqlx::query(
        "UPDATE recon.users SET totp_secret = $3
         WHERE firm_id = $1 AND id = $2 AND totp_enrolled_at IS NULL",
    )
    .bind(firm.as_uuid())
    .bind(user.as_uuid())
    .bind(secret)
    .execute(pool)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(DbError::Inconsistent("the user is already enrolled".into()));
    }
    Ok(())
}

/// Completes enrolment: marks it done and stores the hashed recovery codes, atomically.
pub async fn enrol_totp(
    pool: &PgPool,
    firm: FirmId,
    user: UserId,
    code_hashes: &[[u8; 32]],
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let n = sqlx::query(
        "UPDATE recon.users SET totp_enrolled_at = now()
         WHERE firm_id = $1 AND id = $2 AND totp_secret IS NOT NULL AND totp_enrolled_at IS NULL",
    )
    .bind(firm.as_uuid())
    .bind(user.as_uuid())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(DbError::Inconsistent(
            "no unenrolled secret to enrol".into(),
        ));
    }
    for hash in code_hashes {
        sqlx::query(
            "INSERT INTO recon.user_recovery_codes (firm_id, user_id, code_hash)
             VALUES ($1, $2, $3)",
        )
        .bind(firm.as_uuid())
        .bind(user.as_uuid())
        .bind(&hash[..])
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Marks `hash`'s recovery code used and reports whether it was live.
pub async fn use_recovery_code(
    pool: &PgPool,
    firm: FirmId,
    user: UserId,
    hash: &[u8; 32],
) -> Result<bool, DbError> {
    let n = sqlx::query(
        "UPDATE recon.user_recovery_codes SET used_at = now()
         WHERE firm_id = $1 AND user_id = $2 AND code_hash = $3 AND used_at IS NULL",
    )
    .bind(firm.as_uuid())
    .bind(user.as_uuid())
    .bind(&hash[..])
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n == 1)
}

/// How many of a user's recovery codes remain (reported in the sign-in response).
pub async fn recovery_codes_left(
    pool: &PgPool,
    firm: FirmId,
    user: UserId,
) -> Result<i64, DbError> {
    let n = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM recon.user_recovery_codes
         WHERE firm_id = $1 AND user_id = $2 AND used_at IS NULL",
    )
    .bind(firm.as_uuid())
    .bind(user.as_uuid())
    .fetch_one(pool)
    .await?;
    Ok(n)
}

/// Creates a user and their client assignments in one transaction: a bad client fails
/// the whole create.
pub async fn create_user_with_clients(
    pool: &PgPool,
    firm: FirmId,
    email: &str,
    display_name: &str,
    role: Role,
    password_hash: &str,
    clients: &[ClientId],
) -> Result<UserId, DbError> {
    let mut tx = pool.begin().await?;
    let id = UserId::from_uuid(Uuid::now_v7());
    sqlx::query(
        "INSERT INTO recon.users (firm_id, id, email, display_name, role, password_hash)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(firm.as_uuid())
    .bind(id.as_uuid())
    .bind(email.to_lowercase())
    .bind(display_name)
    .bind(role.code())
    .bind(password_hash)
    .execute(&mut *tx)
    .await?;
    for client in clients {
        sqlx::query(
            "INSERT INTO recon.user_clients (firm_id, user_id, client_id)
             VALUES ($1, $2, $3)",
        )
        .bind(firm.as_uuid())
        .bind(id.as_uuid())
        .bind(client.as_uuid())
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(id)
}

// -----------------------------------------------------------------------------------------------
// The pending bridge and the sign-in throttle.

/// Opens a pending sign-in after the password verified.
pub async fn create_pending(
    pool: &PgPool,
    firm: FirmId,
    user: UserId,
    token_hash: &[u8; 32],
) -> Result<(), DbError> {
    sqlx::query(
        "INSERT INTO recon.sign_in_pending
             (firm_id, user_id, token_hash, expires_at)
         VALUES ($1, $2, $3, now() + interval '5 minutes')",
    )
    .bind(firm.as_uuid())
    .bind(user.as_uuid())
    .bind(&token_hash[..])
    .execute(pool)
    .await?;
    Ok(())
}

/// A live pending row by its token digest: `(firm, user, attempts so far)`. An expired
/// row is deleted and answers `None`.
pub async fn pending(
    pool: &PgPool,
    token_hash: &[u8; 32],
) -> Result<Option<(FirmId, UserId, i32)>, DbError> {
    sqlx::query("DELETE FROM recon.sign_in_pending WHERE expires_at <= now()")
        .execute(pool)
        .await?;
    let row = sqlx::query(
        "SELECT firm_id, user_id, attempts FROM recon.sign_in_pending WHERE token_hash = $1",
    )
    .bind(&token_hash[..])
    .fetch_optional(pool)
    .await?;
    row.map(|r| {
        Ok((
            FirmId::from_uuid(r.try_get("firm_id")?),
            UserId::from_uuid(r.try_get("user_id")?),
            r.try_get::<i32, _>("attempts")?,
        )) as Result<_, DbError>
    })
    .transpose()
}

/// Records a failed TOTP answer; returns `true` once the pending row is destroyed (the
/// attempts cap or an already-gone row).
pub async fn fail_pending(pool: &PgPool, token_hash: &[u8; 32]) -> Result<bool, DbError> {
    let attempts = sqlx::query(
        "UPDATE recon.sign_in_pending SET attempts = LEAST(attempts + 1, $2)
         WHERE token_hash = $1 RETURNING attempts",
    )
    .bind(&token_hash[..])
    .bind(PENDING_MAX_ATTEMPTS)
    .fetch_optional(pool)
    .await?
    .map(|r| r.try_get::<i32, _>("attempts"))
    .transpose()?;
    match attempts {
        Some(n) if n < PENDING_MAX_ATTEMPTS => Ok(false),
        _ => {
            sqlx::query("DELETE FROM recon.sign_in_pending WHERE token_hash = $1")
                .bind(&token_hash[..])
                .execute(pool)
                .await?;
            Ok(true)
        }
    }
}

/// Consumes a pending row (a successful answer).
pub async fn consume_pending(pool: &PgPool, token_hash: &[u8; 32]) -> Result<(), DbError> {
    sqlx::query("DELETE FROM recon.sign_in_pending WHERE token_hash = $1")
        .bind(&token_hash[..])
        .execute(pool)
        .await?;
    Ok(())
}

/// When `key` is still locked: seconds left. A passed lock is cleared so a fresh count
/// begins.
pub async fn throttle_locked(pool: &PgPool, key: &str) -> Result<Option<i64>, DbError> {
    let row = sqlx::query("SELECT locked_until FROM recon.sign_in_throttle WHERE key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    let Some(until): Option<time::OffsetDateTime> =
        row.and_then(|r| r.try_get("locked_until").ok().flatten())
    else {
        return Ok(None);
    };
    let left = sqlx::query_scalar::<_, f64>(
        "SELECT EXTRACT(EPOCH FROM ($1::timestamptz - now()))::float8",
    )
    .bind(until)
    .fetch_one(pool)
    .await?;
    if left <= 0.0 {
        // The lock lapsed; the count stays so repeated failures keep escalating.
        sqlx::query("UPDATE recon.sign_in_throttle SET locked_until = NULL WHERE key = $1")
            .bind(key)
            .execute(pool)
            .await?;
        return Ok(None);
    }
    #[expect(clippy::cast_possible_truncation, reason = "left > 0, ceil <= 900")]
    #[allow(clippy::cast_sign_loss, reason = "left > 0 by the check above")]
    Ok(Some(left.ceil() as i64))
}

/// Records a failed sign-in for `key`; when it earned a lock, its seconds.
pub async fn throttle_fail(pool: &PgPool, key: &str) -> Result<Option<i64>, DbError> {
    let failures: i64 = sqlx::query(
        "INSERT INTO recon.sign_in_throttle (key, failures, last_attempt)
         VALUES ($1, 1, now())
         ON CONFLICT (key) DO UPDATE SET failures = recon.sign_in_throttle.failures + 1,
             last_attempt = now()
         RETURNING failures",
    )
    .bind(key)
    .fetch_one(pool)
    .await?
    .try_get::<i32, _>("failures")
    .map(i64::from)?;
    let Some(seconds) = lock_seconds(failures) else {
        return Ok(None);
    };
    sqlx::query(
        "UPDATE recon.sign_in_throttle
            SET locked_until = now() + ($2::bigint * interval '1 second') WHERE key = $1",
    )
    .bind(key)
    .bind(seconds)
    .execute(pool)
    .await?;
    Ok(Some(seconds))
}

/// A successful sign-in clears `key`.
pub async fn throttle_reset(pool: &PgPool, key: &str) -> Result<(), DbError> {
    sqlx::query("DELETE FROM recon.sign_in_throttle WHERE key = $1")
        .bind(key)
        .execute(pool)
        .await?;
    Ok(())
}

// -----------------------------------------------------------------------------------------------
// Sessions.

/// A live session, with the user's live role and disabled flag joined in.
#[derive(Debug)]
pub struct SessionRow {
    /// The session's id.
    pub id: Uuid,
    /// The firm.
    pub firm: FirmId,
    /// The user.
    pub user: UserId,
    /// The user's current role.
    pub role: Role,
    /// The CSRF secret the client mirrors on mutations.
    pub csrf: [u8; 32],
}

/// Creates a session for `token_hash`/`csrf` (the raw secrets are the caller's to send).
pub async fn create_session(
    pool: &PgPool,
    firm: FirmId,
    user: UserId,
    token_hash: &[u8; 32],
    csrf: &[u8; 32],
) -> Result<Uuid, DbError> {
    sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO recon.sessions
             (firm_id, user_id, token_hash, csrf, idle_expires_at, absolute_expires_at)
         VALUES ($1, $2, $3, $4, now() + interval '30 minutes', now() + interval '12 hours')
         RETURNING id",
    )
    .bind(firm.as_uuid())
    .bind(user.as_uuid())
    .bind(&token_hash[..])
    .bind(&csrf[..])
    .fetch_one(pool)
    .await
    .map_err(DbError::from)
}

/// The live session for `token_hash`: `None` when unknown, expired or its user is
/// disabled — an expired row is deleted. Every `TOUCH_AFTER` of activity the idle expiry
/// slides forward (capped at the absolute).
pub async fn session(pool: &PgPool, token_hash: &[u8; 32]) -> Result<Option<SessionRow>, DbError> {
    let row = sqlx::query(
        "SELECT s.id, s.firm_id, s.user_id, s.csrf, u.role, u.disabled_at IS NOT NULL AS disabled,
                s.idle_expires_at <= now() AS idle_dead, s.absolute_expires_at <= now() AS abs_dead,
                s.last_seen_at + interval '1 minute' <= now() AS retouch
           FROM recon.sessions s JOIN recon.users u
             ON u.firm_id = s.firm_id AND u.id = s.user_id
          WHERE s.token_hash = $1",
    )
    .bind(&token_hash[..])
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    if row.try_get::<bool, _>("idle_dead")? || row.try_get::<bool, _>("abs_dead")? {
        sqlx::query("DELETE FROM recon.sessions WHERE token_hash = $1")
            .bind(&token_hash[..])
            .execute(pool)
            .await?;
        return Ok(None);
    }
    if row.try_get::<bool, _>("disabled")? {
        return Ok(None);
    }
    if row.try_get::<bool, _>("retouch")? {
        sqlx::query(
            "UPDATE recon.sessions
                SET last_seen_at = now(),
                    idle_expires_at = LEAST(now() + interval '30 minutes', absolute_expires_at)
              WHERE token_hash = $1",
        )
        .bind(&token_hash[..])
        .execute(pool)
        .await?;
    }
    let role: String = row.try_get("role")?;
    let csrf: Vec<u8> = row.try_get("csrf")?;
    Ok(Some(SessionRow {
        id: row.try_get("id")?,
        firm: FirmId::from_uuid(row.try_get("firm_id")?),
        user: UserId::from_uuid(row.try_get("user_id")?),
        role: Role::from_code(&role)
            .ok_or_else(|| DbError::Inconsistent(format!("role {role}")))?,
        csrf: csrf
            .try_into()
            .map_err(|_| DbError::Inconsistent("csrf is not 32 bytes".into()))?,
    }))
}

/// Deletes the session (sign-out); `true` when one existed.
pub async fn delete_session(pool: &PgPool, token_hash: &[u8; 32]) -> Result<bool, DbError> {
    let n = sqlx::query("DELETE FROM recon.sessions WHERE token_hash = $1")
        .bind(&token_hash[..])
        .execute(pool)
        .await?
        .rows_affected();
    Ok(n == 1)
}

// -----------------------------------------------------------------------------------------------
// Assignment and the access log.

/// Assigns a client to a user; already-assigned is a no-op.
pub async fn assign_client(
    pool: &PgPool,
    firm: FirmId,
    user: UserId,
    client: ClientId,
) -> Result<(), DbError> {
    sqlx::query(
        "INSERT INTO recon.user_clients (firm_id, user_id, client_id)
         VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
    )
    .bind(firm.as_uuid())
    .bind(user.as_uuid())
    .bind(client.as_uuid())
    .execute(pool)
    .await?;
    Ok(())
}

/// Whether the user is assigned the client.
pub async fn is_assigned(
    pool: &PgPool,
    firm: FirmId,
    user: UserId,
    client: ClientId,
) -> Result<bool, DbError> {
    let n = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM recon.user_clients
         WHERE firm_id = $1 AND user_id = $2 AND client_id = $3",
    )
    .bind(firm.as_uuid())
    .bind(user.as_uuid())
    .bind(client.as_uuid())
    .fetch_one(pool)
    .await?;
    Ok(n == 1)
}

/// Every client assigned to the user.
pub async fn assigned_clients(
    pool: &PgPool,
    firm: FirmId,
    user: UserId,
) -> Result<Vec<ClientId>, DbError> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT client_id FROM recon.user_clients
         WHERE firm_id = $1 AND user_id = $2 ORDER BY client_id",
    )
    .bind(firm.as_uuid())
    .bind(user.as_uuid())
    .fetch_all(pool)
    .await?;
    Ok(ids.into_iter().map(ClientId::from_uuid).collect())
}

/// Records a client-scoped request (user × client × day upsert, D-021).
pub async fn record_access(
    pool: &PgPool,
    firm: FirmId,
    user: UserId,
    client: ClientId,
) -> Result<(), DbError> {
    sqlx::query(
        "INSERT INTO recon.access_log (firm_id, user_id, client_id, day)
         VALUES ($1, $2, $3, CURRENT_DATE)
         ON CONFLICT (firm_id, user_id, client_id, day)
         DO UPDATE SET requests = recon.access_log.requests + 1, last_at = now()",
    )
    .bind(firm.as_uuid())
    .bind(user.as_uuid())
    .bind(client.as_uuid())
    .execute(pool)
    .await?;
    Ok(())
}
