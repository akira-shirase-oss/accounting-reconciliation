-- Accounting Reconciliation, migration 7: R2's matching storage (D-011 to D-014, D-032,
-- D-033, D-070, D-071).
--
--   * match_jobs: a client's matching run as a durable row — leased, fenced by its
--     `lease_token` like an import, its snapshot of open lines and open document amounts
--     pinned at start, its cursor advancing in the same transaction as each chunk of
--     500 decided lines (D-033). At most one `running` job per client, enforced by the
--     partial unique index so raw SQL cannot start a second.
--   * matches, match_lines, match_documents, match_residuals: a match balances exactly
--     (the engine only produces balanced ones; the schema adds the tenancy and liveness
--     guards). A match's status changes only with its decision in the same transaction
--     (RCM02, the RCL04 pattern); only a T1 match may be inserted `committed` (RCM01).
--   * documents gain `allocated_minor`: maintained by trigger from allocation rows, the
--     only mutable column, bounded by CHECK (|allocated| <= |amount|, same sign) so even
--     raw SQL cannot over-allocate (D-032).
--   * proposals (engine now, model in R4) and line_exceptions: the queryable current
--     state behind the decisions.
--   * firms carry the matching settings a job pins: `auto_commit_t1` (D-011) and the
--     per-currency residual tolerances of D-014 (all zero until the firm sets them).
--   * decision_log gains `job_id`: the job whose run caused the decision, provenance
--     outside the hash, unique per (job, subject) so a resumed job cannot write a
--     second decision for one line (D-033).
--
-- New SQLSTATEs: RCJ01 match job refused (identity, finished or stale), RCM01 match
-- refused (identity, transition or tier), RCM02 match status changed without its
-- decision, RCM03 allocation refused, RCD03 document bookkeeping refused, RCL05 a line
-- exception row that does not match its line's state.

-- ---------------------------------------------------------------------------------------
-- Firm settings and the job's provenance on decisions.

ALTER TABLE recon.firms
    ADD COLUMN auto_commit_t1 boolean NOT NULL DEFAULT true,
    ADD COLUMN residual_tolerances jsonb NOT NULL DEFAULT '{}'::jsonb
        CHECK (jsonb_typeof(residual_tolerances) = 'object');

ALTER TABLE recon.decision_log ADD COLUMN job_id uuid;
-- A resumed job may not write a second decision about one subject; the engine version is
-- the job's own pinned `engine_version`, so (job, subject) carries (job, line, engine
-- version) of D-033.
CREATE UNIQUE INDEX decision_log_one_per_job_subject
    ON recon.decision_log (firm_id, job_id, subject_id) WHERE job_id IS NOT NULL;

-- ---------------------------------------------------------------------------------------
-- Matching jobs.

CREATE TABLE recon.match_jobs (
    firm_id uuid NOT NULL,
    id uuid NOT NULL DEFAULT uuidv7(),
    client_id uuid NOT NULL,
    status text NOT NULL DEFAULT 'running'
        CHECK (status IN ('running', 'complete', 'failed')),
    lease_token uuid NOT NULL,
    lease_until timestamptz NOT NULL,
    -- The position in match_job_lines.seq the next chunk starts at (lines decided).
    cursor bigint NOT NULL DEFAULT 0 CHECK (cursor >= 0),
    -- The snapshot's size; NULL until the job pins its lines.
    line_count bigint CHECK (line_count >= 0),
    matched bigint NOT NULL DEFAULT 0 CHECK (matched >= 0),
    proposed bigint NOT NULL DEFAULT 0 CHECK (proposed >= 0),
    exceptions bigint NOT NULL DEFAULT 0 CHECK (exceptions >= 0),
    -- Decided outcomes the commit skipped because the line left `unreviewed` meanwhile.
    skipped bigint NOT NULL DEFAULT 0 CHECK (skipped >= 0),
    run_digest bytea CHECK (octet_length(run_digest) = 32),
    auto_commit_t1 boolean NOT NULL,
    residual_tolerances jsonb NOT NULL CHECK (jsonb_typeof(residual_tolerances) = 'object'),
    engine_version text NOT NULL CHECK (char_length(engine_version) BETWEEN 1 AND 200),
    error_code text CHECK (error_code ~ '^[a-z][a-z0-9_]{0,63}$'),
    created_at timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz,
    PRIMARY KEY (firm_id, id),
    UNIQUE (firm_id, client_id, id),
    FOREIGN KEY (firm_id, client_id) REFERENCES recon.clients (firm_id, id),
    CHECK (cursor <= coalesce(line_count, cursor)),
    CHECK ((status = 'running') = (finished_at IS NULL)),
    CHECK ((status = 'failed') = (error_code IS NOT NULL))
);

