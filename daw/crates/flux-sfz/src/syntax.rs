//! Lossless syntax tree of a single SFZ file.
//!
//! A [`Document`] is the file as written: headers, opcodes, `#define` / `#include` directives and comments,
//! in order, with the exact whitespace between them. Nothing is followed or expanded here (that is
//! [`crate::Instrument`]'s job), so writing an unmodified document back gives the original bytes, and an
//! edited one keeps everything that was not touched.
//!
//! Tokenisation follows sfizz 1.2.3 (verified against the library, see the tests):
//!
//! * Whitespace (space, tab, CR, LF) separates items; `//` comments run to the end of the line and `/* */`
//!   comments may span lines.
//! * `<name>` is a header; the name must be an identifier (`[A-Za-z0-9_]+`).
//! * `name=value` is an opcode. The name is made of identifier characters and `$` (for variables). The value
//!   runs to the end of the line and may contain spaces (`sample=Grand Piano/A0 v1.wav`), but it stops before
//!   another opcode on the same line (whitespace, then a name that does not start with `$`, then `=`), a
//!   header (`<`), a comment (`//`, `/*`) or a directive (`#define`, `#include`). Surrounding whitespace is
//!   trimmed. Note that `//` ends a value even inside a path (`sample=a//b.wav` is the sample `a`), exactly as
//!   in sfizz.
//! * `#define $NAME value` defines a variable; the value is a single whitespace-free token.
//! * `#include "path"` includes another file.
//! * On a syntax error the rest of the line is skipped (kept here as an [`ItemKind::Invalid`] item).

use crate::error::{Diagnostic, Location, Result, Severity};
use crate::text::{self, Encoding};
use std::borrow::Cow;
use std::fmt;
use std::path::Path;

/// One SFZ file as written. See the [module documentation](self).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Document {
    items: Vec<Item>,
    trailing: String,
    encoding: Encoding,
}

/// One element of a [`Document`]: a header, an opcode, a directive or a comment, plus the whitespace in front
/// of it.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    lead: String,
    kind: ItemKind,
    /// The exact source text, as long as the item has not been changed.
    raw: Option<String>,
    line: u32,
    column: u32,
}

/// What an [`Item`] is. Opcode names and values are raw: `$VARIABLES` are not expanded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemKind {
    /// `<name>`, without the angle brackets.
    Header(String),
    /// `name=value`; the value is trimmed.
    Opcode { name: String, value: String },
    /// `#define $name value` (the name without the `$`).
    Define { name: String, value: String },
    /// `#include "path"` (the path as written, without quotes).
    Include(String),
    /// A comment including its delimiters (`// text` or `/* text */`).
    Comment(String),
    /// Text sfizz rejects (it reports an error and skips the rest of the line).
    Invalid(String),
}

impl Item {
    fn new(kind: ItemKind, lead: &str) -> Item {
        Item { lead: lead.to_owned(), kind, raw: None, line: 0, column: 0 }
    }

    /// A header on a new line.
    pub fn header(name: impl Into<String>) -> Item {
        Item::new(ItemKind::Header(name.into()), "\n")
    }

    /// An opcode on a new line.
    pub fn opcode(name: impl Into<String>, value: impl Into<String>) -> Item {
        Item::new(ItemKind::Opcode { name: name.into(), value: value.into() }, "\n")
    }

    /// A `#define` on a new line (`name` without the `$`).
    pub fn define(name: impl Into<String>, value: impl Into<String>) -> Item {
        Item::new(ItemKind::Define { name: name.into(), value: value.into() }, "\n")
    }

    /// An `#include` on a new line.
    pub fn include(path: impl Into<String>) -> Item {
        Item::new(ItemKind::Include(path.into()), "\n")
    }

    /// A comment on a new line; `text` must include the `//` or `/* */` delimiters.
    pub fn comment(text: impl Into<String>) -> Item {
        Item::new(ItemKind::Comment(text.into()), "\n")
    }

    /// Replace the whitespace in front of the item (e.g. `" "` to keep it on the previous item's line).
    pub fn with_lead(mut self, lead: impl Into<String>) -> Item {
        self.lead = lead.into();
        self
    }

    pub fn kind(&self) -> &ItemKind {
        &self.kind
    }

    /// Replace what the item is; it will be written in canonical form from now on.
    pub fn set_kind(&mut self, kind: ItemKind) {
        self.kind = kind;
        self.raw = None;
    }

