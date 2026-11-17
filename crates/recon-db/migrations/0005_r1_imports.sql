-- Accounting Reconciliation, migration 5: R1's imports (D-062 to D-065).
--
--   * import profiles: per firm, immutable per version (D-062);
--   * an import is a statement into an account or documents of a client, records its
--     profile's SHA-256, its date range and the overlap it shares with earlier imports;
--   * a statement that disagrees with stored lines in their shared range is held as a
--     `revision`: its rows kept in a logged table, every difference listed, until a person
--     accepts it (superseding the replaced lines, each with its decision) or rejects it
--     (D-009, D-065);
--   * lines are unique per fingerprint among lines that are not superseded; a line's status
--     changes only with its own decision in the same transaction, and `superseded` is final;
--   * documents: staged, published whole by their import's flip, unique per fingerprint and
--     per number (D-064).
--
-- New SQLSTATEs: RCL04 line status changed without its decision, RCI06 held rows or revision
-- changes for an import that is not staging or held, RCP01 profiles are immutable, RCD01
-- documents for an import that is not staging, RCD02 documents are immutable.

-- ---------------------------------------------------------------------------------------
-- Profiles.

CREATE TABLE recon.import_profiles (
    firm_id uuid NOT NULL REFERENCES recon.firms (id),
    name text NOT NULL CHECK (name ~ '^[a-z][a-z0-9-]{0,62}$'
        AND name NOT IN ('r0-generic-csv', 'ofx', 'r1-generic-documents')),
    version integer NOT NULL CHECK (version BETWEEN 1 AND 1000),
    kind text NOT NULL CHECK (kind IN ('statement_csv', 'documents_csv')),
    definition jsonb NOT NULL CHECK (
        jsonb_typeof(definition) = 'object' AND octet_length(definition::text) <= 65536),
    sha256 bytea NOT NULL CHECK (octet_length(sha256) = 32),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (firm_id, name, version)
);

CREATE FUNCTION recon.import_profiles_immutable() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    RAISE EXCEPTION 'import profiles are immutable: % refused', TG_OP
        USING ERRCODE = 'RCP01', HINT = 'create a new version';
END
$$;

CREATE TRIGGER import_profiles_immutable
    BEFORE UPDATE OR DELETE ON recon.import_profiles
    FOR EACH ROW EXECUTE FUNCTION recon.import_profiles_immutable();
CREATE TRIGGER import_profiles_no_truncate
    BEFORE TRUNCATE ON recon.import_profiles
    FOR EACH STATEMENT EXECUTE FUNCTION recon.import_profiles_immutable();

-- ---------------------------------------------------------------------------------------
-- Imports: kind, profile identity, ranges, overlap, revisions.

ALTER TABLE recon.imports
    ADD COLUMN kind text NOT NULL DEFAULT 'statement' CHECK (kind IN ('statement', 'documents')),
    ADD COLUMN profile_sha256 bytea CHECK (octet_length(profile_sha256) = 32),
    ADD COLUMN first_date date,
    ADD COLUMN last_date date,
    ADD COLUMN overlap_first date,
    ADD COLUMN overlap_last date,
    ADD COLUMN overlap_rows integer CHECK (overlap_rows >= 0),
    ADD COLUMN question jsonb CHECK (question IS NULL OR jsonb_typeof(question) = 'object'),
    ADD COLUMN changes integer CHECK (changes >= 0),
    ADD COLUMN revision_digest bytea CHECK (octet_length(revision_digest) = 32);

ALTER TABLE recon.imports ALTER COLUMN account_id DROP NOT NULL;
ALTER TABLE recon.imports ALTER COLUMN currency DROP NOT NULL;
ALTER TABLE recon.imports ADD CONSTRAINT imports_kind_target CHECK (
    (kind = 'statement') = (account_id IS NOT NULL AND currency IS NOT NULL)
    AND (kind = 'documents') = (account_id IS NULL AND currency IS NULL));
