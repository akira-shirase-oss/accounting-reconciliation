-- 0009_review.sql — R3 review API (D-018, D-032).
--
-- The runtime role may release a finished match's document allocations: the
-- match_documents trigger already confines DELETE to a `rejected`/`reversed`
-- match, so the grant only opens the intended release path, never a live one.

GRANT DELETE ON recon.match_documents TO recon_runtime;
