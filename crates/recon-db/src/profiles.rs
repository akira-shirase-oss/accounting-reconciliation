//! Import profiles stored per firm (D-062): immutable per version, a new version for every
//! change, at most [`MAX_PROFILE_NAMES`] names per firm and 1,000 versions per name. The
//! definition is stored as given (JSON) with the SHA-256 of its canonical form, which the
//! server recomputes when it loads the profile.

use recon_domain::FirmId;
use sqlx::types::Json;
use sqlx::{PgPool, Row};

use crate::DbError;

/// Most profile names one firm may hold.
pub const MAX_PROFILE_NAMES: i64 = 500;

/// A stored profile version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredProfile {
    /// The name.
    pub name: String,
    /// The version.
    pub version: i32,
    /// `statement_csv` or `documents_csv`.
    pub kind: String,
    /// The definition, as stored.
    pub definition: serde_json::Value,
    /// SHA-256 of the canonical definition.
    pub sha256: [u8; 32],
}

/// Stores the next version of `name` for `firm`. [`DbError::Constraint`] when a concurrent
/// create took the version (retry), [`DbError::ProfileLimit`] when the firm holds the most
/// names it may.
pub async fn create(
    pool: &PgPool,
    firm: FirmId,
    name: &str,
    kind: &str,
    definition: &serde_json::Value,
    sha256: [u8; 32],
) -> Result<StoredProfile, DbError> {
    let row = sqlx::query(
        "INSERT INTO recon.import_profiles (firm_id, name, version, kind, definition, sha256)
         SELECT $1, $2,
                coalesce((SELECT max(p.version) FROM recon.import_profiles p
                           WHERE p.firm_id = $1 AND p.name = $2), 0) + 1,
                $3, $4, $5
          WHERE EXISTS (SELECT 1 FROM recon.import_profiles p WHERE p.firm_id = $1 AND p.name = $2)
             OR (SELECT count(DISTINCT p.name) FROM recon.import_profiles p WHERE p.firm_id = $1) < $6
         RETURNING version",
    )
    .bind(firm.as_uuid())
    .bind(name)
    .bind(kind)
    .bind(Json(definition))
    .bind(sha256.as_slice())
    .bind(MAX_PROFILE_NAMES)
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::ProfileLimit)?;
    Ok(StoredProfile {
        name: name.to_owned(),
        version: row.try_get("version")?,
        kind: kind.to_owned(),
        definition: definition.clone(),
        sha256,
    })
}

/// Loads `firm`'s profile `name` at `version`, or its latest version.
pub async fn get(
    pool: &PgPool,
    firm: FirmId,
    name: &str,
    version: Option<i32>,
) -> Result<StoredProfile, DbError> {
    let row = sqlx::query(
        "SELECT name, version, kind, definition, sha256 FROM recon.import_profiles
          WHERE firm_id = $1 AND name = $2 AND ($3::int4 IS NULL OR version = $3)
          ORDER BY version DESC LIMIT 1",
    )
    .bind(firm.as_uuid())
    .bind(name)
    .bind(version)
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::NotFound)?;
    let definition: Json<serde_json::Value> = row.try_get("definition")?;
    let sha256: Vec<u8> = row.try_get("sha256")?;
    Ok(StoredProfile {
        name: row.try_get("name")?,
        version: row.try_get("version")?,
        kind: row.try_get("kind")?,
        definition: definition.0,
        sha256: sha256
            .try_into()
            .map_err(|_| DbError::Inconsistent("profile sha256 is not 32 bytes".into()))?,
    })
}
