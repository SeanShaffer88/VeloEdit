//! Binary templates (roadmap item 17): a small plain-text
//! language for describing the layout of a binary structure —
//! a file header, a packet, a record — and a walker that lays a
//! definition over bytes and reports every field's offset, size
//! and decoded value.
//!
//! ```text
//! // A PE-flavoured header.
//! IMAGE_FILE_HEADER {
//!     u16le machine;
//!     u16le section_count;
//!     u32le time_stamp;
//!     u16le characteristics;
//! }
//! DOS_HEADER {
//!     char  magic[2];          // reads as the text "MZ"
//!     u8    reserved[58];
//!     u32le pe_header_offset;
//!     IMAGE_FILE_HEADER pe;    // nested structs flatten with dots
//! }
//! ```
//!
//! # The language
//!
//! A template file is one or more struct definitions:
//! `NAME { field; field; ... }`. A field is `TYPE name;` or
//! `TYPE name[COUNT];`. Types are the primitives `u8`, `i8`,
//! `char`, and the sized numbers `u16le u16be i16le i16be u32le
//! u32be i32le i32be u64le u64be i64le i64be f32le f32be f64le
//! f64be` (the unsuffixed `u16` / `i32` / `f64` forms mean
//! little-endian), or the name of another struct defined in the
//! same file. `//` comments run to end of line.
//!
//! `COUNT` is a decimal literal, or the name of an earlier field
//! of the same struct whose value gives the count at apply time
//! (the counted-array idiom every real header format uses: a
//! length field followed by that many records). The named field
//! must be a scalar integer field declared before the array.
//! Structs may nest but may not contain themselves, directly or
//! indirectly — a cycle is refused at parse time.
//!
//! How arrays surface in the applied field list:
//!
//! - `char[N]` is one field whose value is the text (when the
//!   bytes are printable) — headers are full of magic strings.
//! - `u8[N]` / `i8[N]` are one field holding the raw bytes.
//! - Any other primitive array is one field per element, named
//!   `name[0]`, `name[1]`, ...
//! - A struct array is the struct's fields per element, named
//!   `name[0].field`, ...
//!
//! # Applying
//!
//! Applying reads through the same `FnMut(u64, &mut [u8]) ->
//! usize` closure shape as the other cores, starting at a base
//! offset, and yields a flat list of leaf fields with dotted
//! paths. Truncation policy: **a short source yields a partial
//! result, never an error.** A field whose bytes run out gets no
//! value and the display `(end of data)`; layout continues while
//! offsets are still computable (they are, for every
//! fixed-size field), and the result's `complete` flag is false.
//! When the walk truly cannot continue — an array count field
//! itself has no usable value, so later offsets are unknowable —
//! it stops there, still flagged incomplete.
//!
//! Bounds: at most [`MAX_FIELDS`] fields are emitted, at most
//! [`MAX_ARRAY_ELEMENTS`] elements per array, nesting at most
//! [`MAX_DEPTH`] structs deep — past a cap the walk stops and
//! reports incomplete rather than producing an unbounded list.

use std::collections::HashMap;
use std::fmt;

/// Cap on emitted fields per apply.
pub const MAX_FIELDS: usize = 8192;

/// Cap on elements in one array.
pub const MAX_ARRAY_ELEMENTS: u64 = 1_000_000;

/// Cap on struct nesting depth at apply time.
pub const MAX_DEPTH: usize = 32;

/// A parse failure: 1-based `line` in the template text and a
/// plain-words message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl ParseError {
    fn at(line: usize, message: impl Into<String>) -> Self {
        Self {
            line,
            message: message.into(),
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "template error at line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

/// Why an apply could not start at all. (Mid-walk problems are
/// not errors — see the module docs on truncation.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyError {
    /// No struct with this name exists in the set.
    UnknownStruct(String),
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApplyError::UnknownStruct(name) => {
                write!(f, "no struct named {name:?} in the template set")
            }
        }
    }
}

impl std::error::Error for ApplyError {}

