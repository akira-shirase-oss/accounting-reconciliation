//! Import profiles at the API (D-062): the built-ins (`r0-generic-csv`, `ofx`,
//! `r1-generic-documents`) and each firm's own, stored immutably per version. A stored
//! definition is parsed into its typed profile and compiled on every use, and its SHA-256
//! recomputed and compared with the stored one, so what an import records is what it read.

use std::sync::Arc;

use axum::http::StatusCode;
use recon_db::imports::ProfileId;
use recon_db::{DbError, profiles};
use recon_domain::FirmId;
use recon_domain::import::{
    PROFILE_DOCUMENTS, PROFILE_DOCUMENTS_VERSION, PROFILE_OFX, PROFILE_OFX_VERSION, PROFILE_R0,
    PROFILE_R0_VERSION,
};
use recon_import::{CompiledDocuments, CompiledStatement, DocumentProfile, StatementProfile};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::error::ApiError;
use crate::upload::{Media, StatementFormat};

/// SHA-256 of the `ofx` profile's identity (it has no definition to hash).
fn ofx_sha256() -> [u8; 32] {
    Sha256::digest(format!("{PROFILE_OFX} v{PROFILE_OFX_VERSION}")).into()
}

/// The profile kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A CSV statement profile.
    StatementCsv,
    /// A CSV document profile.
    DocumentsCsv,
}

impl Kind {
    const fn code(self) -> &'static str {
        match self {
            Self::StatementCsv => "statement_csv",
            Self::DocumentsCsv => "documents_csv",
        }
    }
}

/// `POST /v1/firms/{firm}/profiles`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewProfile {
    name: String,
    kind: Kind,
    definition: Value,
}

fn invalid(problems: &[String]) -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_profile",
        "the profile cannot be used; nothing was stored",
    )
    .with("problems", json!(problems))
}

fn is_reserved(name: &str) -> bool {
    [PROFILE_R0, PROFILE_OFX, PROFILE_DOCUMENTS].contains(&name)
}

fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    matches!(b.first(), Some(c) if c.is_ascii_lowercase())
        && b.len() <= 63
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// Validates, canonicalizes and stores the next version of a profile.
pub async fn create(pool: &sqlx::PgPool, firm: FirmId, new: NewProfile) -> Result<Value, ApiError> {
    if !valid_name(&new.name) || is_reserved(&new.name) {
        return Err(invalid(&[
            "name: 1 to 63 of a-z, 0-9 and -, starting with a letter, not a built-in name"
                .to_owned(),
        ]));
    }
    // serde's own message would echo the caller's field names; a fixed one does not.
    let not_json = |_: serde_json::Error| {
        invalid(&["definition: a field is missing, unknown or of the wrong type".to_owned()])
    };
    let (canonical, sha256) = match new.kind {
        Kind::StatementCsv => {
            let typed: StatementProfile =
                serde_json::from_value(new.definition).map_err(not_json)?;
            let compiled = typed.compile().map_err(|e| invalid(&e.problems))?;
            (json!(compiled.profile()), compiled.sha256())
        }
        Kind::DocumentsCsv => {
            let typed: DocumentProfile =
                serde_json::from_value(new.definition).map_err(not_json)?;
            let compiled = typed.compile().map_err(|e| invalid(&e.problems))?;
            (json!(compiled.profile()), compiled.sha256())
        }
    };
    let stored =
        match profiles::create(pool, firm, &new.name, new.kind.code(), &canonical, sha256).await {
            Err(DbError::ProfileLimit) => {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "profile_limit",
                    "the firm holds 500 profile names; reuse one with a new version",
                ));
            }
            // The firm does not exist (its foreign key), or a concurrent create took the version.
            Err(DbError::Constraint(name)) if name.contains("firm") => {
                return Err(ApiError::not_found());
            }
            other => other?,
        };
    Ok(json!({
        "name": stored.name,
        "version": stored.version,
        "kind": stored.kind,
        "sha256": hex::encode(stored.sha256),
    }))
}

/// A profile resolved for an upload.
#[derive(Debug, Clone)]
pub enum Resolved {
    /// A statement profile.
    Statement(ProfileId, StatementFormat, Media),
    /// A document profile.
    Documents(ProfileId, Arc<CompiledDocuments>),
}

fn mismatch() -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "profile_kind_mismatch",
        "this profile reads the other kind of file (statements or documents)",
    )
}

