-- R3 slice 3 (D-015, D-006, failure case 5): rules and their immutable versions,
-- periods and the closed-period lock.
--
-- A rule is client-scoped typed data; its versions hold the predicate and action and
-- never change once written (only a version's status may move proposed -> approved or
-- withdrawn, once, with a decision). A period is client x account x month and walks
-- open -> closed -> reopened; closing refuses while a line is undecided or an
-- exception is unacknowledged, records the period's decision digest and the chain
-- head, and locks the month's lines against inserts and status changes until an
-- owner reopens it with a reason.

-- ---------------------------------------------------------------------------------------
-- Rules.

CREATE TABLE recon.rules (
    firm_id uuid NOT NULL,
    id uuid NOT NULL DEFAULT uuidv7(),
    client_id uuid NOT NULL,
    name text NOT NULL CHECK (octet_length(name) BETWEEN 1 AND 200),
    -- The version currently in force; NULL while nothing is approved.
    active_version integer CHECK (active_version IS NULL OR active_version >= 1),
    status text NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'retired')),
    row_version bigint NOT NULL DEFAULT 1 CHECK (row_version >= 1),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (firm_id, id),
    FOREIGN KEY (firm_id, client_id) REFERENCES recon.clients (firm_id, id)
);

CREATE TABLE recon.rule_versions (
    firm_id uuid NOT NULL,
    rule_id uuid NOT NULL,
    version integer NOT NULL CHECK (version >= 1),
    client_id uuid NOT NULL,
    predicate jsonb NOT NULL CHECK (octet_length(predicate::text) <= 8192),
    action text NOT NULL CHECK (action IN ('assign_counterparty', 'exclude', 'prefer_document_kind')),
    -- The action's argument: a counterparty name, a reason code or a document kind.
    action_arg text NOT NULL CHECK (octet_length(action_arg) BETWEEN 1 AND 200),
    -- Only the approving owner may mark an exclusion for automatic apply (D-015) and
    -- only exclusions may carry it.
    auto_apply boolean NOT NULL DEFAULT false CHECK (auto_apply = false OR action = 'exclude'),
    status text NOT NULL DEFAULT 'proposed' CHECK (status IN ('proposed', 'approved', 'withdrawn')),
    proposed_by uuid NOT NULL,
    approved_by uuid,
    approved_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (firm_id, rule_id, version),
    FOREIGN KEY (firm_id, rule_id) REFERENCES recon.rules (firm_id, id),
    FOREIGN KEY (firm_id, proposed_by) REFERENCES recon.users (firm_id, id),
    FOREIGN KEY (firm_id, approved_by) REFERENCES recon.users (firm_id, id),
    CHECK ((status = 'approved') = (approved_by IS NOT NULL AND approved_at IS NOT NULL))
);

CREATE INDEX rule_versions_by_client ON recon.rule_versions (firm_id, client_id, status);

