#!/usr/bin/env python3
"""Named mutants for the attack suite — scripts/prove-attacks.sh applies one at
a time, runs the attack it disarms, and requires the test to go red.

Each entry maps a test name to (file, old, new) replacements that remove or
weaken exactly the protection the test asserts. `apply` fails loudly when the
needle is gone, so a code change that drifts a protection site can't silently
skip its proof. `list` prints the mutant names (one per line).
"""

import sys
from pathlib import Path

AUTH = "crates/recon-server/src/auth.rs"
UPLOAD = "crates/recon-server/src/upload.rs"
LIB = "crates/recon-server/src/lib.rs"
SERVE = "crates/recon-server/src/serve.rs"
HANDLERS = "crates/recon-server/src/handlers.rs"
DB_REVIEW = "crates/recon-db/src/review.rs"
CSV = "crates/recon-import/src/csv.rs"
PROFILE = "crates/recon-import/src/profile.rs"

FABRICATED_FIRM = """request
            .uri()
            .path()
            .split('/')
            .nth(3)
            .and_then(|s| s.parse::<uuid::Uuid>().ok())
            .map(FirmId::from_uuid)
            .unwrap_or_else(|| FirmId::from_uuid(uuid::Uuid::from_u128(9)))"""

# A session that passes every check, for the firm the path asked for.
SYNTHETIC = f"""Session {{
            firm: {FABRICATED_FIRM},
            user: UserId::from_uuid(uuid::Uuid::from_u128(9)),
            role: Role::Owner,
            csrf: [0; 32],
        }}"""

# The same, as a match-arm block producing a Session (for require_actor's arms).
SYNTHETIC_ARM = f"""{{
            let firm = {FABRICATED_FIRM};
            Session {{
                firm,
                user: UserId::from_uuid(uuid::Uuid::from_u128(9)),
                role: Role::Owner,
                csrf: [0; 32],
            }}
        }}"""

CSRF_LINE = """    if !matches!(request.method(), &Method::GET | &Method::HEAD) && !csrf_ok(&request, &session) {"""
CSRF_OFF = """    if !matches!(request.method(), &Method::GET | &Method::HEAD) && false {"""

