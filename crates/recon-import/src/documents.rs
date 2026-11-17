//! Document imports (D-064): sales invoices, purchase bills, receipts, credit notes and book
//! entries from CSV, through a declared profile, into the normalized document shape.
//!
//! A document profile declares the same things a statement profile does (encoding,
//! delimiter, exact header, date and number formats) plus where the kind comes from (a
//! column holding the kind's code, or one kind for the whole file), the currency (a column,
//! or one currency for the file), and how amounts are signed: `signed` — as the bank would
//! see the money (in positive, out negative) — or `by_kind`, magnitudes signed by the kind
//! (a sales invoice or a receipt is money in, a purchase bill or a credit note money out; a
//! book entry has no fixed direction, so it must be signed). One signed column or a
//! money-out and a money-in column, as for statements.
//!
//! The built-in `r1-generic-documents` v1:
//!
//! ```text
//! kind,number,issue_date,due_date,amount,currency,counterparty,description
//! ```
//!
//! with kind codes, ISO dates and plain signed decimals. Invoices, bills and credit notes
//! must carry a number. Refusals follow the statement rules: any refused row refuses the
//! file, an undeclared day-month order is a question, formula cells are refused.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use recon_domain::document::{DocumentKind, DocumentTuple, NewDocument, document_tuple_hash};
use recon_domain::refusal::{RefusalCode, RowRefusal};
use recon_domain::text::normalize;
use recon_domain::{ClientId, Currency};

use crate::csv::{CsvLimits, CsvReader, FileRefusal, Record};
use crate::dates::DateFormat;
use crate::decode::{Decoder, TextEncoding};
use crate::number::NumberFormat;
use crate::profile::{
    AmountAt, AmountColumns, ParseSummary, ProfileError, Tally, Undeclared, canonical_sha256,
    column, header_problems, optional, optional_column, read_amount, read_date, text,
};

/// The built-in document layout's columns, in order.
pub const HEADER: [&str; 8] = [
    "kind",
    "number",
    "issue_date",
    "due_date",
    "amount",
    "currency",
    "counterparty",
    "description",
];

/// How a document file signs its amounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AmountSign {
    /// As the bank would see the money: in positive, out negative.
    Signed,
    /// Magnitudes, signed by the document's kind.
    ByKind,
}

/// Which header column holds each field.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentColumns {
    /// The kind's code; without it, `fixed_kind` applies to every row.
    #[serde(default)]
    pub kind: Option<String>,
    /// The document number.
    #[serde(default)]
    pub number: Option<String>,
    /// The issue date (required).
    pub issue_date: String,
    /// The due date.
    #[serde(default)]
    pub due_date: Option<String>,
    /// The amount.
    pub amount: AmountColumns,
    /// The currency code; without it, `fixed_currency` applies to every row.
    #[serde(default)]
    pub currency: Option<String>,
    /// The counterparty.
    #[serde(default)]
    pub counterparty: Option<String>,
    /// A description or narration.
    #[serde(default)]
    pub description: Option<String>,
}

/// A document CSV profile.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentProfile {
    /// The text encoding.
    #[serde(default)]
    pub encoding: TextEncoding,
    /// The field delimiter.
    pub delimiter: char,
    /// The header, exactly.
    pub header: Vec<String>,
    /// Where each field is.
    pub columns: DocumentColumns,
    /// One kind for every row, when there is no kind column.
    #[serde(default)]
    pub fixed_kind: Option<DocumentKind>,
    /// One currency (an ISO code from the D-005 table) for every row, when there is no
    /// currency column.
    #[serde(default)]
    pub fixed_currency: Option<String>,
    /// How amounts are signed.
    pub amount_sign: AmountSign,
    /// The dates' format.
    pub date_format: DateFormat,
    /// The amounts' format.
    pub number_format: NumberFormat,
}

/// Where the kind or the currency of a row comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source<T> {
    Column(usize),
    Fixed(T),
}