-- At most one running job per client — the database-level mutual exclusion (D-033).
CREATE UNIQUE INDEX match_jobs_one_running_per_client
    ON recon.match_jobs (firm_id, client_id) WHERE status = 'running';
-- What the sweeper scans for expired leases.
CREATE INDEX match_jobs_expired_lease
    ON recon.match_jobs (lease_until) WHERE status = 'running';

-- The lines a job decides, pinned in order at its start: a resumed run decides exactly
-- the same set (failure case 4's digest equality).
CREATE TABLE recon.match_job_lines (
    firm_id uuid NOT NULL,
    job_id uuid NOT NULL,
    seq bigint NOT NULL CHECK (seq >= 1),
    line_id uuid NOT NULL,
    PRIMARY KEY (firm_id, job_id, seq),
    UNIQUE (firm_id, job_id, line_id),
    FOREIGN KEY (firm_id, job_id) REFERENCES recon.match_jobs (firm_id, id),
    FOREIGN KEY (firm_id, line_id) REFERENCES recon.statement_lines (firm_id, id)
);

-- The documents' open amounts as the job saw them: the engine's inputs are pinned so a
-- resumed run computes on the same amounts its earlier chunks did.
CREATE TABLE recon.match_job_documents (
    firm_id uuid NOT NULL,
    job_id uuid NOT NULL,
    document_id uuid NOT NULL,
    open_amount_minor bigint NOT NULL CHECK (open_amount_minor <> 0),
    PRIMARY KEY (firm_id, job_id, document_id),
    FOREIGN KEY (firm_id, job_id) REFERENCES recon.match_jobs (firm_id, id),
    FOREIGN KEY (firm_id, document_id) REFERENCES recon.documents (firm_id, id)
);

CREATE FUNCTION recon.match_jobs_guard() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF TG_OP <> 'UPDATE' THEN
        RAISE EXCEPTION 'match jobs are never deleted (% refused)', TG_OP USING ERRCODE = 'RCJ01';
    END IF;
    IF OLD.status <> 'running' THEN
        RAISE EXCEPTION 'a finished match job never changes' USING ERRCODE = 'RCJ01';
    END IF;
    IF (NEW.firm_id, NEW.id, NEW.client_id, NEW.auto_commit_t1, NEW.residual_tolerances,
        NEW.engine_version, NEW.created_at)
       IS DISTINCT FROM
       (OLD.firm_id, OLD.id, OLD.client_id, OLD.auto_commit_t1, OLD.residual_tolerances,
        OLD.engine_version, OLD.created_at) THEN
        RAISE EXCEPTION 'a match job''s identity never changes' USING ERRCODE = 'RCJ01';
    END IF;
    -- The snapshot's size is set exactly once; the cursor and the counters only grow.
    IF OLD.line_count IS NOT NULL AND NEW.line_count IS DISTINCT FROM OLD.line_count THEN
        RAISE EXCEPTION 'a job''s snapshot size never changes' USING ERRCODE = 'RCJ01';
    END IF;
    IF NEW.cursor < OLD.cursor OR NEW.matched < OLD.matched OR NEW.proposed < OLD.proposed
       OR NEW.exceptions < OLD.exceptions OR NEW.skipped < OLD.skipped THEN
        RAISE EXCEPTION 'a job''s cursor and counters never go back' USING ERRCODE = 'RCJ01';
    END IF;
    IF NEW.status = 'complete' THEN
        IF NEW.line_count IS NULL OR NEW.cursor <> NEW.line_count
           OR NEW.run_digest IS NULL THEN
            RAISE EXCEPTION 'a job completes only over its whole snapshot, with a digest'
                USING ERRCODE = 'RCJ01';
        END IF;
        NEW.finished_at := pg_catalog.now();
    ELSIF NEW.status = 'failed' THEN
        NEW.finished_at := pg_catalog.now();
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER match_jobs_guard
    BEFORE UPDATE OR DELETE ON recon.match_jobs
    FOR EACH ROW EXECUTE FUNCTION recon.match_jobs_guard();
CREATE TRIGGER match_jobs_no_truncate
    BEFORE TRUNCATE ON recon.match_jobs
    FOR EACH STATEMENT EXECUTE FUNCTION recon.match_jobs_guard();

-- The snapshot tables are written once at job start, never changed afterwards.
CREATE FUNCTION recon.job_snapshot_immutable() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    RAISE EXCEPTION '% on % refused: a job''s snapshot is fixed', TG_OP, TG_TABLE_NAME
        USING ERRCODE = 'RCJ01';
END
$$;

CREATE TRIGGER match_job_lines_immutable
    BEFORE UPDATE OR DELETE ON recon.match_job_lines
    FOR EACH ROW EXECUTE FUNCTION recon.job_snapshot_immutable();
CREATE TRIGGER match_job_documents_immutable
    BEFORE UPDATE OR DELETE ON recon.match_job_documents
    FOR EACH ROW EXECUTE FUNCTION recon.job_snapshot_immutable();

-- ---------------------------------------------------------------------------------------
-- Documents gain their allocated sum (the only mutable column; D-032).

ALTER TABLE recon.documents
    ADD COLUMN allocated_minor bigint NOT NULL DEFAULT 0
        CHECK (abs(allocated_minor) <= abs(amount_minor));
-- An allocation has the document's sign (both are "as the bank sees it", D-007).
ALTER TABLE recon.documents
    ADD CONSTRAINT documents_allocated_sign
        CHECK (allocated_minor = 0 OR pg_catalog.sign(allocated_minor) = pg_catalog.sign(amount_minor));

-- Composite tenancy target for allocation rows.
CREATE UNIQUE INDEX documents_by_client ON recon.documents (firm_id, client_id, id);
-- The candidate index: the open documents of one client and currency (D-071; the
-- engine's date-window and direction work happens in recon-match).
CREATE INDEX documents_open_candidates
    ON recon.documents (firm_id, client_id, currency, issue_date)
    WHERE allocated_minor <> amount_minor;

-- The lines a job loads, by client.
CREATE INDEX statement_lines_unreviewed
    ON recon.statement_lines (firm_id, client_id, id) WHERE status = 'unreviewed';
CREATE UNIQUE INDEX statement_lines_by_client ON recon.statement_lines (firm_id, client_id, id);

-- ---------------------------------------------------------------------------------------
-- Matches and their allocations.

CREATE TABLE recon.matches (
    firm_id uuid NOT NULL,
    id uuid NOT NULL DEFAULT uuidv7(),
    client_id uuid NOT NULL,
    job_id uuid,
    tier text NOT NULL CHECK (tier IN ('t1', 't2', 't3', 't4', 'manual')),
    status text NOT NULL
        CHECK (status IN ('proposed', 'committed', 'suspended', 'rejected', 'reversed')),
    currency text NOT NULL,
    score smallint CHECK (score BETWEEN 0 AND 100),
    score_version text CHECK (score_version IS NULL OR char_length(score_version) <= 200),
    score_exact_amount smallint CHECK (score_exact_amount IS NULL OR score_exact_amount IN (0, 40)),
    score_exact_reference smallint CHECK (score_exact_reference IS NULL OR score_exact_reference IN (0, 30)),
    score_counterparty smallint CHECK (score_counterparty IS NULL OR score_counterparty IN (0, 15)),
    score_date_proximity smallint CHECK (score_date_proximity IS NULL OR score_date_proximity BETWEEN 0 AND 10),
    score_uniqueness smallint CHECK (score_uniqueness IS NULL OR score_uniqueness IN (0, 5)),
    four_eyes boolean NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now(),
    row_version bigint NOT NULL DEFAULT 1 CHECK (row_version >= 1),
    PRIMARY KEY (firm_id, id),
    UNIQUE (firm_id, client_id, id),
    FOREIGN KEY (firm_id, client_id) REFERENCES recon.clients (firm_id, id),
    FOREIGN KEY (firm_id, job_id) REFERENCES recon.match_jobs (firm_id, id),
    CHECK ((score_version IS NULL) = (score IS NULL)),
    CHECK (score IS NULL OR score = score_exact_amount + score_exact_reference
           + score_counterparty + score_date_proximity + score_uniqueness)
);

CREATE INDEX matches_by_job ON recon.matches (firm_id, job_id) WHERE job_id IS NOT NULL;
CREATE INDEX matches_by_client_status ON recon.matches (firm_id, client_id, status);

CREATE TABLE recon.match_lines (
    firm_id uuid NOT NULL,
    client_id uuid NOT NULL,
    match_id uuid NOT NULL,
    line_id uuid NOT NULL,
    amount_minor bigint NOT NULL CHECK (amount_minor <> 0),
    PRIMARY KEY (firm_id, match_id, line_id),
    FOREIGN KEY (firm_id, client_id, match_id) REFERENCES recon.matches (firm_id, client_id, id),
    FOREIGN KEY (firm_id, client_id, line_id)
        REFERENCES recon.statement_lines (firm_id, client_id, id)
);

CREATE TABLE recon.match_documents (
    firm_id uuid NOT NULL,
    client_id uuid NOT NULL,
    match_id uuid NOT NULL,
    document_id uuid NOT NULL,
    amount_minor bigint NOT NULL CHECK (amount_minor <> 0),
    PRIMARY KEY (firm_id, match_id, document_id),
    FOREIGN KEY (firm_id, client_id, match_id) REFERENCES recon.matches (firm_id, client_id, id),
    FOREIGN KEY (firm_id, client_id, document_id) REFERENCES recon.documents (firm_id, client_id, id)
);

CREATE TABLE recon.match_residuals (
    firm_id uuid NOT NULL,
    client_id uuid NOT NULL,
    match_id uuid NOT NULL,
    code text NOT NULL CHECK (code IN ('bank_fee', 'tax_withheld', 'rounding')),
    amount_minor bigint NOT NULL CHECK (amount_minor <> 0),
    PRIMARY KEY (firm_id, match_id, code),
    FOREIGN KEY (firm_id, client_id, match_id) REFERENCES recon.matches (firm_id, client_id, id)
);

-- A match's lifecycle at the schema level (MatchStatus in recon-domain). Only a T1 match
-- may be inserted already `committed` (D-011); a person's approval is a status change,
-- never an insert, so proposed -> committed stays allowed.
CREATE FUNCTION recon.matches_guard() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'matches are never deleted' USING ERRCODE = 'RCM01';
    END IF;
    IF TG_OP = 'INSERT' THEN
        IF NEW.status = 'committed' AND NEW.tier <> 't1' THEN
            RAISE EXCEPTION 'only a T1 match may be committed at creation' USING ERRCODE = 'RCM01';
        END IF;
        RETURN NEW;
    END IF;
    IF (NEW.firm_id, NEW.id, NEW.client_id, NEW.job_id, NEW.tier, NEW.currency,
        NEW.score, NEW.score_version, NEW.score_exact_amount, NEW.score_exact_reference,
        NEW.score_counterparty, NEW.score_date_proximity, NEW.score_uniqueness,
        NEW.four_eyes, NEW.created_at)
       IS DISTINCT FROM
       (OLD.firm_id, OLD.id, OLD.client_id, OLD.job_id, OLD.tier, OLD.currency,
        OLD.score, OLD.score_version, OLD.score_exact_amount, OLD.score_exact_reference,
        OLD.score_counterparty, OLD.score_date_proximity, OLD.score_uniqueness,
        OLD.four_eyes, OLD.created_at) THEN
        RAISE EXCEPTION 'a match''s identity never changes' USING ERRCODE = 'RCM01';
    END IF;
    IF NEW.status IS DISTINCT FROM OLD.status
       AND NOT (OLD.status = 'proposed' AND NEW.status IN ('committed', 'rejected'))
       AND NOT (OLD.status = 'committed' AND NEW.status IN ('suspended', 'reversed'))
       AND NOT (OLD.status = 'suspended' AND NEW.status IN ('committed', 'reversed')) THEN
        RAISE EXCEPTION 'invalid match transition % -> %', OLD.status, NEW.status
            USING ERRCODE = 'RCM01';
    END IF;
    IF NEW.status IS DISTINCT FROM OLD.status AND NEW.row_version <> OLD.row_version + 1 THEN
        RAISE EXCEPTION 'a status change advances the row version by one' USING ERRCODE = 'RCM01';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER matches_guard
    BEFORE INSERT OR UPDATE OR DELETE ON recon.matches
    FOR EACH ROW EXECUTE FUNCTION recon.matches_guard();
CREATE TRIGGER matches_no_truncate
    BEFORE TRUNCATE ON recon.matches
    FOR EACH STATEMENT EXECUTE FUNCTION recon.matches_guard();

-- At commit: a match's insert or status change has its decision, to that state, made in
-- this transaction (the RCL04 pattern).
CREATE FUNCTION recon.match_has_its_decision() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM recon.decision_log d
         WHERE d.firm_id = NEW.firm_id AND d.client_id = NEW.client_id
           AND d.subject_kind = 'match' AND d.subject_id = NEW.id
           AND d.to_state = NEW.status AND d.decided_at = pg_catalog.now()) THEN
        RAISE EXCEPTION 'match % became % without its decision', NEW.id, NEW.status
            USING ERRCODE = 'RCM02';
    END IF;
    RETURN NULL;
END
$$;

CREATE CONSTRAINT TRIGGER matches_have_their_decision
    AFTER INSERT OR UPDATE OF status ON recon.matches
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION recon.match_has_its_decision();

-- Allocation rows: the match must be live, the line still `unreviewed`, the amount a
-- same-signed part of the line's amount, and the line not already held by another live
-- match (a person decides conflicts; the engine already refuses them).
CREATE FUNCTION recon.match_lines_guard() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    m_status text;
    l_status text;
    l_amount bigint;
BEGIN
    IF TG_OP <> 'INSERT' THEN
        RAISE EXCEPTION 'match line rows never change (% refused)', TG_OP USING ERRCODE = 'RCM03';
    END IF;
    SELECT status INTO m_status FROM recon.matches
     WHERE firm_id = NEW.firm_id AND id = NEW.match_id;
    IF m_status IS NULL OR m_status IN ('rejected', 'reversed') THEN
        RAISE EXCEPTION 'a finished match takes no allocations' USING ERRCODE = 'RCM03';
    END IF;
    SELECT status, amount_minor INTO l_status, l_amount FROM recon.statement_lines
     WHERE firm_id = NEW.firm_id AND id = NEW.line_id;
    IF l_status IS DISTINCT FROM 'unreviewed' THEN
        RAISE EXCEPTION 'only an unreviewed line is allocated' USING ERRCODE = 'RCM03';
    END IF;
    IF abs(NEW.amount_minor) > abs(l_amount)
       OR pg_catalog.sign(NEW.amount_minor) <> pg_catalog.sign(l_amount) THEN
        RAISE EXCEPTION 'a line allocation must be a same-signed part of its line'
            USING ERRCODE = 'RCM03';
    END IF;
    IF EXISTS (
        SELECT 1 FROM recon.match_lines ml
          JOIN recon.matches m ON m.firm_id = ml.firm_id AND m.id = ml.match_id
         WHERE ml.firm_id = NEW.firm_id AND ml.line_id = NEW.line_id
           AND ml.match_id <> NEW.match_id AND m.status NOT IN ('rejected', 'reversed')) THEN
        RAISE EXCEPTION 'the line is already held by a live match' USING ERRCODE = 'RCM03';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER match_lines_guard
    BEFORE INSERT OR UPDATE OR DELETE ON recon.match_lines
    FOR EACH ROW EXECUTE FUNCTION recon.match_lines_guard();
CREATE TRIGGER match_lines_no_truncate
    BEFORE TRUNCATE ON recon.match_lines
    FOR EACH STATEMENT EXECUTE FUNCTION recon.match_lines_guard();

-- Document allocations move the document's allocated sum; the table's CHECK (and the
-- explicit over-allocation refusal below) bound it even for raw SQL. Removing a row —
-- a reversal's release — gives its amount back.
CREATE FUNCTION recon.match_documents_guard() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    d_amount bigint;
    d_allocated bigint;
    d_kind text;
    m_status text;
    next_allocated bigint;
BEGIN
    IF TG_OP = 'INSERT' THEN
        SELECT status INTO m_status FROM recon.matches
         WHERE firm_id = NEW.firm_id AND id = NEW.match_id;
        IF m_status IS NULL OR m_status IN ('rejected', 'reversed') THEN
            RAISE EXCEPTION 'a finished match takes no allocations' USING ERRCODE = 'RCM03';
        END IF;
        SELECT amount_minor, allocated_minor, kind INTO d_amount, d_allocated, d_kind
          FROM recon.documents WHERE firm_id = NEW.firm_id AND id = NEW.document_id;
        IF abs(NEW.amount_minor) > abs(d_amount)
           OR pg_catalog.sign(NEW.amount_minor) <> pg_catalog.sign(d_amount) THEN
            RAISE EXCEPTION 'a document allocation must be a same-signed part of its document'
                USING ERRCODE = 'RCM03';
        END IF;
        next_allocated := d_allocated + NEW.amount_minor;
        IF abs(next_allocated) > abs(d_amount) THEN
            RAISE EXCEPTION 'the document would be over-allocated' USING ERRCODE = 'RCM03';
        END IF;
        UPDATE recon.documents
           SET allocated_minor = next_allocated, row_version = row_version + 1
         WHERE firm_id = NEW.firm_id AND id = NEW.document_id;
        RETURN NEW;
    ELSIF TG_OP = 'DELETE' THEN
        -- Only a finished (rejected or reversed) match's rows may be released.
        SELECT status INTO m_status FROM recon.matches
         WHERE firm_id = OLD.firm_id AND id = OLD.match_id;
        IF m_status IS DISTINCT FROM 'rejected' AND m_status IS DISTINCT FROM 'reversed' THEN
            RAISE EXCEPTION 'a live match''s allocations are never released'
                USING ERRCODE = 'RCM03';
        END IF;
        UPDATE recon.documents
           SET allocated_minor = allocated_minor - OLD.amount_minor,
               row_version = row_version + 1
         WHERE firm_id = OLD.firm_id AND id = OLD.document_id;
        RETURN OLD;
    ELSE
        RAISE EXCEPTION 'match document rows never change (% refused)', TG_OP USING ERRCODE = 'RCM03';
    END IF;
END
$$;

CREATE TRIGGER match_documents_guard
    BEFORE INSERT OR DELETE ON recon.match_documents
    FOR EACH ROW EXECUTE FUNCTION recon.match_documents_guard();
CREATE TRIGGER match_documents_no_update
    BEFORE UPDATE ON recon.match_documents
    FOR EACH ROW EXECUTE FUNCTION recon.match_documents_guard();
CREATE TRIGGER match_documents_no_truncate
    BEFORE TRUNCATE ON recon.match_documents
    FOR EACH STATEMENT EXECUTE FUNCTION recon.match_documents_guard();

-- Residual rows are fixed at their match's creation (D-014: named, never absorbed).
CREATE FUNCTION recon.match_residuals_guard() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    RAISE EXCEPTION '% on % refused: residuals never change', TG_OP, TG_TABLE_NAME
        USING ERRCODE = 'RCM03';
END
$$;

CREATE TRIGGER match_residuals_immutable
    BEFORE UPDATE OR DELETE ON recon.match_residuals
    FOR EACH ROW EXECUTE FUNCTION recon.match_residuals_guard();
CREATE TRIGGER match_residuals_no_truncate
    BEFORE TRUNCATE ON recon.match_residuals
    FOR EACH STATEMENT EXECUTE FUNCTION recon.match_residuals_guard();

-- ---------------------------------------------------------------------------------------
-- Proposals (the engine's now, the model's from R4) and the exception queue's rows.

CREATE TABLE recon.proposals (
    firm_id uuid NOT NULL,
    id uuid NOT NULL DEFAULT uuidv7(),
    client_id uuid NOT NULL,
    line_id uuid NOT NULL,
    match_id uuid NOT NULL,
    job_id uuid,
    actor text NOT NULL CHECK (actor IN ('engine', 'model')),
    engine_version text NOT NULL CHECK (char_length(engine_version) BETWEEN 1 AND 200),
    -- The candidate document ids the suggestion was made against, nearest first.
    candidates uuid[] NOT NULL CHECK (cardinality(candidates) BETWEEN 1 AND 64),
    status text NOT NULL DEFAULT 'open'
        CHECK (status IN ('open', 'approved', 'rejected', 'superseded')),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (firm_id, id),
    -- A match is proposed once.
    UNIQUE (firm_id, match_id),
    FOREIGN KEY (firm_id, client_id) REFERENCES recon.clients (firm_id, id),
    FOREIGN KEY (firm_id, client_id, line_id)
        REFERENCES recon.statement_lines (firm_id, client_id, id),
    FOREIGN KEY (firm_id, client_id, match_id) REFERENCES recon.matches (firm_id, client_id, id),
    FOREIGN KEY (firm_id, job_id) REFERENCES recon.match_jobs (firm_id, id)
);

-- At most one open proposal per line.
CREATE UNIQUE INDEX proposals_one_open_per_line
    ON recon.proposals (firm_id, line_id) WHERE status = 'open';
CREATE INDEX proposals_queue ON recon.proposals (firm_id, client_id, status, created_at);

CREATE FUNCTION recon.proposals_guard() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF TG_OP <> 'UPDATE' THEN
        RAISE EXCEPTION 'proposals are never deleted (% refused)', TG_OP USING ERRCODE = 'RCM03';
    END IF;
    IF (NEW.firm_id, NEW.id, NEW.client_id, NEW.line_id, NEW.match_id, NEW.job_id,
        NEW.actor, NEW.engine_version, NEW.candidates, NEW.created_at)
       IS DISTINCT FROM
       (OLD.firm_id, OLD.id, OLD.client_id, OLD.line_id, OLD.match_id, OLD.job_id,
        OLD.actor, OLD.engine_version, OLD.candidates, OLD.created_at) THEN
        RAISE EXCEPTION 'a proposal''s identity never changes' USING ERRCODE = 'RCM03';
    END IF;
    IF NEW.status IS DISTINCT FROM OLD.status
       AND NOT (OLD.status = 'open' AND NEW.status IN ('approved', 'rejected', 'superseded')) THEN
        RAISE EXCEPTION 'invalid proposal transition % -> %', OLD.status, NEW.status
            USING ERRCODE = 'RCM03';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER proposals_guard
    BEFORE UPDATE OR DELETE ON recon.proposals
    FOR EACH ROW EXECUTE FUNCTION recon.proposals_guard();
CREATE TRIGGER proposals_no_truncate
    BEFORE TRUNCATE ON recon.proposals
    FOR EACH STATEMENT EXECUTE FUNCTION recon.proposals_guard();

-- The exception queue's rows: current state only — a line holds at most one, and only
-- while it is an exception; the decision log carries its history.
CREATE TABLE recon.line_exceptions (
    firm_id uuid NOT NULL,
    client_id uuid NOT NULL,
    line_id uuid NOT NULL,
    kind text NOT NULL CHECK (kind IN (
        'no_candidate', 'multiple_candidates', 'amount_mismatch', 'cross_currency',
        'search_budget_exhausted', 'source_changed', 'duplicate_suspect',
        'narration_needed', 'model_unavailable', 'model_budget_exhausted')),
    candidates uuid[] NOT NULL CHECK (cardinality(candidates) <= 10),
    job_id uuid,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (firm_id, line_id),
    FOREIGN KEY (firm_id, client_id, line_id)
        REFERENCES recon.statement_lines (firm_id, client_id, id),
    FOREIGN KEY (firm_id, client_id) REFERENCES recon.clients (firm_id, id),
    FOREIGN KEY (firm_id, job_id) REFERENCES recon.match_jobs (firm_id, id)
);

CREATE INDEX line_exceptions_queue ON recon.line_exceptions (firm_id, client_id, kind);

CREATE FUNCTION recon.line_exceptions_guard() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    l_status text;
BEGIN
    IF TG_OP <> 'INSERT' THEN
        RAISE EXCEPTION 'exception rows only leave by delete (% refused)', TG_OP
            USING ERRCODE = 'RCL05';
    END IF;
    SELECT status INTO l_status FROM recon.statement_lines
     WHERE firm_id = NEW.firm_id AND id = NEW.line_id;
    IF l_status IS DISTINCT FROM 'exception' THEN
        RAISE EXCEPTION 'an exception row belongs to a line in the exception state'
            USING ERRCODE = 'RCL05';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER line_exceptions_guard
    BEFORE INSERT OR UPDATE ON recon.line_exceptions
    FOR EACH ROW EXECUTE FUNCTION recon.line_exceptions_guard();
CREATE TRIGGER line_exceptions_no_truncate
    BEFORE TRUNCATE ON recon.line_exceptions
    FOR EACH STATEMENT EXECUTE FUNCTION recon.line_exceptions_guard();

-- A line that stops being an exception loses its queue row in the same transaction.
CREATE FUNCTION recon.line_exceptions_resolved() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NEW.status <> 'exception' THEN
        DELETE FROM recon.line_exceptions
         WHERE firm_id = NEW.firm_id AND line_id = NEW.id;
    END IF;
    RETURN NULL;
END
$$;

CREATE TRIGGER line_exceptions_resolved
    AFTER UPDATE OF status ON recon.statement_lines
    FOR EACH ROW EXECUTE FUNCTION recon.line_exceptions_resolved();

-- ---------------------------------------------------------------------------------------
-- Documents: only the allocated sum and its row version may change now (everything else
-- stays immutable, RCD02), and an allocated change advances the version by one.

CREATE OR REPLACE FUNCTION recon.documents_immutable() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF TG_OP <> 'UPDATE' THEN
        RAISE EXCEPTION '% on % refused: documents are never deleted', TG_OP, TG_TABLE_NAME
            USING ERRCODE = 'RCD02';
    END IF;
    IF (NEW.firm_id, NEW.id, NEW.client_id, NEW.import_id, NEW.import_kind, NEW.row_no,
        NEW.kind, NEW.number_raw, NEW.number_norm, NEW.issue_date, NEW.due_date,
        NEW.amount_minor, NEW.currency, NEW.counterparty_raw, NEW.counterparty_norm,
        NEW.description, NEW.tuple_hash, NEW.occurrence, NEW.fingerprint, NEW.created_at)
       IS DISTINCT FROM
       (OLD.firm_id, OLD.id, OLD.client_id, OLD.import_id, OLD.import_kind, OLD.row_no,
        OLD.kind, OLD.number_raw, OLD.number_norm, OLD.issue_date, OLD.due_date,
        OLD.amount_minor, OLD.currency, OLD.counterparty_raw, OLD.counterparty_norm,
        OLD.description, OLD.tuple_hash, OLD.occurrence, OLD.fingerprint, OLD.created_at) THEN
        RAISE EXCEPTION 'a document''s source fields never change' USING ERRCODE = 'RCD02';
    END IF;
    IF NEW.allocated_minor IS DISTINCT FROM OLD.allocated_minor
       AND NEW.row_version <> OLD.row_version + 1 THEN
        RAISE EXCEPTION 'an allocation change advances the row version by one'
            USING ERRCODE = 'RCD03';
    END IF;
    RETURN NEW;
END
$$;

-- ---------------------------------------------------------------------------------------
-- The runtime role's new grants (D-037).

GRANT SELECT, INSERT ON recon.match_jobs, recon.match_job_lines, recon.match_job_documents
    TO recon_runtime;
GRANT UPDATE (status, lease_token, lease_until, cursor, line_count, matched, proposed,
              exceptions, skipped, run_digest, error_code, finished_at)
    ON recon.match_jobs TO recon_runtime;
GRANT SELECT, INSERT ON recon.matches, recon.match_lines, recon.match_documents,
    recon.match_residuals TO recon_runtime;
GRANT UPDATE (status, row_version) ON recon.matches TO recon_runtime;
GRANT SELECT, INSERT ON recon.proposals TO recon_runtime;
GRANT UPDATE (status) ON recon.proposals TO recon_runtime;
GRANT SELECT, INSERT, DELETE ON recon.line_exceptions TO recon_runtime;
GRANT UPDATE (allocated_minor, row_version) ON recon.documents TO recon_runtime;
GRANT UPDATE (auto_commit_t1, residual_tolerances) ON recon.firms TO recon_runtime;
