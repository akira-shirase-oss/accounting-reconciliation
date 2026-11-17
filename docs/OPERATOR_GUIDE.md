# Operator guide

How to install, run, feed, upgrade, back up and restore Accounting
Reconciliation, and what to do when something is refused. Everything here is
exercised end to end by `scripts/production-smoke.sh`, which CI runs on every
push (job *production path*, D-069).

Status: imports (R1) work; matching, review and sign-in arrive in R2 and R3.
Until R3 every `/v1` request carries one operator token, so run one instance
per firm (D-030) and keep the token with the operator.

## What you run

| Piece | What it is |
|---|---|
| `recon-server` | the API, one process, loopback only; writes nothing to disk |
| PostgreSQL 18 | the only state: imports, lines, documents, the decision log |
| a reverse proxy | TLS and the public address; `deploy/Caddyfile` is the tested example |
| `recon` | the command line: database setup and migrations, audit verify, anchor and replay, and the import checks accountants run before an upload |
| the anchor file | signed chain heads (D-049), kept off the database host |

## Install

Build on a machine with rustup (the toolchain is pinned by
`rust-toolchain.toml`):

```sh
cargo build --release --locked -p recon-server -p recon-cli
install -m 0755 target/release/recon-server target/release/recon /usr/local/bin/
```

Create the service account and its configuration directory:

```sh
useradd --system --no-create-home --shell /usr/sbin/nologin recon
install -d -m 0750 -o root -g recon /etc/recon
install -m 0640 -o root -g recon deploy/recon.env.example /etc/recon/recon.env
```

Edit `/etc/recon/recon.env`: every setting is described in the file. Two
must change: `RECON_DATABASE_URL` (the `recon_app` role, with TLS) and
`RECON_OPERATOR_TOKEN` (`openssl rand -hex 32`). The server checks every
value at start and refuses a wrong one by name.

## The database

Use PostgreSQL 18 on an encrypted volume (D-022), with TLS. With an
administrator that may create roles and databases:

```sh
export RECON_ADMIN_DATABASE_URL='postgres://ADMIN:PASSWORD@DB-HOST:5432/postgres?sslmode=require'
export RECON_APP_PASSWORD='the password RECON_DATABASE_URL carries'
recon db setup --database recon     # roles recon_owner (cannot log in), recon_runtime, recon_app
recon db migrate --database recon   # as the owner; safe to run again
```

The server logs in only as `recon_app`, which may read and insert, update an
import's progress and a line's status (with its decision), and delete staged
rows, nothing else (D-037, D-065). It refuses to start as the owner or a
superuser.

## First run

```sh
install -m 0644 deploy/recon-server.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now recon-server
curl -fsS http://127.0.0.1:62480/readyz     # {"ready":true}
journalctl -u recon-server -f               # ids, codes, counts; never file content
```

Then put the proxy in front: copy `deploy/Caddyfile` to
`/etc/caddy/Caddyfile`, replace `recon.example.com` with your host name, run
`caddy validate --config /etc/caddy/Caddyfile`, and reload Caddy. It obtains
the TLS certificate, bounds bodies at 51 MB, streams uploads through, and
keeps the token and file names out of its access log.

If the service stops at once with status 78, the journal says why: a setting,
the database role, or a schema older or newer than the binary (run `recon db
migrate`). systemd does not restart it, because restarting cannot fix that.

## Importing a first statement

Each request below sends `-H "Authorization: Bearer $TOKEN"`; `$API` is your
proxy's address. `examples/` has the files.

1. **Create the firm, a client and its bank account** (currency from the D-005
   table):

   ```sh
   curl -s -X POST "$API/v1/firms" -H "Authorization: Bearer $TOKEN" \
     -H 'content-type: application/json' -d '{"name":"Your firm"}'
   curl -s -X POST "$API/v1/firms/$FIRM/clients" ... -d '{"name":"Client name"}'
   curl -s -X POST "$API/v1/firms/$FIRM/clients/$CLIENT/accounts" ... \
     -d '{"name":"Savings","currency":"INR"}'
   ```

2. **Describe the bank's export once, as a profile.** Start from
   `examples/profiles/indian-bank-savings.json`: the exact header, which
   column is which, the date format, the number format, and whether the bank
   lists newest first. Check it, then store it:

   ```sh
   recon profile check my-bank.json
   curl -s -X POST "$API/v1/firms/$FIRM/profiles" ... --data-binary @my-bank.json
   ```

   Leave `"order"` out of `date_format` if you do not know whether the bank
   writes day-month or month-day: the first file will be refused with the
   question and the order every date fits (`ambiguous_date_format`); store a
   new version with that order.

3. **Check the file before uploading** (no server needed):

   ```sh
   recon import check march.csv --profile my-bank.json --currency INR
   ```

   It prints what would be imported (rows, dates, money in and out exactly),
   or every refused line with its column, what is wrong and how to fix it.