MUTANTS = {
    # The session gate lets an anonymous request straight through.
    "nothing_protected_answers_anonymously": (
        AUTH,
        """    let Some(token) = cookie_token(&request) else {
        return session_required().into_response();
    };""",
        f"""    let Some(token) = cookie_token(&request) else {{
        let firm = {FABRICATED_FIRM};
        request.extensions_mut().insert(Session {{
            firm,
            user: UserId::from_uuid(uuid::Uuid::from_u128(9)),
            role: Role::Owner,
            csrf: [0; 32],
        }});
        return next.run(request).await;
    }};""",
        # …and the same hole in require_actor's no-credential arm.
        AUTH,
        """    let Some(token) = cookie_token(&request) else {
        return ApiError::unauthorized().into_response();
    };""",
        f"""    let Some(token) = cookie_token(&request) else {{
        let firm = {FABRICATED_FIRM};
        request.extensions_mut().insert(Auth::Person(Session {{
            firm,
            user: UserId::from_uuid(uuid::Uuid::from_u128(9)),
            role: Role::Owner,
            csrf: [0; 32],
        }}));
        return next.run(request).await;
    }};""",
    ),
    # An unknown cookie is opened as a real session anyway.
    "a_forged_session_cookie_is_refused": (
        AUTH,
        """        Ok(None) => return session_required().into_response(),
        Err(error) => return error.into_response(),
    };
    if !matches!(request.method(), &Method::GET | &Method::HEAD) && !csrf_ok(&request, &session) {
        return csrf_required().into_response();
    }
    request.extensions_mut().insert(session);""",
        f"""        Ok(None) => {SYNTHETIC},
        Err(error) => return error.into_response(),
    }};
    if !matches!(request.method(), &Method::GET | &Method::HEAD) && !csrf_ok(&request, &session) {{
        return csrf_required().into_response();
    }}
    request.extensions_mut().insert(session);""",
        # …and the same hole in require_actor's unknown-token arm.
        AUTH,
        """        Ok(None) => return session_required().into_response(),
        Err(error) => return error.into_response(),
    };
    if !matches!(request.method(), &Method::GET | &Method::HEAD) && !csrf_ok(&request, &session) {
        return csrf_required().into_response();
    }
    request.extensions_mut().insert(Auth::Person(session));""",
        f"""        Ok(None) => {SYNTHETIC_ARM},
        Err(error) => return error.into_response(),
    }};
    if !matches!(request.method(), &Method::GET | &Method::HEAD) && !csrf_ok(&request, &session) {{
        return csrf_required().into_response();
    }}
    request.extensions_mut().insert(Auth::Person(session));""",
    ),
    # The CSRF mirror check is off for both person-carrying middlewares.
    "a_session_without_csrf_cannot_mutate": (AUTH, CSRF_LINE, CSRF_OFF, True),
    # Sign-out forgets nothing server-side.
    "a_signed_out_session_is_dead": (
        AUTH,
        """    if let Some(token) = cookie_token(&request)
        && let Err(error) = store::delete_session(&state.pool, &sha256(&token)).await
    {
        return ApiError::from_db(&error).into_response();
    }""",
        """    let _ = cookie_token(&request); // mutant: the session is kept""",
    ),
    # Tenancy: the firm path check and the client assignment check both wave through.
    "another_firms_routes_answer_404_to_this_session": (
        AUTH,
        """        if self.firm == firm {
            Ok(())
        } else {
            Err(ApiError::not_found())
        }""",
        """        let _ = firm;
        Ok(())""",
        AUTH,
        """        if !store::is_assigned(pool, self.firm, self.user, client).await? {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "forbidden",
                "the client is not assigned to this user",
            ));
        }""",
        """        let _ = client;""",
    ),
    # The operator token arrives as a fabricated person session, and the
    # fabricated person passes the client-assignment check too — the confused
    # deputy end to end (a synthetic user fails is_assigned otherwise).
    "the_operator_token_is_not_a_session": (
        AUTH,
        "        request.extensions_mut().insert(Auth::Operator);",
        f"""        let firm = {FABRICATED_FIRM};
        request.extensions_mut().insert(Auth::Person(Session {{
            firm,
            user: UserId::from_uuid(uuid::Uuid::from_u128(9)),
            role: Role::Owner,
            csrf: [0; 32],
        }}));""",
        AUTH,
        """        if !store::is_assigned(pool, self.firm, self.user, client).await? {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "forbidden",
                "the client is not assigned to this user",
            ));
        }""",
        """        let _ = client;""",
    ),
    # The connection edge trusts x-original-url and rewrites the URI before the
    # router sees it (a rewrite inside Router::layer would arrive after matchit).
    "forwarded_headers_change_nothing": (
        SERVE,
        """                move |mut request: axum::http::Request<hyper::body::Incoming>| {
                    request
                        .extensions_mut()
                        .insert(axum::extract::ConnectInfo(peer));""",
        """                move |mut request: axum::http::Request<hyper::body::Incoming>| {
                    if let Some(uri) = request
                        .headers()
                        .get("x-original-url")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<axum::http::Uri>().ok())
                    {
                        *request.uri_mut() = uri;
                    }
                    request
                        .extensions_mut()
                        .insert(axum::extract::ConnectInfo(peer));""",
    ),
    # healthz starts gating on the Host header — a lie now changes the answer.
    "a_host_header_lie_changes_nothing": (
        HANDLERS,
        """pub async fn healthz() -> Json<Value> {
    Json(json!({ "ok": true }))
}""",
        """pub async fn healthz(headers: HeaderMap) -> Result<Json<Value>, StatusCode> {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !host.starts_with("127.0.0.1") {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(Json(json!({ "ok": true })))
}""",
    ),
    # sign_in's throttle peer is the client-supplied X-Forwarded-For.
    "a_spoofed_client_ip_does_not_beat_the_sign_in_back_off": (
        AUTH,
        """pub async fn sign_in(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    payload: Result<Json<SignInRequest>, JsonRejection>,
) -> Response {
    match sign_in_inner(&state, peer, payload).await {""",
        """pub async fn sign_in(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    payload: Result<Json<SignInRequest>, JsonRejection>,
) -> Response {
    let peer = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .and_then(|v| v.trim().parse::<std::net::IpAddr>().ok())
        .map(|ip| SocketAddr::new(ip, 0))
        .unwrap_or(peer);
    match sign_in_inner(&state, peer, payload).await {""",
    ),
    # The second throttle key is the account again — the address is never locked.
    "the_sign_in_back_off_locks_and_stays_locked": (
        AUTH,
        """        format!(
            "a:{}",
            hex::encode(sha256(peer.ip().to_string().as_bytes()))
        ),""",
        """        format!("a:{}", hex::encode(sha256(email.as_bytes()))),""",
    ),
    # Both size caps are gone — the declared-length precheck AND the streamed
    # decoder bound: a 50 MiB body walks in and imports.
    "a_body_past_the_cap_is_refused": (
        UPLOAD,
        """    if declared.is_some_and(|n| n > MAX_UPLOAD_BYTES) {
        return Err(file_too_large());
    }""",
        "    let _ = declared; // mutant: no size cap",
        CSV,
        "            max_file_bytes: MAX_UPLOAD_BYTES,",
        "            max_file_bytes: u64::MAX,",
    ),
    # The body-idle deadline is out of the wake computation: a stalled body waits
    # for the lease-renewal/overall deadline instead of being refused at 408.
    "a_drip_fed_body_dies_at_the_idle_deadline": (
        UPLOAD,
        "        let wake = deadline.min(idle_until).min(renewed_at + renew_every);",
        "        let wake = deadline.min(renewed_at + renew_every);",
    ),
    # The replay guard: no decided-check, no version check, no guarded update —
    # a second exclude commits and writes a second decision.
    "a_replayed_mutation_cannot_decide_twice": (
        DB_REVIEW,
        """    if !matches!(status.as_str(), "unreviewed" | "exception") {
        return Err(ReviewError::Conflict(
            "the line was already decided",
            winner_of(&mut tx, firm, client, SubjectKind::Line, line.as_uuid()).await?,
        ));
    }
    if version != expected_version {
        return Err(ReviewError::Conflict(
            "the line changed; reload it",
            winner_of(&mut tx, firm, client, SubjectKind::Line, line.as_uuid()).await?,
        ));
    }""",
        """    let _ = version;""",
        DB_REVIEW,
        """        "UPDATE recon.statement_lines SET status = 'excluded', row_version = row_version + 1
          WHERE firm_id = $1 AND id = $2 AND status IN ('unreviewed', 'exception')
            AND row_version = $3",
    )
    .bind(firm.as_uuid())
    .bind(line.as_uuid())
    .bind(expected_version)""",
        """        "UPDATE recon.statement_lines SET status = 'excluded', row_version = row_version + 1
          WHERE firm_id = $1 AND id = $2",
    )
    .bind(firm.as_uuid())
    .bind(line.as_uuid())""",
    ),
    # The pending token is never consumed — a replay mints a second session.
    "a_replayed_sign_in_pending_token_is_dead": (
        AUTH,
        "    store::consume_pending(&state.pool, &pending_hash).await?;\n",
        "",
    ),
    # An outbound-capable route exists — on the outer router, outside the actor
    # gate, so it answers anonymously.
    "no_route_fetches_a_url": (
        LIB,
        '        .route("/healthz", get(handlers::healthz))',
        """        .route("/healthz", get(handlers::healthz))
        .route("/v1/proxy", post(|| async { StatusCode::OK }))""",
    ),
    # The create schemas are open — unknown fields sail through.
    "a_url_in_a_schema_field_is_refused_or_inert": (
        HANDLERS,
        "#[serde(deny_unknown_fields)]\npub struct NewNamed {",
        "pub struct NewNamed {",
        HANDLERS,
        "#[serde(deny_unknown_fields)]\npub struct NewAccount {",
        "pub struct NewAccount {",
    ),
    # Narration content is interpreted (validated as instructions): a URL in it is refused.
    "metadata_addresses_in_content_are_inert_strings": (
        PROFILE,
        """        let narration_norm = normalize(narration_raw);
        if narration_norm.is_empty() {""",
        """        let narration_norm = normalize(narration_raw);
        if narration_norm.contains("://") {
            return Err(refuse(RefusalCode::MissingRequiredField, Field::Narration));
        }
        if narration_norm.is_empty() {""",
    ),
}

ROOT = Path(__file__).resolve().parent.parent


def apply(name: str) -> None:
    entry = MUTANTS[name]
    # Entries are (file, old, new) repeated; a trailing True marks replace-all.
    replace_all = entry[-1] is True
    triples = entry[:-1] if replace_all else entry
    for file, old, new in zip(triples[::3], triples[1::3], triples[2::3]):
        path = ROOT / file
        text = path.read_text()
        count = text.count(old)
        if count == 0 or (count > 1 and not replace_all):
            sys.exit(f"mutant {name}: needle x{count} in {file}")
        path.write_text(text.replace(old, new) if replace_all else text.replace(old, new, 1))


if __name__ == "__main__":
    cmd = sys.argv[1] if len(sys.argv) > 1 else "list"
    if cmd == "list":
        print("\n".join(MUTANTS))
    elif cmd == "apply":
        apply(sys.argv[2])
    else:
        sys.exit(f"usage: {sys.argv[0]} list | apply <mutant>")
