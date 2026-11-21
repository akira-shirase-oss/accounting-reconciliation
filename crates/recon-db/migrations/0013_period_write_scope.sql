-- S2-6 (D-076): the period write lock covers every write that can move a line's
-- world, not only statement_lines rows (0012). A close serializes against match
-- status and allocation changes, exception rows, proposals, and the line/match
-- decisions those writes record — exactly as it serializes against the lines
-- themselves. Each statement upserts the touched periods, locks them FOR KEY
-- SHARE in the same deterministic order, and then tests `closed`.
--
-- Lock ordering: every guard locks periods in (firm, client, account, month)
-- order. A close locks its single period FOR UPDATE first (0012 + the account
-- advisory lock), so a writer that touches several periods can wait behind a
-- close but never holds one period's lock while reaching past another writer's
-- — the orders are total per (firm, client, account, month) tuple.

-- Resolve a set of line ids to their periods, create any that are missing,
-- lock them FOR KEY SHARE in order, and refuse if any is closed.
CREATE FUNCTION recon.periods_lock_lines(p_firm uuid, p_line_ids uuid[])
RETURNS void
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    INSERT INTO recon.periods (firm_id, client_id, account_id, month)
    SELECT DISTINCT l.firm_id, l.client_id, l.account_id,
                    pg_catalog.date_trunc('month', l.booking_date)::date
      FROM recon.statement_lines l
     WHERE l.firm_id = p_firm AND l.id = ANY (p_line_ids)
     ORDER BY l.firm_id, l.client_id, l.account_id,
              pg_catalog.date_trunc('month', l.booking_date)::date
    ON CONFLICT DO NOTHING;

    PERFORM p.id
      FROM recon.periods p
     WHERE (p.firm_id, p.client_id, p.account_id, p.month) IN (
         SELECT DISTINCT l.firm_id, l.client_id, l.account_id,
                         pg_catalog.date_trunc('month', l.booking_date)::date
           FROM recon.statement_lines l
          WHERE l.firm_id = p_firm AND l.id = ANY (p_line_ids))
     ORDER BY p.firm_id, p.client_id, p.account_id, p.month
     FOR KEY SHARE OF p;

    IF EXISTS (
        SELECT 1
          FROM recon.periods p
         WHERE (p.firm_id, p.client_id, p.account_id, p.month) IN (
             SELECT DISTINCT l.firm_id, l.client_id, l.account_id,
                             pg_catalog.date_trunc('month', l.booking_date)::date
               FROM recon.statement_lines l
              WHERE l.firm_id = p_firm AND l.id = ANY (p_line_ids))
           AND p.status = 'closed')
    THEN
        RAISE EXCEPTION 'a line''s period is closed' USING ERRCODE = 'RCP02';
    END IF;
END
$$;

-- Exception rows: create, the one permitted acknowledgement update, delete.
CREATE FUNCTION recon.line_exceptions_period_guard() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    PERFORM recon.periods_lock_lines(t.firm_id, t.line_ids)
      FROM (SELECT firm_id, array_agg(line_id) AS line_ids
              FROM touched GROUP BY firm_id) t;
    RETURN NULL;
END
$$;

CREATE TRIGGER line_exceptions_period_guard_insert
    AFTER INSERT ON recon.line_exceptions
    REFERENCING NEW TABLE AS touched FOR EACH STATEMENT
    EXECUTE FUNCTION recon.line_exceptions_period_guard();

CREATE TRIGGER line_exceptions_period_guard_update
    AFTER UPDATE ON recon.line_exceptions
    REFERENCING NEW TABLE AS touched FOR EACH STATEMENT
    EXECUTE FUNCTION recon.line_exceptions_period_guard();

CREATE TRIGGER line_exceptions_period_guard_delete
    AFTER DELETE ON recon.line_exceptions
    REFERENCING OLD TABLE AS touched FOR EACH STATEMENT
    EXECUTE FUNCTION recon.line_exceptions_period_guard();

-- Allocations: rows naming the line directly.
CREATE FUNCTION recon.match_lines_period_guard() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    PERFORM recon.periods_lock_lines(t.firm_id, t.line_ids)
      FROM (SELECT firm_id, array_agg(line_id) AS line_ids
              FROM touched GROUP BY firm_id) t;
    RETURN NULL;
END
$$;

CREATE TRIGGER match_lines_period_guard_insert
    AFTER INSERT ON recon.match_lines
    REFERENCING NEW TABLE AS touched FOR EACH STATEMENT
    EXECUTE FUNCTION recon.match_lines_period_guard();

CREATE TRIGGER match_lines_period_guard_delete
    AFTER DELETE ON recon.match_lines
    REFERENCING OLD TABLE AS touched FOR EACH STATEMENT
    EXECUTE FUNCTION recon.match_lines_period_guard();