// ---------------------------------------------------------------------
// Types

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Endian {
    Little,
    Big,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Prim {
    Char,
    U8,
    I8,
    U16(Endian),
    I16(Endian),
    U32(Endian),
    I32(Endian),
    U64(Endian),
    I64(Endian),
    F32(Endian),
    F64(Endian),
}

impl Prim {
    fn size(self) -> u64 {
        match self {
            Prim::Char | Prim::U8 | Prim::I8 => 1,
            Prim::U16(_) | Prim::I16(_) => 2,
            Prim::U32(_) | Prim::I32(_) | Prim::F32(_) => 4,
            Prim::U64(_) | Prim::I64(_) | Prim::F64(_) => 8,
        }
    }

    fn is_integer(self) -> bool {
        !matches!(self, Prim::F32(_) | Prim::F64(_))
    }

    fn from_name(name: &str) -> Option<Prim> {
        use Endian::{Big as B, Little as L};
        Some(match name {
            "char" => Prim::Char,
            "u8" => Prim::U8,
            "i8" => Prim::I8,
            "u16" | "u16le" => Prim::U16(L),
            "u16be" => Prim::U16(B),
            "i16" | "i16le" => Prim::I16(L),
            "i16be" => Prim::I16(B),
            "u32" | "u32le" => Prim::U32(L),
            "u32be" => Prim::U32(B),
            "i32" | "i32le" => Prim::I32(L),
            "i32be" => Prim::I32(B),
            "u64" | "u64le" => Prim::U64(L),
            "u64be" => Prim::U64(B),
            "i64" | "i64le" => Prim::I64(L),
            "i64be" => Prim::I64(B),
            "f32" | "f32le" => Prim::F32(L),
            "f32be" => Prim::F32(B),
            "f64" | "f64le" => Prim::F64(L),
            "f64be" => Prim::F64(B),
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TypeRef {
    Prim(Prim),
    Struct(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ArrayCount {
    Literal(u64),
    /// Name of an earlier scalar integer field of the same
    /// struct; its applied value is the count.
    Field(String),
}

#[derive(Clone, Debug)]
struct FieldDef {
    name: String,
    ty: TypeRef,
    array: Option<ArrayCount>,
    line: usize,
}

#[derive(Clone, Debug)]
struct StructDef {
    name: String,
    fields: Vec<FieldDef>,
    line: usize,
}

// ---------------------------------------------------------------------
// Lexer

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Ident(String),
    Number(u64),
    Punct(char),
    Eof,
}

struct Lexer<'a> {
    chars: std::str::Chars<'a>,
    peeked: Option<char>,
    line: usize,
}

impl<'a> Lexer<'a> {
    fn new(text: &'a str) -> Self {
        let mut chars = text.chars();
        let peeked = chars.next();
        Self {
            chars,
            peeked,
            line: 1,
        }
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peeked;
        self.peeked = self.chars.next();
        if ch == Some('\n') {
            self.line += 1;
        }
        ch
    }

    fn peek(&self) -> Option<char> {
        self.peeked
    }

    /// Skip whitespace and `//` comments.
    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => {
                    self.bump();
                }
                Some('/') => {
                    // Look ahead for the second slash without
                    // committing: a lone '/' is not trivia (and
                    // will be a lex error at token time).
                    let mut clone = self.chars.clone();
                    if clone.next() == Some('/') {
                        while let Some(c) = self.bump() {
                            if c == '\n' {
                                break;
                            }
                        }
                    } else {
                        return;
                    }
                }
                _ => return,
            }
        }
    }

    fn next_token(&mut self) -> Result<(Token, usize), ParseError> {
        self.skip_trivia();
        let line = self.line;
        match self.peek() {
            None => Ok((Token::Eof, line)),
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {
                let mut name = String::new();
                while let Some(c) = self.peek() {
                    if c.is_ascii_alphanumeric() || c == '_' {
                        name.push(c);
                        self.bump();
                    } else {
                        break;
                    }
                }
                Ok((Token::Ident(name), line))
            }
            Some(c) if c.is_ascii_digit() => {
                let mut value: u64 = 0;
                while let Some(c) = self.peek() {
                    match c.to_digit(10) {
                        Some(d) => {
                            value = value
                                .checked_mul(10)
                                .and_then(|v| v.checked_add(d as u64))
                                .ok_or_else(|| ParseError::at(line, "number is too large"))?;
                            self.bump();
                        }
                        None => break,
                    }
                }
                Ok((Token::Number(value), line))
            }
            Some(c) => {
                self.bump();
                Ok((Token::Punct(c), line))
            }
        }
    }
}

// ---------------------------------------------------------------------
// Parser

struct TParser<'a> {
    lexer: Lexer<'a>,
    look: Option<(Token, usize)>,
}

impl<'a> TParser<'a> {
    fn new(text: &'a str) -> Result<Self, ParseError> {
        let mut lexer = Lexer::new(text);
        let look = Some(lexer.next_token()?);
        Ok(Self { lexer, look })
    }

    fn peek(&self) -> (&Token, usize) {
        let (tok, line) = self.look.as_ref().expect("lookahead filled");
        (tok, *line)
    }

    fn advance(&mut self) -> Result<(Token, usize), ParseError> {
        let current = self.look.take().expect("lookahead filled");
        self.look = Some(self.lexer.next_token()?);
        Ok(current)
    }

    fn expect_punct(&mut self, want: char) -> Result<(), ParseError> {
        let (tok, line) = self.advance()?;
        if tok == Token::Punct(want) {
            Ok(())
        } else {
            Err(ParseError::at(
                line,
                format!("expected {want:?} but found {}", describe(&tok)),
            ))
        }
    }

    fn expect_ident(&mut self, what: &str) -> Result<(String, usize), ParseError> {
        let (tok, line) = self.advance()?;
        match tok {
            Token::Ident(name) => Ok((name, line)),
            other => Err(ParseError::at(
                line,
                format!("expected {what} but found {}", describe(&other)),
            )),
        }
    }

    fn parse_struct(&mut self) -> Result<StructDef, ParseError> {
        let (name, line) = self.expect_ident("a struct name")?;
        self.expect_punct('{')?;
        let mut fields: Vec<FieldDef> = Vec::new();
        loop {
            match self.peek().0.clone() {
                Token::Punct('}') => {
                    self.advance()?;
                    break;
                }
                Token::Eof => {
                    return Err(ParseError::at(
                        line,
                        format!("struct {name} is missing its closing '}}'"),
                    ));
                }
                _ => {
                    let field = self.parse_field(&name, &fields)?;
                    fields.push(field);
                }
            }
        }
        // A C-style trailing semicolon after the definition is
        // tolerated.
        if *self.peek().0 == Token::Punct(';') {
            self.advance()?;
        }
        Ok(StructDef { name, fields, line })
    }

    fn parse_field(
        &mut self,
        struct_name: &str,
        earlier: &[FieldDef],
    ) -> Result<FieldDef, ParseError> {
        let (type_name, line) = self.expect_ident("a field type")?;
        let ty = match Prim::from_name(&type_name) {
            Some(prim) => TypeRef::Prim(prim),
            None => TypeRef::Struct(type_name),
        };
        let (field_name, _) = self.expect_ident("a field name")?;
        if earlier.iter().any(|f| f.name == field_name) {
            return Err(ParseError::at(
                line,
                format!("struct {struct_name} has two fields named {field_name:?}"),
            ));
        }
        let array = if *self.peek().0 == Token::Punct('[') {
            self.advance()?;
            let count = match self.advance()? {
                (Token::Number(n), _) => ArrayCount::Literal(n),
                (Token::Ident(name), count_line) => {
                    // The count-from-field form: the name must be
                    // an earlier scalar integer field of this
                    // struct.
                    match earlier.iter().find(|f| f.name == name) {
                        None => {
                            return Err(ParseError::at(
                                count_line,
                                format!(
                                    "array count {name:?} is not an earlier field of struct {struct_name}"
                                ),
                            ));
                        }
                        Some(target) => {
                            let ok = target.array.is_none()
                                && matches!(&target.ty, TypeRef::Prim(p) if p.is_integer() && *p != Prim::Char);
                            if !ok {
                                return Err(ParseError::at(
                                    count_line,
                                    format!(
                                        "array count {name:?} must name a scalar integer field"
                                    ),
                                ));
                            }
                            ArrayCount::Field(name)
                        }
                    }
                }
                (other, count_line) => {
                    return Err(ParseError::at(
                        count_line,
                        format!("expected an array count but found {}", describe(&other)),
                    ));
                }
            };
            self.expect_punct(']')?;
            Some(count)
        } else {
            None
        };
        self.expect_punct(';')?;
        Ok(FieldDef {
            name: field_name,
            ty,
            array,
            line,
        })
    }
}

fn describe(tok: &Token) -> String {
    match tok {
        Token::Ident(name) => format!("the name {name:?}"),
        Token::Number(n) => format!("the number {n}"),
        Token::Punct(c) => format!("{c:?}"),
        Token::Eof => "the end of the file".to_string(),
    }
}

// ---------------------------------------------------------------------
// The parsed set

/// A set of struct definitions parsed from template text.
#[derive(Clone, Debug)]
pub struct TemplateSet {
    structs: Vec<StructDef>,
    by_name: HashMap<String, usize>,
    /// Static byte size per struct, where computable (a struct
    /// with a field-counted array has no static size).
    sizes: Vec<Option<u64>>,
}

impl TemplateSet {
    /// Parse template text. Every error carries the 1-based line
    /// it was found on.
    pub fn parse(text: &str) -> Result<TemplateSet, ParseError> {
        let mut parser = TParser::new(text)?;
        let mut structs = Vec::new();
        let mut by_name: HashMap<String, usize> = HashMap::new();
        while *parser.peek().0 != Token::Eof {
            let def = parser.parse_struct()?;
            if by_name.contains_key(&def.name) {
                return Err(ParseError::at(
                    def.line,
                    format!("struct {} is defined twice", def.name),
                ));
            }
            by_name.insert(def.name.clone(), structs.len());
            structs.push(def);
        }
        // Resolve type references and compute static sizes;
        // unknown names and containment cycles are refused here.
        let mut sizes = vec![None; structs.len()];
        for idx in 0..structs.len() {
            let mut visiting: Vec<usize> = Vec::new();
            compute_size(idx, &structs, &by_name, &mut sizes, &mut visiting)?;
        }
        Ok(TemplateSet {
            structs,
            by_name,
            sizes,
        })
    }

    /// The names of the structs in the set, in definition order.
    pub fn struct_names(&self) -> impl Iterator<Item = &str> {
        self.structs.iter().map(|def| def.name.as_str())
    }

    /// The static byte size of a struct, when it has one (a
    /// struct containing a field-counted array does not).
    pub fn static_size(&self, name: &str) -> Option<u64> {
        self.by_name.get(name).map(|&idx| self.sizes[idx])?
    }

    /// Lay the named struct over the source at `base`, reading
    /// through `read_at` (see the module docs for the truncation
    /// policy).
    pub fn apply(
        &self,
        name: &str,
        base: u64,
        read_at: impl FnMut(u64, &mut [u8]) -> usize,
    ) -> Result<AppliedStruct, ApplyError> {
        let &idx = self
            .by_name
            .get(name)
            .ok_or_else(|| ApplyError::UnknownStruct(name.to_string()))?;
        let mut walker = Walker {
            set: self,
            read_at,
            fields: Vec::new(),
            complete: true,
            stopped: false,
        };
        let mut offset = base;
        walker.walk_struct(idx, name, 0, &mut offset);
        Ok(AppliedStruct {
            name: name.to_string(),
            offset: base,
            size: offset.saturating_sub(base),
            complete: walker.complete && !walker.stopped,
            fields: walker.fields,
        })
    }

    /// Apply over an in-memory byte string (the small-input /
    /// test convenience).
    pub fn apply_bytes(
        &self,
        name: &str,
        base: u64,
        data: &[u8],
    ) -> Result<AppliedStruct, ApplyError> {
        self.apply(name, base, move |offset: u64, out: &mut [u8]| -> usize {
            let start = offset as usize;
            if start >= data.len() {
                return 0;
            }
            let n = out.len().min(data.len() - start);
            out[..n].copy_from_slice(&data[start..start + n]);
            n
        })
    }
}

/// Static size of struct `idx`: `None` when any reachable field
/// is a field-counted array. Refuses unknown types and cycles.
fn compute_size(
    idx: usize,
    structs: &[StructDef],
    by_name: &HashMap<String, usize>,
    sizes: &mut [Option<u64>],
    visiting: &mut Vec<usize>,
) -> Result<Option<u64>, ParseError> {
    if let Some(cached) = sizes.get(idx).copied().flatten().map(Some) {
        return Ok(cached);
    }
    if visiting.contains(&idx) {
        return Err(ParseError::at(
            structs[idx].line,
            format!(
                "struct {} contains itself (templates must not be recursive)",
                structs[idx].name
            ),
        ));
    }
    visiting.push(idx);
    let mut total: u64 = 0;
    let mut dynamic = false;
    for field in &structs[idx].fields {
        let elem = match &field.ty {
            TypeRef::Prim(prim) => prim.size(),
            TypeRef::Struct(name) => {
                let &sub = by_name.get(name).ok_or_else(|| {
                    ParseError::at(
                        field.line,
                        format!("unknown type {name:?} (not a primitive, not a defined struct)"),
                    )
                })?;
                match compute_size(sub, structs, by_name, sizes, visiting)? {
                    Some(size) => size,
                    None => {
                        dynamic = true;
                        0
                    }
                }
            }
        };
        match &field.array {
            None => total += elem,
            Some(ArrayCount::Literal(n)) => total += elem.saturating_mul(*n),
            Some(ArrayCount::Field(_)) => dynamic = true,
        }
    }
    visiting.pop();
    let result = if dynamic { None } else { Some(total) };
    sizes[idx] = result;
    Ok(result)
}

// ---------------------------------------------------------------------
// Applying

/// One decoded value.
#[derive(Clone, Debug, PartialEq)]
pub enum FieldValue {
    UInt(u64),
    Int(i64),
    Float(f64),
    /// Raw bytes (`u8`/`i8` arrays, and unprintable char data).
    Bytes(Vec<u8>),
    /// Text (char scalars and printable char arrays).
    Text(String),
}

/// One leaf field of an applied struct.
#[derive(Clone, Debug, PartialEq)]
pub struct AppliedField {
    /// Dotted path from the root struct: `pe.machine`,
    /// `items[2].x`.
    pub path: String,
    /// Absolute offset in the source.
    pub offset: u64,
    /// Byte size of the field.
    pub size: u64,
    /// The decoded value, or `None` when the source ran out
    /// before this field's bytes.
    pub value: Option<FieldValue>,
    /// The display form: integers as `4660 (0x1234)`, text as
    /// itself, bytes as a hex preview, and `(end of data)` when
    /// there is no value.
    pub display: String,
}

/// The result of laying a struct over a source.
#[derive(Clone, Debug, PartialEq)]
pub struct AppliedStruct {
    pub name: String,
    /// The base offset the struct was applied at.
    pub offset: u64,
    /// Bytes spanned by the walk.
    pub size: u64,
    /// False when the source ran out or a cap stopped the walk
    /// early (see the module docs).
    pub complete: bool,
    /// Leaf fields in layout order.
    pub fields: Vec<AppliedField>,
}

impl AppliedStruct {
    /// Look up a field by its dotted path.
    pub fn field(&self, path: &str) -> Option<&AppliedField> {
        self.fields.iter().find(|f| f.path == path)
    }
}

struct Walker<'a, F>
where
    F: FnMut(u64, &mut [u8]) -> usize,
{
    set: &'a TemplateSet,
    read_at: F,
    fields: Vec<AppliedField>,
    complete: bool,
    stopped: bool,
}

impl<F> Walker<'_, F>
where
    F: FnMut(u64, &mut [u8]) -> usize,
{
    /// Read exactly `len` bytes at `offset`; `None` on a short
    /// read. Reads go through 64 KiB windows so a huge field
    /// never forces a matching allocation up front — the output
    /// Vec is the field's own bytes, which the caps bound.
    fn read_exact(&mut self, offset: u64, len: u64) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        let mut chunk = [0u8; 64 * 1024];
        let mut pos = offset;
        let mut remaining = len;
        while remaining > 0 {
            let want = remaining.min(chunk.len() as u64) as usize;
            let got = (self.read_at)(pos, &mut chunk[..want]);
            if got == 0 {
                return None;
            }
            let got = got.min(want);
            out.extend_from_slice(&chunk[..got]);
            pos += got as u64;
            remaining -= got as u64;
        }
        Some(out)
    }

    fn push_field(&mut self, field: AppliedField) -> bool {
        if self.fields.len() >= MAX_FIELDS {
            self.complete = false;
            self.stopped = true;
            return false;
        }
        self.fields.push(field);
        true
    }

    /// Walk one struct instance at `*offset`, advancing the
    /// offset past everything it spans. `path` is the dotted
    /// prefix for this instance (the root call passes the struct
    /// name, nested calls pass `parent.field` etc.). Returns
    /// false when the walk must stop entirely (a cap, or a count
    /// that could not be resolved).
    fn walk_struct(&mut self, idx: usize, path: &str, depth: usize, offset: &mut u64) -> bool {
        if depth > MAX_DEPTH {
            self.complete = false;
            return false;
        }
        let def = &self.set.structs[idx];
        // Integer values of this instance's scalar fields, for
        // count-from-field arrays. `None` records "had a field
        // but no usable count value".
        let mut counts: HashMap<String, Option<u64>> = HashMap::new();
        for field in &def.fields {
            if self.stopped {
                return false;
            }
            let field_path = format!("{path}.{}", field.name);
            let resolved = match &field.array {
                None => Some(1),
                Some(ArrayCount::Literal(n)) => Some(*n),
                Some(ArrayCount::Field(name)) => counts.get(name).copied().flatten(),
            };
            let Some(count) = resolved else {
                // The count field had no usable value: later
                // offsets are unknowable. Stop, flagged.
                self.complete = false;
                return false;
            };
            let count = if count > MAX_ARRAY_ELEMENTS {
                self.complete = false;
                MAX_ARRAY_ELEMENTS
            } else {
                count
            };
            match &field.ty {
                TypeRef::Prim(prim) => {
                    if !self.walk_prim(field, *prim, count, &field_path, offset, &mut counts) {
                        return false;
                    }
                }
                TypeRef::Struct(name) => {
                    let sub = self.set.by_name[name];
                    for i in 0..count {
                        let elem_path = if field.array.is_some() {
                            format!("{field_path}[{i}]")
                        } else {
                            field_path.clone()
                        };
                        if !self.walk_struct(sub, &elem_path, depth + 1, offset) {
                            return false;
                        }
                    }
                }
            }
        }
        true
    }

    /// Walk one primitive field (scalar or array) at `*offset`.
    fn walk_prim(
        &mut self,
        field: &FieldDef,
        prim: Prim,
        count: u64,
        path: &str,
        offset: &mut u64,
        counts: &mut HashMap<String, Option<u64>>,
    ) -> bool {
        let elem = prim.size();
        let total = elem.saturating_mul(count);
        let at = *offset;
        *offset += total;
        if field.array.is_none() {
            // Scalar: decode one value, and remember it as a
            // possible array count for later fields.
            let bytes = self.read_exact(at, elem);
            let value = bytes.as_deref().and_then(|b| decode_scalar(prim, b));
            if bytes.is_none() {
                self.complete = false;
            }
            if prim.is_integer() && prim != Prim::Char {
                let as_count = match &value {
                    Some(FieldValue::UInt(v)) => Some(*v),
                    Some(FieldValue::Int(v)) => u64::try_from(*v).ok(),
                    _ => None,
                };
                counts.insert(field.name.clone(), as_count);
            }
            return self.push_field(AppliedField {
                path: path.to_string(),
                offset: at,
                size: elem,
                display: display_of(value.as_ref()),
                value,
            });
        }
        match prim {
            // char[N]: one text field.
            Prim::Char => {
                let bytes = self.read_exact(at, total);
                if bytes.is_none() {
                    self.complete = false;
                }
                let value = bytes.map(|b| bytes_value(&b));
                self.push_field(AppliedField {
                    path: path.to_string(),
                    offset: at,
                    size: total,
                    display: display_of(value.as_ref()),
                    value,
                })
            }
            // u8[N] / i8[N]: one raw-bytes field.
            Prim::U8 | Prim::I8 => {
                let bytes = self.read_exact(at, total);
                if bytes.is_none() {
                    self.complete = false;
                }
                let value = bytes.map(FieldValue::Bytes);
                self.push_field(AppliedField {
                    path: path.to_string(),
                    offset: at,
                    size: total,
                    display: display_of(value.as_ref()),
                    value,
                })
            }
            // Other numeric arrays: one field per element.
            _ => {
                for i in 0..count {
                    let elem_at = at + i * elem;
                    let bytes = self.read_exact(elem_at, elem);
                    if bytes.is_none() {
                        self.complete = false;
                    }
                    let value = bytes.as_deref().and_then(|b| decode_scalar(prim, b));
                    if !self.push_field(AppliedField {
                        path: format!("{path}[{i}]"),
                        offset: elem_at,
                        size: elem,
                        display: display_of(value.as_ref()),
                        value,
                    }) {
                        return false;
                    }
                }
                true
            }
        }
    }
}

