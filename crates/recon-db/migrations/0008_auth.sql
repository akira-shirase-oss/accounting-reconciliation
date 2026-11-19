-- 0008: sign-in (D-021) — users gain credentials, sessions, TOTP enrolment, recovery
-- codes, client assignment, the sign-in back-off and the read access log.
--
-- Passwords are argon2id PHC strings (verified, never stored reversibly); a user without
-- one cannot sign in. TOTP secrets are 160-bit keys (RFC 6238, enrolment mandatory at
-- first sign-in). The cookie carries a random session token — only its SHA-256 is stored,
-- so a database read never yields a usable credential; the same for pending sign-ins and
-- recovery codes. A session's csrf secret is stored raw: it is a request-correlation
-- secret, not a credential, and must be compared against what the client sends.

ALTER TABLE recon.users
    ADD COLUMN password_hash text
        CHECK (password_hash IS NULL OR char_length(password_hash) <= 200),
    ADD COLUMN totp_secret bytea
        CHECK (totp_secret IS NULL OR octet_length(totp_secret) = 20),
    ADD COLUMN totp_enrolled_at timestamptz,
    ADD COLUMN disabled_at timestamptz,
    -- Enrolment has a real middle state: the secret exists from the first sign-in's
    -- qr/secret answer until the first TOTP confirms it. Enrolled therefore implies a
    -- secret, but a secret alone is not yet enrolled.
    ADD CONSTRAINT users_totp_enrolment_coherent
        CHECK (totp_enrolled_at IS NULL OR totp_secret IS NOT NULL);

-- Ten hashed recovery codes per user, each single-use (D-021).
CREATE TABLE recon.user_recovery_codes (
    firm_id uuid NOT NULL,
    user_id uuid NOT NULL,
    code_hash bytea NOT NULL CHECK (octet_length(code_hash) = 32),
    used_at timestamptz,
    PRIMARY KEY (firm_id, user_id, code_hash),
    FOREIGN KEY (firm_id, user_id) REFERENCES recon.users (firm_id, id)
);

-- A user sees only the clients assigned to them (D-021); empty means none.
CREATE TABLE recon.user_clients (
    firm_id uuid NOT NULL,
    user_id uuid NOT NULL,
    client_id uuid NOT NULL,
    PRIMARY KEY (firm_id, user_id, client_id),
    FOREIGN KEY (firm_id, user_id) REFERENCES recon.users (firm_id, id),
    FOREIGN KEY (firm_id, client_id) REFERENCES recon.clients (firm_id, id)
);

-- Sessions (D-021): 30 min idle inside a 12 h absolute; rotated at sign-in (a fresh row
-- and token every time); sign-out deletes the row.
CREATE TABLE recon.sessions (
    firm_id uuid NOT NULL,
    id uuid NOT NULL DEFAULT uuidv7(),
    user_id uuid NOT NULL,
    token_hash bytea NOT NULL CHECK (octet_length(token_hash) = 32),
    csrf bytea NOT NULL CHECK (octet_length(csrf) = 32),
    created_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    idle_expires_at timestamptz NOT NULL,
    absolute_expires_at timestamptz NOT NULL,
    PRIMARY KEY (firm_id, id),
    UNIQUE (token_hash),
    FOREIGN KEY (firm_id, user_id) REFERENCES recon.users (firm_id, id),
    CHECK (idle_expires_at <= absolute_expires_at)
);
CREATE INDEX sessions_by_user ON recon.sessions (firm_id, user_id);

-- Password verified and TOTP pending: a five-minute, at-most-three-tries bridge (D-021).
CREATE TABLE recon.sign_in_pending (
    firm_id uuid NOT NULL,
    id uuid NOT NULL DEFAULT uuidv7(),
    user_id uuid NOT NULL,
    token_hash bytea NOT NULL CHECK (octet_length(token_hash) = 32),
    attempts integer NOT NULL DEFAULT 0 CHECK (attempts BETWEEN 0 AND 3),
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    PRIMARY KEY (firm_id, id),
    UNIQUE (token_hash),
    FOREIGN KEY (firm_id, user_id) REFERENCES recon.users (firm_id, id)
);