    /// The whitespace in front of the item.
    pub fn lead(&self) -> &str {
        &self.lead
    }

    pub fn set_lead(&mut self, lead: impl Into<String>) {
        self.lead = lead.into();
    }

    /// Where the item started in the source (1-based); 0 for items created in code.
    pub fn line(&self) -> u32 {
        self.line
    }

    pub fn column(&self) -> u32 {
        self.column
    }

    /// True if the item was created or changed in code (it is written in canonical form).
    pub fn is_modified(&self) -> bool {
        self.raw.is_none()
    }

    /// The opcode's name and value, if the item is an opcode.
    pub fn as_opcode(&self) -> Option<(&str, &str)> {
        match &self.kind {
            ItemKind::Opcode { name, value } => Some((name, value)),
            _ => None,
        }
    }

    /// The header name, if the item is a header.
    pub fn as_header(&self) -> Option<&str> {
        match &self.kind {
            ItemKind::Header(h) => Some(h),
            _ => None,
        }
    }

    /// Change an opcode's value (no-op on other items). Returns whether the item is an opcode.
    pub fn set_value(&mut self, new_value: impl Into<String>) -> bool {
        match &mut self.kind {
            ItemKind::Opcode { value, .. } => {
                *value = new_value.into();
                self.raw = None;
                true
            }
            _ => false,
        }
    }

    /// The item's text without the leading whitespace: the original source text if unchanged, else the
    /// canonical form (`<name>`, `name=value`, `#define $name value`, `#include "path"`).
    pub fn text(&self) -> Cow<'_, str> {
        if let Some(raw) = &self.raw {
            return Cow::Borrowed(raw);
        }
        match &self.kind {
            ItemKind::Header(h) => Cow::Owned(format!("<{h}>")),
            ItemKind::Opcode { name, value } => Cow::Owned(format!("{name}={value}")),
            ItemKind::Define { name, value } => Cow::Owned(format!("#define ${name} {value}")),
            ItemKind::Include(p) => Cow::Owned(format!("#include \"{p}\"")),
            ItemKind::Comment(t) | ItemKind::Invalid(t) => Cow::Borrowed(t),
        }
    }
}

impl Document {
    /// An empty document (UTF-8).
    pub fn new() -> Document {
        Document::default()
    }

    /// Parse SFZ text. `file` is only used for the locations in the diagnostics.
    pub fn parse(text: &str, file: &Path) -> (Document, Vec<Diagnostic>) {
        let mut lx = Lexer::new(text);
        lx.run();
        let lines = LineIndex::new(text);
        let mut items = lx.items;
        for it in &mut items {
            let (l, c) = lines.position(text, it.line as usize);
            it.line = l;
            it.column = c;
        }
        let diags = lx
            .diags
            .into_iter()
            .map(|(off, severity, message)| {
                let (line, column) = lines.position(text, off);
                Diagnostic { severity, location: Location::new(file, line, column), message }
            })
            .collect();
        (Document { items, trailing: lx.trailing, encoding: Encoding::Utf8 }, diags)
    }

    /// Parse raw file bytes (UTF-8 with or without BOM; anything else is read as Latin-1 with a warning).
    pub fn parse_bytes(bytes: &[u8], file: &Path) -> (Document, Vec<Diagnostic>) {
        let (text, encoding) = text::decode(bytes);
        let (mut doc, mut diags) = Document::parse(&text, file);
        doc.encoding = encoding;
        if encoding == Encoding::Latin1 {
            diags.insert(
                0,
                Diagnostic {
                    severity: Severity::Warning,
                    location: Location::new(file, 1, 1),
                    message: "the file is not valid UTF-8 (read as Latin-1); sfizz reads paths as UTF-8, so non-ASCII \
                              sample names will not be found"
                        .into(),
                },
            );
        }
        (doc, diags)
    }

    /// Read and parse a file.
    pub fn read(path: &Path) -> Result<(Document, Vec<Diagnostic>)> {
        let bytes = crate::error::read_file(path)?;
        Ok(Document::parse_bytes(&bytes, path))
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    /// The items, for editing (insert, remove, change).
    pub fn items_mut(&mut self) -> &mut Vec<Item> {
        &mut self.items
    }

    pub fn push(&mut self, item: Item) {
        self.items.push(item);
    }

    /// Whitespace after the last item.
    pub fn trailing(&self) -> &str {
        &self.trailing
    }

    pub fn set_trailing(&mut self, trailing: impl Into<String>) {
        self.trailing = trailing.into();
    }

    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    pub fn set_encoding(&mut self, encoding: Encoding) {
        self.encoding = encoding;
    }

    /// The document as text (unmodified documents give back the source exactly).
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for it in &self.items {
            out.push_str(&it.lead);
            out.push_str(&it.text());
        }
        out.push_str(&self.trailing);
        out
    }

