//! A strict, streaming RFC 4180 reader over UTF-8 text (the [`crate::decode`] stage has
//! already decided the encoding, dropped a byte-order mark and counted the file's bytes).
//! Bytes arrive in chunks of any size; records come out as soon as they end. Memory is
//! bounded by one record (at most 64 KiB) whatever the file size.
//!
//! Strict means refusing, never guessing (D-010):
//!
//! * fields are separated by the profile's delimiter (`,` by default; `;`, `|` or tab) and
//!   records end with `\n` or `\r\n`; a `\r` anywhere else is `bare_carriage_return`;
//! * a quoted field is `"…"` with `""` for a literal quote; a quote inside an unquoted
//!   field, or anything but the delimiter or a terminator after a closing quote, is
//!   `stray_quote`; a quoted field open at the end of the file is `unterminated_quote`;
//! * an empty record is `empty_row`; fields must be valid UTF-8 without control,
//!   byte-order-mark, bidirectional or invisible format characters;
//! * records over 64 KiB, fields over 8 KiB, more than 256 fields or more than 2,000,000
//!   data rows refuse the record or the file.
//!
//! After a refused record the reader keeps scanning (without storing bytes) to the end of
//! that record, so one bad row does not hide the next one's error in the report.

use std::ops::Range;

use recon_domain::limits::{
    MAX_FIELD_BYTES, MAX_FIELDS, MAX_LINE_BYTES, MAX_ROWS, MAX_UPLOAD_BYTES,
};
use recon_domain::refusal::{RefusalCode, RowRefusal};
use recon_domain::text::is_forbidden;

use crate::dates::DateQuestion;

/// The delimiters a profile may declare.
pub const DELIMITERS: [char; 4] = [',', ';', '|', '\t'];

/// The bounds a reader enforces (D-010 by default; smaller in tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CsvLimits {
    /// Longest record, in bytes, terminator excluded.
    pub max_line_bytes: usize,
    /// Longest field, in bytes, after unquoting.
    pub max_field_bytes: usize,
    /// Most fields per record.
    pub max_fields: usize,
    /// Most records after the first (the header).
    pub max_data_rows: u32,
    /// Most bytes in the file, before decoding (enforced by the decoder).
    pub max_file_bytes: u64,
}

impl Default for CsvLimits {
    fn default() -> Self {
        Self {
            max_line_bytes: MAX_LINE_BYTES,
            max_field_bytes: MAX_FIELD_BYTES,
            max_fields: MAX_FIELDS,
            max_data_rows: MAX_ROWS,
            max_file_bytes: MAX_UPLOAD_BYTES,
        }
    }
}

/// A refusal of the whole file: nothing after it is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{}{}", code.as_str(), row.map(|r| format!(" at row {r}")).unwrap_or_default())]
pub struct FileRefusal {
    /// Why.
    pub code: RefusalCode,
    /// The record it concerns, when there is one.
    pub row: Option<u32>,
    /// The question to put to the user, for `ambiguous_date_format`.
    pub question: Option<DateQuestion>,
}

impl FileRefusal {
    /// A refusal without a question.
    pub const fn new(code: RefusalCode, row: Option<u32>) -> Self {
        Self {
            code,
            row,
            question: None,
        }
    }
}

/// One complete, valid record: its fields are UTF-8 text without forbidden characters.
#[derive(Debug, Clone, Copy)]
pub struct Record<'a> {
    /// The record number (the first record is 1).
    pub row: u32,
    text: &'a str,
    bounds: &'a [Range<usize>],
}

impl<'a> Record<'a> {
    /// The number of fields.
    pub fn len(&self) -> usize {
        self.bounds.len()
    }

    /// True for a record without fields (never produced: empty records are refused).
    pub fn is_empty(&self) -> bool {
        self.bounds.is_empty()
    }

    /// Field `i`, or `""` past the end.
    pub fn field(&self, i: usize) -> &'a str {
        self.bounds
            .get(i)
            .and_then(|r| self.text.get(r.clone()))
            .unwrap_or_default()
    }

    /// Every field in order.
    pub fn fields(&self) -> impl Iterator<Item = &'a str> + '_ {
        (0..self.bounds.len()).map(|i| self.field(i))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// At the start of a field (after `,` or at the start of a record).
    FieldStart,
    /// Inside an unquoted field.
    Unquoted,
    /// Inside a quoted field.
    Quoted,
    /// Just saw `"` inside a quoted field: either `""` or the closing quote.
    QuoteInQuoted,
    /// Just saw `\r`; only `\n` may follow.
    AfterCr,
}