-- A version's content is frozen once written; its status may move proposed ->
-- approved | withdrawn exactly once (approving stamps approved_by/at), and nothing
-- may delete it. The rule row freezes name and client; active_version, status and the
-- row version are the only writable fields.
CREATE FUNCTION recon.rule_versions_guard() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'a rule version is never deleted' USING ERRCODE = 'RCR01';
    END IF;
    IF (NEW.firm_id, NEW.rule_id, NEW.version, NEW.client_id, NEW.predicate, NEW.action,
        NEW.action_arg, NEW.proposed_by, NEW.created_at)
       IS DISTINCT FROM
       (OLD.firm_id, OLD.rule_id, OLD.version, OLD.client_id, OLD.predicate, OLD.action,
        OLD.action_arg, OLD.proposed_by, OLD.created_at) THEN
        RAISE EXCEPTION 'a rule version''s content never changes' USING ERRCODE = 'RCR01';
    END IF;
    -- The owner's auto-apply mark is set once, in the proposed -> approved step.
    IF NEW.auto_apply IS DISTINCT FROM OLD.auto_apply
       AND NOT (OLD.status = 'proposed' AND NEW.status = 'approved') THEN
        RAISE EXCEPTION 'the auto-apply mark is set at approval only' USING ERRCODE = 'RCR01';
    END IF;
    IF NEW.status = OLD.status THEN
        NULL;
    ELSIF OLD.status = 'proposed' AND NEW.status IN ('approved', 'withdrawn') THEN
        NULL;
    ELSE
        RAISE EXCEPTION 'a decided rule version never reverts' USING ERRCODE = 'RCR01';
    END IF;
    IF NEW.status = 'approved' AND OLD.status = 'proposed' THEN
        IF NEW.approved_by IS NULL OR NEW.approved_at IS NULL THEN
            RAISE EXCEPTION 'approving stamps who and when' USING ERRCODE = 'RCR01';
        END IF;
    ELSIF (NEW.approved_by, NEW.approved_at) IS DISTINCT FROM (OLD.approved_by, OLD.approved_at) THEN
        RAISE EXCEPTION 'approval stamps once' USING ERRCODE = 'RCR01';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER rule_versions_guard
    BEFORE UPDATE OR DELETE ON recon.rule_versions
    FOR EACH ROW EXECUTE FUNCTION recon.rule_versions_guard();

-- At commit: a version-status change carries its decision (subject 'rule', the rule's
-- id, to_state the new status, made in this transaction).
CREATE FUNCTION recon.rule_version_has_its_decision() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM recon.decision_log d
         WHERE d.firm_id = NEW.firm_id AND d.subject_kind = 'rule' AND d.subject_id = NEW.rule_id
           AND d.to_state = NEW.status AND d.decided_at = pg_catalog.now()) THEN
        RAISE EXCEPTION 'rule version % became % without its decision', NEW.version, NEW.status
            USING ERRCODE = 'RCR02';
    END IF;
    RETURN NULL;
END
$$;

CREATE CONSTRAINT TRIGGER rule_versions_have_their_decision
    AFTER UPDATE OF status ON recon.rule_versions
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    WHEN (NEW.status IS DISTINCT FROM OLD.status)
    EXECUTE FUNCTION recon.rule_version_has_its_decision();

CREATE FUNCTION recon.rules_guard() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'a rule is never deleted' USING ERRCODE = 'RCR01';
    END IF;
    IF (NEW.firm_id, NEW.id, NEW.client_id, NEW.name, NEW.created_at)
       IS DISTINCT FROM
       (OLD.firm_id, OLD.id, OLD.client_id, OLD.name, OLD.created_at) THEN
        RAISE EXCEPTION 'a rule''s identity never changes' USING ERRCODE = 'RCR01';
    END IF;
    IF OLD.status = 'retired' AND NEW.status <> 'retired' THEN
        RAISE EXCEPTION 'a retired rule stays retired' USING ERRCODE = 'RCR01';
    END IF;
    IF (NEW.status, NEW.active_version) IS DISTINCT FROM (OLD.status, OLD.active_version)
       AND NEW.row_version <> OLD.row_version + 1 THEN
        RAISE EXCEPTION 'a rule change advances the row version by one' USING ERRCODE = 'RCR01';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER rules_guard
    BEFORE UPDATE OR DELETE ON recon.rules
    FOR EACH ROW EXECUTE FUNCTION recon.rules_guard();

-- A status change on the rule (retirement) carries a decision too.
CREATE FUNCTION recon.rule_has_its_decision() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM recon.decision_log d
         WHERE d.firm_id = NEW.firm_id AND d.subject_kind = 'rule' AND d.subject_id = NEW.id
           AND d.to_state = NEW.status AND d.decided_at = pg_catalog.now()) THEN
        RAISE EXCEPTION 'rule % became % without its decision', NEW.id, NEW.status
            USING ERRCODE = 'RCR02';
    END IF;
    RETURN NULL;
END
$$;

CREATE CONSTRAINT TRIGGER rules_have_their_decision
    AFTER UPDATE OF status ON recon.rules
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    WHEN (NEW.status IS DISTINCT FROM OLD.status)
    EXECUTE FUNCTION recon.rule_has_its_decision();

