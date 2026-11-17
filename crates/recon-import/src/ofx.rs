//! OFX bank and credit-card statements (D-063): OFX 1.x (an `OFXHEADER:100` header, then
//! SGML whose leaf elements have no end tags) and OFX 2.x (XML, an `<?xml?>` declaration
//! and an `<?OFX OFXHEADER="200"?>` instruction), read by one streaming tokenizer with
//! bounded memory, in chunks of any size.
//!
//! Nothing is declared, expanded or resolved: any `<!` markup (a DTD, an entity
//! declaration, a comment, a CDATA section) refuses the file at once as
//! `declaration_refused`, before anything after it is read; only the five predefined XML
//! entities and numeric character references are decoded, and any other `&name;` is
//! `entity_refused`. The 1.x charset comes from the file's own header (`CHARSET:1252` is
//! read as Windows-1252); 2.x must be UTF-8 or US-ASCII.
//!
//! The file holds exactly one `STMTRS` or `CCSTMTRS` (a second is `multiple_statements`),
//! whose `CURDEF` must be the account's currency. Each `STMTTRN` is one row, numbered by its
//! 1-based position. Profile `ofx` v1 maps it onto a statement line: the booking date is
//! `DTPOSTED`'s calendar date, the value date `DTAVAIL`'s, the amount `TRNAMT`, the bank
//! reference `FITID` (required), the narration `NAME` and `MEMO` joined by a space (a
//! `PAYEE` aggregate's `NAME` counts as `NAME`), the counterparty `NAME`; there is no running
//! balance. Row refusals follow the CSV rules (D-010): every one counted, the first 1,000
//! reported, no line handed on after the first, reading continued to the end.
//!
//! Bounds: tag names of at most 32 bytes of `[A-Z0-9.]`, element values of at most 8 KiB
//! (`field_too_long`), nesting at most 32 deep, at most 4 KiB before the `<OFX>` element,
//! the file and row limits of [`CsvLimits`].

use recon_domain::fingerprint::{LineTuple, tuple_hash};
use recon_domain::import::NewLine;
use recon_domain::limits::MAX_FIELD_BYTES;
use recon_domain::refusal::{RefusalCode, RowRefusal};
use recon_domain::text::{is_forbidden, normalize};
use recon_domain::{Date, Money};
use sha2::{Digest, Sha256};

use crate::csv::{CsvLimits, FileRefusal};
use crate::decode::push_cp1252;
use crate::profile::{AccountContext, ParseSummary, Tally, is_formula};

/// The longest tag name.
const MAX_TAG_BYTES: usize = 32;
/// The deepest nesting of aggregates.
const MAX_DEPTH: usize = 32;
/// The most bytes before the `<OFX>` element (the 1.x header, or the 2.x prolog).
const MAX_HEADER_BYTES: usize = 4 * 1024;
/// The longest entity reference between `&` and `;`.
const MAX_ENTITY_BYTES: usize = 12;
/// Leading bytes held to recognize the format.
const DETECT_BYTES: usize = 16;

fn refused(code: RefusalCode) -> FileRefusal {
    FileRefusal::new(code, None)
}

fn malformed() -> FileRefusal {
    refused(RefusalCode::MalformedOfx)
}

const fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}

/// Which stage the bytes are in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Holding the first bytes until the format is known.
    Detect,
    /// Reading the 1.x `KEY:VALUE` header up to its blank line.
    SgmlHeader,
    /// Tokenizing the body (and, for 2.x, its prolog).
    Body,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Sgml,
    Xml,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Charset {
    Utf8,
    Windows1252,
}

/// The tokenizer's position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok {
    /// Between tags.
    Text,
    /// After `&` in text.
    Entity,
    /// After `<`.
    TagStart,
    /// In a start tag's name.
    StartName,
    /// In an end tag's name (after `</`).
    EndName,
    /// After a name, before `>`.
    TagTail { end: bool },
    /// In a processing instruction (after `<?`).
    Instruction,
}

/// Where the root element is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Root {
    /// Before the first start tag.
    Prolog,
    /// The first start tag was read; it is not yet known to be an aggregate.
    Pending,
    /// Inside `<OFX>`.
    Open,
    /// After `</OFX>`: only white space may follow.
    Closed,
}

/// The 2.x prolog read so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Prolog {
    Nothing,
    Declaration,
    Instruction,
}

/// The statement aggregate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Statement {
    NotSeen,
    Open { curdef: bool },
    Closed,
}

/// The 1.x header fields this parser reads.
#[derive(Debug, Default)]
struct SgmlHeader {
    ofxheader: Option<String>,
    data: Option<String>,
    encoding: Option<String>,
    charset: Option<String>,
}

/// A transaction's fields as read.
#[derive(Debug, Default)]
struct Txn {
    posted: Option<String>,
    avail: Option<String>,
    amount: Option<String>,
    fitid: Option<String>,
    name: Option<String>,
    memo: Option<String>,
    payee_name: Option<String>,
    cursym: Option<String>,
    /// The first value that could not be read.
    error: Option<RefusalCode>,
}

/// A transaction field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Posted,
    Avail,
    Amount,
    Fitid,
    Name,
    Memo,
    PayeeName,
    Cursym,
}

/// What an element's value is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Txn(Field),
    Curdef,
    Ignore,
}

/// A streaming parser for one OFX statement.
#[derive(Debug)]
pub struct OfxParser {
    context: AccountContext,
    limits: CsvLimits,
    hasher: Sha256,
    raw_bytes: u64,
    poisoned: Option<FileRefusal>,
    phase: Phase,
    format: Format,
    charset: Charset,
    head: Vec<u8>,
    header: SgmlHeader,
    header_line: Vec<u8>,
    header_bytes: usize,
    tok: Tok,
    name: Vec<u8>,
    entity: Vec<u8>,
    instruction: Vec<u8>,
    prolog: Prolog,
    pending: Option<String>,
    text: Vec<u8>,
    text_nonspace: bool,
    text_overflow: bool,
    root: Root,
    stack: Vec<String>,
    statement: Statement,
    txn: Option<Txn>,
    ordinal: u32,
    tally: Tally,
}

impl OfxParser {
    /// A parser with the D-010 bounds.
    pub fn new(context: AccountContext) -> Self {
        Self::with_limits(context, CsvLimits::default())
    }

