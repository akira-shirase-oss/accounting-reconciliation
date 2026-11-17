-- Accounting Reconciliation, migration 4: what the import invariants also need (D-056).
--   * duplicates, like lines, are recorded only while their import is staging (RCI04);
--   * an import becomes complete, refused or discarded only with the decision that says so,
--     in the same transaction (RCI05, D-038).

CREATE FUNCTION recon.import_duplicates_inserted() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM new_rows n
          LEFT JOIN recon.imports i ON i.firm_id = n.firm_id AND i.id = n.import_id
         WHERE i.status IS DISTINCT FROM 'staging') THEN
        RAISE EXCEPTION 'duplicates may only be recorded while their import is staging'
            USING ERRCODE = 'RCI04';
    END IF;
    RETURN NULL;
END
$$;

CREATE TRIGGER import_duplicates_inserted
    AFTER INSERT ON recon.import_duplicates
    REFERENCING NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION recon.import_duplicates_inserted();

CREATE FUNCTION recon.import_has_its_decision() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM recon.decision_log d
         WHERE d.firm_id = NEW.firm_id AND d.client_id = NEW.client_id
           AND d.subject_kind = 'import' AND d.subject_id = NEW.id AND d.to_state = NEW.status) THEN
        RAISE EXCEPTION 'import % became % without its decision', NEW.id, NEW.status
            USING ERRCODE = 'RCI05';
    END IF;
    RETURN NULL;
END
$$;

CREATE CONSTRAINT TRIGGER imports_have_their_decision
    AFTER UPDATE ON recon.imports
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    WHEN (NEW.status <> 'staging')
    EXECUTE FUNCTION recon.import_has_its_decision();
