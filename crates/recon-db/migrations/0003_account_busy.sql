-- Accounting Reconciliation, migration 3: an import refused because another flip holds its
-- account is discarded with its own reason (D-053), instead of waiting out a lock timeout.
ALTER TABLE recon.imports DROP CONSTRAINT imports_discard_reason_check;
ALTER TABLE recon.imports ADD CONSTRAINT imports_discard_reason_check
    CHECK (discard_reason IN ('duplicate_file', 'lease_expired', 'upload_failed', 'account_busy'));