-- A document import names its client directly (a statement import's account names it).
ALTER TABLE recon.imports ADD CONSTRAINT imports_client_fk
    FOREIGN KEY (firm_id, client_id) REFERENCES recon.clients (firm_id, id);
ALTER TABLE recon.imports ADD CONSTRAINT imports_kind_key UNIQUE (firm_id, id, client_id, kind);

ALTER TABLE recon.imports DROP CONSTRAINT imports_profile_check;
ALTER TABLE recon.imports ADD CONSTRAINT imports_profile_check
    CHECK (profile ~ '^[a-z][a-z0-9-]{0,62}$');
ALTER TABLE recon.imports DROP CONSTRAINT imports_status_check;
ALTER TABLE recon.imports ADD CONSTRAINT imports_status_check
    CHECK (status IN ('staging', 'complete', 'refused', 'discarded', 'revision'));
ALTER TABLE recon.imports DROP CONSTRAINT imports_discard_reason_check;
ALTER TABLE recon.imports ADD CONSTRAINT imports_discard_reason_check
    CHECK (discard_reason IN ('duplicate_file', 'lease_expired', 'upload_failed', 'account_busy',
                              'revision_rejected'));
-- A held revision says how many differences it lists and pins them by digest.
ALTER TABLE recon.imports ADD CONSTRAINT imports_revision_listed CHECK (
    status <> 'revision' OR (changes >= 1 AND revision_digest IS NOT NULL AND file_hash IS NOT NULL
                             AND rows_read IS NOT NULL));
-- A complete statement import knows the dates it covers.
ALTER TABLE recon.imports ADD CONSTRAINT imports_complete_range CHECK (
    status <> 'complete' OR kind <> 'statement'
    OR (first_date IS NOT NULL AND last_date IS NOT NULL AND first_date <= last_date)) NOT VALID;

-- The same document file is complete at most once per client; a held revision of a file
-- answers for it while it is held.
CREATE UNIQUE INDEX imports_one_complete_document_file
    ON recon.imports (firm_id, client_id, file_hash) WHERE status = 'complete' AND kind = 'documents';
CREATE UNIQUE INDEX imports_one_held_revision_per_file
    ON recon.imports (firm_id, account_id, file_hash) WHERE status = 'revision';

-- R0's complete imports get their date ranges (overlap detection reads them). The guard
-- refuses any change to a final import, so it is off for this one statement only.
ALTER TABLE recon.imports DISABLE TRIGGER imports_guard;
UPDATE recon.imports i
   SET first_date = r.lo, last_date = r.hi
  FROM (SELECT u.firm_id, u.import_id, min(u.d) AS lo, max(u.d) AS hi
          FROM (SELECT l.firm_id, l.import_id, l.booking_date AS d FROM recon.statement_lines l
                UNION ALL
                SELECT x.firm_id, x.import_id, l.booking_date
                  FROM recon.import_duplicates x
                  JOIN recon.statement_lines l ON l.firm_id = x.firm_id AND l.id = x.existing_line_id) u
         GROUP BY u.firm_id, u.import_id) r
 WHERE i.firm_id = r.firm_id AND i.id = r.import_id AND i.status = 'complete';
ALTER TABLE recon.imports ENABLE TRIGGER imports_guard;

-- A staging import moves to any final state or is held as a revision; a held revision only
-- becomes complete (accepted) or discarded (rejected). Identity columns never change.
-- `complete` requires the import's lines (or documents) and duplicates to be exactly the
-- counts it claims.
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
        NEW.profile_version, NEW.lease_token, NEW.created_at, NEW.kind, NEW.profile_sha256)
       IS DISTINCT FROM
       (OLD.firm_id, OLD.id, OLD.client_id, OLD.account_id, OLD.currency, OLD.profile,
        OLD.profile_version, OLD.lease_token, OLD.created_at, OLD.kind, OLD.profile_sha256) THEN
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

-- Lines and duplicates are inserted while their import flips: from staging, or from a held
-- revision being accepted.
CREATE OR REPLACE FUNCTION recon.statement_lines_inserted() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM new_lines n
          LEFT JOIN recon.imports i ON i.firm_id = n.firm_id AND i.id = n.import_id
         WHERE i.status IS DISTINCT FROM 'staging' AND i.status IS DISTINCT FROM 'revision') THEN
        RAISE EXCEPTION 'lines may only be inserted while their import is staging or held'
            USING ERRCODE = 'RCL01';
    END IF;
    INSERT INTO recon.import_flips (firm_id, import_id)
    SELECT DISTINCT n.firm_id, n.import_id FROM new_lines n
    ON CONFLICT DO NOTHING;
    RETURN NULL;
END
$$;

