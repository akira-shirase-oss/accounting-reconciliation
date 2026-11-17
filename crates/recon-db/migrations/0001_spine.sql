-- Accounting Reconciliation, migration 1: the spine.
--
-- Run by recon_owner (`recon db migrate`). The server logs in as recon_app, a member of
-- recon_runtime, which receives only the grants at the end of this file: it is not a
-- superuser, owns nothing, has no TRUNCATE, no DDL and no TEMP (D-037).
--
-- Every function references only schema-qualified objects and pins
-- `SET search_path = pg_catalog, pg_temp`, so no session's search_path (and no object
-- planted in another schema) can change what it calls. Built-ins in the SECURITY DEFINER
-- functions are qualified as well.
--
-- What the database itself enforces here:
--   * firm tenancy: every key is (firm_id, id) and every foreign key carries firm_id, so a
--     cross-firm reference cannot exist (D-020);
--   * the decision log is append-only (UPDATE, DELETE, TRUNCATE refused) and hash-chained
--     per client by a trigger the application cannot bypass (D-019, D-042);
--   * statement lines become visible only with their import's flip to `complete`, in the
--     same transaction; a complete, refused or discarded import is final; lines are never
--     deleted or rewritten (invariants 1 and 4).
--
-- Custom SQLSTATEs (class RC):
--   RCA01 decision log is append-only        RCA02 no decision chain for the client
--   RCI01 import is final                    RCI02 import counts do not match its rows
--   RCI03 immutable import column changed    RCL01 lines for an import that is not staging
--   RCL02 lines committed without the flip   RCL03 lines are immutable and never deleted

CREATE SCHEMA recon;
REVOKE ALL ON SCHEMA recon FROM PUBLIC;
REVOKE ALL ON SCHEMA public FROM PUBLIC;
GRANT USAGE ON SCHEMA recon TO recon_runtime;
-- The server reads the applied migration version (its startup check) and nothing else here.
GRANT USAGE ON SCHEMA public TO recon_runtime;
GRANT SELECT ON public._sqlx_migrations TO recon_runtime;
-- Functions are not executable by PUBLIC unless granted (trigger functions never need it).
ALTER DEFAULT PRIVILEGES IN SCHEMA recon REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;

-- ---------------------------------------------------------------------------------------
-- Tenancy: firms, users, clients, accounts.

CREATE TABLE recon.firms (
    id uuid PRIMARY KEY,
    name text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 200),
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE recon.users (
    firm_id uuid NOT NULL REFERENCES recon.firms (id),
    id uuid NOT NULL,
    email text NOT NULL CHECK (
        email = lower(email) AND char_length(email) BETWEEN 3 AND 254 AND position('@' IN email) > 1),
    display_name text NOT NULL CHECK (char_length(display_name) BETWEEN 1 AND 200),
    role text NOT NULL CHECK (role IN ('owner', 'reviewer', 'preparer', 'viewer')),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (firm_id, id),
    UNIQUE (firm_id, email)
);

CREATE TABLE recon.clients (
    firm_id uuid NOT NULL REFERENCES recon.firms (id),
    id uuid NOT NULL,
    name text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 200),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (firm_id, id)
);

CREATE TABLE recon.accounts (
    firm_id uuid NOT NULL,
    client_id uuid NOT NULL,
    id uuid NOT NULL,
    name text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 200),
    currency text NOT NULL CHECK (
        currency IN ('INR', 'USD', 'EUR', 'GBP', 'AED', 'SGD', 'AUD', 'CAD', 'JPY', 'KWD')),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (firm_id, id),
    UNIQUE (firm_id, client_id, id, currency),
    FOREIGN KEY (firm_id, client_id) REFERENCES recon.clients (firm_id, id)
);

-- ---------------------------------------------------------------------------------------
-- Imports, staging, lines.