    /// A parser with other bounds (tests).
    pub fn with_limits(context: AccountContext, limits: CsvLimits) -> Self {
        let mut tally = Tally::new();
        // OFX has no header row: every counted row is a transaction.
        tally.header_seen = true;
        Self {
            context,
            limits,
            hasher: Sha256::new(),
            raw_bytes: 0,
            poisoned: None,
            phase: Phase::Detect,
            format: Format::Sgml,
            charset: Charset::Utf8,
            head: Vec::with_capacity(DETECT_BYTES),
            header: SgmlHeader::default(),
            header_line: Vec::new(),
            header_bytes: 0,
            tok: Tok::Text,
            name: Vec::with_capacity(MAX_TAG_BYTES),
            entity: Vec::with_capacity(MAX_ENTITY_BYTES),
            instruction: Vec::new(),
            prolog: Prolog::Nothing,
            pending: None,
            text: Vec::new(),
            text_nonspace: false,
            text_overflow: false,
            root: Root::Prolog,
            stack: Vec::with_capacity(MAX_DEPTH),
            statement: Statement::NotSeen,
            txn: None,
            ordinal: 0,
            tally,
        }
    }

    /// Rows refused so far.
    pub const fn rows_refused(&self) -> u32 {
        self.tally.rows_refused
    }

    /// Bytes read so far.
    pub const fn file_bytes(&self) -> u64 {
        self.raw_bytes
    }

    /// Feeds the next chunk; each valid line goes to `on_line` until a row is refused.
    pub fn push(
        &mut self,
        chunk: &[u8],
        on_line: &mut impl FnMut(NewLine),
    ) -> Result<(), FileRefusal> {
        if let Some(refusal) = self.poisoned {
            return Err(refusal);
        }
        self.hasher.update(chunk);
        let result = self.push_inner(chunk, on_line);
        if let Err(refusal) = result {
            self.poisoned = Some(refusal);
        }
        result
    }

    fn push_inner<F: FnMut(NewLine)>(
        &mut self,
        chunk: &[u8],
        on_line: &mut F,
    ) -> Result<(), FileRefusal> {
        let len = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
        self.raw_bytes = self.raw_bytes.saturating_add(len);
        if self.raw_bytes > self.limits.max_file_bytes {
            return Err(refused(RefusalCode::FileTooLarge));
        }
        for &b in chunk {
            self.byte(b, on_line)?;
        }
        Ok(())
    }

    /// Ends the file and reports.
    pub fn finish(
        mut self,
        on_line: &mut impl FnMut(NewLine),
    ) -> Result<ParseSummary, FileRefusal> {
        if let Some(refusal) = self.poisoned {
            return Err(refusal);
        }
        if self.phase == Phase::Detect {
            if self.head.is_empty() {
                return Err(refused(RefusalCode::EmptyFile));
            }
            self.detect(on_line)?;
        }
        if self.phase != Phase::Body
            || self.tok != Tok::Text
            || self.root != Root::Closed
            || self.statement != Statement::Closed
        {
            return Err(malformed());
        }
        let bytes = self.raw_bytes;
        self.tally.summary(self.hasher, bytes)
    }

    fn byte<F: FnMut(NewLine)>(&mut self, b: u8, on_line: &mut F) -> Result<(), FileRefusal> {
        match self.phase {
            Phase::Detect => {
                self.head.push(b);
                if self.head.len() >= DETECT_BYTES {
                    self.detect(on_line)?;
                }
                Ok(())
            }
            Phase::SgmlHeader => self.header_byte(b),
            Phase::Body => self.body_byte(b, on_line),
        }
    }

    /// Decides the format from the first bytes (or all of a shorter file), then replays
    /// them into the stage the format starts with.
    fn detect<F: FnMut(NewLine)>(&mut self, on_line: &mut F) -> Result<(), FileRefusal> {
        let head = std::mem::take(&mut self.head);
        let marks: [&[u8]; 3] = [&[0xFF, 0xFE], &[0xFE, 0xFF], &[0x00, 0x00, 0xFE, 0xFF]];
        if marks.iter().any(|m| head.starts_with(m)) {
            return Err(refused(RefusalCode::UnsupportedEncoding));
        }
        let skip = if head.starts_with(&[0xEF, 0xBB, 0xBF]) {
            3
        } else {
            0
        };
        let body = head.get(skip..).unwrap_or_default();
        let (phase, format) = if body.starts_with(b"OFXHEADER:") {
            (Phase::SgmlHeader, Format::Sgml)
        } else if body.starts_with(b"<?xml") {
            (Phase::Body, Format::Xml)
        } else {
            // Not OFX, or a file cut short before its header ended.
            return Err(malformed());
        };
        self.phase = phase;
        self.format = format;
        for &b in body {
            self.byte(b, on_line)?;
        }
        Ok(())
    }

    /// One byte of the 1.x header.
    fn header_byte(&mut self, b: u8) -> Result<(), FileRefusal> {
        self.header_bytes = self.header_bytes.saturating_add(1);
        if self.header_bytes > MAX_HEADER_BYTES {
            return Err(malformed());
        }
        if b != b'\n' {
            self.header_line.push(b);
            return Ok(());
        }
        let mut line = std::mem::take(&mut self.header_line);
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.is_empty() {
            self.charset = self.header_charset()?;
            self.phase = Phase::Body;
            return Ok(());
        }
        let line = String::from_utf8(line).map_err(|_| malformed())?;
        let (key, value) = line.split_once(':').ok_or_else(malformed)?;
        if key.is_empty() || !key.bytes().all(|c| c.is_ascii_uppercase()) || !value.is_ascii() {
            return Err(malformed());
        }
        let value = value.trim().to_owned();
        let slot = match key {
            "OFXHEADER" => &mut self.header.ofxheader,
            "DATA" => &mut self.header.data,
            "ENCODING" => &mut self.header.encoding,
            "CHARSET" => &mut self.header.charset,
            _ => return Ok(()),
        };
        if slot.is_some() {
            return Err(malformed());
        }
        *slot = Some(value);
        Ok(())
    }

    /// The body's charset, from the 1.x header.
    fn header_charset(&self) -> Result<Charset, FileRefusal> {
        let h = &self.header;
        if h.ofxheader.as_deref() != Some("100") || h.data.as_deref() != Some("OFXSGML") {
            return Err(malformed());
        }
        match h.encoding.as_deref() {
            Some("USASCII") => match h.charset.as_deref() {
                None | Some("1252" | "ISO-8859-1" | "NONE") => Ok(Charset::Windows1252),
                Some(_) => Err(refused(RefusalCode::UnsupportedEncoding)),
            },
            Some("UTF-8") => Ok(Charset::Utf8),
            Some(_) => Err(refused(RefusalCode::UnsupportedEncoding)),
            None => Err(malformed()),
        }
    }