CREATE OR REPLACE FUNCTION recon.import_duplicates_inserted() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM new_rows n
          LEFT JOIN recon.imports i ON i.firm_id = n.firm_id AND i.id = n.import_id
         WHERE i.status IS DISTINCT FROM 'staging' AND i.status IS DISTINCT FROM 'revision') THEN
        RAISE EXCEPTION 'duplicates may only be recorded while their import is staging or held'
            USING ERRCODE = 'RCI04';
    END IF;
    RETURN NULL;
END
$$;

-- ---------------------------------------------------------------------------------------
-- Statement lines: OFX rows are numbered from 1; fingerprints are unique among live lines;
-- a status change carries its decision; superseded is final.

ALTER TABLE recon.statement_lines DROP CONSTRAINT statement_lines_row_no_check;
ALTER TABLE recon.statement_lines ADD CONSTRAINT statement_lines_row_no_check
    CHECK (row_no BETWEEN 1 AND 2000001);
ALTER TABLE recon.statement_lines DROP CONSTRAINT statement_lines_firm_id_account_id_fingerprint_key;
CREATE UNIQUE INDEX statement_lines_live_fingerprint
    ON recon.statement_lines (firm_id, account_id, fingerprint) WHERE status <> 'superseded';
CREATE INDEX statement_lines_by_account_date
    ON recon.statement_lines (firm_id, account_id, booking_date);

CREATE OR REPLACE FUNCTION recon.statement_lines_guard() RETURNS trigger
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
    IF OLD.status = 'superseded' THEN
        RAISE EXCEPTION 'a superseded line is final' USING ERRCODE = 'RCL03';
    END IF;
    IF NEW.status IS DISTINCT FROM OLD.status AND NEW.row_version <> OLD.row_version + 1 THEN
        RAISE EXCEPTION 'a status change advances the row version by one' USING ERRCODE = 'RCL03';
    END IF;
    RETURN NEW;
END
$$;

-- At commit: a line whose status changed has a decision about it, to that state, made in this
-- transaction (the chain trigger stamps decided_at with the transaction's now()).
CREATE FUNCTION recon.statement_line_has_its_decision() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM recon.decision_log d
         WHERE d.firm_id = NEW.firm_id AND d.client_id = NEW.client_id
           AND d.subject_kind = 'line' AND d.subject_id = NEW.id
           AND d.to_state = NEW.status AND d.decided_at = pg_catalog.now()) THEN
        RAISE EXCEPTION 'line % became % without its decision', NEW.id, NEW.status
            USING ERRCODE = 'RCL04';
    END IF;
    RETURN NULL;
END
$$;

CREATE CONSTRAINT TRIGGER statement_lines_have_their_decision
    AFTER UPDATE ON recon.statement_lines
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    WHEN (NEW.status IS DISTINCT FROM OLD.status)
    EXECUTE FUNCTION recon.statement_line_has_its_decision();

-- ---------------------------------------------------------------------------------------
-- Held revisions: the rows, and the differences they list.

CREATE TABLE recon.import_held_rows (
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
    PRIMARY KEY (firm_id, import_id, row_no),
    FOREIGN KEY (firm_id, import_id) REFERENCES recon.imports (firm_id, id)
);

CREATE TABLE recon.import_revision_changes (
    firm_id uuid NOT NULL,
    import_id uuid NOT NULL,
    ordinal integer NOT NULL CHECK (ordinal >= 1),
    change text NOT NULL CHECK (change IN ('added', 'removed', 'changed')),
    -- The new file's row (added, changed) and the stored line (removed, changed).
    row_no integer,
    line_id uuid,
    -- For a change: the fields that differ.
    fields text[] CHECK (fields <@ ARRAY['amount', 'narration', 'reference', 'value_date',
                                         'counterparty', 'balance_after']::text[]),
    PRIMARY KEY (firm_id, import_id, ordinal),
    FOREIGN KEY (firm_id, import_id) REFERENCES recon.imports (firm_id, id),
    FOREIGN KEY (firm_id, line_id) REFERENCES recon.statement_lines (firm_id, id),
    CHECK ((change = 'added') = (line_id IS NULL)),
    CHECK ((change = 'removed') = (row_no IS NULL)),
    CHECK ((change = 'changed') = (fields IS NOT NULL AND cardinality(fields) >= 1))
);