CREATE TABLE recon.imports (
    firm_id uuid NOT NULL,
    id uuid NOT NULL,
    client_id uuid NOT NULL,
    account_id uuid NOT NULL,
    currency text NOT NULL,
    profile text NOT NULL CHECK (profile = 'r0-generic-csv'),
    profile_version integer NOT NULL CHECK (profile_version >= 1),
    status text NOT NULL DEFAULT 'staging'
        CHECK (status IN ('staging', 'complete', 'refused', 'discarded')),
    file_hash bytea CHECK (octet_length(file_hash) = 32),
    file_bytes bigint CHECK (file_bytes BETWEEN 0 AND 52428800),
    rows_read integer CHECK (rows_read BETWEEN 0 AND 2000000),
    lines_created integer CHECK (lines_created >= 0),
    duplicates integer CHECK (duplicates >= 0),
    rows_refused integer CHECK (rows_refused >= 0),
    -- Codes and row numbers only, never content (D-025).
    refusal_code text CHECK (refusal_code ~ '^[a-z][a-z0-9_]{0,63}$'),
    refusals jsonb CHECK (refusals IS NULL OR jsonb_typeof(refusals) = 'array'),
    staged_rows integer NOT NULL DEFAULT 0 CHECK (staged_rows >= 0),
    duplicate_of uuid,
    discard_reason text CHECK (discard_reason IN ('duplicate_file', 'lease_expired', 'upload_failed')),
    lease_token uuid NOT NULL,
    lease_until timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz,
    PRIMARY KEY (firm_id, id),
    UNIQUE (firm_id, id, client_id, account_id, currency),
    FOREIGN KEY (firm_id, client_id, account_id, currency)
        REFERENCES recon.accounts (firm_id, client_id, id, currency),
    FOREIGN KEY (firm_id, duplicate_of) REFERENCES recon.imports (firm_id, id),
    CHECK (status <> 'complete' OR (
        file_hash IS NOT NULL AND rows_read IS NOT NULL AND lines_created IS NOT NULL
        AND duplicates IS NOT NULL AND rows_refused = 0 AND rows_read >= 1
        AND rows_read = lines_created + duplicates)),
    CHECK ((status = 'refused') = (refusal_code IS NOT NULL)),
    CHECK ((status = 'discarded') = (discard_reason IS NOT NULL)),
    CHECK ((discard_reason IS NOT DISTINCT FROM 'duplicate_file') = (duplicate_of IS NOT NULL)),
    CHECK ((status = 'staging') = (finished_at IS NULL))
);

-- The same file is complete at most once per account (D-008).
CREATE UNIQUE INDEX imports_one_complete_per_file
    ON recon.imports (firm_id, account_id, file_hash) WHERE status = 'complete';
CREATE INDEX imports_staging_by_lease ON recon.imports (lease_until) WHERE status = 'staging';

-- Rows waiting for their import's flip. UNLOGGED (D-039): a crash may empty it, and the
-- flip refuses unless every staged row is present, so a lost row can never become a
-- partial import; the import is then discarded and the file re-uploaded.
CREATE UNLOGGED TABLE recon.import_staging (
    firm_id uuid NOT NULL,
    import_id uuid NOT NULL,
    row_no integer NOT NULL,
    booking_date date NOT NULL,
    value_date date,
    amount_minor bigint NOT NULL,
    narration_raw text NOT NULL,
    narration_norm text NOT NULL,
    reference text,
    counterparty_raw text,
    counterparty_norm text,
    balance_after_minor bigint,
    tuple_hash bytea NOT NULL,
    PRIMARY KEY (firm_id, import_id, row_no)
);

CREATE TABLE recon.statement_lines (
    firm_id uuid NOT NULL,
    id uuid NOT NULL DEFAULT uuidv7(),
    client_id uuid NOT NULL,
    account_id uuid NOT NULL,
    import_id uuid NOT NULL,
    currency text NOT NULL,
    row_no integer NOT NULL CHECK (row_no BETWEEN 2 AND 2000001),
    booking_date date NOT NULL,
    value_date date,
    amount_minor bigint NOT NULL
        CHECK (amount_minor <> 0 AND amount_minor BETWEEN -1000000000000 AND 1000000000000),
    narration_raw text NOT NULL CHECK (octet_length(narration_raw) BETWEEN 1 AND 8192),
    narration_norm text NOT NULL CHECK (octet_length(narration_norm) BETWEEN 1 AND 8192),
    reference text CHECK (octet_length(reference) BETWEEN 1 AND 8192),
    counterparty_raw text CHECK (octet_length(counterparty_raw) BETWEEN 1 AND 8192),
    counterparty_norm text CHECK (octet_length(counterparty_norm) BETWEEN 1 AND 8192),
    balance_after_minor bigint CHECK (balance_after_minor BETWEEN -1000000000000 AND 1000000000000),
    tuple_hash bytea NOT NULL CHECK (octet_length(tuple_hash) = 32),
    occurrence integer NOT NULL CHECK (occurrence >= 0),
    fingerprint bytea NOT NULL CHECK (octet_length(fingerprint) = 32),
    status text NOT NULL DEFAULT 'unreviewed' CHECK (
        status IN ('unreviewed', 'proposed', 'matched', 'excluded', 'exception', 'superseded')),
    row_version bigint NOT NULL DEFAULT 1 CHECK (row_version >= 1),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (firm_id, id),
    -- One line per identity per account (D-008); one line per source row.
    UNIQUE (firm_id, account_id, fingerprint),
    UNIQUE (firm_id, import_id, row_no),
    -- The line's client, account and currency are its import's, which are the account's.
    FOREIGN KEY (firm_id, import_id, client_id, account_id, currency)
        REFERENCES recon.imports (firm_id, id, client_id, account_id, currency)
);