    /// One byte of the body.
    fn body_byte<F: FnMut(NewLine)>(&mut self, b: u8, on_line: &mut F) -> Result<(), FileRefusal> {
        if self.root == Root::Prolog {
            self.header_bytes = self.header_bytes.saturating_add(1);
            if self.header_bytes > MAX_HEADER_BYTES {
                return Err(malformed());
            }
        }
        match self.tok {
            Tok::Text => match b {
                b'<' => self.tok = Tok::TagStart,
                b'&' => {
                    if self.pending.is_none() {
                        return Err(malformed());
                    }
                    self.entity.clear();
                    self.tok = Tok::Entity;
                }
                _ => self.text_byte(b)?,
            },
            Tok::Entity => {
                if b == b';' {
                    let c = entity(&self.entity)?;
                    self.push_char(c);
                    self.tok = Tok::Text;
                } else if self.entity.len() < MAX_ENTITY_BYTES
                    && (b.is_ascii_alphanumeric() || b == b'#')
                {
                    self.entity.push(b);
                } else {
                    return Err(refused(RefusalCode::EntityRefused));
                }
            }
            Tok::TagStart => match b {
                b'!' => return Err(refused(RefusalCode::DeclarationRefused)),
                b'?' if self.format == Format::Xml && self.root == Root::Prolog => {
                    self.instruction.clear();
                    self.tok = Tok::Instruction;
                }
                b'/' => {
                    self.name.clear();
                    self.tok = Tok::EndName;
                }
                b'A'..=b'Z' | b'0'..=b'9' => {
                    self.name.clear();
                    self.name.push(b);
                    self.tok = Tok::StartName;
                }
                _ => return Err(malformed()),
            },
            Tok::StartName | Tok::EndName => {
                let end = self.tok == Tok::EndName;
                match b {
                    b'A'..=b'Z' | b'0'..=b'9' | b'.' if self.name.len() < MAX_TAG_BYTES => {
                        self.name.push(b);
                    }
                    b'>' => {
                        self.tok = Tok::Text;
                        self.tag(end, on_line)?;
                    }
                    _ if is_space(b) => self.tok = Tok::TagTail { end },
                    _ => return Err(malformed()),
                }
            }
            Tok::TagTail { end } => match b {
                b'>' => {
                    self.tok = Tok::Text;
                    self.tag(end, on_line)?;
                }
                _ if is_space(b) => {}
                _ => return Err(malformed()),
            },
            Tok::Instruction => {
                self.instruction.push(b);
                if self.instruction.ends_with(b"?>") {
                    self.instruction_done()?;
                    self.tok = Tok::Text;
                }
            }
        }
        Ok(())
    }

    /// A text byte: part of the pending element's value, or white space between elements.
    fn text_byte(&mut self, b: u8) -> Result<(), FileRefusal> {
        let space = is_space(b);
        if self.pending.is_none() {
            return if space { Ok(()) } else { Err(malformed()) };
        }
        if !space {
            self.text_nonspace = true;
        }
        if self.text.len() >= MAX_FIELD_BYTES {
            self.text_overflow = true;
            return Ok(());
        }
        match self.charset {
            Charset::Utf8 => self.text.push(b),
            Charset::Windows1252 => push_cp1252(&mut self.text, b),
        }
        Ok(())
    }

    /// A character from a reference.
    fn push_char(&mut self, c: char) {
        if !c.is_ascii_whitespace() {
            self.text_nonspace = true;
        }
        if self.text.len() >= MAX_FIELD_BYTES {
            self.text_overflow = true;
            return;
        }
        let mut buf = [0_u8; 4];
        self.text
            .extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    }

    /// A finished `<?…?>` in the 2.x prolog: the XML declaration, then the OFX instruction.
    fn instruction_done(&mut self) -> Result<(), FileRefusal> {
        let content = self
            .instruction
            .strip_suffix(b"?>")
            .unwrap_or_default()
            .to_vec();
        let content = String::from_utf8(content).map_err(|_| malformed())?;
        match self.prolog {
            Prolog::Nothing => {
                let rest = content.strip_prefix("xml").ok_or_else(malformed)?;
                if !rest.starts_with([' ', '\t', '\r', '\n']) {
                    return Err(malformed());
                }
                match xml_attribute(rest, "encoding") {
                    None => {}
                    Some(e)
                        if e.eq_ignore_ascii_case("utf-8")
                            || e.eq_ignore_ascii_case("us-ascii") => {}
                    Some(_) => return Err(refused(RefusalCode::UnsupportedEncoding)),
                }
                self.charset = Charset::Utf8;
                self.prolog = Prolog::Declaration;
            }
            Prolog::Declaration => {
                let rest = content.strip_prefix("OFX").ok_or_else(malformed)?;
                if !rest.starts_with([' ', '\t', '\r', '\n'])
                    || xml_attribute(rest, "OFXHEADER") != Some("200")
                {
                    return Err(malformed());
                }
                self.prolog = Prolog::Instruction;
            }
            Prolog::Instruction => return Err(malformed()),
        }
        Ok(())
    }

    /// A complete tag: first decides what the pending start tag was, then handles this one.
    fn tag<F: FnMut(NewLine)>(&mut self, end: bool, on_line: &mut F) -> Result<(), FileRefusal> {
        let name = String::from_utf8(std::mem::take(&mut self.name)).map_err(|_| malformed())?;
        if name.is_empty() {
            return Err(malformed());
        }
        if let Some(pending) = self.pending.take() {
            let text = std::mem::take(&mut self.text);
            let nonspace = std::mem::take(&mut self.text_nonspace);
            let overflow = std::mem::take(&mut self.text_overflow);
            if nonspace {
                // A leaf: its value ends here; an end tag of the same name is its own.
                self.leaf(&pending, &text, overflow)?;
                if end && name == pending {
                    return Ok(());
                }
            } else {
                // Nothing but white space before the next tag: an aggregate (an empty
                // element is an empty aggregate, so an empty STMTTRN is still a row).
                self.open(pending)?;
            }
        }
        if end {
            return self.close(&name, on_line);
        }
        match self.root {
            Root::Closed => return Err(malformed()),
            Root::Prolog => {
                if self.format == Format::Xml && self.prolog != Prolog::Instruction {
                    return Err(malformed());
                }
                self.root = Root::Pending;
            }
            Root::Pending | Root::Open => {}
        }
        self.pending = Some(name);
        Ok(())
    }