-- The sign-in back-off, per account ('u:…') and per address ('a:…') (D-021). Keys are
-- labelled digests: the application hashes the email or address into them, so the table
-- holds no raw identifiers.
CREATE TABLE recon.sign_in_throttle (
    key text NOT NULL PRIMARY KEY CHECK (key LIKE 'u:%' OR key LIKE 'a:%'),
    failures integer NOT NULL DEFAULT 0 CHECK (failures >= 0),
    locked_until timestamptz,
    last_attempt timestamptz NOT NULL DEFAULT now()
);

-- Reads recorded per user, client and day (D-021): one row per combination, upserted.
CREATE TABLE recon.access_log (
    firm_id uuid NOT NULL,
    user_id uuid NOT NULL,
    client_id uuid NOT NULL,
    day date NOT NULL,
    requests bigint NOT NULL DEFAULT 1 CHECK (requests > 0),
    first_at timestamptz NOT NULL DEFAULT now(),
    last_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (firm_id, user_id, client_id, day),
    FOREIGN KEY (firm_id, user_id) REFERENCES recon.users (firm_id, id),
    FOREIGN KEY (firm_id, client_id) REFERENCES recon.clients (firm_id, id)
);

-- A person's manual proposal needs its actor (slice 2): the engine's and the model's
-- join 'person'. CHECKs cannot be altered — dropped and recreated.
ALTER TABLE recon.proposals DROP CONSTRAINT proposals_actor_check;
ALTER TABLE recon.proposals
    ADD CONSTRAINT proposals_actor_check CHECK (actor IN ('engine', 'model', 'person'));

-- Four-eyes bookkeeping (D-018): a four-eyes match's first approver is recorded on the
-- match; the second, different approver commits it. Not part of the match's immutable
-- identity (matches_guard does not list it), and never set on an ordinary match.
ALTER TABLE recon.matches
    ADD COLUMN first_approver uuid,
    ADD CONSTRAINT matches_first_approver_needs_four_eyes
        CHECK (first_approver IS NULL OR four_eyes),
    ADD CONSTRAINT matches_first_approver_fkey
        FOREIGN KEY (firm_id, first_approver) REFERENCES recon.users (firm_id, id);

-- The per-currency threshold over which a match needs four eyes and a preparer may not
-- approve alone (D-018), shaped like residual_tolerances: {"USD": 1000000} minor units.
ALTER TABLE recon.firms
    ADD COLUMN four_eyes_thresholds jsonb NOT NULL DEFAULT '{}'::jsonb
        CHECK (jsonb_typeof(four_eyes_thresholds) = 'object');

-- The runtime role's new grants (D-037): credential columns updated; sessions, pending
-- rows and the throttle live and die; the access log is upserted.
GRANT UPDATE (password_hash, totp_secret, totp_enrolled_at, disabled_at)
    ON recon.users TO recon_runtime;
GRANT SELECT, INSERT, UPDATE (used_at) ON recon.user_recovery_codes TO recon_runtime;
GRANT SELECT, INSERT, DELETE ON recon.user_clients TO recon_runtime;
GRANT SELECT, INSERT, DELETE ON recon.sessions TO recon_runtime;
GRANT UPDATE (last_seen_at, idle_expires_at) ON recon.sessions TO recon_runtime;
GRANT SELECT, INSERT, DELETE ON recon.sign_in_pending TO recon_runtime;
GRANT UPDATE (attempts) ON recon.sign_in_pending TO recon_runtime;
GRANT SELECT, INSERT, UPDATE (failures, locked_until, last_attempt), DELETE
    ON recon.sign_in_throttle TO recon_runtime;
GRANT SELECT, INSERT, UPDATE (requests, last_at) ON recon.access_log TO recon_runtime;
GRANT UPDATE (first_approver) ON recon.matches TO recon_runtime;
GRANT UPDATE (four_eyes_thresholds) ON recon.firms TO recon_runtime;