-- Rows of an import that an existing line of the account already has: reported, never
-- inserted twice (invariant 1).
CREATE TABLE recon.import_duplicates (
    firm_id uuid NOT NULL,
    import_id uuid NOT NULL,
    row_no integer NOT NULL,
    existing_line_id uuid NOT NULL,
    PRIMARY KEY (firm_id, import_id, row_no),
    FOREIGN KEY (firm_id, import_id) REFERENCES recon.imports (firm_id, id),
    FOREIGN KEY (firm_id, existing_line_id) REFERENCES recon.statement_lines (firm_id, id)
);

-- One row per import that received lines in a transaction; its deferred check refuses the
-- commit unless the import is complete by then. Written only by the trigger below.
CREATE TABLE recon.import_flips (
    firm_id uuid NOT NULL,
    import_id uuid NOT NULL,
    PRIMARY KEY (firm_id, import_id),
    FOREIGN KEY (firm_id, import_id) REFERENCES recon.imports (firm_id, id)
);

-- ---------------------------------------------------------------------------------------
-- The decision log and its per-client chain heads.

CREATE TABLE recon.chain_heads (
    firm_id uuid NOT NULL,
    client_id uuid NOT NULL,
    seq bigint NOT NULL CHECK (seq >= 0),
    head_hash bytea NOT NULL CHECK (octet_length(head_hash) = 32),
    PRIMARY KEY (firm_id, client_id),
    FOREIGN KEY (firm_id, client_id) REFERENCES recon.clients (firm_id, id)
);

CREATE TABLE recon.decision_log (
    firm_id uuid NOT NULL,
    client_id uuid NOT NULL,
    seq bigint NOT NULL CHECK (seq >= 1),
    id uuid NOT NULL,
    prev_hash bytea NOT NULL CHECK (octet_length(prev_hash) = 32),
    record_hash bytea NOT NULL CHECK (octet_length(record_hash) = 32),
    subject_kind text NOT NULL CHECK (subject_kind IN ('import', 'line', 'match', 'rule', 'period')),
    subject_id uuid NOT NULL,
    from_state text CHECK (char_length(from_state) BETWEEN 1 AND 64),
    to_state text NOT NULL CHECK (char_length(to_state) BETWEEN 1 AND 64),
    actor_kind text NOT NULL CHECK (
        actor_kind IN ('operator', 'person', 'rule', 'approval', 'engine', 'model', 'system')),
    actor_ref text NOT NULL CHECK (char_length(actor_ref) BETWEEN 1 AND 200),
    reason_code text NOT NULL CHECK (reason_code ~ '^[a-z][a-z0-9_]{0,63}$'),
    evidence bytea NOT NULL CHECK (octet_length(evidence) <= 65536),
    engine_version text NOT NULL CHECK (char_length(engine_version) BETWEEN 1 AND 200),
    decided_at timestamptz NOT NULL,
    PRIMARY KEY (firm_id, client_id, seq),
    UNIQUE (firm_id, id),
    FOREIGN KEY (firm_id, client_id) REFERENCES recon.clients (firm_id, id)
);

CREATE INDEX decision_log_by_subject ON recon.decision_log (firm_id, subject_kind, subject_id, seq);