    fn open(&mut self, name: String) -> Result<(), FileRefusal> {
        if self.stack.is_empty() {
            if self.root != Root::Pending || name != "OFX" {
                return Err(malformed());
            }
            self.root = Root::Open;
        }
        if self.stack.len() >= MAX_DEPTH {
            return Err(malformed());
        }
        match name.as_str() {
            "STMTRS" | "CCSTMTRS" => {
                if self.statement != Statement::NotSeen {
                    return Err(refused(RefusalCode::MultipleStatements));
                }
                self.statement = Statement::Open { curdef: false };
            }
            "STMTTRN" => {
                if !matches!(self.statement, Statement::Open { .. }) || self.txn.is_some() {
                    return Err(malformed());
                }
                self.ordinal = self.ordinal.saturating_add(1);
                if self.ordinal > self.limits.max_data_rows {
                    return Err(FileRefusal::new(
                        RefusalCode::TooManyRows,
                        Some(self.ordinal),
                    ));
                }
                self.txn = Some(Txn::default());
            }
            _ => {}
        }
        self.stack.push(name);
        Ok(())
    }

    fn close<F: FnMut(NewLine)>(&mut self, name: &str, on_line: &mut F) -> Result<(), FileRefusal> {
        match self.stack.pop() {
            Some(top) if top == name => {}
            _ => return Err(malformed()),
        }
        match name {
            "STMTTRN" => {
                let txn = self.txn.take().unwrap_or_default();
                self.finish_txn(&txn, on_line);
            }
            "STMTRS" | "CCSTMTRS" => {
                if self.statement != (Statement::Open { curdef: true }) {
                    return Err(malformed());
                }
                self.statement = Statement::Closed;
            }
            "OFX" if self.stack.is_empty() => self.root = Root::Closed,
            _ => {}
        }
        Ok(())
    }

    /// What a leaf named `name` under the current aggregate is for.
    fn slot(&self, name: &str) -> Slot {
        let depth = self.stack.len();
        let top = self.stack.last().map(String::as_str);
        let parent = depth
            .checked_sub(2)
            .and_then(|i| self.stack.get(i))
            .map(String::as_str);
        match (top, parent, name) {
            (Some("STMTTRN"), _, "DTPOSTED") => Slot::Txn(Field::Posted),
            (Some("STMTTRN"), _, "DTAVAIL") => Slot::Txn(Field::Avail),
            (Some("STMTTRN"), _, "TRNAMT") => Slot::Txn(Field::Amount),
            (Some("STMTTRN"), _, "FITID") => Slot::Txn(Field::Fitid),
            (Some("STMTTRN"), _, "NAME") => Slot::Txn(Field::Name),
            (Some("STMTTRN"), _, "MEMO") => Slot::Txn(Field::Memo),
            (Some("PAYEE"), Some("STMTTRN"), "NAME") => Slot::Txn(Field::PayeeName),
            (Some("CURRENCY" | "ORIGCURRENCY"), Some("STMTTRN"), "CURSYM") => {
                Slot::Txn(Field::Cursym)
            }
            (Some("STMTRS" | "CCSTMTRS"), _, "CURDEF") => Slot::Curdef,
            _ => Slot::Ignore,
        }
    }

    fn leaf(&mut self, name: &str, text: &[u8], overflow: bool) -> Result<(), FileRefusal> {
        if self.stack.is_empty() {
            // Text directly in no element, or in the root itself.
            return Err(malformed());
        }
        match self.slot(name) {
            Slot::Ignore => Ok(()),
            Slot::Curdef => {
                if self.statement != (Statement::Open { curdef: false }) {
                    return Err(malformed());
                }
                if leaf_value(text, overflow).ok().flatten().as_deref()
                    != Some(self.context.currency.code())
                {
                    return Err(refused(RefusalCode::CurrencyMismatch));
                }
                self.statement = Statement::Open { curdef: true };
                Ok(())
            }
            Slot::Txn(field) => {
                let Some(txn) = self.txn.as_mut() else {
                    return Err(malformed());
                };
                let value = match leaf_value(text, overflow) {
                    Ok(v) => v,
                    Err(code) => {
                        if txn.error.is_none() {
                            txn.error = Some(code);
                        }
                        return Ok(());
                    }
                };
                let slot = match field {
                    Field::Posted => &mut txn.posted,
                    Field::Avail => &mut txn.avail,
                    Field::Amount => &mut txn.amount,
                    Field::Fitid => &mut txn.fitid,
                    Field::Name => &mut txn.name,
                    Field::Memo => &mut txn.memo,
                    Field::PayeeName => &mut txn.payee_name,
                    Field::Cursym => &mut txn.cursym,
                };
                if slot.is_some() {
                    // The same element twice in one transaction is not a statement.
                    return Err(malformed());
                }
                *slot = value;
                Ok(())
            }
        }
    }

    fn finish_txn<F: FnMut(NewLine)>(&mut self, txn: &Txn, on_line: &mut F) {
        self.tally.rows_read = self.tally.rows_read.saturating_add(1);
        match self.build(txn) {
            Ok(line) => {
                if self.tally.rows_refused == 0 {
                    on_line(line);
                }
            }
            Err(code) => self.tally.refuse(RowRefusal::new(self.ordinal, code)),
        }
    }

    /// Validates and normalizes one transaction (profile `ofx` v1).
    fn build(&self, txn: &Txn) -> Result<NewLine, RefusalCode> {
        if let Some(code) = txn.error {
            return Err(code);
        }
        let posted = txn
            .posted
            .as_deref()
            .ok_or(RefusalCode::MissingRequiredField)?;
        let booking_date = ofx_date(posted)?;
        let value_date = txn.avail.as_deref().map(ofx_date).transpose()?;
        let currency = self.context.currency;
        let amount_text = txn
            .amount
            .as_deref()
            .ok_or(RefusalCode::MissingRequiredField)?;
        let amount = ofx_amount(amount_text, currency)?;
        if txn
            .cursym
            .as_deref()
            .is_some_and(|sym| sym != currency.code())
        {
            return Err(RefusalCode::CurrencyMismatch);
        }
        let fitid = txn
            .fitid
            .as_deref()
            .ok_or(RefusalCode::MissingRequiredField)?;
        let name = txn.name.as_deref().or(txn.payee_name.as_deref());
        let memo = txn.memo.as_deref();
        if [Some(fitid), name, memo]
            .into_iter()
            .flatten()
            .any(is_formula)
        {
            return Err(RefusalCode::FormulaCell);
        }
        let narration_raw = match (name, memo) {
            (Some(n), Some(m)) => format!("{n} {m}"),
            (Some(t), None) | (None, Some(t)) => t.to_owned(),
            (None, None) => return Err(RefusalCode::MissingRequiredField),
        };
        let narration_norm = normalize(&narration_raw);
        if narration_norm.is_empty() {
            return Err(RefusalCode::MissingRequiredField);
        }
        let tuple = tuple_hash(&LineTuple {
            account: self.context.account,
            booking_date,
            amount,
            narration_norm: &narration_norm,
            reference: Some(fitid),
        });
        Ok(NewLine {
            row: self.ordinal,
            booking_date,
            value_date,
            amount,
            narration_raw,
            narration_norm,
            reference: Some(fitid.to_owned()),
            counterparty_raw: name.map(str::to_owned),
            counterparty_norm: name.map(normalize).filter(|c| !c.is_empty()),
            balance_after: None,
            tuple_hash: tuple,
        })
    }
}

