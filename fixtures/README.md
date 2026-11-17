# Fixtures

Nothing here is real client data, anonymized or not (D-028).

- **The seeded generator** is `recon-cli`'s `generate` module (`gen-v1`):
  `recon gen statement --seed N --lines N [--currency USD] [--start
  YYYY-MM-DD] [--days N]` writes a statement in the R0 profile. The same seed
  always writes the same bytes. Seeds in use: the kill drill `20261116`
  (200,000 lines from 2026-03-01); the db-down drill `1`–`4`; the load run
  `0x5EED0000 ^ (client << 8) ^ file`.
- **Hostile files, labelled sets, injection and wrong-match fixtures and
  recorded model responses** arrive with R1, R2 and R4. R0's malformed-row and
  bound cases are inline in `recon-import`'s tests.