-- ---------------------------------------------------------------------------------------
-- Periods: client x account x month, open -> closed -> reopened. The digest and chain
-- head stored at close are the period's proof; a reopened period keeps them as the
-- record of its last close.

CREATE TABLE recon.periods (
    firm_id uuid NOT NULL,
    id uuid NOT NULL DEFAULT uuidv7(),
    client_id uuid NOT NULL,
    account_id uuid NOT NULL,
    month date NOT NULL CHECK (month = date_trunc('month', month)::date),
    status text NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'closed', 'reopened')),
    closed_by uuid,
    closed_at timestamptz,
    -- sha256 over the period's line decisions (subject 'line', chain order) and the
    -- client's head at close.
    digest bytea CHECK (digest IS NULL OR octet_length(digest) = 32),
    chain_seq bigint,
    chain_head bytea CHECK (chain_head IS NULL OR octet_length(chain_head) = 32),
    reopened_by uuid,
    reopened_at timestamptz,
    row_version bigint NOT NULL DEFAULT 1 CHECK (row_version >= 1),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (firm_id, client_id, account_id, month),
    FOREIGN KEY (firm_id, client_id) REFERENCES recon.clients (firm_id, id),
    FOREIGN KEY (firm_id, account_id) REFERENCES recon.accounts (firm_id, id),
    FOREIGN KEY (firm_id, closed_by) REFERENCES recon.users (firm_id, id),
    FOREIGN KEY (firm_id, reopened_by) REFERENCES recon.users (firm_id, id),
    CHECK (status <> 'closed' OR (
        digest IS NOT NULL AND chain_seq IS NOT NULL AND chain_head IS NOT NULL
            AND closed_by IS NOT NULL AND closed_at IS NOT NULL)),
    CHECK (status <> 'reopened' OR (reopened_by IS NOT NULL AND reopened_at IS NOT NULL))
);

CREATE FUNCTION recon.periods_guard() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'a period is never deleted' USING ERRCODE = 'RCP03';
    END IF;
    IF TG_OP = 'INSERT' THEN
        IF NEW.status <> 'open' THEN
            RAISE EXCEPTION 'a period begins open' USING ERRCODE = 'RCP03';
        END IF;
        RETURN NEW;
    END IF;
    IF (NEW.firm_id, NEW.id, NEW.client_id, NEW.account_id, NEW.month, NEW.created_at)
       IS DISTINCT FROM
       (OLD.firm_id, OLD.id, OLD.client_id, OLD.account_id, OLD.month, OLD.created_at) THEN
        RAISE EXCEPTION 'a period''s identity never changes' USING ERRCODE = 'RCP03';
    END IF;
    IF NOT ((OLD.status IN ('open', 'reopened') AND NEW.status = 'closed')
         OR (OLD.status = 'closed' AND NEW.status = 'reopened')
         OR (NEW.status = OLD.status)) THEN
        RAISE EXCEPTION 'a period moves open -> closed -> reopened only' USING ERRCODE = 'RCP03';
    END IF;
    IF NEW.status IS DISTINCT FROM OLD.status AND NEW.row_version <> OLD.row_version + 1 THEN
        RAISE EXCEPTION 'a period change advances the row version by one' USING ERRCODE = 'RCP03';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER periods_guard
    BEFORE INSERT OR UPDATE OR DELETE ON recon.periods
    FOR EACH ROW EXECUTE FUNCTION recon.periods_guard();

-- At commit: a status change carries its decision (subject 'period').
CREATE FUNCTION recon.period_has_its_decision() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM recon.decision_log d
         WHERE d.firm_id = NEW.firm_id AND d.subject_kind = 'period' AND d.subject_id = NEW.id
           AND d.to_state = NEW.status AND d.decided_at = pg_catalog.now()) THEN
        RAISE EXCEPTION 'period % became % without its decision', NEW.id, NEW.status
            USING ERRCODE = 'RCP04';
    END IF;
    RETURN NULL;