/// Decode one scalar primitive from its bytes. Returns `None`
/// when the slice is shorter than the primitive — callers pass
/// exactly-sized reads, so this is a belt-and-braces guard, not
/// an expected path.
fn decode_scalar(prim: Prim, bytes: &[u8]) -> Option<FieldValue> {
    if (bytes.len() as u64) < prim.size() {
        return None;
    }
    Some(match prim {
        Prim::Char => bytes_value(&bytes[..1]),
        Prim::U8 => FieldValue::UInt(bytes[0] as u64),
        Prim::I8 => FieldValue::Int(bytes[0] as i8 as i64),
        Prim::U16(e) => FieldValue::UInt(read_int::<2>(bytes, e)),
        Prim::I16(e) => FieldValue::Int(read_int::<2>(bytes, e) as i16 as i64),
        Prim::U32(e) => FieldValue::UInt(read_int::<4>(bytes, e)),
        Prim::I32(e) => FieldValue::Int(read_int::<4>(bytes, e) as i32 as i64),
        Prim::U64(e) => FieldValue::UInt(read_int::<8>(bytes, e)),
        Prim::I64(e) => FieldValue::Int(read_int::<8>(bytes, e) as i64),
        Prim::F32(e) => {
            let bits = read_int::<4>(bytes, e) as u32;
            FieldValue::Float(f32::from_bits(bits) as f64)
        }
        Prim::F64(e) => {
            let bits = read_int::<8>(bytes, e);
            FieldValue::Float(f64::from_bits(bits))
        }
    })
}