/// A validated document profile with its columns resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledDocuments {
    profile: DocumentProfile,
    sha256: [u8; 32],
    kind: Source<DocumentKind>,
    number: Option<usize>,
    issue_date: usize,
    due_date: Option<usize>,
    amount: AmountAt,
    currency: Source<Currency>,
    counterparty: Option<usize>,
    description: Option<usize>,
}

impl DocumentProfile {
    /// The built-in `r1-generic-documents` v1.
    pub fn builtin() -> Self {
        Self {
            encoding: TextEncoding::Utf8,
            delimiter: ',',
            header: HEADER.map(str::to_owned).to_vec(),
            columns: DocumentColumns {
                kind: Some("kind".into()),
                number: Some("number".into()),
                issue_date: "issue_date".into(),
                due_date: Some("due_date".into()),
                amount: AmountColumns::Signed("amount".into()),
                currency: Some("currency".into()),
                counterparty: Some("counterparty".into()),
                description: Some("description".into()),
            },
            fixed_kind: None,
            fixed_currency: None,
            amount_sign: AmountSign::Signed,
            date_format: DateFormat::ISO,
            number_format: NumberFormat::plain(),
        }
    }

    /// Validates the profile and resolves its columns, reporting every problem at once.
    pub fn compile(self) -> Result<CompiledDocuments, ProfileError> {
        let mut p = Vec::new();
        header_problems(&self.header, self.delimiter, &mut p);
        self.date_format.problems("date_format", &mut p);
        self.number_format.problems("number_format", &mut p);
        let h = &self.header;
        let c = &self.columns;
        let kind = match (&c.kind, self.fixed_kind) {
            (Some(name), None) => Source::Column(column(h, name, "columns.kind", &mut p)),
            (None, Some(kind)) => Source::Fixed(kind),
            _ => {
                p.push("columns.kind: exactly one of a kind column and fixed_kind".to_owned());
                Source::Fixed(DocumentKind::BookEntry)
            }
        };
        let currency = match (&c.currency, &self.fixed_currency) {
            (Some(name), None) => Source::Column(column(h, name, "columns.currency", &mut p)),
            (None, Some(code)) => Source::Fixed(Currency::from_code(code).unwrap_or_else(|_| {
                p.push("fixed_currency: not in the supported table".to_owned());
                Currency::Usd
            })),
            _ => {
                p.push(
                    "columns.currency: exactly one of a currency column and fixed_currency"
                        .to_owned(),
                );
                Source::Fixed(Currency::Usd)
            }
        };
        let amount = c.amount.compile(h, &mut p);
        if self.amount_sign == AmountSign::ByKind {
            if matches!(amount, AmountAt::Split { .. }) {
                p.push("amount_sign: by_kind needs one column of magnitudes".to_owned());
            }
            if self.fixed_kind == Some(DocumentKind::BookEntry) {
                p.push("amount_sign: book entries have no fixed direction".to_owned());
            }
        }
        let number = optional_column(h, c.number.as_ref(), "columns.number", &mut p);
        let issue_date = column(h, &c.issue_date, "columns.issue_date", &mut p);
        let due_date = optional_column(h, c.due_date.as_ref(), "columns.due_date", &mut p);
        let counterparty =
            optional_column(h, c.counterparty.as_ref(), "columns.counterparty", &mut p);
        let description = optional_column(h, c.description.as_ref(), "columns.description", &mut p);
        if !p.is_empty() {
            return Err(ProfileError { problems: p });
        }
        let sha256 = canonical_sha256(&self);
        Ok(CompiledDocuments {
            profile: self,
            sha256,
            kind,
            number,
            issue_date,
            due_date,
            amount,
            currency,
            counterparty,
            description,
        })
    }
}

impl CompiledDocuments {
    /// The built-in `r1-generic-documents` v1, compiled.
    pub fn builtin() -> Self {
        DocumentProfile::builtin().compile().unwrap_or_else(|e| {
            unreachable!("invariant: the built-in document profile compiles: {e}")
        })
    }