/// The streaming reader.
#[derive(Debug)]
pub struct CsvReader {
    limits: CsvLimits,
    delimiter: u8,
    state: State,
    /// Unquoted bytes of the current record's fields, concatenated.
    buf: Vec<u8>,
    /// Each field's range in `buf`.
    bounds: Vec<Range<usize>>,
    /// Start of the current field in `buf`.
    field_start: usize,
    /// Raw bytes seen in the current record (terminator excluded).
    record_bytes: usize,
    /// The current record's number.
    record_no: u32,
    /// The first refusal of the current record.
    error: Option<RefusalCode>,
    /// Set once the file is refused; every later call returns it.
    poisoned: Option<FileRefusal>,
}

impl CsvReader {
    /// A comma-separated reader with the given bounds.
    pub fn new(limits: CsvLimits) -> Self {
        Self::with_delimiter(limits, b',')
    }

    /// A reader for `delimiter` (one of [`DELIMITERS`], checked by the profile).
    pub fn with_delimiter(limits: CsvLimits, delimiter: u8) -> Self {
        Self {
            limits,
            delimiter,
            state: State::FieldStart,
            buf: Vec::with_capacity(1024),
            bounds: Vec::with_capacity(16),
            field_start: 0,
            record_bytes: 0,
            record_no: 1,
            error: None,
            poisoned: None,
        }
    }