/// Resolves `name` (at `version`, or its latest) for `firm`.
pub async fn resolve(
    pool: &sqlx::PgPool,
    firm: FirmId,
    name: &str,
    version: Option<i32>,
) -> Result<Resolved, ApiError> {
    let builtin = |v: i32| version.is_none_or(|asked| asked == v);
    match name {
        PROFILE_R0 if builtin(PROFILE_R0_VERSION) => {
            let compiled = Arc::new(CompiledStatement::r0());
            return Ok(Resolved::Statement(
                ProfileId {
                    name: PROFILE_R0.to_owned(),
                    version: PROFILE_R0_VERSION,
                    sha256: compiled.sha256(),
                },
                StatementFormat::Csv(compiled),
                Media::Csv,
            ));
        }
        PROFILE_OFX if builtin(PROFILE_OFX_VERSION) => {
            return Ok(Resolved::Statement(
                ProfileId {
                    name: PROFILE_OFX.to_owned(),
                    version: PROFILE_OFX_VERSION,
                    sha256: ofx_sha256(),
                },
                StatementFormat::Ofx,
                Media::Ofx,
            ));
        }
        PROFILE_DOCUMENTS if builtin(PROFILE_DOCUMENTS_VERSION) => {
            let compiled = Arc::new(CompiledDocuments::builtin());
            return Ok(Resolved::Documents(
                ProfileId {
                    name: PROFILE_DOCUMENTS.to_owned(),
                    version: PROFILE_DOCUMENTS_VERSION,
                    sha256: compiled.sha256(),
                },
                compiled,
            ));
        }
        _ if is_reserved(name) || !valid_name(name) => return Err(unknown_profile()),
        _ => {}
    }
    let stored = match profiles::get(pool, firm, name, version).await {
        Err(DbError::NotFound) => return Err(unknown_profile()),
        other => other?,
    };
    let id = ProfileId {
        name: stored.name.clone(),
        version: stored.version,
        sha256: stored.sha256,
    };
    let corrupt = || {
        tracing::error!(
            profile_version = stored.version,
            "a stored profile does not compile to its hash"
        );
        ApiError::internal()
    };
    match stored.kind.as_str() {
        "statement_csv" => {
            let typed: StatementProfile =
                serde_json::from_value(stored.definition.clone()).map_err(|_| corrupt())?;
            let compiled = typed.compile().map_err(|_| corrupt())?;
            if compiled.sha256() != stored.sha256 {
                return Err(corrupt());
            }
            Ok(Resolved::Statement(
                id,
                StatementFormat::Csv(Arc::new(compiled)),
                Media::Csv,
            ))
        }
        "documents_csv" => {
            let typed: DocumentProfile =
                serde_json::from_value(stored.definition.clone()).map_err(|_| corrupt())?;
            let compiled = typed.compile().map_err(|_| corrupt())?;
            if compiled.sha256() != stored.sha256 {
                return Err(corrupt());
            }
            Ok(Resolved::Documents(id, Arc::new(compiled)))
        }
        _ => Err(corrupt()),
    }
}

/// Resolves a statement profile, refusing a document one.
pub async fn statement(
    pool: &sqlx::PgPool,
    firm: FirmId,
    name: &str,
    version: Option<i32>,
) -> Result<(ProfileId, StatementFormat, Media), ApiError> {
    match resolve(pool, firm, name, version).await? {
        Resolved::Statement(id, format, media) => Ok((id, format, media)),
        Resolved::Documents(..) => Err(mismatch()),
    }
}

/// Resolves a document profile, refusing a statement one.
pub async fn documents(
    pool: &sqlx::PgPool,
    firm: FirmId,
    name: &str,
    version: Option<i32>,
) -> Result<(ProfileId, Arc<CompiledDocuments>), ApiError> {
    match resolve(pool, firm, name, version).await? {
        Resolved::Documents(id, compiled) => Ok((id, compiled)),
        Resolved::Statement(..) => Err(mismatch()),
    }
}

fn unknown_profile() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "unknown_profile",
        "no such profile (or version) in this firm",
    )
}

/// `GET /v1/firms/{firm}/profiles/{name}`: the profile, built-in or the firm's.
pub async fn describe(
    pool: &sqlx::PgPool,
    firm: FirmId,
    name: &str,
    version: Option<i32>,
) -> Result<Value, ApiError> {
    Ok(match resolve(pool, firm, name, version).await? {
        Resolved::Statement(id, StatementFormat::Csv(c), _) => json!({
            "name": id.name, "version": id.version, "kind": "statement_csv",
            "sha256": hex::encode(id.sha256), "definition": c.profile(),
        }),
        Resolved::Statement(id, StatementFormat::Ofx, _) => json!({
            "name": id.name, "version": id.version, "kind": "statement_ofx",
            "sha256": hex::encode(id.sha256), "definition": Value::Null,
        }),
        Resolved::Documents(id, c) => json!({
            "name": id.name, "version": id.version, "kind": "documents_csv",
            "sha256": hex::encode(id.sha256), "definition": c.profile(),
        }),
    })
}