    /// The profile.
    pub const fn profile(&self) -> &DocumentProfile {
        &self.profile
    }

    /// SHA-256 of the profile's canonical JSON.
    pub const fn sha256(&self) -> [u8; 32] {
        self.sha256
    }
}

/// The client documents are imported for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocumentContext {
    /// The client.
    pub client: ClientId,
}

/// A streaming parser for one document file.
#[derive(Debug)]
pub struct DocumentParser {
    decoder: Decoder,
    reader: CsvReader,
    hasher: Sha256,
    rows: DocumentRows,
}

#[derive(Debug)]
struct DocumentRows {
    profile: Arc<CompiledDocuments>,
    context: DocumentContext,
    tally: Tally,
    undeclared: Undeclared,
}

impl DocumentParser {
    /// A parser for `profile`.
    pub fn with_profile(
        context: DocumentContext,
        profile: Arc<CompiledDocuments>,
        limits: CsvLimits,
    ) -> Self {
        let delimiter = u8::try_from(profile.profile.delimiter).unwrap_or(b',');
        Self {
            decoder: Decoder::new(profile.profile.encoding, limits.max_file_bytes),
            reader: CsvReader::with_delimiter(limits, delimiter),
            hasher: Sha256::new(),
            rows: DocumentRows {
                profile,
                context,
                tally: Tally::new(),
                undeclared: Undeclared::new(),
            },
        }
    }

    /// Rows refused so far.
    pub const fn rows_refused(&self) -> u32 {
        self.rows.tally.rows_refused
    }

    /// Bytes read so far.
    pub const fn file_bytes(&self) -> u64 {
        self.decoder.raw_bytes()
    }

    /// Feeds the next chunk; each valid document goes to `on_document` until a row is
    /// refused.
    pub fn push(
        &mut self,
        chunk: &[u8],
        on_document: &mut impl FnMut(NewDocument),
    ) -> Result<(), FileRefusal> {
        self.hasher.update(chunk);
        let Self {
            decoder,
            reader,
            rows,
            ..
        } = self;
        decoder.push(chunk, &mut |text| {
            reader.push(text, &mut |outcome| rows.handle(outcome, on_document))
        })
    }

    /// Ends the file and reports.
    pub fn finish(
        mut self,
        on_document: &mut impl FnMut(NewDocument),
    ) -> Result<ParseSummary, FileRefusal> {
        let Self {
            decoder,
            reader,
            rows,
            ..
        } = &mut self;
        decoder.finish(&mut |text| {
            reader.push(text, &mut |outcome| rows.handle(outcome, on_document))
        })?;
        reader.finish(&mut |outcome| rows.handle(outcome, on_document))?;
        if let Some(refusal) = self.rows.undeclared.refusal() {
            return Err(refusal);
        }
        let bytes = self.decoder.raw_bytes();
        self.rows.tally.summary(self.hasher, bytes)
    }
}

impl DocumentRows {
    fn handle(
        &mut self,
        outcome: Result<Record<'_>, RowRefusal>,
        on_document: &mut impl FnMut(NewDocument),
    ) -> Result<(), FileRefusal> {
        if self.tally.header(&outcome, &self.profile.profile.header)? {
            return Ok(());
        }
        match outcome.and_then(|record| self.parse_row(&record)) {
            Ok(Some(document)) if self.tally.rows_refused == 0 => on_document(document),
            Ok(_) => {}
            Err(refusal) => self.tally.refuse(refusal),
        }
        Ok(())
    }

