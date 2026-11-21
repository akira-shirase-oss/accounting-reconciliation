-- S1-1 (D-076): the close/write serialization. A statement that writes
-- statement_lines upserts the touched period rows and locks them FOR KEY SHARE
-- before testing status, so a concurrent close — holding the period row FOR
-- UPDATE — either waits on this write or is itself seen; under READ COMMITTED
-- the post-wait check sees the committed truth. A closed month refuses the
-- whole statement. One lock set per statement, in a fixed order.

DROP TRIGGER statement_lines_period_guard ON recon.statement_lines;
DROP TRIGGER statement_lines_period_guard_status ON recon.statement_lines;
DROP FUNCTION recon.statement_lines_period_guard();

CREATE FUNCTION recon.statement_lines_period_guard() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    closed_months integer;
BEGIN
    -- A period row exists for every month this statement touches: a backdated
    -- import creates the month it writes, so a concurrent close can lock it.
    -- Deterministic order keeps two writers from blocking on each other.
    INSERT INTO recon.periods (firm_id, client_id, account_id, month)
    SELECT DISTINCT n.firm_id, n.client_id, n.account_id,
                    date_trunc('month', n.booking_date)::date
      FROM new_rows n
     ORDER BY n.firm_id, n.client_id, n.account_id,
              date_trunc('month', n.booking_date)::date
    ON CONFLICT DO NOTHING;
    -- Lock the touched periods FOR KEY SHARE in a fixed order — waits on a
    -- close's FOR UPDATE, then the plain test below sees the post-commit status.
    PERFORM p.id
      FROM recon.periods p
     WHERE (p.firm_id, p.client_id, p.account_id, p.month) IN (
         SELECT DISTINCT n.firm_id, n.client_id, n.account_id,
                         date_trunc('month', n.booking_date)::date
           FROM new_rows n)
      ORDER BY p.firm_id, p.client_id, p.account_id, p.month
      FOR KEY SHARE OF p;
    SELECT count(*) INTO closed_months
      FROM recon.periods p
     WHERE (p.firm_id, p.client_id, p.account_id, p.month) IN (
         SELECT DISTINCT n.firm_id, n.client_id, n.account_id,
                         date_trunc('month', n.booking_date)::date
           FROM new_rows n)
       AND p.status = 'closed';
    IF closed_months > 0 THEN
        RAISE EXCEPTION 'the line''s period is closed' USING ERRCODE = 'RCP02';
    END IF;
    RETURN NULL;
END
$$;

CREATE TRIGGER statement_lines_period_guard
    AFTER INSERT ON recon.statement_lines
    REFERENCING NEW TABLE AS new_rows
    FOR EACH STATEMENT
    EXECUTE FUNCTION recon.statement_lines_period_guard();

CREATE TRIGGER statement_lines_period_guard_status
    AFTER UPDATE ON recon.statement_lines
    REFERENCING NEW TABLE AS new_rows
    FOR EACH STATEMENT
    EXECUTE FUNCTION recon.statement_lines_period_guard();