    /// The document as bytes in its original encoding.
    pub fn to_bytes(&self) -> Vec<u8> {
        text::encode(&self.to_text(), self.encoding)
    }

    /// Remove the item at `index` together with the comments that follow it on the same line, so no comment
    /// is left dangling at the end of the previous line. Returns how many items were removed.
    pub fn remove_with_trailing_comments(&mut self, index: usize) -> usize {
        let mut end = index + 1;
        while end < self.items.len()
            && matches!(self.items[end].kind, ItemKind::Comment(_))
            && !self.items[end].lead.contains(['\n', '\r'])
        {
            end += 1;
        }
        // keep the line structure: the item after the removed run inherits the removed item's lead when the
        // removed run started a line and the next item continues on it
        let lead = self.items[index].lead.clone();
        self.items.drain(index..end);
        if let Some(next) = self.items.get_mut(index)
            && lead.contains('\n')
            && !next.lead.contains('\n')
        {
            next.lead = lead;
        }
        end - index
    }
}

impl fmt::Display for Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_text())
    }
}

// ---------------------------------------------------------------------------------------------
// character classes

/// `[A-Za-z0-9_]`: header names, variable names, opcode names after expansion.
pub(crate) fn is_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Opcode names as written may contain variables.
fn is_name_char(c: u8) -> bool {
    is_ident_char(c) || c == b'$'
}

pub(crate) fn is_identifier(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(is_ident_char)
}

fn is_blank(c: u8) -> bool {
    c == b' ' || c == b'\t'
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\r' | b'\n')
}

/// Where an opcode value that starts at `v[0]` ends (`v` runs to the end of the line). See the module docs.
pub(crate) fn value_end(v: &[u8]) -> usize {
    let n = v.len();
    let mut i = 0;
    while i < n {
        match v[i] {
            b'<' => return i,
            b'/' if i + 1 < n && (v[i + 1] == b'/' || v[i + 1] == b'*') => return i,
            b'#' if v[i..].starts_with(b"#define") || v[i..].starts_with(b"#include") => return i,
            b'=' => {
                let mut j = i;
                while j > 0 && is_name_char(v[j - 1]) {
                    j -= 1;
                }
                // `label_cc$N=` starts an opcode, `$K=` does not (sfizz 1.2.3)
                if j < i && j > 0 && is_blank(v[j - 1]) && v[j] != b'$' {
                    let mut k = j;
                    while k > 0 && is_blank(v[k - 1]) {
                        k -= 1;
                    }
                    return k;
                }
            }
            _ => {}
        }
        i += 1;
    }
    n
}

// ---------------------------------------------------------------------------------------------
// lexer