/// An element's value: trimmed of white space, `None` when empty.
fn leaf_value(text: &[u8], overflow: bool) -> Result<Option<String>, RefusalCode> {
    if overflow {
        return Err(RefusalCode::FieldTooLong);
    }
    let text = std::str::from_utf8(text).map_err(|_| RefusalCode::InvalidUtf8)?;
    let text = text.trim_matches([' ', '\t', '\r', '\n']);
    if text.chars().any(is_forbidden) {
        return Err(RefusalCode::ForbiddenCharacter);
    }
    Ok((!text.is_empty()).then(|| text.to_owned()))
}

/// Decodes an entity reference's name (between `&` and `;`). Nothing else is resolved.
fn entity(name: &[u8]) -> Result<char, FileRefusal> {
    let refuse = || refused(RefusalCode::EntityRefused);
    match name {
        b"amp" => Ok('&'),
        b"lt" => Ok('<'),
        b"gt" => Ok('>'),
        b"quot" => Ok('"'),
        b"apos" => Ok('\''),
        _ => {
            let digits = name.strip_prefix(b"#").ok_or_else(refuse)?;
            let (digits, radix) = match digits {
                [b'x' | b'X', hex @ ..] => (hex, 16),
                decimal => (decimal, 10),
            };
            if digits.is_empty() || digits.len() > 8 {
                return Err(refuse());
            }
            let text = std::str::from_utf8(digits).map_err(|_| refuse())?;
            let scalar = u32::from_str_radix(text, radix).map_err(|_| refuse())?;
            char::from_u32(scalar).ok_or_else(refuse)
        }
    }
}

/// An attribute's value in an XML declaration or the OFX instruction.
fn xml_attribute<'t>(text: &'t str, name: &str) -> Option<&'t str> {
    let mut rest = text;
    while let Some(at) = rest.find(name) {
        let after = rest.get(at.saturating_add(name.len())..)?;
        let before_ok = at == 0
            || rest
                .get(..at)
                .is_some_and(|b| b.ends_with([' ', '\t', '\r', '\n']));
        let value = after.trim_start().strip_prefix('=').map(str::trim_start);
        if let (true, Some(value)) = (before_ok, value) {
            let quote = value.chars().next().filter(|q| matches!(q, '"' | '\''))?;
            let inner = value.get(1..)?;
            return inner.split_once(quote).map(|(v, _)| v);
        }
        rest = after;
    }
    None
}

/// `DTPOSTED`/`DTAVAIL`: `YYYYMMDD`, then optionally a time and zone (`HHMMSS.XXX[-5:EST]`),
/// of which only the calendar date is kept.
fn ofx_date(text: &str) -> Result<Date, RefusalCode> {
    let bytes = text.as_bytes();
    let (date, rest) = (bytes.get(..8), bytes.get(8..).unwrap_or_default());
    let date = date.ok_or(RefusalCode::InvalidDate)?;
    if !date.iter().all(u8::is_ascii_digit) {
        return Err(RefusalCode::InvalidDate);
    }
    let allowed =
        |b: &u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'[' | b']' | b':' | b'+' | b'-');
    if !rest.iter().all(allowed) {
        return Err(RefusalCode::InvalidDate);
    }
    let number = |range: std::ops::Range<usize>| -> u32 {
        date.get(range)
            .unwrap_or_default()
            .iter()
            .fold(0_u32, |n, d| {
                n.saturating_mul(10)
                    .saturating_add(u32::from(d.wrapping_sub(b'0')))
            })
    };
    let year = u16::try_from(number(0..4)).map_err(|_| RefusalCode::InvalidDate)?;
    let month = u8::try_from(number(4..6)).map_err(|_| RefusalCode::InvalidDate)?;
    let day = u8::try_from(number(6..8)).map_err(|_| RefusalCode::InvalidDate)?;
    Date::new(year, month, day).map_err(RefusalCode::from)
}

