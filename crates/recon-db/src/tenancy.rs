//! Firms, clients and accounts. Every lookup is scoped by firm (D-020): a query for another
//! firm's row finds nothing.

use recon_domain::{AccountId, ClientId, Currency, FirmId};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::DbError;

/// A bank account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// The firm.
    pub firm: FirmId,
    /// The client.
    pub client: ClientId,
    /// The account.
    pub id: AccountId,
    /// Display name.
    pub name: String,
    /// Its currency.
    pub currency: Currency,
}

/// Creates a firm.
pub async fn create_firm(pool: &PgPool, id: FirmId, name: &str) -> Result<(), DbError> {
    sqlx::query("INSERT INTO recon.firms (id, name) VALUES ($1, $2)")
        .bind(id.as_uuid())
        .bind(name)
        .execute(pool)
        .await?;
    Ok(())
}

/// True when the firm exists.
pub async fn firm_exists(pool: &PgPool, id: FirmId) -> Result<bool, DbError> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM recon.firms WHERE id = $1)")
            .bind(id.as_uuid())
            .fetch_one(pool)
            .await?,
    )
}

/// Creates a client of `firm` (its decision chain opens with it).
pub async fn create_client(
    pool: &PgPool,
    firm: FirmId,
    id: ClientId,
    name: &str,
) -> Result<(), DbError> {
    sqlx::query("INSERT INTO recon.clients (firm_id, id, name) VALUES ($1, $2, $3)")
        .bind(firm.as_uuid())
        .bind(id.as_uuid())
        .bind(name)
        .execute(pool)
        .await?;
    Ok(())
}

/// Creates an account of `firm`'s `client`.
pub async fn create_account(
    pool: &PgPool,
    firm: FirmId,
    client: ClientId,
    id: AccountId,
    name: &str,
    currency: Currency,
) -> Result<Account, DbError> {
    sqlx::query(
        "INSERT INTO recon.accounts (firm_id, client_id, id, name, currency)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(firm.as_uuid())
    .bind(client.as_uuid())
    .bind(id.as_uuid())
    .bind(name)
    .bind(currency.code())
    .execute(pool)
    .await?;
    Ok(Account {
        firm,
        client,
        id,
        name: name.to_owned(),
        currency,
    })
}

/// Loads `firm`'s account `id`.
pub async fn account(pool: &PgPool, firm: FirmId, id: AccountId) -> Result<Account, DbError> {
    let row = sqlx::query(
        "SELECT client_id, name, currency FROM recon.accounts WHERE firm_id = $1 AND id = $2",
    )
    .bind(firm.as_uuid())
    .bind(id.as_uuid())
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::NotFound)?;
    let currency: String = row.try_get("currency")?;
    Ok(Account {
        firm,
        client: ClientId::from_uuid(row.try_get::<Uuid, _>("client_id")?),
        id,
        name: row.try_get("name")?,
        currency: Currency::from_code(&currency)
            .map_err(|_| DbError::Inconsistent(format!("stored currency {currency:?}")))?,
    })
}