-- lp(x): the 4-byte big-endian length, then the bytes (the canonical encoding, D-042).
CREATE FUNCTION recon.lp(t text) RETURNS bytea
LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
SET search_path = pg_catalog, pg_temp
RETURN pg_catalog.int4send(pg_catalog.octet_length(pg_catalog.convert_to(t, 'UTF8')))
    || pg_catalog.convert_to(t, 'UTF8');

CREATE FUNCTION recon.lp_bytes(b bytea) RETURNS bytea
LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
SET search_path = pg_catalog, pg_temp
RETURN pg_catalog.int4send(pg_catalog.octet_length(b)) || b;

-- The canonical bytes of a decision, v1: exactly recon_domain::decision's definition.
CREATE FUNCTION recon.decision_canonical(
    p_firm uuid, p_client uuid, p_seq bigint, p_id uuid, p_prev bytea,
    p_subject_kind text, p_subject_id uuid, p_from text, p_to text,
    p_actor_kind text, p_actor_ref text, p_reason text, p_evidence bytea,
    p_engine text, p_at timestamptz) RETURNS bytea
LANGUAGE sql STABLE PARALLEL SAFE
SET search_path = pg_catalog, pg_temp
RETURN pg_catalog.convert_to('recon-decision-v1', 'UTF8')
    || pg_catalog.uuid_send(p_firm) || pg_catalog.uuid_send(p_client)
    || pg_catalog.int8send(p_seq) || pg_catalog.uuid_send(p_id)
    || p_prev
    || recon.lp(p_subject_kind) || pg_catalog.uuid_send(p_subject_id)
    || CASE WHEN p_from IS NULL THEN '\x00'::bytea ELSE '\x01'::bytea || recon.lp(p_from) END
    || recon.lp(p_to) || recon.lp(p_actor_kind) || recon.lp(p_actor_ref) || recon.lp(p_reason)
    || recon.lp_bytes(p_evidence) || recon.lp(p_engine)
    || pg_catalog.int8send((EXTRACT(EPOCH FROM p_at) * 1000000)::bigint);

-- A new client's chain starts at its genesis hash: sha256("recon-chain-v1" firm client).
CREATE FUNCTION recon.clients_open_chain() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    INSERT INTO recon.chain_heads (firm_id, client_id, seq, head_hash)
    VALUES (NEW.firm_id, NEW.id, 0,
            pg_catalog.sha256(pg_catalog.convert_to('recon-chain-v1', 'UTF8')
                || pg_catalog.uuid_send(NEW.firm_id) || pg_catalog.uuid_send(NEW.id)));
    RETURN NULL;
END
$$;

CREATE TRIGGER clients_open_chain
    AFTER INSERT ON recon.clients
    FOR EACH ROW EXECUTE FUNCTION recon.clients_open_chain();

-- Every decision takes the client's chain lock, gets the next seq, links to the head and
-- is hashed with the database clock. The caller's values for these columns are ignored.
CREATE FUNCTION recon.decision_log_chain() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    head_seq bigint;
    head bytea;
BEGIN
    SELECT h.seq, h.head_hash INTO head_seq, head
      FROM recon.chain_heads h
     WHERE h.firm_id = NEW.firm_id AND h.client_id = NEW.client_id
       FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'no decision chain for this client' USING ERRCODE = 'RCA02';
    END IF;
    NEW.seq := head_seq + 1;
    NEW.prev_hash := head;
    NEW.decided_at := pg_catalog.now();
    NEW.record_hash := pg_catalog.sha256(recon.decision_canonical(
        NEW.firm_id, NEW.client_id, NEW.seq, NEW.id, NEW.prev_hash,
        NEW.subject_kind, NEW.subject_id, NEW.from_state, NEW.to_state,
        NEW.actor_kind, NEW.actor_ref, NEW.reason_code, NEW.evidence,
        NEW.engine_version, NEW.decided_at));
    UPDATE recon.chain_heads
       SET seq = NEW.seq, head_hash = NEW.record_hash
     WHERE firm_id = NEW.firm_id AND client_id = NEW.client_id;
    RETURN NEW;
END
$$;

CREATE TRIGGER decision_log_chain
    BEFORE INSERT ON recon.decision_log
    FOR EACH ROW EXECUTE FUNCTION recon.decision_log_chain();

