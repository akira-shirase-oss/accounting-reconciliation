-- 0013's periods_lock_lines ran only because functions default to EXECUTE for
-- PUBLIC (the per-schema default-privilege revoke does not remove it). Grant the
-- runtime role by name and take the default away, so the grant is the reason it works.
GRANT EXECUTE ON FUNCTION recon.periods_lock_lines(uuid, uuid[]) TO recon_runtime;
REVOKE EXECUTE ON FUNCTION recon.periods_lock_lines(uuid, uuid[]) FROM PUBLIC;

-- The decision paths lock the client's chain head before the period row (D-077).
-- chain_heads is not granted to the runtime role, so the lock lives in a
-- definer function: EXECUTE is the only privilege the caller needs.
CREATE FUNCTION recon.chain_heads_lock(p_firm uuid, p_client uuid)
RETURNS void
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    PERFORM 1 FROM recon.chain_heads
      WHERE firm_id = p_firm AND client_id = p_client
      FOR UPDATE;
END;
$$;
GRANT EXECUTE ON FUNCTION recon.chain_heads_lock(uuid, uuid) TO recon_runtime;
REVOKE EXECUTE ON FUNCTION recon.chain_heads_lock(uuid, uuid) FROM PUBLIC;