    fn parse_row(&mut self, record: &Record<'_>) -> Result<Option<NewDocument>, RowRefusal> {
        let p = &*self.profile;
        let row = record.row;
        let refuse = |code: RefusalCode| RowRefusal { row, code };
        if record.len() != p.profile.header.len() {
            return Err(refuse(RefusalCode::FieldCountMismatch));
        }
        let kind = match p.kind {
            Source::Fixed(kind) => kind,
            Source::Column(i) => {
                let code =
                    optional(record.field(i)).ok_or(refuse(RefusalCode::MissingRequiredField))?;
                DocumentKind::from_code(code).ok_or(refuse(RefusalCode::InvalidDocumentKind))?
            }
        };
        let number_raw = match p.number {
            Some(i) => text(record.field(i), row)?,
            None => None,
        };
        let number_norm = number_raw.map(normalize).filter(|n| !n.is_empty());
        if kind.requires_number() && number_norm.is_none() {
            return Err(refuse(RefusalCode::MissingRequiredField));
        }
        let format = p.profile.date_format;
        let issue = optional(record.field(p.issue_date))
            .ok_or(refuse(RefusalCode::MissingRequiredField))?;
        let issue_date = read_date(format, issue, row, &mut self.undeclared)?;
        let due_date = match p.due_date.and_then(|i| optional(record.field(i))) {
            Some(t) => read_date(format, t, row, &mut self.undeclared)?,
            None => None,
        };
        let currency = match p.currency {
            Source::Fixed(c) => c,
            Source::Column(i) => {
                let code =
                    optional(record.field(i)).ok_or(refuse(RefusalCode::MissingRequiredField))?;
                Currency::from_code(code).map_err(|e| refuse(e.into()))?
            }
        };
        let amount =
            read_amount(p.amount, &p.profile.number_format, record, currency).map_err(refuse)?;
        let amount = match p.profile.amount_sign {
            AmountSign::Signed => amount,
            AmountSign::ByKind => {
                if amount.minor() < 0 {
                    return Err(refuse(RefusalCode::UnexpectedSign));
                }
                match kind.sign() {
                    Some(1) => amount,
                    Some(_) => amount.checked_neg().map_err(|e| refuse(e.into()))?,
                    // A book entry has no fixed direction: it needs a signed profile.
                    None => return Err(refuse(RefusalCode::InvalidDocumentKind)),
                }
            }
        };
        let counterparty_raw = match p.counterparty {
            Some(i) => text(record.field(i), row)?,
            None => None,
        };
        let description = match p.description {
            Some(i) => text(record.field(i), row)?,
            None => None,
        };
        let Some(issue_date) = issue_date else {
            return Ok(None);
        };
        let counterparty_norm = counterparty_raw.map(normalize).filter(|c| !c.is_empty());
        let tuple = document_tuple_hash(&DocumentTuple {
            client: self.context.client,
            kind,
            number_norm: number_norm.as_deref(),
            issue_date,
            amount,
            counterparty_norm: counterparty_norm.as_deref(),
        });
        Ok(Some(NewDocument {
            row,
            kind,
            number_raw: number_norm.as_ref().and(number_raw).map(str::to_owned),
            number_norm,
            issue_date,
            due_date,
            amount,
            counterparty_raw: counterparty_norm
                .as_ref()
                .and(counterparty_raw)
                .map(str::to_owned),
            counterparty_norm,
            description: description.map(str::to_owned),
            tuple_hash: tuple,
        }))
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::arithmetic_side_effects,
        clippy::indexing_slicing,
        reason = "tests: a panic is the failure report"
    )]

    use super::*;
    use crate::dates::DateOrder;
    use crate::number::{GroupStyle, Negative};
    use uuid::Uuid;

    const HEAD: &str = "kind,number,issue_date,due_date,amount,currency,counterparty,description\n";

    fn ctx() -> DocumentContext {
        DocumentContext {
            client: ClientId::from_uuid(Uuid::from_u128(3)),
        }
    }

    fn run(
        profile: CompiledDocuments,
        input: &[u8],
        chunk: usize,
    ) -> (Result<ParseSummary, FileRefusal>, Vec<NewDocument>) {
        let mut parser =
            DocumentParser::with_profile(ctx(), Arc::new(profile), CsvLimits::default());
        let mut docs = vec![];
        let mut push = |d| docs.push(d);
        for part in input.chunks(chunk.max(1)) {
            if let Err(e) = parser.push(part, &mut push) {
                return (Err(e), docs);
            }
        }
        let summary = parser.finish(&mut push);
        (summary, docs)
    }

    fn builtin(body: &str) -> (Result<ParseSummary, FileRefusal>, Vec<NewDocument>) {
        run(
            CompiledDocuments::builtin(),
            format!("{HEAD}{body}").as_bytes(),
            usize::MAX,
        )
    }

    /// A sales register: invoices only, magnitudes with Indian grouping, day-first dates.
    fn sales_register() -> DocumentProfile {
        DocumentProfile {
            encoding: TextEncoding::Utf8,
            delimiter: ',',
            header: ["Invoice No", "Date", "Customer", "Total"]
                .map(str::to_owned)
                .to_vec(),
            columns: DocumentColumns {
                kind: None,
                number: Some("Invoice No".into()),
                issue_date: "Date".into(),
                due_date: None,
                amount: AmountColumns::Signed("Total".into()),
                currency: None,
                counterparty: Some("Customer".into()),
                description: None,
            },
            fixed_kind: Some(DocumentKind::SalesInvoice),
            fixed_currency: Some("INR".into()),
            amount_sign: AmountSign::ByKind,
            date_format: DateFormat {
                order: Some(DateOrder::Dmy),
                separator: Some('-'),
                month_names: true,
                padded: false,
            },
            number_format: NumberFormat {
                decimal: '.',
                grouping: Some(','),
                group_style: GroupStyle::Indian,
                negatives: vec![Negative::Minus],
                plus_sign: false,
            },
        }
    }

    #[test]
    fn the_builtin_profile_reads_every_kind_signed_as_the_bank_sees_it() {
        let body = "sales_invoice,INV/2026-27/0012,2026-04-03,2026-05-03,1180.00,INR,Acme Traders,Consulting\n\
                    purchase_bill,B-77,2026-04-04,,-250.50,INR,Kestrel Freight,\n\
                    receipt,,2026-04-05,,100.00,USD,,Cash\n\
                    credit_note,CN-1,2026-04-06,,-50.00,INR,Acme Traders,\n\
                    book_entry,,2026-04-07,,-12.00,INR,,Bank charges\n";
        let (summary, docs) = builtin(body);
        assert!(summary.unwrap().is_clean());
        assert_eq!(docs.len(), 5);
        assert_eq!(docs[0].kind, DocumentKind::SalesInvoice);
        assert_eq!(docs[0].number_norm.as_deref(), Some("INV/2026-27/0012"));
        assert_eq!(docs[0].due_date.unwrap().to_iso(), "2026-05-03");
        assert_eq!(docs[0].amount.minor(), 118_000);
        assert_eq!(docs[1].amount.minor(), -25_050);
        assert_eq!(docs[2].amount.currency(), Currency::Usd);
        assert_eq!(docs[2].number_raw, None);
        assert_eq!(docs[4].description.as_deref(), Some("Bank charges"));
        assert_ne!(docs[1].tuple_hash, docs[3].tuple_hash);
    }

    #[test]
    fn bad_rows_are_named() {
        use RefusalCode as C;
        let body = "invoice,1,2026-04-03,,1.00,INR,,\n\
                    sales_invoice,,2026-04-03,,1.00,INR,,\n\
                    receipt,,03/04/2026,,1.00,INR,,\n\
                    receipt,,2026-04-03,,0,INR,,\n\
                    receipt,,2026-04-03,,1.00,XYZ,,\n\
                    receipt,=1+1,2026-04-03,,1.00,INR,,\n\
                    receipt,,2026-04-03,,1.00,INR,,@x\n\
                    receipt,,2026-04-03,,1.00,INR\n\
                    receipt,,2026-04-03,,1.00,INR,,\n";
        let (summary, docs) = builtin(body);
        let codes: Vec<_> = summary
            .unwrap()
            .refusals
            .iter()
            .map(|r| (r.row, r.code))
            .collect();
        assert_eq!(
            codes,
            [
                (2, C::InvalidDocumentKind),
                (3, C::MissingRequiredField),
                (4, C::InvalidDate),
                (5, C::ZeroAmount),
                (6, C::UnknownCurrency),
                (7, C::FormulaCell),
                (8, C::FormulaCell),
                (9, C::FieldCountMismatch),
            ]
        );
        assert_eq!(docs, Vec::<NewDocument>::new());
    }

    #[test]
    fn amounts_signed_by_kind_are_magnitudes() {
        let csv = "Invoice No,Date,Customer,Total\n\
                   S-1,3-Apr-2026,Acme,\"1,18,000.00\"\n\
                   S-2,4-apr-2026,Indigo Print,500.00\n";
        let (summary, docs) = run(sales_register().compile().unwrap(), csv.as_bytes(), 3);
        assert!(summary.unwrap().is_clean());
        assert_eq!(docs[0].amount.minor(), 11_800_000);
        assert_eq!(docs[0].amount.currency(), Currency::Inr);
        assert_eq!(docs[1].issue_date.to_iso(), "2026-04-04");
        let negative = "Invoice No,Date,Customer,Total\nS-3,5-Apr-2026,Acme,-1.00\n";
        let (summary, _) = run(sales_register().compile().unwrap(), negative.as_bytes(), 64);
        assert_eq!(
            summary.unwrap().refusals[0].code,
            RefusalCode::UnexpectedSign
        );
        // A bill register signs the same magnitudes as money out.
        let mut bills = sales_register();
        bills.fixed_kind = Some(DocumentKind::PurchaseBill);
        let (_, docs) = run(bills.compile().unwrap(), csv.as_bytes(), 64);
        assert_eq!(docs[0].amount.minor(), -11_800_000);
    }

    #[test]
    fn an_undeclared_order_is_a_question_here_too() {
        let mut open = DocumentProfile::builtin();
        open.date_format = DateFormat {
            order: None,
            separator: Some('/'),
            month_names: false,
            padded: false,
        };
        let body = format!("{HEAD}receipt,,03/04/2026,,1.00,INR,,\n");
        let (summary, docs) = run(open.compile().unwrap(), body.as_bytes(), 9);
        let refusal = summary.unwrap_err();
        assert_eq!(refusal.code, RefusalCode::AmbiguousDateFormat);
        assert_eq!(refusal.question.unwrap().options(), ["dmy", "mdy"]);
        assert_eq!(docs, Vec::<NewDocument>::new());
    }

    #[test]
    fn unusable_profiles_name_every_problem() {
        let mut p = sales_register();
        p.columns.kind = Some("Invoice No".into());
        p.fixed_currency = Some("XYZ".into());
        p.columns.amount = AmountColumns::Split {
            money_out: "Total".into(),
            money_in: "Customer".into(),
        };
        let problems = p.compile().unwrap_err().problems;
        assert_eq!(problems.len(), 3, "{problems:?}");
        let mut book = sales_register();
        book.fixed_kind = Some(DocumentKind::BookEntry);
        assert_eq!(book.compile().unwrap_err().problems.len(), 1);
        assert_eq!(
            CompiledDocuments::builtin().profile(),
            &DocumentProfile::builtin()
        );
    }

    proptest::proptest! {
        #[test]
        fn arbitrary_bytes_never_panic(input in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..400)) {
            let mut with_head = HEAD.as_bytes().to_vec();
            with_head.extend_from_slice(&input);
            let _ = run(CompiledDocuments::builtin(), &with_head, 7);
            let _ = run(sales_register().compile().unwrap(), &input, 5);
        }
    }
}