/// `TRNAMT`: a plain decimal with `.`, an optional leading `+` or `-`; never zero.
fn ofx_amount(text: &str, currency: recon_domain::Currency) -> Result<Money, RefusalCode> {
    let plain = match text.strip_prefix('+') {
        Some(rest) if rest.starts_with(['+', '-']) || rest.is_empty() => {
            return Err(RefusalCode::InvalidAmount);
        }
        Some(rest) => rest,
        None => text,
    };
    let amount = Money::parse(plain, currency).map_err(RefusalCode::from)?;
    if amount.is_zero() {
        return Err(RefusalCode::ZeroAmount);
    }
    Ok(amount)
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
    use proptest::prelude::*;
    use recon_domain::{AccountId, Currency};
    use uuid::Uuid;

    use RefusalCode as C;

    fn ctx() -> AccountContext {
        AccountContext {
            account: AccountId::from_uuid(Uuid::from_u128(21)),
            currency: Currency::Usd,
        }
    }

    const SGML_HEAD: &str = "OFXHEADER:100\r\nDATA:OFXSGML\r\nVERSION:102\r\nSECURITY:NONE\r\n\
        ENCODING:USASCII\r\nCHARSET:1252\r\nCOMPRESSION:NONE\r\nOLDFILEUID:NONE\r\nNEWFILEUID:NONE\r\n\r\n";

    fn sgml_body(transactions: &str) -> String {
        format!(
            "<OFX>\r\n<SIGNONMSGSRSV1><SONRS><STATUS><CODE>0<SEVERITY>INFO</STATUS>\
             <DTSERVER>20260331120000<LANGUAGE>ENG</SONRS></SIGNONMSGSRSV1>\r\n\
             <BANKMSGSRSV1><STMTTRNRS><TRNUID>1<STATUS><CODE>0<SEVERITY>INFO</STATUS>\r\n\
             <STMTRS><CURDEF>USD<BANKACCTFROM><BANKID>121000248<ACCTID>99887766<ACCTTYPE>CHECKING</BANKACCTFROM>\r\n\
             <BANKTRANLIST><DTSTART>20260301<DTEND>20260331\r\n{transactions}</BANKTRANLIST>\r\n\
             <LEDGERBAL><BALAMT>1000.00<DTASOF>20260331</LEDGERBAL>\r\n\
             </STMTRS></STMTTRNRS></BANKMSGSRSV1>\r\n</OFX>\r\n"
        )
    }

    fn sgml(transactions: &str) -> Vec<u8> {
        format!("{SGML_HEAD}{}", sgml_body(transactions)).into_bytes()
    }

    const TXN_A: &str = "<STMTTRN><TRNTYPE>DEBIT<DTPOSTED>20260302120000.000[-5:EST]<DTAVAIL>20260303\
        <TRNAMT>-125.50<FITID>F0001<NAME>ACME SUPPLIES<MEMO>INV 77</STMTTRN>\r\n";
    const TXN_B: &str = "<STMTTRN><TRNTYPE>CREDIT<DTPOSTED>20260305<TRNAMT>+2000.00<FITID>F0002\
        <PAYEE><NAME>Payroll Co<ADDR1>1 Main St</PAYEE></STMTTRN>\r\n";
    const TXN_C: &str = "<STMTTRN><TRNTYPE>FEE<DTPOSTED>20260306<TRNAMT>-3.00<FITID>F0003<MEMO>Service fee\r\n</STMTTRN>\r\n";

    fn xml(transactions: &str) -> Vec<u8> {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"no\"?>\n\
             <?OFX OFXHEADER=\"200\" VERSION=\"211\" SECURITY=\"NONE\" OLDFILEUID=\"NONE\" NEWFILEUID=\"NONE\"?>\n\
             <OFX><BANKMSGSRSV1><STMTTRNRS><TRNUID>1</TRNUID><STMTRS><CURDEF>USD</CURDEF>\n\
             <BANKACCTFROM><BANKID>1</BANKID><ACCTID>2</ACCTID><ACCTTYPE>CHECKING</ACCTTYPE></BANKACCTFROM>\n\
             <BANKTRANLIST><DTSTART>20260301</DTSTART><DTEND>20260331</DTEND>\n{transactions}</BANKTRANLIST>\n\
             </STMTRS></STMTTRNRS></BANKMSGSRSV1></OFX>\n"
        )
        .into_bytes()
    }

    const XML_A: &str = "<STMTTRN><TRNTYPE>DEBIT</TRNTYPE><DTPOSTED>20260302</DTPOSTED>\
        <TRNAMT>-42.10</TRNAMT><FITID>X1</FITID><NAME>AT&amp;T Wireless</NAME><MEMO></MEMO></STMTTRN>\n";
    const XML_B: &str = "<STMTTRN><TRNTYPE>CREDIT</TRNTYPE><DTPOSTED>20260304</DTPOSTED>\
        <TRNAMT>99.00</TRNAMT><FITID>X2</FITID><NAME>Refund &#8211; &#x41;cme</NAME></STMTTRN>\n";

    fn run(input: &[u8], chunk: usize) -> (Result<ParseSummary, FileRefusal>, Vec<NewLine>) {
        run_with(OfxParser::new(ctx()), input, chunk)
    }

    fn run_with(
        mut parser: OfxParser,
        input: &[u8],
        chunk: usize,
    ) -> (Result<ParseSummary, FileRefusal>, Vec<NewLine>) {
        let mut lines = vec![];
        let mut push = |l| lines.push(l);
        for part in input.chunks(chunk.max(1)) {
            if let Err(e) = parser.push(part, &mut push) {
                return (Err(e), lines);
            }
        }
        let summary = parser.finish(&mut push);
        (summary, lines)
    }

    fn file_code(input: &[u8]) -> RefusalCode {
        match run(input, 7).0 {
            Err(e) => e.code,
            Ok(s) => panic!("expected a file refusal, got {s:?}"),
        }
    }

    fn row_codes(input: &[u8]) -> Vec<(u32, RefusalCode)> {
        let summary = run(input, 5).0.unwrap();
        summary.refusals.iter().map(|r| (r.row, r.code)).collect()
    }

    #[test]
    fn sgml_1x_reads_its_transactions_and_its_charset() {
        let mut input = sgml(&format!("{TXN_A}{TXN_B}{TXN_C}"));
        // A Windows-1252 é in a narration (the header says CHARSET:1252).
        let at = input.windows(6).position(|w| w == b"ACME S").unwrap();
        input[at + 2] = 0xE9;
        let (summary, lines) = run(&input, usize::MAX);
        let summary = summary.unwrap();
        assert!(summary.is_clean(), "{:?}", summary.refusals);
        assert_eq!((summary.rows_read, lines.len()), (3, 3));
        assert_eq!(
            summary.file_hash.0,
            <[u8; 32]>::from(Sha256::digest(&input))
        );
        let a = &lines[0];
        assert_eq!(a.row, 1);
        assert_eq!(a.booking_date, Date::new(2026, 3, 2).unwrap());
        assert_eq!(a.value_date, Some(Date::new(2026, 3, 3).unwrap()));
        assert_eq!(a.amount.minor(), -12_550);
        assert_eq!(a.reference.as_deref(), Some("F0001"));
        assert_eq!(a.narration_raw, "ACéE SUPPLIES INV 77");
        assert_eq!(a.counterparty_raw.as_deref(), Some("ACéE SUPPLIES"));
        assert_eq!(a.balance_after, None);
        let b = &lines[1];
        assert_eq!((b.row, b.amount.minor()), (2, 200_000));
        assert_eq!(b.narration_raw, "Payroll Co");
        assert_eq!(b.counterparty_norm.as_deref(), Some("PAYROLL CO"));
        let c = &lines[2];
        assert_eq!(
            (c.narration_raw.as_str(), c.counterparty_raw.as_deref()),
            ("Service fee", None)
        );
        // The tuple is the line's identity, exactly as for CSV rows.
        assert_eq!(
            a.tuple_hash,
            tuple_hash(&LineTuple {
                account: ctx().account,
                booking_date: a.booking_date,
                amount: a.amount,
                narration_norm: &a.narration_norm,
                reference: Some("F0001"),
            })
        );
    }

    #[test]
    fn xml_2x_decodes_predefined_and_numeric_references_only() {
        let (summary, lines) = run(&xml(&format!("{XML_A}{XML_B}")), 3);
        assert!(summary.unwrap().is_clean());
        assert_eq!(lines[0].narration_raw, "AT&T Wireless");
        assert_eq!(lines[0].amount.minor(), -4_210);
        assert_eq!(lines[1].narration_raw, "Refund – Acme");
        assert_eq!(lines[1].row, 2);
        // A UTF-8 byte-order mark before the declaration is allowed.
        let mut marked = vec![0xEF, 0xBB, 0xBF];
        marked.extend(xml(XML_A));
        assert!(run(&marked, 2).0.unwrap().is_clean());
        assert_eq!(
            file_code(&xml(&XML_A.replace("&amp;", "&nbsp;"))),
            C::EntityRefused
        );
        assert_eq!(
            file_code(&xml(&XML_A.replace("&amp;", "& "))),
            C::EntityRefused
        );
        assert_eq!(
            file_code(&xml(&XML_A.replace("&amp;", "&#xD800;"))),
            C::EntityRefused
        );
        // A reference to a forbidden character is refused by its row, like the character.
        assert_eq!(
            row_codes(&xml(&XML_A.replace("&amp;", "&#8238;"))),
            [(1, C::ForbiddenCharacter)]
        );
    }

    #[test]
    fn declarations_are_refused_unread() {
        for markup in [
            "<!DOCTYPE OFX [<!ENTITY a \"x\">]>",
            "<!ENTITY lol \"lol\">",
            "<!-- a comment -->",
            "<![CDATA[x]]>",
        ] {
            let input = String::from_utf8(xml(XML_A))
                .unwrap()
                .replace("<OFX>", &format!("{markup}<OFX>"));
            assert_eq!(
                file_code(input.as_bytes()),
                C::DeclarationRefused,
                "{markup}"
            );
            let inside = String::from_utf8(sgml(TXN_A))
                .unwrap()
                .replace("<STMTTRN>", &format!("{markup}<STMTTRN>"));
            assert_eq!(
                file_code(inside.as_bytes()),
                C::DeclarationRefused,
                "{markup}"
            );
        }
    }

    #[test]
    fn malformed_structure_is_one_named_refusal() {
        let good = String::from_utf8(sgml(TXN_A)).unwrap();
        for (what, input) in [
            (
                "mismatched end",
                good.replace("</BANKTRANLIST>", "</STMTRS>"),
            ),
            ("text after the root", format!("{good}trailing")),
            ("truncated", good[..good.len() - 30].to_owned()),
            ("no header", good.replace(SGML_HEAD, "")),
            (
                "no OFX root",
                good.replace("<OFX>", "<XFO>").replace("</OFX>", "</XFO>"),
            ),
            ("lower-case tag", good.replace("<TRNAMT>", "<trnamt>")),
            (
                "a tag with attributes",
                good.replace("<TRNAMT>", "<TRNAMT x=1>"),
            ),
            (
                "a long tag",
                good.replace("<MEMO>", &format!("<{}>", "M".repeat(33))),
            ),
            ("no CURDEF", good.replace("<CURDEF>USD", "")),
            (
                "a repeated field",
                good.replace("<FITID>F0001", "<FITID>F0001<FITID>F0009"),
            ),
            ("no statement", good.replace("STMTRS>", "OTHERRS>")),
            (
                "a transaction outside the statement",
                good.replace("<OFX>\r\n", &format!("<OFX>\r\n{TXN_A}")),
            ),
            (
                "an instruction in SGML",
                good.replace("<OFX>", "<?x?><OFX>"),
            ),
        ] {
            assert_eq!(file_code(input.as_bytes()), C::MalformedOfx, "{what}");
        }
        let xml_good = String::from_utf8(xml(XML_A)).unwrap();
        assert_eq!(
            file_code(
                xml_good
                    .replace("<?OFX OFXHEADER=\"200\"", "<?OFX OFXHEADER=\"100\"")
                    .as_bytes()
            ),
            C::MalformedOfx
        );
        assert_eq!(
            file_code(xml_good.replace("<STMTRS>", "<?late?><STMTRS>").as_bytes()),
            C::MalformedOfx
        );
        // Nesting deeper than 32.
        let deep = format!("{}x{}", "<A>".repeat(40), "</A>".repeat(40));
        assert_eq!(
            file_code(
                good.replace("<BANKTRANLIST>", &format!("{deep}<BANKTRANLIST>"))
                    .as_bytes()
            ),
            C::MalformedOfx
        );
    }

    #[test]
    fn one_statement_in_the_accounts_currency() {
        let two = String::from_utf8(sgml(TXN_A)).unwrap().replace(
            "</STMTTRNRS>",
            "</STMTTRNRS><STMTTRNRS><STMTRS><CURDEF>USD</STMTRS></STMTTRNRS>",
        );
        assert_eq!(file_code(two.as_bytes()), C::MultipleStatements);
        let euro = String::from_utf8(sgml(TXN_A))
            .unwrap()
            .replace("<CURDEF>USD", "<CURDEF>EUR");
        assert_eq!(file_code(euro.as_bytes()), C::CurrencyMismatch);
        let card = String::from_utf8(sgml(TXN_A))
            .unwrap()
            .replace("STMTRS>", "CCSTMTRS>");
        assert!(run(card.as_bytes(), 9).0.unwrap().is_clean());
        // A transaction in another currency is a row refusal (no FX matching, D-005).
        let fx = TXN_A.replace(
            "<MEMO>",
            "<ORIGCURRENCY><CURRATE>1.1<CURSYM>EUR</ORIGCURRENCY><MEMO>",
        );
        assert_eq!(row_codes(&sgml(&fx)), [(1, C::CurrencyMismatch)]);
        let same = TXN_A.replace("<MEMO>", "<CURRENCY><CURRATE>1<CURSYM>USD</CURRENCY><MEMO>");
        assert!(run(&sgml(&same), 4).0.unwrap().is_clean());
    }

    #[test]
    fn encodings_are_declared_by_the_file_and_otherwise_refused() {
        let utf8 = String::from_utf8(sgml(TXN_A))
            .unwrap()
            .replace("ENCODING:USASCII", "ENCODING:UTF-8");
        let (summary, lines) = run(utf8.replace("ACME", "ÆCME").as_bytes(), 6);
        assert!(summary.unwrap().is_clean());
        assert_eq!(lines[0].counterparty_raw.as_deref(), Some("ÆCME SUPPLIES"));
        // A Windows-1252 byte in a file that says UTF-8 is refused by its row.
        let mut bad = utf8.into_bytes();
        let at = bad.windows(4).position(|w| w == b"ACME").unwrap();
        bad[at] = 0xC6;
        assert_eq!(row_codes(&bad), [(1, C::InvalidUtf8)]);
        let unicode = String::from_utf8(sgml(TXN_A))
            .unwrap()
            .replace("ENCODING:USASCII", "ENCODING:UNICODE");
        assert_eq!(file_code(unicode.as_bytes()), C::UnsupportedEncoding);
        let charset = String::from_utf8(sgml(TXN_A))
            .unwrap()
            .replace("CHARSET:1252", "CHARSET:KOI8");
        assert_eq!(file_code(charset.as_bytes()), C::UnsupportedEncoding);
        let utf16 = String::from_utf8(xml(XML_A))
            .unwrap()
            .replace("encoding=\"UTF-8\"", "encoding=\"UTF-16\"");
        assert_eq!(file_code(utf16.as_bytes()), C::UnsupportedEncoding);
        let mut marked = vec![0xFF, 0xFE];
        marked.extend(xml(XML_A));
        assert_eq!(file_code(&marked), C::UnsupportedEncoding);
    }

    #[test]
    fn every_row_refusal_is_named_counted_and_stops_the_lines() {
        let txns = [
            TXN_A.to_owned(),
            TXN_A.replace(
                "<DTPOSTED>20260302120000.000[-5:EST]",
                "<DTPOSTED>2026-03-02",
            ),
            TXN_A.replace("<DTPOSTED>20260302120000.000[-5:EST]", "<DTPOSTED>20260230"),
            TXN_A.replace(
                "<DTPOSTED>20260302120000.000[-5:EST]",
                "<DTPOSTED>20260302 12:00",
            ),
            TXN_A.replace("<DTPOSTED>20260302120000.000[-5:EST]", ""),
            TXN_A.replace("-125.50", "-125.505"),
            TXN_A.replace("-125.50", "-0.00"),
            TXN_A.replace("-125.50", "1,25"),
            TXN_A.replace("-125.50", "+-1"),
            TXN_A.replace("<FITID>F0001", ""),
            TXN_A.replace("<NAME>ACME SUPPLIES<MEMO>INV 77", ""),
            TXN_A.replace("<NAME>ACME SUPPLIES", "<NAME>=HYPERLINK(1)"),
            TXN_A.replace("<FITID>F0001", "<FITID>@cmd"),
            TXN_A.replace("<MEMO>INV 77", &format!("<MEMO>{}", "M".repeat(9_000))),
            // A tab: a control character in any charset.
            TXN_A.replace("<MEMO>INV 77", "<MEMO>a\tb"),
            "<STMTTRN></STMTTRN>".to_owned(),
            TXN_B.to_owned(),
        ];
        let (summary, lines) = run(&sgml(&txns.concat()), 13);
        let summary = summary.unwrap();
        assert_eq!(summary.rows_read, 17);
        let codes: Vec<_> = summary.refusals.iter().map(|r| (r.row, r.code)).collect();
        assert_eq!(
            codes,
            [
                (2, C::InvalidDate),
                (3, C::InvalidDate),
                (4, C::InvalidDate),
                (5, C::MissingRequiredField),
                (6, C::TooManyDecimalPlaces),
                (7, C::ZeroAmount),
                (8, C::InvalidAmount),
                (9, C::InvalidAmount),
                (10, C::MissingRequiredField),
                (11, C::MissingRequiredField),
                (12, C::FormulaCell),
                (13, C::FormulaCell),
                (14, C::FieldTooLong),
                (15, C::ForbiddenCharacter),
                (16, C::MissingRequiredField),
            ]
        );
        assert_eq!(summary.rows_refused, 15);
        // The first row went on before the refusals; the last valid one did not.
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn bounds_and_empty_statements() {
        let small = CsvLimits {
            max_data_rows: 2,
            ..CsvLimits::default()
        };
        let (r, _) = run_with(
            OfxParser::with_limits(ctx(), small),
            &sgml(&format!("{TXN_A}{TXN_B}{TXN_C}")),
            8,
        );
        assert_eq!(r.unwrap_err(), FileRefusal::new(C::TooManyRows, Some(3)));
        let tiny = CsvLimits {
            max_file_bytes: 100,
            ..CsvLimits::default()
        };
        let (r, _) = run_with(OfxParser::with_limits(ctx(), tiny), &sgml(TXN_A), 64);
        assert_eq!(r.unwrap_err().code, C::FileTooLarge);
        assert_eq!(file_code(&sgml("")), C::EmptyFile);
        assert_eq!(file_code(b""), C::EmptyFile);
        assert_eq!(file_code(b"OFXHEAD"), C::MalformedOfx);
        // A header that never ends.
        let endless = format!("OFXHEADER:100\r\n{}", "X:Y\r\n".repeat(1_000));
        assert_eq!(file_code(endless.as_bytes()), C::MalformedOfx);
        // A refused file stays refused.
        let mut parser = OfxParser::new(ctx());
        let first = parser
            .push(b"<!DOCTYPE x>....................", &mut |_| {})
            .unwrap_err();
        assert_eq!(parser.push(b"more", &mut |_| {}), Err(first));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Streaming is invisible: any chunking gives the same outcome as one piece.
        #[test]
        fn any_chunking_reads_the_same(chunk in 1_usize..40) {
            for input in [sgml(&format!("{TXN_A}{TXN_B}{TXN_C}")), xml(&format!("{XML_A}{XML_B}"))] {
                prop_assert_eq!(run(&input, usize::MAX), run(&input, chunk));
            }
        }

        /// Arbitrary bytes, alone or after a valid prefix, never panic.
        #[test]
        fn arbitrary_bytes_never_panic(
            tail in proptest::collection::vec(any::<u8>(), 0..400),
            cut in 0_usize..600,
        ) {
            let _ = run(&tail, 3);
            for good in [sgml(TXN_A), xml(XML_A)] {
                let mut input = good[..cut.min(good.len())].to_vec();
                input.extend_from_slice(&tail);
                let _ = run(&input, 11);
            }
        }
    }
}
