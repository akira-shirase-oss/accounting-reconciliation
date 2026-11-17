# Examples

Files to try every import command on. Each command below runs in
`crates/recon-cli/tests/examples.rs`, so this page stays true. Build the
command line first:

```sh
cargo build --release -p recon-cli && export PATH="$PWD/target/release:$PATH"
```

None of this is real client data (D-028): the banks, customers and amounts
are made up.

## What is here

| File | What it is |
|---|---|
| `profiles/indian-bank-savings.json` | a statement profile for a typical Indian bank export: `DD/MM/YYYY`, Indian digit grouping, withdrawal and deposit columns, a closing balance, newest first |
| `profiles/indian-bank-draft.json` | the same without the date order: what you write before you know the bank |
| `profiles/sales-register.json` | a document profile for a sales register: invoice numbers, `D-Mon-YYYY` dates, totals as magnitudes |
| `statements/indian-bank-march-2026.csv` | a clean month (12 lines) |
| `statements/indian-bank-march-2026-with-errors.csv` | the same month with three mistakes a spreadsheet makes |
| `documents/sales-register-march-2026.csv` | four sales invoices |

The OFX samples are in `fixtures/ofx/`.

## Check a profile

```sh
recon profile check examples/profiles/indian-bank-savings.json
```

```text
examples/profiles/indian-bank-savings.json: OK, indian-bank-savings (from examples/profiles/indian-bank-savings.json).
  Header:  Date,Narration,Chq./Ref.No.,Value Dt,Withdrawal Amt.,Deposit Amt.,Closing Balance
  Dates:   DD/MM/YYYY
  Amounts: like 12,34,567.89, no currency symbol; no negative sign (magnitudes)
```

## Check a statement before uploading it

```sh
recon import check examples/statements/indian-bank-march-2026.csv \
  --profile examples/profiles/indian-bank-savings.json --currency INR
```

```text
examples/statements/indian-bank-march-2026.csv: would import 12 rows.
  Profile: indian-bank-savings (from examples/profiles/indian-bank-savings.json)
  Dates:   2026-03-01 to 2026-03-30
  INR:     money in 228384.50 INR, money out 120661.25 INR
```

The same month with mistakes is refused whole, with every line explained:

```sh
recon import check examples/statements/indian-bank-march-2026-with-errors.csv \
  --profile examples/profiles/indian-bank-savings.json --currency INR
```

```text
examples/statements/indian-bank-march-2026-with-errors.csv: refused (read with indian-bank-savings …).
Nothing was imported: 3 of 12 rows have a problem. Fix them in the file and upload the whole file again; a file is imported whole or not at all.

  line 4, column "Date" [invalid_date]
    What is wrong: The date is not a real day written in the profile's date format. The profile reads dates as DD/MM/YYYY.
    How to fix:    Correct the date in the file, or the profile's date_format if the bank writes dates differently.

  line 7, column "Withdrawal Amt." [invalid_amount]
    ...
  line 10, column "Withdrawal Amt. / Deposit Amt." [debit_and_credit]
    ...
```

With the draft profile the file is a question, never a guess:

```sh
recon import check examples/statements/indian-bank-march-2026.csv \
  --profile examples/profiles/indian-bank-draft.json --currency INR
```

It answers that every date reads as day-month-year and tells you to store a
profile version with `"order": "dmy"`.

## Documents and OFX

```sh
recon import check examples/documents/sales-register-march-2026.csv \
  --profile examples/profiles/sales-register.json
recon import check fixtures/ofx/sgml-1x.ofx --profile ofx --currency USD
```

## Upload to the server

`docs/OPERATOR_GUIDE.md` (*Importing a first statement*) has the requests:
create the firm, client and account, `POST` the same profile file to
`/v1/firms/{firm}/profiles`, then upload the statement with `X-File-Name`.
The server returns the same report as `recon import check` when it refuses a
file, and finds what only it can: rows already imported and changes to lines
already imported. `--json` prints the check in the server's report format.