-- Held rows and listed changes are written only while their import is staging (being held);
-- held rows are deleted only while it is held (being accepted or rejected); listed changes
-- are never updated or deleted.
CREATE FUNCTION recon.revision_rows_inserted() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM new_rows n
          LEFT JOIN recon.imports i ON i.firm_id = n.firm_id AND i.id = n.import_id
         WHERE i.status IS DISTINCT FROM 'staging') THEN
        RAISE EXCEPTION '% rows may only be written while their import is staging', TG_TABLE_NAME
            USING ERRCODE = 'RCI06';
    END IF;
    RETURN NULL;
END
$$;

CREATE FUNCTION recon.revision_rows_deleted() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM old_rows o
          LEFT JOIN recon.imports i ON i.firm_id = o.firm_id AND i.id = o.import_id
         WHERE i.status IS DISTINCT FROM 'revision') THEN
        RAISE EXCEPTION 'held rows are deleted only while their revision is decided'
            USING ERRCODE = 'RCI06';
    END IF;
    RETURN NULL;
END
$$;

CREATE FUNCTION recon.revision_rows_refused() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    RAISE EXCEPTION '% on % refused: revisions are kept', TG_OP, TG_TABLE_NAME
        USING ERRCODE = 'RCI06';
END
$$;

CREATE TRIGGER import_held_rows_inserted
    AFTER INSERT ON recon.import_held_rows
    REFERENCING NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION recon.revision_rows_inserted();
CREATE TRIGGER import_held_rows_deleted
    AFTER DELETE ON recon.import_held_rows
    REFERENCING OLD TABLE AS old_rows
    FOR EACH STATEMENT EXECUTE FUNCTION recon.revision_rows_deleted();
CREATE TRIGGER import_held_rows_no_update
    BEFORE UPDATE OR TRUNCATE ON recon.import_held_rows
    FOR EACH STATEMENT EXECUTE FUNCTION recon.revision_rows_refused();
CREATE TRIGGER import_revision_changes_inserted
    AFTER INSERT ON recon.import_revision_changes
    REFERENCING NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION recon.revision_rows_inserted();
CREATE TRIGGER import_revision_changes_kept
    BEFORE UPDATE OR DELETE OR TRUNCATE ON recon.import_revision_changes
    FOR EACH STATEMENT EXECUTE FUNCTION recon.revision_rows_refused();

-- ---------------------------------------------------------------------------------------
-- Documents (D-064).

CREATE UNLOGGED TABLE recon.document_staging (
    firm_id uuid NOT NULL,
    import_id uuid NOT NULL,
    row_no integer NOT NULL,
    kind text NOT NULL,
    number_raw text,
    number_norm text,
    issue_date date NOT NULL,
    due_date date,
    amount_minor bigint NOT NULL,
    currency text NOT NULL,
    counterparty_raw text,
    counterparty_norm text,
    description text,
    tuple_hash bytea NOT NULL,
    PRIMARY KEY (firm_id, import_id, row_no)
);

CREATE TABLE recon.documents (
    firm_id uuid NOT NULL,
    id uuid NOT NULL DEFAULT uuidv7(),
    client_id uuid NOT NULL,
    import_id uuid NOT NULL,
    import_kind text NOT NULL DEFAULT 'documents' CHECK (import_kind = 'documents'),
    row_no integer NOT NULL CHECK (row_no BETWEEN 2 AND 2000001),
    kind text NOT NULL CHECK (
        kind IN ('sales_invoice', 'purchase_bill', 'receipt', 'credit_note', 'book_entry')),
    number_raw text CHECK (octet_length(number_raw) BETWEEN 1 AND 8192),
    number_norm text CHECK (octet_length(number_norm) BETWEEN 1 AND 8192),
    issue_date date NOT NULL,
    due_date date,
    amount_minor bigint NOT NULL
        CHECK (amount_minor <> 0 AND amount_minor BETWEEN -1000000000000 AND 1000000000000),
    currency text NOT NULL CHECK (
        currency IN ('INR', 'USD', 'EUR', 'GBP', 'AED', 'SGD', 'AUD', 'CAD', 'JPY', 'KWD')),
    counterparty_raw text CHECK (octet_length(counterparty_raw) BETWEEN 1 AND 8192),
    counterparty_norm text CHECK (octet_length(counterparty_norm) BETWEEN 1 AND 8192),
    description text CHECK (octet_length(description) BETWEEN 1 AND 8192),
    tuple_hash bytea NOT NULL CHECK (octet_length(tuple_hash) = 32),
    occurrence integer NOT NULL CHECK (occurrence >= 0),
    fingerprint bytea NOT NULL CHECK (octet_length(fingerprint) = 32),
    row_version bigint NOT NULL DEFAULT 1 CHECK (row_version >= 1),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (firm_id, id),
    UNIQUE (firm_id, client_id, fingerprint),
    UNIQUE (firm_id, import_id, row_no),
    CHECK ((number_raw IS NULL) = (number_norm IS NULL)),
    CHECK (kind NOT IN ('sales_invoice', 'purchase_bill', 'credit_note') OR number_norm IS NOT NULL),
    FOREIGN KEY (firm_id, import_id, client_id, import_kind)
        REFERENCES recon.imports (firm_id, id, client_id, kind)
);