CREATE FUNCTION recon.refuse_change() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    RAISE EXCEPTION 'the decision log is append-only: % refused', TG_OP
        USING ERRCODE = 'RCA01', HINT = 'correct a decision with a new decision that reverses it';
END
$$;

CREATE TRIGGER decision_log_append_only
    BEFORE UPDATE OR DELETE ON recon.decision_log
    FOR EACH ROW EXECUTE FUNCTION recon.refuse_change();
CREATE TRIGGER decision_log_no_truncate
    BEFORE TRUNCATE ON recon.decision_log
    FOR EACH STATEMENT EXECUTE FUNCTION recon.refuse_change();

-- ---------------------------------------------------------------------------------------
-- Import and line guards.

-- The database stamps an import's creation; the caller cannot backdate it.
CREATE FUNCTION recon.imports_stamp() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    NEW.created_at := pg_catalog.now();
    NEW.status := 'staging';
    NEW.finished_at := NULL;
    RETURN NEW;
END
$$;

CREATE TRIGGER imports_stamp
    BEFORE INSERT ON recon.imports
    FOR EACH ROW EXECUTE FUNCTION recon.imports_stamp();

-- Only a staging import changes; identity columns never do; `complete` requires the
-- import's lines and duplicates to be exactly the counts it claims.
CREATE FUNCTION recon.imports_guard() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    n_lines bigint;
    n_dups bigint;
BEGIN
    IF OLD.status <> 'staging' THEN
        RAISE EXCEPTION 'import is final (%)', OLD.status USING ERRCODE = 'RCI01';
    END IF;
    IF (NEW.firm_id, NEW.id, NEW.client_id, NEW.account_id, NEW.currency, NEW.profile,
        NEW.profile_version, NEW.lease_token, NEW.created_at)
       IS DISTINCT FROM
       (OLD.firm_id, OLD.id, OLD.client_id, OLD.account_id, OLD.currency, OLD.profile,
        OLD.profile_version, OLD.lease_token, OLD.created_at) THEN
        RAISE EXCEPTION 'an import''s identity never changes' USING ERRCODE = 'RCI03';
    END IF;
    IF NEW.status = 'staging' THEN
        -- A lease renewal or a staged batch.
        RETURN NEW;
    END IF;
    SELECT count(*) INTO n_lines FROM recon.statement_lines
     WHERE firm_id = NEW.firm_id AND import_id = NEW.id;
    SELECT count(*) INTO n_dups FROM recon.import_duplicates
     WHERE firm_id = NEW.firm_id AND import_id = NEW.id;
    IF NEW.status = 'complete' AND (n_lines <> NEW.lines_created OR n_dups <> NEW.duplicates) THEN
        RAISE EXCEPTION 'import counts do not match its rows' USING ERRCODE = 'RCI02';
    END IF;
    IF NEW.status IN ('refused', 'discarded') AND (n_lines <> 0 OR n_dups <> 0) THEN
        RAISE EXCEPTION 'a refused or discarded import has no lines' USING ERRCODE = 'RCI02';
    END IF;
    NEW.finished_at := pg_catalog.now();
    RETURN NEW;
END
$$;

CREATE TRIGGER imports_guard
    BEFORE UPDATE ON recon.imports
    FOR EACH ROW EXECUTE FUNCTION recon.imports_guard();

-- After each INSERT statement into statement_lines: every line's import is staging (so it
-- is flipping in this transaction), and each import is registered for the commit check.
CREATE FUNCTION recon.statement_lines_inserted() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM new_lines n
          LEFT JOIN recon.imports i ON i.firm_id = n.firm_id AND i.id = n.import_id
         WHERE i.status IS DISTINCT FROM 'staging') THEN
        RAISE EXCEPTION 'lines may only be inserted while their import is staging'
            USING ERRCODE = 'RCL01';
    END IF;
    INSERT INTO recon.import_flips (firm_id, import_id)
    SELECT DISTINCT n.firm_id, n.import_id FROM new_lines n
    ON CONFLICT DO NOTHING;
    RETURN NULL;
END
$$;

CREATE TRIGGER statement_lines_inserted
    AFTER INSERT ON recon.statement_lines
    REFERENCING NEW TABLE AS new_lines
    FOR EACH STATEMENT EXECUTE FUNCTION recon.statement_lines_inserted();