4. **Upload it**, naming the file so a report can say which one:

   ```sh
   curl -s -X POST "$API/v1/firms/$FIRM/accounts/$ACCOUNT/imports?profile=my-bank" \
     -H "Authorization: Bearer $TOKEN" -H 'content-type: text/csv' \
     -H 'X-File-Name: march.csv' --data-binary @march.csv
   ```

   - `201`: imported whole; `duplicates` counts rows already imported,
     `overlap` the dates it shares with earlier statements.
   - `200` with `duplicate_file: true`: the same file was already imported.
   - `422`: refused whole; nothing was stored. `error.report` lists each line
     and how to fix it; `GET /v1/firms/$FIRM/imports/$IMPORT` shows it again.
   - `202`: held as a revision: the bank's file disagrees with lines already
     imported for the same dates. `GET …/imports/$IMPORT/changes` lists every
     difference (added, removed, changed with its fields); `POST
     …/imports/$IMPORT/accept` replaces the old lines (kept, marked
     superseded) or `…/reject` discards the file. Nothing changes until then.

   OFX files go to the same route with `?profile=ofx` and `content-type:
   application/x-ofx`. Invoices, bills, receipts, credit notes and book entries
   go to `POST …/clients/$CLIENT/document-imports` (default profile
   `r1-generic-documents`, or your own `documents_csv` profile, as
   `examples/profiles/sales-register.json`).

## Upgrade

1. Read the release's entries in `docs/DECISIONS.md` and `docs/ROADMAP.md`.
2. Anchor every firm and take a backup (below).
3. `systemctl stop recon-server` (it drains requests in flight for up to 30 s).
4. Install the new binaries; `recon db migrate --database recon`.
5. `systemctl start recon-server`; check `/readyz`;
   `recon audit verify FIRM --anchors FILE`.

The server refuses to start on a schema it was not built for, so a skipped
migration is caught at step 5, not in production traffic.

## Backup and restore

**Anchor, then back up**, every night and after every period close:

```sh
export RECON_DATABASE_URL='postgres://recon_app:...@DB-HOST:5432/recon?sslmode=require'
export RECON_ANCHOR_KEY_FILE=/etc/recon/anchor.key   # >= 32 bytes, mode 0600, kept off the DB host
recon audit anchor "$FIRM" --out "/srv/anchors/$FIRM.anchors"
pg_dump -Fc -d recon > "recon-$(date -u +%F).dump"   # encrypt to your offline key; keep 35 days
```

`recon audit anchor` verifies every chain first and signs only heads that
verify (D-059). Keep the anchor file where neither the server nor the
database owner can write: another host or write-once storage. Back up the
anchor key separately from the database: whoever has both can sign a
rewritten chain.

**Restore** into a new database, verify, then switch:

```sh
recon db setup --database recon_restored
pg_restore -d recon_restored --exit-on-error recon-2026-11-17.dump
RECON_DATABASE_URL='postgres://recon_app:...@DB-HOST:5432/recon_restored?sslmode=require' \
  recon audit verify "$FIRM" --anchors "/srv/anchors/$FIRM.anchors"
RECON_DATABASE_URL='…/recon_restored…' recon audit replay "$FIRM"
```

Then point `RECON_DATABASE_URL` in `/etc/recon/recon.env` at the restored
database and restart. Imports made after the backup are absent whole, never
in part (each is one transaction); upload those files again, and duplicates
and revisions are found as usual. `audit verify --anchors` fails if the
restored chains do not contain every anchored head: then the backup is older
than the last anchor, or was altered.

## Troubleshooting

The log never carries file content, file names or tokens (D-025), so for
anything about a file, read the import's report (`GET …/imports/$IMPORT`) or
run `recon import check` on the file.

| You see | It means | Do |
|---|---|---|
| service exits 78 | a setting, the role or the schema is wrong | read the journal; fix the setting, or run `recon db migrate` |
| `/readyz` `database_unavailable` | the database has not answered for 1.5 s | check the database and the network; uploads are refused with `503`, never half-done |
| `401 unauthorized` | the token is missing or wrong | send `Authorization: Bearer <RECON_OPERATOR_TOKEN>` |
| `415 unsupported_media_type` | the content type does not suit the profile | `text/csv` for CSV profiles, `application/x-ofx` with `profile=ofx` |
| `413 file_too_large` | over 50 MiB | export a shorter period |
| `422 file_refused` | at least one line is wrong; nothing was stored | fix every line in `error.report` and upload the whole file again |
| `422 ambiguous_date_format` | the profile does not declare the date order | store a profile version with the `order` the question offers |
| `422 profile_kind_mismatch` | a document profile on a statement (or the reverse) | use the other route or profile |
| `404 unknown_profile` | no such profile (or version) in this firm | `GET …/profiles/NAME` |
| `409 account_busy` | another file is being published into the account | retry after `Retry-After` |
| `503 import_capacity` | `RECON_IMPORT_CONCURRENCY` uploads in flight | retry after `Retry-After` |
| `202` with `revision` | the file disagrees with lines already imported | review `…/changes`, then `…/accept` or `…/reject` |
| `409 revision_stale` | the lines changed since the revision was listed | upload the file again for a fresh list |
| `409 revision_touches_decided_line` | a line it replaces is already decided | arrives with R3's review screens |
| `audit verify` exits 3 | a chain does not verify (it names the first broken record) | stop, keep the database as it is, restore from a backup that verifies against the anchors |