END
$$;

CREATE CONSTRAINT TRIGGER periods_have_their_decision
    AFTER UPDATE OF status ON recon.periods
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    WHEN (NEW.status IS DISTINCT FROM OLD.status)
    EXECUTE FUNCTION recon.period_has_its_decision();

-- ---------------------------------------------------------------------------------------
-- The lock: nothing may insert a line into a closed month or change a line's status in
-- one. A reviewer acknowledges an exception in place; the flag lives on the queue row.
-- An import refused on a closed period is discarded with its own reason.

ALTER TABLE recon.imports DROP CONSTRAINT imports_discard_reason_check;
ALTER TABLE recon.imports ADD CONSTRAINT imports_discard_reason_check
    CHECK (discard_reason IN ('duplicate_file', 'lease_expired', 'upload_failed', 'account_busy',
                              'revision_rejected', 'period_closed'));

ALTER TABLE recon.line_exceptions
    ADD COLUMN acknowledged_by uuid,
    ADD COLUMN acknowledged_at timestamptz,
    ADD CONSTRAINT line_exceptions_acknowledged_by_fkey
        FOREIGN KEY (firm_id, acknowledged_by) REFERENCES recon.users (firm_id, id),
    ADD CONSTRAINT line_exceptions_acknowledgement_pair
        CHECK ((acknowledged_by IS NULL) = (acknowledged_at IS NULL));

-- The 0007 guard refused every UPDATE; the acknowledgement is the one allowed write:
-- the row's identity holds and the stamp is written once, never edited or cleared.
CREATE OR REPLACE FUNCTION recon.line_exceptions_guard() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    l_status text;
BEGIN
    IF TG_OP = 'UPDATE' THEN
        IF (NEW.firm_id, NEW.client_id, NEW.line_id, NEW.kind, NEW.candidates,
            NEW.job_id, NEW.created_at)
           IS DISTINCT FROM
           (OLD.firm_id, OLD.client_id, OLD.line_id, OLD.kind, OLD.candidates,
            OLD.job_id, OLD.created_at)
           OR NEW.acknowledged_at IS NULL
           OR OLD.acknowledged_at IS NOT NULL THEN
            RAISE EXCEPTION 'exception rows only leave by delete (UPDATE refused)'
                USING ERRCODE = 'RCL05';
        END IF;
        RETURN NEW;
    END IF;
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

CREATE FUNCTION recon.statement_lines_period_guard() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM recon.periods p
         WHERE p.firm_id = NEW.firm_id AND p.client_id = NEW.client_id
           AND p.account_id = NEW.account_id
           AND p.month = date_trunc('month', NEW.booking_date)::date
           AND p.status = 'closed') THEN
        RAISE EXCEPTION 'the line''s period is closed' USING ERRCODE = 'RCP02';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER statement_lines_period_guard
    BEFORE INSERT ON recon.statement_lines
    FOR EACH ROW EXECUTE FUNCTION recon.statement_lines_period_guard();

CREATE TRIGGER statement_lines_period_guard_status
    BEFORE UPDATE OF status ON recon.statement_lines
    FOR EACH ROW
    WHEN (NEW.status IS DISTINCT FROM OLD.status)
    EXECUTE FUNCTION recon.statement_lines_period_guard();

-- ---------------------------------------------------------------------------------------
-- Grants.

GRANT SELECT, INSERT ON recon.rules, recon.rule_versions, recon.periods TO recon_runtime;
GRANT UPDATE (active_version, status, row_version) ON recon.rules TO recon_runtime;
GRANT UPDATE (status, approved_by, approved_at, auto_apply) ON recon.rule_versions TO recon_runtime;
GRANT UPDATE (status, closed_by, closed_at, digest, chain_seq, chain_head,
              reopened_by, reopened_at, row_version) ON recon.periods TO recon_runtime;
GRANT UPDATE (acknowledged_by, acknowledged_at) ON recon.line_exceptions TO recon_runtime;