    /// Feeds the next chunk; `on_record` receives each finished record or its refusal.
    pub fn push<F>(&mut self, chunk: &[u8], on_record: &mut F) -> Result<(), FileRefusal>
    where
        F: FnMut(Result<Record<'_>, RowRefusal>) -> Result<(), FileRefusal>,
    {
        if let Some(refusal) = self.poisoned {
            return Err(refusal);
        }
        let result = self.push_inner(chunk, on_record);
        if let Err(refusal) = result {
            self.poisoned = Some(refusal);
        }
        result
    }

    fn push_inner<F>(&mut self, chunk: &[u8], on_record: &mut F) -> Result<(), FileRefusal>
    where
        F: FnMut(Result<Record<'_>, RowRefusal>) -> Result<(), FileRefusal>,
    {
        self.scan(chunk, on_record)
    }

    fn scan<F>(&mut self, bytes: &[u8], on_record: &mut F) -> Result<(), FileRefusal>
    where
        F: FnMut(Result<Record<'_>, RowRefusal>) -> Result<(), FileRefusal>,
    {
        for &b in bytes {
            self.byte(b, on_record)?;
        }
        Ok(())
    }

    fn refuse(&mut self, code: RefusalCode) {
        if self.error.is_none() {
            self.error = Some(code);
        }
    }

    fn store(&mut self, b: u8) {
        if self.error.is_some() {
            return;
        }
        let field_len = self.buf.len().saturating_sub(self.field_start);
        if field_len >= self.limits.max_field_bytes {
            self.refuse(RefusalCode::FieldTooLong);
            return;
        }
        self.buf.push(b);
    }

    fn end_field(&mut self) {
        if self.error.is_some() {
            return;
        }
        if self.bounds.len() >= self.limits.max_fields {
            self.refuse(RefusalCode::TooManyFields);
            return;
        }
        self.bounds.push(self.field_start..self.buf.len());
        self.field_start = self.buf.len();
    }

    fn count_raw(&mut self) {
        self.record_bytes = self.record_bytes.saturating_add(1);
        if self.record_bytes > self.limits.max_line_bytes {
            self.refuse(RefusalCode::LineTooLong);
        }
    }

    fn byte<F>(&mut self, b: u8, on_record: &mut F) -> Result<(), FileRefusal>
    where
        F: FnMut(Result<Record<'_>, RowRefusal>) -> Result<(), FileRefusal>,
    {
        match (self.state, b) {
            (State::AfterCr, b'\n') => return self.end_record(on_record),
            (State::AfterCr, _) => {
                // The `\r` was not part of a terminator.
                self.refuse(RefusalCode::BareCarriageReturn);
                self.state = State::Unquoted;
                return self.byte(b, on_record);
            }
            (State::Quoted, b'"') => self.state = State::QuoteInQuoted,
            (State::Quoted, _) => self.store(b),
            (State::QuoteInQuoted, b'"') => {
                self.store(b'"');
                self.state = State::Quoted;
            }
            (State::QuoteInQuoted | State::FieldStart | State::Unquoted, d)
                if d == self.delimiter =>
            {
                self.end_field();
                self.state = State::FieldStart;
            }
            (State::QuoteInQuoted | State::FieldStart | State::Unquoted, b'\n') => {
                return self.end_record(on_record);
            }
            (State::QuoteInQuoted | State::FieldStart | State::Unquoted, b'\r') => {
                self.state = State::AfterCr;
                return Ok(());
            }
            (State::QuoteInQuoted, _) => {
                self.refuse(RefusalCode::StrayQuote);
                self.state = State::Unquoted;
                self.store(b);
            }
            (State::FieldStart, b'"') => self.state = State::Quoted,
            (State::Unquoted, b'"') => {
                self.refuse(RefusalCode::StrayQuote);
                self.state = State::Unquoted;
            }
            (State::FieldStart | State::Unquoted, _) => {
                self.state = State::Unquoted;
                self.store(b);
            }
        }
        self.count_raw();
        Ok(())
    }

    fn end_record<F>(&mut self, on_record: &mut F) -> Result<(), FileRefusal>
    where
        F: FnMut(Result<Record<'_>, RowRefusal>) -> Result<(), FileRefusal>,
    {
        let row = self.record_no;
        // Data rows are every record after the header; the first one past the bound
        // refuses the file before it is handed on.
        if row.saturating_sub(1) > self.limits.max_data_rows {
            return Err(FileRefusal::new(RefusalCode::TooManyRows, Some(row)));
        }
        if self.record_bytes == 0 && self.error.is_none() {
            self.refuse(RefusalCode::EmptyRow);
        }
        self.end_field();
        let outcome = match self.error {
            Some(code) => Err(RowRefusal { row, code }),
            None => self.validate(),
        };
        let result = match outcome {
            Ok((text, bounds)) => on_record(Ok(Record { row, text, bounds })),
            Err(refusal) => on_record(Err(refusal)),
        };
        self.reset_record();
        result?;
        self.record_no = row.saturating_add(1);
        Ok(())
    }

    /// UTF-8 and forbidden characters, checked once per record: the whole buffer must be
    /// valid and every field boundary a character boundary, so each field is valid.
    fn validate(&self) -> Result<(&str, &[Range<usize>]), RowRefusal> {
        let row = self.record_no;
        let text = std::str::from_utf8(&self.buf).map_err(|_| RowRefusal {
            row,
            code: RefusalCode::InvalidUtf8,
        })?;
        let boundaries_ok = self
            .bounds
            .iter()
            .all(|r| text.is_char_boundary(r.start) && text.is_char_boundary(r.end));
        if !boundaries_ok {
            return Err(RowRefusal {
                row,
                code: RefusalCode::InvalidUtf8,
            });
        }
        if text.chars().any(is_forbidden) {
            return Err(RowRefusal {
                row,
                code: RefusalCode::ForbiddenCharacter,
            });
        }
        Ok((text, &self.bounds))
    }

    fn reset_record(&mut self) {
        self.buf.clear();
        self.bounds.clear();
        self.field_start = 0;
        self.record_bytes = 0;
        self.error = None;
        self.state = State::FieldStart;
    }

    /// Ends the file: a record without a final terminator is still a record; an open
    /// quote is refused. Returns the number of records read.
    pub fn finish<F>(&mut self, on_record: &mut F) -> Result<u32, FileRefusal>
    where
        F: FnMut(Result<Record<'_>, RowRefusal>) -> Result<(), FileRefusal>,
    {
        if let Some(refusal) = self.poisoned {
            return Err(refusal);
        }
        match self.state {
            State::Quoted => {
                self.refuse(RefusalCode::UnterminatedQuote);
                self.end_record(on_record)?;
            }
            State::AfterCr => {
                self.refuse(RefusalCode::BareCarriageReturn);
                self.end_record(on_record)?;
            }
            State::FieldStart if self.record_bytes == 0 && self.bounds.is_empty() => {}
            State::FieldStart | State::Unquoted | State::QuoteInQuoted => {
                self.end_record(on_record)?;
            }
        }
        Ok(self.record_no.saturating_sub(1))
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::arithmetic_side_effects,
        clippy::indexing_slicing,
        clippy::unnecessary_wraps,
        reason = "tests: a panic is the failure report"
    )]

    use super::*;
    use proptest::prelude::*;

    type Outcome = Result<Vec<String>, RowRefusal>;

    fn read_with(
        limits: CsvLimits,
        input: &[u8],
        chunk: usize,
    ) -> Result<Vec<Outcome>, FileRefusal> {
        let mut reader = CsvReader::new(limits);
        let mut out = vec![];
        let mut sink = |r: Result<Record<'_>, RowRefusal>| {
            out.push(r.map(|rec| rec.fields().map(str::to_owned).collect()));
            Ok(())
        };
        for part in input.chunks(chunk.max(1)) {
            reader.push(part, &mut sink)?;
        }
        reader.finish(&mut sink)?;
        Ok(out)
    }

    fn read(input: &str) -> Vec<Outcome> {
        read_with(CsvLimits::default(), input.as_bytes(), usize::MAX).unwrap()
    }

    fn ok(fields: &[&str]) -> Outcome {
        Ok(fields.iter().map(|s| (*s).to_owned()).collect())
    }

    fn bad(row: u32, code: RefusalCode) -> Outcome {
        Err(RowRefusal { row, code })
    }

    #[test]
    fn rfc4180_records() {
        assert_eq!(read("a,b\n1,2\n"), [ok(&["a", "b"]), ok(&["1", "2"])]);
        assert_eq!(read("a,b\r\n1,2"), [ok(&["a", "b"]), ok(&["1", "2"])]);
        assert_eq!(
            read("\"x,y\",\"say \"\"hi\"\"\",\n"),
            [ok(&["x,y", "say \"hi\"", ""])]
        );
        assert_eq!(read(",,"), [ok(&["", "", ""])]);
        assert_eq!(read("\"\""), [ok(&[""])]);
        assert_eq!(read(""), Vec::<Outcome>::new());
        // The decoder drops a leading byte-order mark; one that reaches the reader is data.
        assert_eq!(
            read("\u{feff}a\n"),
            [bad(1, RefusalCode::ForbiddenCharacter)]
        );
        assert_eq!(read("é,₹\n"), [ok(&["é", "₹"])]);
    }

    #[test]
    fn other_delimiters_split_and_commas_become_text() {
        for (delimiter, input) in [
            (b';', "a;\"x;y\";1,50\n"),
            (b'|', "a|\"x|y\"|1,50\n"),
            (b'\t', "a\t\"x\ty\"\t1,50\n"),
        ] {
            let mut reader = CsvReader::with_delimiter(CsvLimits::default(), delimiter);
            let mut out = vec![];
            let mut sink = |r: Result<Record<'_>, RowRefusal>| {
                out.push(r.map(|rec| rec.fields().map(str::to_owned).collect::<Vec<_>>()));
                Ok(())
            };
            reader.push(input.as_bytes(), &mut sink).unwrap();
            reader.finish(&mut sink).unwrap();
            let quoted = format!("x{}y", char::from(delimiter));
            if delimiter == b'\t' {
                // A tab inside a quoted field is still a control character in the text.
                assert_eq!(out, [bad(1, RefusalCode::ForbiddenCharacter)]);
            } else {
                assert_eq!(out, [ok(&["a", &quoted, "1,50"])]);
            }
        }
    }

    #[test]
    fn strictness_refuses_by_name_and_keeps_reading() {
        use RefusalCode as C;
        assert_eq!(
            read("a\n\nb\n"),
            [ok(&["a"]), bad(2, C::EmptyRow), ok(&["b"])]
        );
        assert_eq!(read("a\n\n"), [ok(&["a"]), bad(2, C::EmptyRow)]);
        assert_eq!(read("a\"b\nc\n"), [bad(1, C::StrayQuote), ok(&["c"])]);
        assert_eq!(read("\"a\"b\nc\n"), [bad(1, C::StrayQuote), ok(&["c"])]);
        assert_eq!(
            read("a\rb\nc\n"),
            [bad(1, C::BareCarriageReturn), ok(&["c"])]
        );
        assert_eq!(read("a\r"), [bad(1, C::BareCarriageReturn)]);
        assert_eq!(
            read("a\n\"open,b\n"),
            [ok(&["a"]), bad(2, C::UnterminatedQuote)]
        );
        assert_eq!(read("a\tb\n"), [bad(1, C::ForbiddenCharacter)]);
        assert_eq!(read("\"a\nb\"\n"), [bad(1, C::ForbiddenCharacter)]);
        assert_eq!(read("a\u{202e}b\n"), [bad(1, C::ForbiddenCharacter)]);
        assert_eq!(read("a\0\n"), [bad(1, C::ForbiddenCharacter)]);
        let invalid = read_with(CsvLimits::default(), b"a,\xff\nb\n", 64).unwrap();
        assert_eq!(invalid, [bad(1, C::InvalidUtf8), ok(&["b"])]);
        // Two fields that are each a half of one character are not valid text.
        let split = read_with(CsvLimits::default(), b"\xc3,\xa9\n", 64).unwrap();
        assert_eq!(split, [bad(1, C::InvalidUtf8)]);
    }

    #[test]
    fn bounds_refuse_records_and_files() {
        use RefusalCode as C;
        let small = CsvLimits {
            max_line_bytes: 8,
            max_field_bytes: 4,
            max_fields: 3,
            max_data_rows: 2,
            max_file_bytes: 64,
        };
        assert_eq!(
            read_with(small, b"abcde\nab\n", 3).unwrap(),
            [bad(1, C::FieldTooLong), ok(&["ab"])]
        );
        assert_eq!(
            read_with(small, b"a,b,c,d\nab\n", 3).unwrap(),
            [bad(1, C::TooManyFields), ok(&["ab"])]
        );
        assert_eq!(
            read_with(small, b"ab,ab,abc\nab\n", 3).unwrap(),
            [bad(1, C::LineTooLong), ok(&["ab"])]
        );
        assert_eq!(read_with(small, b"h\n1\n2\n", 1).unwrap().len(), 3);
        assert_eq!(
            read_with(small, b"h\n1\n2\n3\n", 1),
            Err(FileRefusal::new(C::TooManyRows, Some(4)))
        );
        assert_eq!(
            read_with(CsvLimits::default(), b"ab", 1).unwrap(),
            [ok(&["ab"])]
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// Streaming is invisible: any chunking of any input gives the same outcome as
        /// reading it in one piece.
        #[test]
        fn any_chunking_reads_the_same(
            input in proptest::collection::vec(
                prop::sample::select(b"ab,\"\r\n\xc3\xa9\t ".to_vec()), 0..200),
            chunk in 1_usize..17,
        ) {
            let small = CsvLimits { max_line_bytes: 40, max_field_bytes: 12, max_fields: 6, max_data_rows: 50, max_file_bytes: 1_000 };
            let whole = read_with(small, &input, usize::MAX);
            let parts = read_with(small, &input, chunk);
            prop_assert_eq!(whole, parts);
        }

        /// Arbitrary bytes never panic, and every outcome is a record or a named refusal.
        #[test]
        fn arbitrary_bytes_never_panic(input in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = read_with(CsvLimits::default(), &input, 13);
        }

        /// Quoting any text and reading it back returns it.
        #[test]
        fn quoted_fields_round_trip(fields in proptest::collection::vec("[a-z,\" é]{0,12}", 1..6)) {
            let line: Vec<String> = fields.iter().map(|f| format!("\"{}\"", f.replace('"', "\"\""))).collect();
            let input = format!("{}\n", line.join(","));
            let out = read_with(CsvLimits::default(), input.as_bytes(), 5).unwrap();
            prop_assert_eq!(out, vec![Ok(fields)]);
        }
    }
}
