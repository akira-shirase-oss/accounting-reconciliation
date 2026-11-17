-- Accounting Reconciliation, migration 6: an import may carry the name of the file it read,
-- so a refusal report can say which file (D-068). Text the uploader chose: shown back in
-- reports, never logged (D-025), never part of the file's identity (its SHA-256 is).

ALTER TABLE recon.imports
    ADD COLUMN file_name text CHECK (char_length(file_name) BETWEEN 1 AND 255);

CREATE OR REPLACE FUNCTION recon.imports_guard() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    n_lines bigint;
    n_dups bigint;
BEGIN
    IF NOT (OLD.status = 'staging'
            OR (OLD.status = 'revision' AND NEW.status IN ('complete', 'discarded'))) THEN
        RAISE EXCEPTION 'import is final (%)', OLD.status USING ERRCODE = 'RCI01';
    END IF;
    IF (NEW.firm_id, NEW.id, NEW.client_id, NEW.account_id, NEW.currency, NEW.profile,
        NEW.profile_version, NEW.lease_token, NEW.created_at, NEW.kind, NEW.profile_sha256,
        NEW.file_name)
       IS DISTINCT FROM
       (OLD.firm_id, OLD.id, OLD.client_id, OLD.account_id, OLD.currency, OLD.profile,
        OLD.profile_version, OLD.lease_token, OLD.created_at, OLD.kind, OLD.profile_sha256,
        OLD.file_name) THEN
        RAISE EXCEPTION 'an import''s identity never changes' USING ERRCODE = 'RCI03';
    END IF;
    IF NEW.status = 'staging' THEN
        -- A lease renewal or a staged batch.
        RETURN NEW;
    END IF;
    IF NEW.kind = 'documents' THEN
        SELECT count(*) INTO n_lines FROM recon.documents
         WHERE firm_id = NEW.firm_id AND import_id = NEW.id;
        SELECT count(*) INTO n_dups FROM recon.document_duplicates
         WHERE firm_id = NEW.firm_id AND import_id = NEW.id;
    ELSE
        SELECT count(*) INTO n_lines FROM recon.statement_lines
         WHERE firm_id = NEW.firm_id AND import_id = NEW.id;
        SELECT count(*) INTO n_dups FROM recon.import_duplicates
         WHERE firm_id = NEW.firm_id AND import_id = NEW.id;
    END IF;
    IF NEW.status = 'complete' AND (n_lines <> NEW.lines_created OR n_dups <> NEW.duplicates) THEN
        RAISE EXCEPTION 'import counts do not match its rows' USING ERRCODE = 'RCI02';
    END IF;
    IF NEW.status IN ('refused', 'discarded', 'revision') AND (n_lines <> 0 OR n_dups <> 0) THEN
        RAISE EXCEPTION 'a refused, discarded or held import has no lines' USING ERRCODE = 'RCI02';
    END IF;
    NEW.finished_at := pg_catalog.now();
    RETURN NEW;
END
$$;