-- A number names one document per client, kind and counterparty (D-064).
CREATE UNIQUE INDEX documents_one_per_number
    ON recon.documents (firm_id, client_id, kind, coalesce(counterparty_norm, ''), number_norm)
    WHERE number_norm IS NOT NULL;

CREATE TABLE recon.document_duplicates (
    firm_id uuid NOT NULL,
    import_id uuid NOT NULL,
    row_no integer NOT NULL,
    existing_document_id uuid NOT NULL,
    PRIMARY KEY (firm_id, import_id, row_no),
    FOREIGN KEY (firm_id, import_id) REFERENCES recon.imports (firm_id, id),
    FOREIGN KEY (firm_id, existing_document_id) REFERENCES recon.documents (firm_id, id)
);

CREATE FUNCTION recon.documents_inserted() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM new_rows n
          LEFT JOIN recon.imports i ON i.firm_id = n.firm_id AND i.id = n.import_id
         WHERE i.status IS DISTINCT FROM 'staging') THEN
        RAISE EXCEPTION 'documents may only be inserted while their import is staging'
            USING ERRCODE = 'RCD01';
    END IF;
    IF TG_TABLE_NAME = 'documents' THEN
        INSERT INTO recon.import_flips (firm_id, import_id)
        SELECT DISTINCT n.firm_id, n.import_id FROM new_rows n
        ON CONFLICT DO NOTHING;
    END IF;
    RETURN NULL;
END
$$;

CREATE TRIGGER documents_inserted
    AFTER INSERT ON recon.documents
    REFERENCING NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION recon.documents_inserted();
CREATE TRIGGER document_duplicates_inserted
    AFTER INSERT ON recon.document_duplicates
    REFERENCING NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION recon.documents_inserted();

CREATE FUNCTION recon.documents_immutable() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    RAISE EXCEPTION '% on % refused: documents are immutable in R1', TG_OP, TG_TABLE_NAME
        USING ERRCODE = 'RCD02';
END
$$;

CREATE TRIGGER documents_immutable
    BEFORE UPDATE OR DELETE ON recon.documents
    FOR EACH ROW EXECUTE FUNCTION recon.documents_immutable();
CREATE TRIGGER documents_no_truncate
    BEFORE TRUNCATE ON recon.documents
    FOR EACH STATEMENT EXECUTE FUNCTION recon.documents_immutable();
CREATE TRIGGER document_duplicates_immutable
    BEFORE UPDATE OR DELETE ON recon.document_duplicates
    FOR EACH ROW EXECUTE FUNCTION recon.documents_immutable();
CREATE TRIGGER document_duplicates_no_truncate
    BEFORE TRUNCATE ON recon.document_duplicates
    FOR EACH STATEMENT EXECUTE FUNCTION recon.documents_immutable();

-- ---------------------------------------------------------------------------------------
-- The runtime role's new grants (D-037).

GRANT SELECT, INSERT ON recon.import_profiles TO recon_runtime;
GRANT SELECT, INSERT, DELETE ON recon.import_held_rows TO recon_runtime;
GRANT SELECT, INSERT ON recon.import_revision_changes TO recon_runtime;
GRANT SELECT, INSERT, DELETE ON recon.document_staging TO recon_runtime;
GRANT SELECT, INSERT ON recon.documents, recon.document_duplicates TO recon_runtime;
GRANT UPDATE (status, row_version) ON recon.statement_lines TO recon_runtime;
GRANT UPDATE (first_date, last_date, overlap_first, overlap_last, overlap_rows, question,
              changes, revision_digest)
    ON recon.imports TO recon_runtime;