-- Match rows: a status change moves every line the match allocates. INSERT
-- needs no guard (a bare match has no lines yet — its match_lines insert
-- guards it); DELETE is already refused by the RCM01 lifecycle guard.
CREATE FUNCTION recon.matches_period_guard() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    PERFORM recon.periods_lock_lines(t.firm_id, t.line_ids)
      FROM (SELECT m.firm_id, array_agg(DISTINCT ml.line_id) AS line_ids
              FROM touched m
              JOIN recon.match_lines ml
                ON ml.firm_id = m.firm_id AND ml.match_id = m.id
             GROUP BY m.firm_id) t;
    RETURN NULL;
END
$$;

CREATE TRIGGER matches_period_guard_update
    AFTER UPDATE ON recon.matches
    REFERENCING NEW TABLE AS touched FOR EACH STATEMENT
    EXECUTE FUNCTION recon.matches_period_guard();

-- Document and residual allocations resolve to their match's lines.
CREATE FUNCTION recon.match_documents_period_guard() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    PERFORM recon.periods_lock_lines(t.firm_id, t.line_ids)
      FROM (SELECT d.firm_id, array_agg(DISTINCT ml.line_id) AS line_ids
              FROM touched d
              JOIN recon.match_lines ml
                ON ml.firm_id = d.firm_id AND ml.match_id = d.match_id
             GROUP BY d.firm_id) t;
    RETURN NULL;
END
$$;

CREATE TRIGGER match_documents_period_guard_insert
    AFTER INSERT ON recon.match_documents
    REFERENCING NEW TABLE AS touched FOR EACH STATEMENT
    EXECUTE FUNCTION recon.match_documents_period_guard();

CREATE TRIGGER match_documents_period_guard_delete
    AFTER DELETE ON recon.match_documents
    REFERENCING OLD TABLE AS touched FOR EACH STATEMENT
    EXECUTE FUNCTION recon.match_documents_period_guard();

CREATE FUNCTION recon.match_residuals_period_guard() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    PERFORM recon.periods_lock_lines(t.firm_id, t.line_ids)
      FROM (SELECT r.firm_id, array_agg(DISTINCT ml.line_id) AS line_ids
              FROM touched r
              JOIN recon.match_lines ml
                ON ml.firm_id = r.firm_id AND ml.match_id = r.match_id
             GROUP BY r.firm_id) t;
    RETURN NULL;
END
$$;

CREATE TRIGGER match_residuals_period_guard_insert
    AFTER INSERT ON recon.match_residuals
    REFERENCING NEW TABLE AS touched FOR EACH STATEMENT
    EXECUTE FUNCTION recon.match_residuals_period_guard();

CREATE TRIGGER match_residuals_period_guard_delete
    AFTER DELETE ON recon.match_residuals
    REFERENCING OLD TABLE AS touched FOR EACH STATEMENT
    EXECUTE FUNCTION recon.match_residuals_period_guard();

-- Proposals name their line directly; a proposal landing on a closed period's
-- line is refused like any other write.
CREATE FUNCTION recon.proposals_period_guard() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    PERFORM recon.periods_lock_lines(t.firm_id, t.line_ids)
      FROM (SELECT firm_id, array_agg(line_id) AS line_ids
              FROM touched GROUP BY firm_id) t;
    RETURN NULL;
END
$$;

CREATE TRIGGER proposals_period_guard_insert
    AFTER INSERT ON recon.proposals
    REFERENCING NEW TABLE AS touched FOR EACH STATEMENT
    EXECUTE FUNCTION recon.proposals_period_guard();

CREATE TRIGGER proposals_period_guard_update
    AFTER UPDATE ON recon.proposals
    REFERENCING NEW TABLE AS touched FOR EACH STATEMENT
    EXECUTE FUNCTION recon.proposals_period_guard();

CREATE TRIGGER proposals_period_guard_delete
    AFTER DELETE ON recon.proposals
    REFERENCING OLD TABLE AS touched FOR EACH STATEMENT
    EXECUTE FUNCTION recon.proposals_period_guard();

-- The audit spine itself: a line- or match-subject decision is a write on the
-- period too. Period/rule/import subjects are client-scoped, not line-scoped —
-- the close's own `closed` decision (subject `period`) stays free to land.
CREATE FUNCTION recon.decision_log_period_guard() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    PERFORM recon.periods_lock_lines(t.firm_id, t.line_ids)
      FROM (SELECT firm_id, array_agg(subject_id) AS line_ids
              FROM touched WHERE subject_kind = 'line'
             GROUP BY firm_id) t;
    PERFORM recon.periods_lock_lines(t.firm_id, t.line_ids)
      FROM (SELECT d.firm_id, array_agg(DISTINCT ml.line_id) AS line_ids
              FROM touched d
              JOIN recon.match_lines ml
                ON ml.firm_id = d.firm_id AND ml.match_id = d.subject_id
             WHERE d.subject_kind = 'match'
             GROUP BY d.firm_id) t;
    RETURN NULL;
END
$$;

CREATE TRIGGER decision_log_period_guard_insert
    AFTER INSERT ON recon.decision_log
    REFERENCING NEW TABLE AS touched FOR EACH STATEMENT
    EXECUTE FUNCTION recon.decision_log_period_guard();
