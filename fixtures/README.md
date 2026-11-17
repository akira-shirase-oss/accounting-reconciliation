# Fixtures

Nothing here is real client data, anonymized or not (D-028).

- **The seeded generator** is `recon-cli`'s `generate` module (`gen-v2`, D-061):
  `recon gen statement --seed N --lines N [--currency USD] [--start
  YYYY-MM-DD] [--days N]` writes a statement in the `r0-generic-csv` layout
  with running balances that continue row to row. The same seed always writes
  the same bytes; a day window of one spec repeats its rows byte for byte (the
  `r1-import` overlaps). Seeds in use: the kill drill `20261116` (200,000
  lines from 2026-03-01); the db-down drill `1`–`4`; `r0-import`
  `0x5EED0000 ^ (client << 8) ^ file`; `r1-import` `0x5EED1000 ^ (client << 8)`.
- **`hostile-files/`** — failure case 6 (D-066): each file, its expected
  refusal and row, in `hostile-files/README.md`.
- **`ofx/`** — a valid OFX 1.x SGML file (Windows-1252) and a valid OFX 2.x
  XML file, parsed by `recon-import`'s `ofx_fixtures` test.
- **Labelled sets, injection and wrong-match fixtures and recorded model
  responses** arrive with R2 and R4.