struct Lexer<'a> {
    src: &'a str,
    b: &'a [u8],
    pos: usize,
    items: Vec<Item>,
    trailing: String,
    /// (byte offset, severity, message)
    diags: Vec<(usize, Severity, String)>,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Lexer<'a> {
        Lexer { src, b: src.as_bytes(), pos: 0, items: vec![], trailing: String::new(), diags: vec![] }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }

    fn at(&self, s: &[u8]) -> bool {
        self.b[self.pos..].starts_with(s)
    }

    fn line_end(&self, from: usize) -> usize {
        self.b[from..].iter().position(|&c| c == b'\n' || c == b'\r').map_or(self.b.len(), |p| from + p)
    }

    fn skip_while(&mut self, f: impl Fn(u8) -> bool) {
        while self.pos < self.b.len() && f(self.b[self.pos]) {
            self.pos += 1;
        }
    }

    /// Store an item; `line` temporarily holds the byte offset (converted to line/column afterwards).
    fn push(&mut self, kind: ItemKind, lead_start: usize, start: usize) {
        self.items.push(Item {
            lead: self.src[lead_start..start].to_owned(),
            kind,
            raw: Some(self.src[start..self.pos].to_owned()),
            line: start as u32,
            column: 0,
        });
    }

    /// Report an error at `start` and skip the rest of the line, as sfizz does.
    fn fail(&mut self, lead_start: usize, start: usize, message: String) {
        self.diags.push((start, Severity::Error, message));
        self.pos = self.line_end(self.pos.max(start));
        let text = self.src[start..self.pos].to_owned();
        self.push(ItemKind::Invalid(text), lead_start, start);
    }

    fn run(&mut self) {
        loop {
            let lead_start = self.pos;
            self.skip_while(is_space);
            if self.at(b"//") {
                let start = self.pos;
                self.pos = self.line_end(start);
                let text = self.src[start..self.pos].to_owned();
                self.push(ItemKind::Comment(text), lead_start, start);
                continue;
            }
            if self.at(b"/*") {
                let start = self.pos;
                match self.src[start + 2..].find("*/") {
                    Some(p) => self.pos = start + 2 + p + 2,
                    None => {
                        self.diags.push((start, Severity::Error, "Unterminated block comment.".into()));
                        self.pos = self.b.len();
                    }
                }
                let text = self.src[start..self.pos].to_owned();
                self.push(ItemKind::Comment(text), lead_start, start);
                continue;
            }
            match self.peek() {
                None => {
                    self.trailing = self.src[lead_start..].to_owned();
                    return;
                }
                Some(b'#') => self.directive(lead_start),
                Some(b'<') => self.header(lead_start),
                Some(_) => self.opcode(lead_start),
            }
        }
    }

    fn directive(&mut self, lead_start: usize) {
        let start = self.pos;
        self.pos += 1;
        let name_start = self.pos;
        self.skip_while(is_ident_char);
        let name = &self.src[name_start..self.pos];
        match name {
            "define" => {
                self.skip_while(is_blank);
                if self.peek() != Some(b'$') {
                    return self.fail(lead_start, start, "Expected $identifier after #define.".into());
                }
                self.pos += 1;
                let id_start = self.pos;
                self.skip_while(is_ident_char);
                if self.pos == id_start {
                    return self.fail(lead_start, start, "Expected $identifier after #define.".into());
                }
                let var = self.src[id_start..self.pos].to_owned();
                self.skip_while(is_blank);
                let v_start = self.pos;
                while self.pos < self.b.len() && !is_space(self.b[self.pos]) && !self.at(b"//") && !self.at(b"/*") {
                    self.pos += 1;
                }
                let value = self.src[v_start..self.pos].to_owned();
                self.push(ItemKind::Define { name: var, value }, lead_start, start);
            }
            "include" => {
                self.skip_while(is_blank);
                if self.peek() != Some(b'"') {
                    return self.fail(lead_start, start, "Expected \"file.sfz\" after #include.".into());
                }
                self.pos += 1;
                let p_start = self.pos;
                self.skip_while(|c| c != b'"' && c != b'\r' && c != b'\n');
                if self.peek() != Some(b'"') {
                    return self.fail(lead_start, start, "Expected \"file.sfz\" after #include.".into());
                }
                let path = self.src[p_start..self.pos].to_owned();
                self.pos += 1;
                self.push(ItemKind::Include(path), lead_start, start);
            }
            _ => {
                let msg = format!("Unrecognized directive `{name}`.");
                self.fail(lead_start, start, msg)
            }
        }
    }

    fn header(&mut self, lead_start: usize) {
        let start = self.pos;
        self.pos += 1;
        let name_start = self.pos;
        self.skip_while(|c| c != b'>' && c != b'\r' && c != b'\n');
        if self.peek() != Some(b'>') {
            return self.fail(lead_start, start, "Expected `>` at end of header.".into());
        }
        let name = self.src[name_start..self.pos].to_owned();
        self.pos += 1;
        if !is_identifier(&name) {
            return self.fail(lead_start, start, format!("The header name `{name}` is not a valid identifier."));
        }
        self.push(ItemKind::Header(name), lead_start, start);
    }

    fn opcode(&mut self, lead_start: usize) {
        let start = self.pos;
        self.skip_while(is_name_char);
        if self.pos == start {
            return self.fail(lead_start, start, "Expected opcode name.".into());
        }
        if self.peek() != Some(b'=') {
            return self.fail(lead_start, start, "Expected `=` after opcode name.".into());
        }
        let name = self.src[start..self.pos].to_owned();
        self.pos += 1;
        let v_start = self.pos;
        let eol = self.line_end(v_start);
        let end = v_start + value_end(&self.b[v_start..eol]);
        let mut vs = v_start;
        while vs < end && is_blank(self.b[vs]) {
            vs += 1;
        }
        let mut ve = end;
        while ve > vs && is_blank(self.b[ve - 1]) {
            ve -= 1;
        }
        let value = self.src[vs..ve].to_owned();
        self.pos = if vs == ve { v_start } else { ve };
        self.push(ItemKind::Opcode { name, value }, lead_start, start);
    }
}

