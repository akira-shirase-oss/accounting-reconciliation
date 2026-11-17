-- Accounting Reconciliation, migration 2: a chain head always names a real record (D-046).
--
-- The chain trigger runs BEFORE INSERT (it must: it assigns seq, prev_hash, decided_at and
-- record_hash, and a multi-row INSERT needs each row to see the previous row's head).
-- PostgreSQL fires BEFORE ROW triggers before an ON CONFLICT arbiter check, and a skipped
-- row does not undo the trigger's head update: INSERT ... ON CONFLICT DO NOTHING with an
-- existing decision id inserted nothing and moved the head to a record that did not exist.
-- Two guards close that:
--   * the trigger refuses an id the firm has already recorded (RCA03), before touching the
--     head - the common case, refused by name;
--   * a deferred constraint trigger on chain_heads refuses the commit unless the head names
--     a decision that exists with that seq and hash (RCA04) - every other path, including a
--     duplicate id that another transaction commits while this one waits on ON CONFLICT,
--     and the owner moving a head by hand.

CREATE OR REPLACE FUNCTION recon.decision_log_chain() RETURNS trigger
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
    -- After the head lock: a same-client duplicate committed meanwhile is visible here.
    IF EXISTS (SELECT 1 FROM recon.decision_log d WHERE d.firm_id = NEW.firm_id AND d.id = NEW.id) THEN
        RAISE EXCEPTION 'decision % is already recorded', NEW.id
            USING ERRCODE = 'RCA03', HINT = 'a decision is appended once; ON CONFLICT cannot skip one';
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

-- At commit, for every head update: the head names a decision with that seq and hash.
CREATE FUNCTION recon.chain_head_names_a_record() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM recon.decision_log d
         WHERE d.firm_id = NEW.firm_id AND d.client_id = NEW.client_id
           AND d.seq = NEW.seq AND d.record_hash = NEW.head_hash) THEN
        RAISE EXCEPTION 'the decision chain head names no recorded decision (seq %)', NEW.seq
            USING ERRCODE = 'RCA04';
    END IF;
    RETURN NULL;
END
$$;

CREATE CONSTRAINT TRIGGER chain_head_names_a_record
    AFTER UPDATE ON recon.chain_heads
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION recon.chain_head_names_a_record();

-- A head is created at seq 0 with the genesis hash and never deleted.
CREATE FUNCTION recon.chain_heads_guard() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF TG_OP <> 'UPDATE' OR NEW.firm_id IS DISTINCT FROM OLD.firm_id
       OR NEW.client_id IS DISTINCT FROM OLD.client_id OR NEW.seq <= OLD.seq THEN
        RAISE EXCEPTION 'a decision chain head only moves forward (% refused)', TG_OP
            USING ERRCODE = 'RCA04';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER chain_heads_forward_only
    BEFORE UPDATE OR DELETE ON recon.chain_heads
    FOR EACH ROW EXECUTE FUNCTION recon.chain_heads_guard();
CREATE TRIGGER chain_heads_no_truncate
    BEFORE TRUNCATE ON recon.chain_heads
    FOR EACH STATEMENT EXECUTE FUNCTION recon.chain_heads_guard();