-- At commit: an import that received lines is complete, or the commit fails.
CREATE FUNCTION recon.import_flips_check() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM recon.imports
         WHERE firm_id = NEW.firm_id AND id = NEW.import_id AND status = 'complete') THEN
        RAISE EXCEPTION 'statement lines committed without their import becoming complete'
            USING ERRCODE = 'RCL02';
    END IF;
    RETURN NULL;
END
$$;

CREATE CONSTRAINT TRIGGER import_flips_complete
    AFTER INSERT ON recon.import_flips
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION recon.import_flips_check();

-- A line's source fields never change; only its status and row version do (from R2).
CREATE FUNCTION recon.statement_lines_guard() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF TG_OP <> 'UPDATE' THEN
        RAISE EXCEPTION 'statement lines are never deleted (% refused)', TG_OP USING ERRCODE = 'RCL03';
    END IF;
    IF (NEW.firm_id, NEW.id, NEW.client_id, NEW.account_id, NEW.import_id, NEW.currency,
        NEW.row_no, NEW.booking_date, NEW.value_date, NEW.amount_minor, NEW.narration_raw,
        NEW.narration_norm, NEW.reference, NEW.counterparty_raw, NEW.counterparty_norm,
        NEW.balance_after_minor, NEW.tuple_hash, NEW.occurrence, NEW.fingerprint, NEW.created_at)
       IS DISTINCT FROM
       (OLD.firm_id, OLD.id, OLD.client_id, OLD.account_id, OLD.import_id, OLD.currency,
        OLD.row_no, OLD.booking_date, OLD.value_date, OLD.amount_minor, OLD.narration_raw,
        OLD.narration_norm, OLD.reference, OLD.counterparty_raw, OLD.counterparty_norm,
        OLD.balance_after_minor, OLD.tuple_hash, OLD.occurrence, OLD.fingerprint, OLD.created_at) THEN
        RAISE EXCEPTION 'a statement line''s source fields never change' USING ERRCODE = 'RCL03';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER statement_lines_guard
    BEFORE UPDATE OR DELETE ON recon.statement_lines
    FOR EACH ROW EXECUTE FUNCTION recon.statement_lines_guard();
CREATE TRIGGER statement_lines_no_truncate
    BEFORE TRUNCATE ON recon.statement_lines
    FOR EACH STATEMENT EXECUTE FUNCTION recon.statement_lines_guard();

-- Imports and their duplicate reports are history too: never deleted.
CREATE FUNCTION recon.refuse_delete() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    RAISE EXCEPTION '% on % refused: import history is kept', TG_OP, TG_TABLE_NAME
        USING ERRCODE = 'RCI01';
END
$$;

CREATE TRIGGER imports_no_delete
    BEFORE DELETE ON recon.imports
    FOR EACH ROW EXECUTE FUNCTION recon.refuse_delete();
CREATE TRIGGER imports_no_truncate
    BEFORE TRUNCATE ON recon.imports
    FOR EACH STATEMENT EXECUTE FUNCTION recon.refuse_delete();
CREATE TRIGGER import_duplicates_no_delete
    BEFORE UPDATE OR DELETE ON recon.import_duplicates
    FOR EACH ROW EXECUTE FUNCTION recon.refuse_delete();
CREATE TRIGGER import_duplicates_no_truncate
    BEFORE TRUNCATE ON recon.import_duplicates
    FOR EACH STATEMENT EXECUTE FUNCTION recon.refuse_delete();

-- ---------------------------------------------------------------------------------------
-- The runtime role's grants: the only privileges the server has (D-037).

GRANT SELECT, INSERT ON recon.firms, recon.users, recon.clients, recon.accounts TO recon_runtime;
GRANT SELECT, INSERT ON recon.statement_lines, recon.import_duplicates, recon.decision_log
    TO recon_runtime;
GRANT SELECT ON recon.chain_heads TO recon_runtime;
GRANT SELECT, INSERT, DELETE ON recon.import_staging TO recon_runtime;
GRANT SELECT, INSERT ON recon.imports TO recon_runtime;
GRANT UPDATE (status, file_hash, file_bytes, rows_read, lines_created, duplicates, rows_refused,
              refusal_code, refusals, staged_rows, duplicate_of, discard_reason, lease_until)
    ON recon.imports TO recon_runtime;
-- recon.import_flips: nothing; only the SECURITY DEFINER trigger writes it.