/// Byte offset -> (line, column), both 1-based; columns count characters.
struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(text: &str) -> LineIndex {
        let b = text.as_bytes();
        let mut starts = vec![0];
        for (i, &c) in b.iter().enumerate() {
            // CRLF counts once; a lone CR is a line break too
            if c == b'\n' || (c == b'\r' && b.get(i + 1) != Some(&b'\n')) {
                starts.push(i + 1);
            }
        }
        LineIndex { starts }
    }

    fn position(&self, text: &str, off: usize) -> (u32, u32) {
        let line = self.starts.partition_point(|&s| s <= off).max(1);
        let start = self.starts[line - 1];
        let col = text.get(start..off).map_or(off - start, |s| s.chars().count()) + 1;
        (line as u32, col as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> (Document, Vec<Diagnostic>) {
        Document::parse(text, Path::new("t.sfz"))
    }

    fn opcodes(text: &str) -> Vec<(String, String)> {
        parse(text)
            .0
            .items()
            .iter()
            .filter_map(|i| i.as_opcode().map(|(n, v)| (n.to_owned(), v.to_owned())))
            .collect()
    }

    fn ops(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[test]
    fn round_trip_is_byte_exact() {
        let src = "// head\r\n<control>\ndefault_path=Samples\\x/\n\n<global> volume=-3 /* c\n */ pan=1\n\
                   #define $K 60\n#include \"a b.sfz\"\n<region>sample=my file.wav lokey=$K hikey=c#4 // t\n\t\n";
        let (doc, diags) = parse(src);
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(doc.to_text(), src);
    }

    #[test]
    fn values_with_spaces_end_before_the_next_opcode() {
        // each case was checked against sfizz 1.2.3
        assert_eq!(
            opcodes("<region> sample=my file.wav lokey=60 hikey=61"),
            ops(&[("sample", "my file.wav"), ("lokey", "60"), ("hikey", "61")])
        );
        assert_eq!(opcodes("sw_label=a=b"), ops(&[("sw_label", "a=b")]));
        assert_eq!(opcodes("sw_label=x y=z"), ops(&[("sw_label", "x"), ("y", "z")]));
        assert_eq!(opcodes("sw_label=a b c"), ops(&[("sw_label", "a b c")]));
        assert_eq!(opcodes("sw_label=a\tb  c   "), ops(&[("sw_label", "a\tb  c")]));
        assert_eq!(opcodes("sw_label=  x"), ops(&[("sw_label", "x")]));
        assert_eq!(opcodes("key=60 = 5 lovel=10"), ops(&[("key", "60 = 5"), ("lovel", "10")]));
        assert_eq!(opcodes("sample=*sine  key = 61"), ops(&[("sample", "*sine  key = 61")]));
        assert_eq!(opcodes("key=64 bad-name=1 lovel=5"), ops(&[("key", "64 bad-name=1"), ("lovel", "5")]));
        assert_eq!(opcodes("sample= lokey=60"), ops(&[("sample", ""), ("lokey", "60")]));
        assert_eq!(opcodes("a=1\tb=2"), ops(&[("a", "1"), ("b", "2")]));
        assert_eq!(opcodes("lokey=1 label_cc$N=x"), ops(&[("lokey", "1"), ("label_cc$N", "x")]));
        assert_eq!(opcodes("sw_label=x k$K=66"), ops(&[("sw_label", "x"), ("k$K", "66")]));
        // a name that starts with a variable does not end the value...
        assert_eq!(opcodes("sample=*sine $K=66"), ops(&[("sample", "*sine $K=66")]));
        // ...but it is an opcode at the start of a line
        assert_eq!(opcodes("$K=67"), ops(&[("$K", "67")]));
    }

    #[test]
    fn values_stop_at_headers_comments_and_directives() {
        assert_eq!(opcodes("key=60<region>sample=*saw"), ops(&[("key", "60"), ("sample", "*saw")]));
        assert_eq!(opcodes("sample=*sine/*c*/key=62"), ops(&[("sample", "*sine"), ("key", "62")]));
        assert_eq!(opcodes("sample=*sine // comment key=70"), ops(&[("sample", "*sine")]));
        // like sfizz, `//` ends the value even inside a path
        assert_eq!(opcodes("sample=sub//a.wav"), ops(&[("sample", "sub")]));
        assert_eq!(opcodes("sw_label=a #define $Q 5"), ops(&[("sw_label", "a")]));
        assert_eq!(opcodes("sw_label=a #b"), ops(&[("sw_label", "a #b")]));
        assert_eq!(opcodes("sw_label=a#b"), ops(&[("sw_label", "a#b")]));
        assert_eq!(opcodes("sample=A#3_v1.wav"), ops(&[("sample", "A#3_v1.wav")]));
        assert_eq!(opcodes("sw_label=a /b /*c*/"), ops(&[("sw_label", "a /b")]));
    }

    #[test]
    fn directives() {
        let (doc, d) = parse("#define $NAME Long Name\n#include \"x/y.sfz\"\n#define $E\n");
        let kinds: Vec<_> = doc.items().iter().map(|i| i.kind().clone()).collect();
        assert_eq!(kinds[0], ItemKind::Define { name: "NAME".into(), value: "Long".into() });
        // sfizz: the define value is one token, "Name" is then read as an opcode name and rejected
        assert_eq!(kinds[1], ItemKind::Invalid("Name".into()));
        assert_eq!(kinds[2], ItemKind::Include("x/y.sfz".into()));
        assert_eq!(kinds[3], ItemKind::Define { name: "E".into(), value: "".into() });
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].message, "Expected `=` after opcode name.");
        assert_eq!((d[0].location.line, d[0].location.column), (1, 20));
        assert_eq!(opcodes("#define $S *sine // c\nsample=$S"), ops(&[("sample", "$S")]));
    }

    #[test]
    fn errors_skip_the_rest_of_the_line() {
        let (doc, d) = parse("<region> sample=*sine <bad header> key=61\n<region> key=62\n#foo key=1\n");
        assert_eq!(d.len(), 2);
        assert!(d[0].message.contains("not a valid identifier"));
        assert!(d[1].message.contains("Unrecognized directive"));
        let o: Vec<_> = doc.items().iter().filter_map(|i| i.as_opcode()).collect();
        assert_eq!(o, vec![("sample", "*sine"), ("key", "62")]);
        assert_eq!(doc.to_text(), "<region> sample=*sine <bad header> key=61\n<region> key=62\n#foo key=1\n");
    }

    #[test]
    fn header_errors() {
        let (_, d) = parse("<region\nkey=1");
        assert_eq!(d[0].message, "Expected `>` at end of header.");
        let (_, d) = parse("sample = x");
        assert_eq!(d[0].message, "Expected `=` after opcode name.");
        let (_, d) = parse("/* open");
        assert_eq!(d[0].message, "Unterminated block comment.");
        let (_, d) = parse("#include x.sfz");
        assert!(d[0].message.starts_with("Expected \"file.sfz\""));
    }

    #[test]
    fn locations_count_characters() {
        let (doc, _) = parse("// é\r\n  <région>\n\tkey=1");
        assert_eq!((doc.items()[1].line(), doc.items()[1].column()), (2, 3));
        let (doc, _) = parse("<group>\n\tkey=1");
        assert_eq!((doc.items()[1].line(), doc.items()[1].column()), (2, 2));
    }

    #[test]
    fn edits_render_canonically_and_keep_the_rest() {
        let (mut doc, _) = parse("<global>\nloop_mode=continuous // ARIA alias\ncutoff=250\nvar01_x=1 // gone\nvol=1\n");
        for it in doc.items_mut() {
            if it.as_opcode() == Some(("loop_mode", "continuous")) {
                it.set_value("loop_continuous");
            }
        }
        let idx = doc.items().iter().position(|i| i.as_opcode().is_some_and(|(n, _)| n == "cutoff")).unwrap();
        doc.items_mut().insert(idx + 1, Item::opcode("cutoff_oncc120", "6000"));
        let idx = doc.items().iter().position(|i| i.as_opcode().is_some_and(|(n, _)| n == "var01_x")).unwrap();
        assert_eq!(doc.remove_with_trailing_comments(idx), 2);
        assert_eq!(
            doc.to_text(),
            "<global>\nloop_mode=loop_continuous // ARIA alias\ncutoff=250\ncutoff_oncc120=6000\nvol=1\n"
        );
    }
}