/// Read an N-byte integer as u64 (sign handled by the caller via
/// the target type's cast).
fn read_int<const N: usize>(bytes: &[u8], endian: Endian) -> u64 {
    let mut buf = [0u8; 8];
    let n = N.min(bytes.len());
    match endian {
        Endian::Little => buf[..n].copy_from_slice(&bytes[..n]),
        Endian::Big => buf[8 - n..].copy_from_slice(&bytes[..n]),
    }
    match endian {
        Endian::Little => u64::from_le_bytes(buf),
        Endian::Big => u64::from_be_bytes(buf),
    }
}

/// A char field's value: text when every byte is printable
/// ASCII (or a trailing NUL padding), else raw bytes.
fn bytes_value(bytes: &[u8]) -> FieldValue {
    let trimmed = match bytes.iter().rposition(|&b| b != 0) {
        Some(last) => &bytes[..=last],
        None => &bytes[..0],
    };
    if trimmed.iter().all(|&b| (0x20..0x7F).contains(&b)) {
        FieldValue::Text(String::from_utf8_lossy(trimmed).into_owned())
    } else {
        FieldValue::Bytes(bytes.to_vec())
    }
}

fn display_of(value: Option<&FieldValue>) -> String {
    match value {
        None => "(end of data)".to_string(),
        Some(FieldValue::UInt(v)) => format!("{v} (0x{v:x})"),
        Some(FieldValue::Int(v)) => {
            if *v < 0 {
                format!("{v} (-0x{:x})", v.unsigned_abs())
            } else {
                format!("{v} (0x{v:x})")
            }
        }
        Some(FieldValue::Float(v)) => format!("{v}"),
        Some(FieldValue::Text(s)) => s.clone(),
        Some(FieldValue::Bytes(bytes)) => {
            if bytes.is_empty() {
                return "(empty)".to_string();
            }
            let preview: Vec<String> = bytes.iter().take(16).map(|b| format!("{b:02x}")).collect();
            if bytes.len() > 16 {
                format!("{} … ({} bytes)", preview.join(" "), bytes.len())
            } else {
                preview.join(" ")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PE_TEMPLATE: &str = "\
// A trimmed PE-flavoured layout.
IMAGE_FILE_HEADER {
    u16le machine;
    u16le section_count;
    u32le time_stamp;
    u16le optional_header_size;
    u16le characteristics;
}
DOS_HEADER {
    char  magic[2];
    u8    reserved[60];
    u32le pe_header_offset;
    IMAGE_FILE_HEADER pe;
    f32le scale_factor;
    i16be signed_be;
}
";

    fn pe_bytes() -> Vec<u8> {
        let mut data = vec![0u8; 2 + 60 + 4 + 12 + 4 + 2];
        data[0] = b'M';
        data[1] = b'Z';
        // pe_header_offset at 62.
        data[62..66].copy_from_slice(&64u32.to_le_bytes());
        // pe.* starts at 66: machine 0x8664, sections 3,
        // time_stamp 0x5E2B1234, opt size 240, chars 0x2022.
        data[66..68].copy_from_slice(&0x8664u16.to_le_bytes());
        data[68..70].copy_from_slice(&3u16.to_le_bytes());
        data[70..74].copy_from_slice(&0x5E2B_1234u32.to_le_bytes());
        data[74..76].copy_from_slice(&240u16.to_le_bytes());
        data[76..78].copy_from_slice(&0x2022u16.to_le_bytes());
        // scale_factor 1.5f at 78; signed_be -2 at 82.
        data[78..82].copy_from_slice(&1.5f32.to_le_bytes());
        data[82..84].copy_from_slice(&(-2i16).to_be_bytes());
        data
    }

    #[test]
    fn pe_header_decodes_known_values() {
        let set = TemplateSet::parse(PE_TEMPLATE).unwrap();
        assert_eq!(set.static_size("DOS_HEADER"), Some(84));
        assert_eq!(set.static_size("IMAGE_FILE_HEADER"), Some(12));
        let applied = set.apply_bytes("DOS_HEADER", 0, &pe_bytes()).unwrap();
        assert!(applied.complete);
        assert_eq!(applied.size, 84);
        let magic = applied.field("DOS_HEADER.magic").unwrap();
        assert_eq!(magic.value, Some(FieldValue::Text("MZ".to_string())));
        assert_eq!(magic.offset, 0);
        assert_eq!(magic.size, 2);
        let offset_field = applied.field("DOS_HEADER.pe_header_offset").unwrap();
        assert_eq!(offset_field.value, Some(FieldValue::UInt(64)));
        assert_eq!(offset_field.display, "64 (0x40)");
        let machine = applied.field("DOS_HEADER.pe.machine").unwrap();
        assert_eq!(machine.offset, 66);
        assert_eq!(machine.value, Some(FieldValue::UInt(0x8664)));
        assert_eq!(machine.display, "34404 (0x8664)");
        let stamp = applied.field("DOS_HEADER.pe.time_stamp").unwrap();
        assert_eq!(stamp.value, Some(FieldValue::UInt(0x5E2B_1234)));
        let scale = applied.field("DOS_HEADER.scale_factor").unwrap();
        assert_eq!(scale.value, Some(FieldValue::Float(1.5)));
        assert_eq!(scale.display, "1.5");
        let signed = applied.field("DOS_HEADER.signed_be").unwrap();
        assert_eq!(signed.value, Some(FieldValue::Int(-2)));
        assert_eq!(signed.display, "-2 (-0x2)");
    }

    #[test]
    fn apply_at_a_base_offset_shifts_everything() {
        let set = TemplateSet::parse(PE_TEMPLATE).unwrap();
        // Place the header at offset 16 in a bigger blob.
        let mut blob = vec![0u8; 16];
        blob.extend_from_slice(&pe_bytes());
        let applied = set.apply_bytes("DOS_HEADER", 16, &blob).unwrap();
        assert!(applied.complete);
        assert_eq!(applied.field("DOS_HEADER.magic").unwrap().offset, 16);
        assert_eq!(applied.field("DOS_HEADER.pe.machine").unwrap().offset, 82);
    }

    #[test]
    fn counted_arrays_use_the_earlier_field() {
        let set = TemplateSet::parse("BLOB { u16le count; u32le items[count]; u8 tail; }").unwrap();
        assert_eq!(set.static_size("BLOB"), None); // dynamic
        let mut data = Vec::new();
        data.extend_from_slice(&3u16.to_le_bytes());
        for v in [10u32, 20, 30] {
            data.extend_from_slice(&v.to_le_bytes());
        }
        data.push(0xFF);
        let applied = set.apply_bytes("BLOB", 0, &data).unwrap();
        assert!(applied.complete);
        assert_eq!(applied.size, 2 + 12 + 1);
        assert_eq!(
            applied.field("BLOB.items[0]").unwrap().value,
            Some(FieldValue::UInt(10))
        );
        assert_eq!(
            applied.field("BLOB.items[2]").unwrap().value,
            Some(FieldValue::UInt(30))
        );
        assert_eq!(applied.field("BLOB.items[2]").unwrap().offset, 10);
        assert_eq!(
            applied.field("BLOB.tail").unwrap().value,
            Some(FieldValue::UInt(0xFF))
        );
    }

    #[test]
    fn struct_arrays_and_byte_arrays() {
        let set = TemplateSet::parse(
            "POINT { i16le x; i16le y; } SHAPE { POINT corners[2]; u8 blob[4]; }",
        )
        .unwrap();
        let mut data = Vec::new();
        for (x, y) in [(1i16, -1i16), (300, 400)] {
            data.extend_from_slice(&x.to_le_bytes());
            data.extend_from_slice(&y.to_le_bytes());
        }
        data.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let applied = set.apply_bytes("SHAPE", 0, &data).unwrap();
        assert!(applied.complete);
        assert_eq!(
            applied.field("SHAPE.corners[0].x").unwrap().value,
            Some(FieldValue::Int(1))
        );
        assert_eq!(
            applied.field("SHAPE.corners[1].y").unwrap().value,
            Some(FieldValue::Int(400))
        );
        let blob = applied.field("SHAPE.blob").unwrap();
        assert_eq!(
            blob.value,
            Some(FieldValue::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]))
        );
        assert_eq!(blob.display, "de ad be ef");
    }

    #[test]
    fn unprintable_char_array_falls_back_to_bytes() {
        let set = TemplateSet::parse("T { char tag[3]; }").unwrap();
        let applied = set.apply_bytes("T", 0, &[0x01, 0x02, 0x03]).unwrap();
        assert_eq!(
            applied.field("T.tag").unwrap().value,
            Some(FieldValue::Bytes(vec![1, 2, 3]))
        );
        let applied = set.apply_bytes("T", 0, b"AB\0").unwrap();
        assert_eq!(
            applied.field("T.tag").unwrap().value,
            Some(FieldValue::Text("AB".to_string()))
        );
    }

    #[test]
    fn truncation_is_partial_never_an_error() {
        let set = TemplateSet::parse(PE_TEMPLATE).unwrap();
        let full = pe_bytes();
        // Cut inside the nested struct: early fields decode,
        // later ones are present with no value, flagged.
        let applied = set.apply_bytes("DOS_HEADER", 0, &full[..70]).unwrap();
        assert!(!applied.complete);
        assert_eq!(
            applied.field("DOS_HEADER.magic").unwrap().value,
            Some(FieldValue::Text("MZ".into()))
        );
        let machine = applied.field("DOS_HEADER.pe.machine").unwrap();
        assert_eq!(machine.value, Some(FieldValue::UInt(0x8664)));
        let stamp = applied.field("DOS_HEADER.pe.time_stamp").unwrap();
        assert_eq!(stamp.value, None);
        assert_eq!(stamp.display, "(end of data)");
        // The layout still spans the full static size.
        assert_eq!(applied.size, 84);
        // Cut before a count field's value: the counted array and
        // everything after it simply do not appear.
        let set = TemplateSet::parse("B { u32le n; u8 items[n]; u8 tail; }").unwrap();
        let applied = set.apply_bytes("B", 0, &[1, 0]).unwrap();
        assert!(!applied.complete);
        assert!(applied.field("B.items").is_none());
        assert!(applied.field("B.tail").is_none());
    }

    #[test]
    fn parse_errors_carry_line_numbers() {
        let cases: &[(&str, usize, &str)] = &[
            ("A { u16le x }", 1, "expected ';'"),
            ("A { u16le x; ", 1, "closing"),
            ("A { nosuch x; }", 1, "unknown type"),
            ("A { u8 x; u8 x; }", 1, "two fields named"),
            ("A { u8 items[n]; u8 n; }", 1, "not an earlier field"),
            ("A { f32le n; u8 items[n]; }", 1, "scalar integer"),
            ("A { u8 items[]; }", 1, "array count"),
            ("A { B b; } B { A a; }", 1, "contains itself"),
            ("A { u8 x; } A { u8 y; }", 1, "defined twice"),
            ("A {\n u8 ok;\n nosuch bad;\n}", 3, "unknown type"),
            ("A {\n u8 ok;\n u8 ok;\n}", 3, "two fields named"),
        ];
        for (text, line, fragment) in cases {
            let err = TemplateSet::parse(text).unwrap_err();
            assert_eq!(err.line, *line, "template {text:?}: {err}");
            assert!(
                err.message.contains(fragment),
                "template {text:?}: message {:?} should mention {fragment:?}",
                err.message
            );
        }
    }

    #[test]
    fn shipped_example_templates_parse_and_apply() {
        // The examples under examples/templates/ are user-facing:
        // they must parse, and their layouts must match the public
        // format specifications they describe.
        let pe = TemplateSet::parse(include_str!("../examples/templates/pe.txt")).unwrap();
        assert_eq!(pe.static_size("IMAGE_DOS_HEADER"), Some(64));
        assert_eq!(pe.static_size("IMAGE_FILE_HEADER"), Some(20));
        assert_eq!(pe.static_size("PE_SIGNATURE"), Some(24));
        let mut exe = vec![0u8; 64 + 24];
        exe[0] = b'M';
        exe[1] = b'Z';
        exe[0x3C..0x40].copy_from_slice(&64u32.to_le_bytes());
        exe[64..68].copy_from_slice(b"PE\0\0");
        exe[68..70].copy_from_slice(&0x8664u16.to_le_bytes());
        exe[70..72].copy_from_slice(&7u16.to_le_bytes());
        let dos = pe.apply_bytes("IMAGE_DOS_HEADER", 0, &exe).unwrap();
        assert!(dos.complete);
        assert_eq!(
            dos.field("IMAGE_DOS_HEADER.pe_header_offset")
                .unwrap()
                .value,
            Some(FieldValue::UInt(64))
        );
        let sig = pe.apply_bytes("PE_SIGNATURE", 64, &exe).unwrap();
        assert!(sig.complete);
        assert_eq!(
            sig.field("PE_SIGNATURE.file_header.machine").unwrap().value,
            Some(FieldValue::UInt(0x8664))
        );
        assert_eq!(
            sig.field("PE_SIGNATURE.file_header.section_count")
                .unwrap()
                .value,
            Some(FieldValue::UInt(7))
        );

        let png = TemplateSet::parse(include_str!("../examples/templates/png.txt")).unwrap();
        assert_eq!(png.static_size("PNG_FILE"), Some(8 + 8 + 13 + 4));
        let mut img = vec![0u8; 8 + 25];
        img[..8].copy_from_slice(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
        img[8..12].copy_from_slice(&13u32.to_be_bytes());
        img[12..16].copy_from_slice(b"IHDR");
        img[16..20].copy_from_slice(&1920u32.to_be_bytes());
        img[20..24].copy_from_slice(&1080u32.to_be_bytes());
        img[24] = 8; // bit depth
        img[25] = 2; // color type: truecolor
        let file = png.apply_bytes("PNG_FILE", 0, &img).unwrap();
        assert!(file.complete);
        assert_eq!(
            file.field("PNG_FILE.ihdr.width").unwrap().value,
            Some(FieldValue::UInt(1920))
        );
        assert_eq!(
            file.field("PNG_FILE.ihdr.height").unwrap().value,
            Some(FieldValue::UInt(1080))
        );
        assert_eq!(file.field("PNG_FILE.ihdr.bit_depth").unwrap().offset, 24);
    }

    #[test]
    fn unknown_struct_at_apply_is_an_error() {
        let set = TemplateSet::parse("A { u8 x; }").unwrap();
        assert_eq!(
            set.apply_bytes("MISSING", 0, &[0]),
            Err(ApplyError::UnknownStruct("MISSING".to_string()))
        );
        assert_eq!(set.struct_names().collect::<Vec<_>>(), vec!["A"]);
    }
}
