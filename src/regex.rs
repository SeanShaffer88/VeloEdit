//! Streaming regular expressions (roadmap item 9): a
//! backtracking-free regex engine that finds matches in a byte
//! source of any size while holding only a small read window plus
//! the match state.
//!
//! The engine is a Thompson NFA run as a Pike VM: every live
//! thread of the pattern is advanced together, one input byte at
//! a time, so the cost of a search is bounded by
//! `input length × program size` and no pattern — however it is
//! written — can make it blow up the way a backtracking search
//! can. There is no recursion over the input and no backtracking
//! stack; a pattern like `a*a*a*b` over a long run of `a`s with
//! no `b` simply walks the input once and reports no match.
//!
//! # The byte source
//!
//! Like the rest of the cores ([`crate::patch::build_patch`],
//! [`crate::structdiff::compare_sources`]), the source is a
//! `FnMut(u64, &mut [u8]) -> usize` closure: fill the buffer from
//! the offset, return how many bytes were filled (0 at or past
//! the end). A closure was chosen over a reader trait because it
//! is the established house shape, it needs no trait objects or
//! associated types at the call site, and the VM consumes the
//! input almost strictly in order — the only random access it
//! ever performs is one byte behind and one byte ahead of the
//! scan position (for the `^` / `$` line anchors), which the
//! iterator serves from its read window. A piece table, a file
//! and an in-memory slice all adapt to the closure in a line.
//!
//! # Syntax
//!
//! ```text
//! pattern    := alternation
//! alternation:= concat ('|' concat)*
//! concat     := repeat*
//! repeat     := atom quantifier? '?'?     ('?' after a quantifier makes it lazy)
//! quantifier := '*' | '+' | '?' | '{' n '}' | '{' n ',}' | '{' n ',' m '}'
//! atom       := '(' alternation ')' | '(?:' alternation ')'
//!             | '[' class ']' | '.' | '^' | '$' | escape | literal byte
//! class      := '^'? (range | escape | byte)+   (']' first is a literal)
//! escape     := '\' ('d'|'D'|'w'|'W'|'s'|'S'|'n'|'r'|'t'|'0'|'x' hex hex | punctuation)
//! ```
//!
//! `.` matches any byte except `\n`. `\d` is `[0-9]`, `\w` is
//! `[0-9A-Za-z_]`, `\s` is `[ \t\n\r\x0B\x0C]`; the capitals are
//! the complements. `^` and `$` are line anchors: `^` matches at
//! offset 0 and just after a `\n`; `$` matches at the end of the
//! source and just before a `\n`. Groups are non-capturing in
//! effect — they set precedence only; the API reports whole-match
//! spans. Matching is leftmost with the usual preference order
//! (greedy quantifiers prefer more, lazy ones prefer less,
//! alternation prefers the earlier branch) — the same answers a
//! backtracking search gives, without the backtracking.
//!
//! # Bounds
//!
//! A compiled program is capped at [`MAX_PROGRAM_INSTRUCTIONS`]
//! instructions; counted repeats (`{n,m}`) are expanded, so a
//! pattern whose expansion passes the cap is refused at compile
//! time with a positioned error. Live VM state is two thread
//! lists of at most one thread per instruction plus the read
//! window: memory does not grow with the input, however long a
//! single match runs (a `.*` over a whole file keeps only its
//! start offset).

use std::fmt;

/// Cap on the compiled program size, in instructions. Counted
/// repeats expand into copies of their body, so this is also the
/// cap on repeat expansion.
pub const MAX_PROGRAM_INSTRUCTIONS: usize = 8192;

/// Cap on group nesting depth while parsing.
pub const MAX_PATTERN_DEPTH: usize = 128;

/// The find iterator's default read window.
pub const DEFAULT_WINDOW: usize = 64 * 1024;

/// Why a pattern was refused. `position` is a byte offset into
/// the pattern string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegexError {
    pub position: usize,
    pub message: String,
}

impl RegexError {
    fn at(position: usize, message: impl Into<String>) -> Self {
        Self {
            position,
            message: message.into(),
        }
    }
}

impl fmt::Display for RegexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "regex error at byte {}: {}", self.position, self.message)
    }
}

impl std::error::Error for RegexError {}

// ---------------------------------------------------------------------
// Pattern syntax tree

#[derive(Clone, Debug)]
enum Ast {
    Empty,
    Byte(u8),
    Any,
    Class(Box<[u64; 4]>),
    StartLine,
    EndLine,
    Concat(Vec<Ast>),
    Alt(Vec<Ast>),
    Repeat {
        node: Box<Ast>,
        min: u32,
        max: Option<u32>,
        greedy: bool,
        /// Byte offset of the quantifier, for the size-cap error.
        at: usize,
    },
}

/// A 256-bit byte set as four words.
type ByteSet = [u64; 4];

fn set_add(set: &mut ByteSet, byte: u8) {
    set[(byte >> 6) as usize] |= 1u64 << (byte & 63);
}

fn set_add_range(set: &mut ByteSet, lo: u8, hi: u8) {
    for b in lo..=hi {
        set_add(set, b);
    }
}

fn set_union(set: &mut ByteSet, other: &ByteSet) {
    for i in 0..4 {
        set[i] |= other[i];
    }
}

fn set_complement(set: &mut ByteSet) {
    for word in set.iter_mut() {
        *word = !*word;
    }
}

fn digit_set() -> ByteSet {
    let mut set = [0; 4];
    set_add_range(&mut set, b'0', b'9');
    set
}

fn word_set() -> ByteSet {
    let mut set = digit_set();
    set_add_range(&mut set, b'A', b'Z');
    set_add_range(&mut set, b'a', b'z');
    set_add(&mut set, b'_');
    set
}

fn space_set() -> ByteSet {
    let mut set = [0; 4];
    for b in [b' ', b'\t', b'\n', b'\r', 0x0B, 0x0C] {
        set_add(&mut set, b);
    }
    set
}

// ---------------------------------------------------------------------
// Parser

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek2(&self) -> Option<u8> {
        self.bytes.get(self.pos + 1).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    fn parse_pattern(&mut self) -> Result<Ast, RegexError> {
        let ast = self.parse_alt()?;
        if let Some(b) = self.peek() {
            // parse_alt stops only at ')' or end of input.
            return Err(RegexError::at(
                self.pos,
                format!("unmatched '{}'", b as char),
            ));
        }
        Ok(ast)
    }

    fn parse_alt(&mut self) -> Result<Ast, RegexError> {
        let mut branches = vec![self.parse_concat()?];
        while self.peek() == Some(b'|') {
            self.pos += 1;
            branches.push(self.parse_concat()?);
        }
        if branches.len() == 1 {
            Ok(branches.pop().expect("one branch"))
        } else {
            Ok(Ast::Alt(branches))
        }
    }

    fn parse_concat(&mut self) -> Result<Ast, RegexError> {
        let mut items = Vec::new();
        while let Some(b) = self.peek() {
            if b == b'|' || b == b')' {
                break;
            }
            items.push(self.parse_repeat()?);
        }
        match items.len() {
            0 => Ok(Ast::Empty),
            1 => Ok(items.pop().expect("one item")),
            _ => Ok(Ast::Concat(items)),
        }
    }

    fn parse_repeat(&mut self) -> Result<Ast, RegexError> {
        let atom = self.parse_atom()?;
        let at = self.pos;
        let (min, max) = match self.peek() {
            Some(b'*') => {
                self.pos += 1;
                (0, None)
            }
            Some(b'+') => {
                self.pos += 1;
                (1, None)
            }
            Some(b'?') => {
                self.pos += 1;
                (0, Some(1))
            }
            Some(b'{') if self.peek2().is_some_and(|b| b.is_ascii_digit()) => {
                self.parse_counted()?
            }
            _ => return Ok(atom),
        };
        let greedy = if self.peek() == Some(b'?') {
            self.pos += 1;
            false
        } else {
            true
        };
        // A second quantifier directly after the first is a
        // mistake in every dialect this engine speaks.
        if matches!(self.peek(), Some(b'*' | b'+' | b'?'))
            || (self.peek() == Some(b'{') && self.peek2().is_some_and(|b| b.is_ascii_digit()))
        {
            return Err(RegexError::at(self.pos, "multiple quantifiers in a row"));
        }
        Ok(Ast::Repeat {
            node: Box::new(atom),
            min,
            max,
            greedy,
            at,
        })
    }

    /// Parse `{n}`, `{n,}` or `{n,m}`; the caller has checked the
    /// opening brace and a leading digit.
    fn parse_counted(&mut self) -> Result<(u32, Option<u32>), RegexError> {
        let at = self.pos;
        self.pos += 1; // '{'
        let min = self.parse_count_number(at)?;
        match self.bump() {
            Some(b'}') => Ok((min, Some(min))),
            Some(b',') => {
                if self.peek() == Some(b'}') {
                    self.pos += 1;
                    Ok((min, None))
                } else {
                    let max = self.parse_count_number(at)?;
                    if self.bump() != Some(b'}') {
                        return Err(RegexError::at(at, "expected '}' to close the repeat"));
                    }
                    if max < min {
                        return Err(RegexError::at(
                            at,
                            format!("repeat range {{{min},{max}}} ends before it starts"),
                        ));
                    }
                    Ok((min, Some(max)))
                }
            }
            _ => Err(RegexError::at(at, "expected ',' or '}' in the repeat")),
        }
    }

    fn parse_count_number(&mut self, at: usize) -> Result<u32, RegexError> {
        let mut value: u32 = 0;
        let mut digits = 0;
        while let Some(b) = self.peek() {
            if !b.is_ascii_digit() {
                break;
            }
            value = value
                .checked_mul(10)
                .and_then(|v| v.checked_add((b - b'0') as u32))
                .ok_or_else(|| RegexError::at(at, "repeat count is too large"))?;
            digits += 1;
            self.pos += 1;
        }
        if digits == 0 {
            return Err(RegexError::at(at, "expected a number in the repeat"));
        }
        Ok(value)
    }

    fn parse_atom(&mut self) -> Result<Ast, RegexError> {
        let at = self.pos;
        match self.bump() {
            None => Err(RegexError::at(at, "unexpected end of pattern")),
            Some(b'(') => {
                // (?:...) is accepted and means the same as (...).
                if self.peek() == Some(b'?') && self.peek2() == Some(b':') {
                    self.pos += 2;
                }
                self.depth += 1;
                if self.depth > MAX_PATTERN_DEPTH {
                    return Err(RegexError::at(at, "groups are nested too deeply"));
                }
                let inner = self.parse_alt()?;
                self.depth -= 1;
                if self.bump() != Some(b')') {
                    return Err(RegexError::at(at, "unclosed group"));
                }
                Ok(inner)
            }
            Some(b'[') => self.parse_class(at),
            Some(b'.') => Ok(Ast::Any),
            Some(b'^') => Ok(Ast::StartLine),
            Some(b'$') => Ok(Ast::EndLine),
            Some(b'\\') => self.parse_escape(at),
            Some(b'*' | b'+' | b'?') => Err(RegexError::at(at, "quantifier has nothing to repeat")),
            Some(b) => Ok(Ast::Byte(b)),
        }
    }

    /// Parse an escape outside a character class; `at` is the
    /// position of the backslash.
    fn parse_escape(&mut self, at: usize) -> Result<Ast, RegexError> {
        match self.bump() {
            None => Err(RegexError::at(at, "trailing backslash")),
            Some(b'd') => Ok(Ast::Class(Box::new(digit_set()))),
            Some(b'D') => {
                let mut set = digit_set();
                set_complement(&mut set);
                Ok(Ast::Class(Box::new(set)))
            }
            Some(b'w') => Ok(Ast::Class(Box::new(word_set()))),
            Some(b'W') => {
                let mut set = word_set();
                set_complement(&mut set);
                Ok(Ast::Class(Box::new(set)))
            }
            Some(b's') => Ok(Ast::Class(Box::new(space_set()))),
            Some(b'S') => {
                let mut set = space_set();
                set_complement(&mut set);
                Ok(Ast::Class(Box::new(set)))
            }
            Some(b'n') => Ok(Ast::Byte(b'\n')),
            Some(b'r') => Ok(Ast::Byte(b'\r')),
            Some(b't') => Ok(Ast::Byte(b'\t')),
            Some(b'0') => Ok(Ast::Byte(0)),
            Some(b'x') => Ok(Ast::Byte(self.parse_hex_byte(at)?)),
            Some(b) if b.is_ascii_alphanumeric() => Err(RegexError::at(
                at,
                format!("unsupported escape '\\{}'", b as char),
            )),
            Some(b) => Ok(Ast::Byte(b)),
        }
    }

    fn parse_hex_byte(&mut self, at: usize) -> Result<u8, RegexError> {
        let mut value: u8 = 0;
        for _ in 0..2 {
            let digit = self
                .bump()
                .and_then(|b| (b as char).to_digit(16))
                .ok_or_else(|| RegexError::at(at, "expected two hex digits after '\\x'"))?;
            value = value * 16 + digit as u8;
        }
        Ok(value)
    }

    /// Parse a character class; `at` is the position of the `[`.
    fn parse_class(&mut self, at: usize) -> Result<Ast, RegexError> {
        let mut set: ByteSet = [0; 4];
        let negate = if self.peek() == Some(b'^') {
            self.pos += 1;
            true
        } else {
            false
        };
        // A ']' as the very first character is a literal.
        let mut first = true;
        loop {
            let Some(b) = self.peek() else {
                return Err(RegexError::at(at, "unclosed character class"));
            };
            if b == b']' && !first {
                self.pos += 1;
                break;
            }
            first = false;
            // One class item: either a set escape (\d and friends
            // union into the class) or a single byte that may open
            // a range.
            let lo = match b {
                b'\\' => {
                    self.pos += 1;
                    match self.class_escape(at)? {
                        ClassItem::Set(extra) => {
                            set_union(&mut set, &extra);
                            continue;
                        }
                        ClassItem::Byte(byte) => byte,
                    }
                }
                _ => {
                    self.pos += 1;
                    b
                }
            };
            // A '-' followed by something other than ']' opens a
            // range; a trailing '-' is a literal.
            if self.peek() == Some(b'-') && self.peek2().is_some_and(|next| next != b']') {
                self.pos += 1; // '-'
                let hi_at = self.pos;
                let hi = match self.bump() {
                    Some(b'\\') => match self.class_escape(at)? {
                        ClassItem::Byte(byte) => byte,
                        ClassItem::Set(_) => {
                            return Err(RegexError::at(hi_at, "a class escape cannot end a range"))
                        }
                    },
                    Some(byte) => byte,
                    None => return Err(RegexError::at(at, "unclosed character class")),
                };
                if hi < lo {
                    return Err(RegexError::at(
                        hi_at,
                        format!(
                            "class range '{}-{}' ends before it starts",
                            lo as char, hi as char
                        ),
                    ));
                }
                set_add_range(&mut set, lo, hi);
            } else {
                set_add(&mut set, lo);
            }
        }
        if negate {
            set_complement(&mut set);
        }
        Ok(Ast::Class(Box::new(set)))
    }

    /// An escape inside a character class: either a byte or a
    /// whole named set. The backslash is already consumed.
    fn class_escape(&mut self, at: usize) -> Result<ClassItem, RegexError> {
        match self.bump() {
            None => Err(RegexError::at(at, "trailing backslash in class")),
            Some(b'd') => Ok(ClassItem::Set(digit_set())),
            Some(b'w') => Ok(ClassItem::Set(word_set())),
            Some(b's') => Ok(ClassItem::Set(space_set())),
            Some(b'D') => {
                let mut set = digit_set();
                set_complement(&mut set);
                Ok(ClassItem::Set(set))
            }
            Some(b'W') => {
                let mut set = word_set();
                set_complement(&mut set);
                Ok(ClassItem::Set(set))
            }
            Some(b'S') => {
                let mut set = space_set();
                set_complement(&mut set);
                Ok(ClassItem::Set(set))
            }
            Some(b'n') => Ok(ClassItem::Byte(b'\n')),
            Some(b'r') => Ok(ClassItem::Byte(b'\r')),
            Some(b't') => Ok(ClassItem::Byte(b'\t')),
            Some(b'0') => Ok(ClassItem::Byte(0)),
            Some(b'x') => Ok(ClassItem::Byte(self.parse_hex_byte(at)?)),
            Some(b) if b.is_ascii_alphanumeric() => Err(RegexError::at(
                at,
                format!("unsupported escape '\\{}' in class", b as char),
            )),
            Some(b) => Ok(ClassItem::Byte(b)),
        }
    }
}

enum ClassItem {
    Byte(u8),
    Set(ByteSet),
}

// ---------------------------------------------------------------------
// Compiler: AST -> Thompson NFA program

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Inst {
    Byte(u8),
    Any,
    Class(usize),
    Split(usize, usize),
    Jmp(usize),
    StartLine,
    EndLine,
    Match,
}

struct Compiler {
    prog: Vec<Inst>,
    classes: Vec<ByteSet>,
}

impl Compiler {
    fn emit(&mut self, inst: Inst) -> Result<usize, RegexError> {
        if self.prog.len() >= MAX_PROGRAM_INSTRUCTIONS {
            return Err(RegexError::at(
                0,
                format!(
                    "the pattern compiles to more than {MAX_PROGRAM_INSTRUCTIONS} instructions (counted repeats expand; make a repeat smaller)"
                ),
            ));
        }
        self.prog.push(inst);
        Ok(self.prog.len() - 1)
    }

    fn compile(&mut self, ast: &Ast) -> Result<(), RegexError> {
        match ast {
            Ast::Empty => Ok(()),
            Ast::Byte(b) => {
                self.emit(Inst::Byte(*b))?;
                Ok(())
            }
            Ast::Any => {
                self.emit(Inst::Any)?;
                Ok(())
            }
            Ast::Class(set) => {
                let idx = self.classes.len();
                self.classes.push(**set);
                self.emit(Inst::Class(idx))?;
                Ok(())
            }
            Ast::StartLine => {
                self.emit(Inst::StartLine)?;
                Ok(())
            }
            Ast::EndLine => {
                self.emit(Inst::EndLine)?;
                Ok(())
            }
            Ast::Concat(items) => {
                for item in items {
                    self.compile(item)?;
                }
                Ok(())
            }
            Ast::Alt(branches) => self.compile_alt(branches),
            Ast::Repeat {
                node,
                min,
                max,
                greedy,
                at,
            } => self.compile_repeat(node, *min, *max, *greedy, *at),
        }
    }

    fn compile_alt(&mut self, branches: &[Ast]) -> Result<(), RegexError> {
        // Split(b0, rest) b0 Jmp(end) rest: Split(b1, rest2) b1
        // Jmp(end) rest2: ... last Jmp fixups collected for `end`.
        let mut jmp_fixups = Vec::new();
        for (i, branch) in branches.iter().enumerate() {
            if i + 1 == branches.len() {
                self.compile(branch)?;
            } else {
                let split = self.emit(Inst::Split(0, 0))?;
                let body_start = self.prog.len();
                self.compile(branch)?;
                let jmp = self.emit(Inst::Jmp(0))?;
                jmp_fixups.push(jmp);
                let next_start = self.prog.len();
                self.prog[split] = Inst::Split(body_start, next_start);
            }
        }
        let end = self.prog.len();
        for jmp in jmp_fixups {
            self.prog[jmp] = Inst::Jmp(end);
        }
        Ok(())
    }

    fn compile_repeat(
        &mut self,
        node: &Ast,
        min: u32,
        max: Option<u32>,
        greedy: bool,
        at: usize,
    ) -> Result<(), RegexError> {
        // Sanity: a repeat whose expansion cannot fit is refused
        // up front with the quantifier's own position, before any
        // instructions are emitted for it.
        let copies = match max {
            Some(m) => m,
            None => min.saturating_add(1),
        };
        if copies as usize > MAX_PROGRAM_INSTRUCTIONS {
            return Err(RegexError::at(
                at,
                format!(
                    "repeat of up to {copies} copies exceeds the {MAX_PROGRAM_INSTRUCTIONS}-instruction program limit"
                ),
            ));
        }
        for _ in 0..min {
            self.compile(node)?;
        }
        match max {
            None => self.compile_star(node, greedy),
            Some(m) => {
                for _ in min..m {
                    self.compile_opt(node, greedy)?;
                }
                Ok(())
            }
        }
    }

    /// `node*` (used for the unbounded tail of a repeat).
    fn compile_star(&mut self, node: &Ast, greedy: bool) -> Result<(), RegexError> {
        let split = self.emit(Inst::Split(0, 0))?;
        let body_start = self.prog.len();
        self.compile(node)?;
        self.emit(Inst::Jmp(split))?;
        let after = self.prog.len();
        self.prog[split] = if greedy {
            Inst::Split(body_start, after)
        } else {
            Inst::Split(after, body_start)
        };
        Ok(())
    }

    /// `node?` — one optional copy.
    fn compile_opt(&mut self, node: &Ast, greedy: bool) -> Result<(), RegexError> {
        let split = self.emit(Inst::Split(0, 0))?;
        let body_start = self.prog.len();
        self.compile(node)?;
        let after = self.prog.len();
        self.prog[split] = if greedy {
            Inst::Split(body_start, after)
        } else {
            Inst::Split(after, body_start)
        };
        Ok(())
    }
}

// ---------------------------------------------------------------------
// The compiled pattern

/// A compiled regular expression: an NFA program plus its class
/// table. Build with [`Regex::new`], search with
/// [`Regex::find_iter`] (or the slice conveniences
/// [`Regex::find_all_in`] / [`Regex::find_first_in`]).
#[derive(Clone, Debug)]
pub struct Regex {
    prog: Vec<Inst>,
    classes: Vec<ByteSet>,
    source: String,
}

impl Regex {
    /// Compile a pattern. Errors carry the byte position in the
    /// pattern and a plain-words message.
    pub fn new(pattern: &str) -> Result<Regex, RegexError> {
        let mut parser = Parser {
            bytes: pattern.as_bytes(),
            pos: 0,
            depth: 0,
        };
        let ast = parser.parse_pattern()?;
        let mut compiler = Compiler {
            prog: Vec::new(),
            classes: Vec::new(),
        };
        compiler.compile(&ast)?;
        compiler.emit(Inst::Match)?;
        Ok(Regex {
            prog: compiler.prog,
            classes: compiler.classes,
            source: pattern.to_string(),
        })
    }

    /// The pattern text this regex was compiled from.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Number of compiled instructions (the program-size cap
    /// applies to this count).
    pub fn program_len(&self) -> usize {
        self.prog.len()
    }

    /// Iterate the non-overlapping matches in a source of `total`
    /// bytes read through `read_at`, left to right. After an
    /// empty match the scan resumes one byte later, so an empty
    /// match is reported at every position where one occurs —
    /// the same convention as `str::matches` and the standard
    /// regex crates.
    pub fn find_iter<F>(&self, total: u64, read_at: F) -> FindIter<'_, F>
    where
        F: FnMut(u64, &mut [u8]) -> usize,
    {
        self.find_iter_with_window(total, read_at, DEFAULT_WINDOW)
    }

    /// [`Regex::find_iter`] with an explicit read window (clamped
    /// to at least 1 byte). A tiny window is only useful in
    /// tests, to prove matches spanning window edges work; the VM
    /// state itself never depends on the window.
    pub fn find_iter_with_window<F>(&self, total: u64, read_at: F, window: usize) -> FindIter<'_, F>
    where
        F: FnMut(u64, &mut [u8]) -> usize,
    {
        FindIter {
            re: self,
            total,
            read_at,
            window: vec![0; window.max(1)],
            win_start: 0,
            win_len: 0,
            next_from: 0,
            done: false,
        }
    }

    /// Every match in an in-memory byte string (the small-input /
    /// test convenience over [`Regex::find_iter`]).
    pub fn find_all_in(&self, data: &[u8]) -> Vec<(u64, u64)> {
        self.find_iter(data.len() as u64, slice_reader(data))
            .collect()
    }

    /// The first match in an in-memory byte string, if any.
    pub fn find_first_in(&self, data: &[u8]) -> Option<(u64, u64)> {
        self.find_iter(data.len() as u64, slice_reader(data)).next()
    }

    /// True when the pattern matches anywhere in `data`.
    pub fn is_match_in(&self, data: &[u8]) -> bool {
        self.find_first_in(data).is_some()
    }
}

/// A `read_at` closure over an in-memory byte string.
fn slice_reader(data: &[u8]) -> impl FnMut(u64, &mut [u8]) -> usize + '_ {
    move |offset: u64, out: &mut [u8]| -> usize {
        let start = offset as usize;
        if start >= data.len() {
            return 0;
        }
        let n = out.len().min(data.len() - start);
        out[..n].copy_from_slice(&data[start..start + n]);
        n
    }
}

// ---------------------------------------------------------------------
// Pike VM

/// What the byte-lookahead for `$` sees at the scan position.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Look {
    Byte(u8),
    End,
}

struct Thread {
    pc: usize,
    start: u64,
}

/// The VM's per-step scratch: the two thread lists, the
/// generation-stamped visited marks for epsilon closure, and the
/// best match found so far in the current run.
struct Vm<'a> {
    re: &'a Regex,
    clist: Vec<Thread>,
    nlist: Vec<Thread>,
    seen: Vec<u32>,
    gen: u32,
    /// The recorded match for the current run: `(start, end)`.
    /// Only ever overwritten by a strictly higher-priority thread
    /// (an earlier start, or a preferred path for the same
    /// start), which is what makes the last write the answer.
    candidate: Option<(u64, u64)>,
}

impl<'a> Vm<'a> {
    fn new(re: &'a Regex) -> Self {
        Self {
            re,
            clist: Vec::new(),
            nlist: Vec::new(),
            seen: vec![0; re.prog.len()],
            gen: 0,
            candidate: None,
        }
    }

    /// Add `pc` and its epsilon closure to `list`, in priority
    /// order, for a thread that started at `start`. `pos` is the
    /// position the closure is evaluated at; `prev` / `next`
    /// describe the bytes around it for the line anchors. Sets
    /// `self.candidate` when the closure reaches Match, and cuts
    /// the rest of the closure — nothing added after a match in
    /// priority order can beat it.
    #[allow(clippy::too_many_arguments)]
    fn add(
        &mut self,
        list: usize, // 0 = clist, 1 = nlist (index gymnastics for borrow rules)
        pc: usize,
        start: u64,
        pos: u64,
        prev: Option<u8>,
        next: Look,
    ) {
        // Explicit stack, pushed in reverse so pops follow the
        // recursive depth-first priority order exactly.
        let mut stack: Vec<usize> = vec![pc];
        while let Some(pc) = stack.pop() {
            if self.seen[pc] == self.gen {
                continue;
            }
            self.seen[pc] = self.gen;
            match self.re.prog[pc] {
                Inst::Jmp(target) => stack.push(target),
                Inst::Split(first, second) => {
                    stack.push(second);
                    stack.push(first);
                }
                Inst::StartLine => {
                    if pos == 0 || prev == Some(b'\n') {
                        stack.push(pc + 1);
                    }
                }
                Inst::EndLine => {
                    if next == Look::End || next == Look::Byte(b'\n') {
                        stack.push(pc + 1);
                    }
                }
                Inst::Match => {
                    self.candidate = Some((start, pos));
                    return; // cut: discard everything still stacked
                }
                _ => {
                    let thread = Thread { pc, start };
                    if list == 0 {
                        self.clist.push(thread);
                    } else {
                        self.nlist.push(thread);
                    }
                }
            }
        }
    }

    /// Run the VM from `from` and return the leftmost match at or
    /// after it, reading bytes through `byte_at` (which returns
    /// `None` at or past the source end).
    fn run(
        &mut self,
        total: u64,
        from: u64,
        byte_at: &mut impl FnMut(u64) -> Option<u8>,
    ) -> Option<(u64, u64)> {
        self.clist.clear();
        self.candidate = None;
        let mut prev: Option<u8> = if from > 0 { byte_at(from - 1) } else { None };
        let mut pos = from;
        loop {
            self.gen += 1;
            // Inject a fresh thread starting here — but only
            // while no match has been recorded: a later start can
            // never beat an earlier one, so once a candidate
            // exists new starts are not even added.
            if self.candidate.is_none() {
                let next = match byte_at(pos) {
                    Some(b) => Look::Byte(b),
                    None => Look::End,
                };
                self.add(0, 0, pos, pos, prev, next);
            }
            if pos >= total {
                return self.candidate;
            }
            if self.clist.is_empty() {
                match self.candidate {
                    // Everything alive died and a match stands:
                    // it is final.
                    Some(_) => return self.candidate,
                    // Nothing alive and nothing matched at this
                    // start (an anchor refused it, say): step the
                    // scan to the next start position.
                    None => {
                        prev = byte_at(pos);
                        pos += 1;
                        continue;
                    }
                }
            }
            // A reader that returns no byte before the declared
            // total is lying; treat it as the end of the source
            // rather than failing.
            let Some(byte) = byte_at(pos) else {
                return self.candidate;
            };
            // Step every live thread over this byte, in priority
            // order. The first thread whose closure changes the
            // recorded match is the highest-priority completion
            // at this step: the threads after it are all lower
            // priority (a later start, or a less preferred path
            // for the same start) and are cut — if one of them
            // completed too, its span must not replace this one.
            // Threads before it already wrote their successors
            // into nlist and keep playing; if one of those
            // completes at a later step it still wins, being
            // higher priority.
            self.nlist.clear();
            let next_next = match byte_at(pos + 1) {
                Some(b) => Look::Byte(b),
                None => Look::End,
            };
            self.gen += 1;
            let step_gen = self.gen;
            let mut i = 0;
            // Take clist out so `add` can borrow self freely.
            let mut clist = std::mem::take(&mut self.clist);
            while i < clist.len() {
                let thread = &clist[i];
                let matched_byte = match self.re.prog[thread.pc] {
                    Inst::Byte(want) => want == byte,
                    Inst::Any => byte != b'\n',
                    Inst::Class(idx) => {
                        let set = &self.re.classes[idx];
                        set[(byte >> 6) as usize] & (1u64 << (byte & 63)) != 0
                    }
                    _ => false,
                };
                if matched_byte {
                    let (pc, start) = (thread.pc + 1, thread.start);
                    let before = self.candidate;
                    // All closures in one step share this step's
                    // visited marks: two threads landing on the
                    // same instruction merge, the higher-priority
                    // (earlier-processed) one carrying the day.
                    self.gen = step_gen;
                    self.add(1, pc, start, pos + 1, Some(byte), next_next);
                    if self.candidate != before {
                        break;
                    }
                }
                i += 1;
            }
            clist.clear();
            self.clist = std::mem::take(&mut self.nlist);
            prev = Some(byte);
            pos += 1;
        }
    }
}

// ---------------------------------------------------------------------
// Find iterator

/// Iterator over the non-overlapping matches of a [`Regex`] in a
/// byte source. See [`Regex::find_iter`].
pub struct FindIter<'a, F>
where
    F: FnMut(u64, &mut [u8]) -> usize,
{
    re: &'a Regex,
    total: u64,
    read_at: F,
    window: Vec<u8>,
    win_start: u64,
    win_len: usize,
    next_from: u64,
    done: bool,
}

impl<F> FindIter<'_, F>
where
    F: FnMut(u64, &mut [u8]) -> usize,
{
    /// The byte at `pos` through the read window; `None` at or
    /// past the end of the source.
    fn byte_at(&mut self, pos: u64) -> Option<u8> {
        if pos >= self.total {
            return None;
        }
        if pos < self.win_start || pos >= self.win_start + self.win_len as u64 {
            self.win_start = pos;
            self.win_len = (self.read_at)(pos, &mut self.window);
            if self.win_len == 0 {
                return None;
            }
        }
        let idx = (pos - self.win_start) as usize;
        if idx >= self.win_len {
            return None;
        }
        Some(self.window[idx])
    }
}

impl<F> Iterator for FindIter<'_, F>
where
    F: FnMut(u64, &mut [u8]) -> usize,
{
    type Item = (u64, u64);

    fn next(&mut self) -> Option<(u64, u64)> {
        if self.done {
            return None;
        }
        let mut vm = Vm::new(self.re);
        let from = self.next_from;
        let total = self.total;
        let found = vm.run(total, from, &mut |pos| self.byte_at(pos));
        match found {
            None => {
                self.done = true;
                None
            }
            Some((start, end)) => {
                // Resume after this match; an empty match steps
                // one byte forward so iteration always progresses.
                self.next_from = if end > start { end } else { start + 1 };
                if self.next_from > self.total {
                    self.done = true;
                }
                Some((start, end))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn re(pattern: &str) -> Regex {
        Regex::new(pattern).unwrap_or_else(|e| panic!("pattern {pattern:?} refused: {e}"))
    }

    #[test]
    fn literal_and_dot_matches() {
        assert_eq!(re("hello").find_all_in(b"say hello world"), vec![(4, 9)]);
        assert_eq!(
            re("h.llo").find_all_in(b"hallo hello"),
            vec![(0, 5), (6, 11)]
        );
        // '.' does not cross a newline.
        assert_eq!(re("a.b").find_all_in(b"a\nb"), Vec::new());
        assert_eq!(re(".").find_all_in(b"a\nb"), vec![(0, 1), (2, 3)]);
    }

    #[test]
    fn classes_ranges_and_negation() {
        assert_eq!(
            re("[0-9]+").find_all_in(b"ab 123 cd 45"),
            vec![(3, 6), (10, 12)]
        );
        assert_eq!(re("[^0-9]+").find_all_in(b"12ab34cd"), vec![(2, 4), (6, 8)]);
        assert_eq!(re("[]a]+").find_all_in(b"]a]b"), vec![(0, 3)]);
        assert_eq!(re("[a-]+").find_all_in(b"a-a-b"), vec![(0, 4)]);
        assert_eq!(re("[\\d]+").find_all_in(b"x42y"), vec![(1, 3)]);
        assert_eq!(re("[\\D]+").find_all_in(b"42xy42"), vec![(2, 4)]);
    }

    #[test]
    fn escapes_and_named_sets() {
        assert_eq!(re("\\d+").find_all_in(b"ab123"), vec![(2, 5)]);
        assert_eq!(re("\\w+").find_all_in(b"  a_b9 !"), vec![(2, 6)]);
        assert_eq!(re("\\s").find_all_in(b"a b\tc"), vec![(1, 2), (3, 4)]);
        assert_eq!(re("\\S+").find_all_in(b"  ab cd "), vec![(2, 4), (5, 7)]);
        assert_eq!(re("a\\.b").find_all_in(b"a.b axb"), vec![(0, 3)]);
        assert_eq!(re("\\x41+").find_all_in(b"BAAB"), vec![(1, 3)]);
    }

    #[test]
    fn greedy_and_lazy_quantifiers() {
        assert_eq!(re("a+").find_first_in(b"caaa"), Some((1, 4)));
        assert_eq!(re("a+?").find_first_in(b"caaa"), Some((1, 2)));
        assert_eq!(re("a*?").find_first_in(b"aaa"), Some((0, 0)));
        assert_eq!(re("<.*>").find_first_in(b"<a><b>"), Some((0, 6)));
        assert_eq!(re("<.*?>").find_first_in(b"<a><b>"), Some((0, 3)));
        assert_eq!(re("ab?c").find_all_in(b"ac abc"), vec![(0, 2), (3, 6)]);
    }

    #[test]
    fn counted_repeats() {
        assert_eq!(
            re("a{3}").find_all_in(b"aa aaa aaaa"),
            vec![(3, 6), (7, 10)]
        );
        assert_eq!(re("a{2,}").find_all_in(b"a aa aaaa"), vec![(2, 4), (5, 9)]);
        assert_eq!(re("a{1,2}").find_all_in(b"aaa"), vec![(0, 2), (2, 3)]);
        assert_eq!(re("(ab){2}").find_first_in(b"xxabab"), Some((2, 6)));
        assert_eq!(re("a{0}").find_first_in(b"b"), Some((0, 0)));
        // A '{' that cannot start a count is a literal.
        assert_eq!(re("a{b}").find_first_in(b"a{b}"), Some((0, 4)));
    }

    #[test]
    fn alternation_and_groups() {
        assert_eq!(
            re("cat|dog").find_all_in(b"hotdog cat"),
            vec![(3, 6), (7, 10)]
        );
        // Earlier alternative wins even when a later one is longer...
        assert_eq!(re("a|ab").find_first_in(b"ab"), Some((0, 1)));
        // ...and the longer one wins when it comes first.
        assert_eq!(re("ab|a").find_first_in(b"ab"), Some((0, 2)));
        assert_eq!(re("(ab|a)+").find_first_in(b"aba"), Some((0, 3)));
        assert_eq!(re("(?:xy)+z").find_first_in(b"xyxyz"), Some((0, 5)));
        assert_eq!(re("a(|b)c").find_all_in(b"ac abc"), vec![(0, 2), (3, 6)]);
    }

    #[test]
    fn same_step_competition_keeps_the_leftmost_match() {
        // The `b*` branch records an empty match at offset 1 while
        // the `ab` branch (started at 0) is still live; when both
        // complete on the same byte, the leftmost start wins.
        assert_eq!(re("ab|b*").find_first_in(b"ab"), Some((0, 2)));
        assert_eq!(re("ab|b*").find_all_in(b"ab"), vec![(0, 2), (2, 2)]);
        // Same shape, other side: the nullable branch is preferred
        // at offset 0 and its greedy run is the answer.
        assert_eq!(re("b*|ab").find_first_in(b"ab"), Some((0, 0)));
        // Two same-length completions on one byte: the earlier
        // alternative wins.
        assert_eq!(re("ab|a.").find_first_in(b"ab"), Some((0, 2)));
        assert_eq!(re("a.|ab").find_first_in(b"ab"), Some((0, 2)));
    }

    #[test]
    fn line_anchors() {
        assert_eq!(re("^b").find_all_in(b"a\nb\nb"), vec![(2, 3), (4, 5)]);
        assert_eq!(
            re("a$").find_all_in(b"a\nba\na"),
            vec![(0, 1), (3, 4), (5, 6)]
        );
        assert_eq!(re("^$").find_all_in(b"a\n\nb"), vec![(2, 2)]);
        assert_eq!(re("^ERROR").find_all_in(b"ok\nERROR x"), vec![(3, 8)]);
        assert_eq!(re("x$").find_all_in(b"x\nx"), vec![(0, 1), (2, 3)]);
    }

    #[test]
    fn empty_matches_step_one_byte() {
        assert_eq!(
            re("a*").find_all_in(b"baab"),
            vec![(0, 0), (1, 3), (3, 3), (4, 4)]
        );
        assert_eq!(re("").find_all_in(b"ab"), vec![(0, 0), (1, 1), (2, 2)]);
    }

    #[test]
    fn compile_errors_are_positioned() {
        for (pattern, fragment) in [
            ("(abc", "unclosed group"),
            ("[abc", "unclosed character class"),
            ("a**", "multiple quantifiers"),
            ("*a", "nothing to repeat"),
            ("a{3,2}", "ends before it starts"),
            ("\\q", "unsupported escape"),
            ("[z-a]", "ends before it starts"),
            ("abc\\", "trailing backslash"),
            ("a)", "unmatched"),
        ] {
            let err = Regex::new(pattern).unwrap_err();
            assert!(
                err.message.contains(fragment),
                "pattern {pattern:?}: message {:?} should mention {fragment:?}",
                err.message
            );
            assert!(err.position <= pattern.len());
        }
        // The size cap refuses absurd expansions, positioned at
        // the quantifier.
        let err = Regex::new("(a{9000})").unwrap_err();
        assert!(err.message.contains("repeat"), "{err}");
        let err = Regex::new(&format!("a{}", "{9000,9001}")).unwrap_err();
        assert!(
            err.message.contains("instruction") || err.message.contains("repeat"),
            "{err}"
        );
    }

    #[test]
    fn matches_span_tiny_windows() {
        let data = b"xxabcdefghyy abcdefgh";
        let spans = re("abcdefgh")
            .find_iter_with_window(data.len() as u64, slice_reader(data), 3)
            .collect::<Vec<_>>();
        assert_eq!(spans, vec![(2, 10), (13, 21)]);
        // Window size never changes the answers.
        let big = re("abcdefgh").find_all_in(data);
        assert_eq!(spans, big);
        // Anchor lookahead across a window edge.
        let data = b"ab\ncd\n";
        let spans = re("b$")
            .find_iter_with_window(data.len() as u64, slice_reader(data), 2)
            .collect::<Vec<_>>();
        assert_eq!(spans, vec![(1, 2)]);
    }

    #[test]
    fn pathological_pattern_stays_linear() {
        // The classic backtracking bomb: with a Pike VM this is
        // one pass and no match.
        let data = vec![b'a'; 200_000];
        assert_eq!(re("a*a*a*b").find_first_in(&data), None);
        let mut with_b = data.clone();
        with_b.push(b'b');
        assert_eq!(re("a*a*a*b").find_first_in(&with_b), Some((0, 200_001)));
        assert_eq!(re("(a|a)*b").find_first_in(&data), None);
    }

    // ---------------------------------------------------------
    // Equivalence against an independent reference matcher.
    //
    // The reference below is written directly against the
    // pattern text with a continuation-passing backtracking
    // search — it shares no code with the engine above, so a
    // disagreement is a real bug on one side. It implements the
    // same syntax subset and the same preference rules
    // (leftmost start; greedy before lazy; earlier alternative
    // first), and returns the first complete match found in
    // preference order, which is what the VM computes.

    #[derive(Clone, Debug)]
    enum RefAst {
        Byte(u8),
        Any,
        Set(Vec<u8>), // sorted membership list, pre-negation applied by caller forms
        Start,
        End,
        Concat(Vec<RefAst>),
        Alt(Vec<RefAst>),
        Rep(Box<RefAst>, u32, Option<u32>, bool),
    }

    struct RefParser<'a> {
        bytes: &'a [u8],
        pos: usize,
    }

    impl<'a> RefParser<'a> {
        fn peek(&self) -> Option<u8> {
            self.bytes.get(self.pos).copied()
        }
        fn parse(&mut self) -> RefAst {
            let ast = self.alt();
            assert_eq!(self.pos, self.bytes.len(), "ref parse must consume");
            ast
        }
        fn alt(&mut self) -> RefAst {
            let mut branches = vec![self.concat()];
            while self.peek() == Some(b'|') {
                self.pos += 1;
                branches.push(self.concat());
            }
            if branches.len() == 1 {
                branches.pop().unwrap()
            } else {
                RefAst::Alt(branches)
            }
        }
        fn concat(&mut self) -> RefAst {
            let mut items = Vec::new();
            while let Some(b) = self.peek() {
                if b == b'|' || b == b')' {
                    break;
                }
                items.push(self.repeat());
            }
            RefAst::Concat(items)
        }
        fn repeat(&mut self) -> RefAst {
            let atom = self.atom();
            let (min, max) = match self.peek() {
                Some(b'*') => {
                    self.pos += 1;
                    (0, None)
                }
                Some(b'+') => {
                    self.pos += 1;
                    (1, None)
                }
                Some(b'?') => {
                    self.pos += 1;
                    (0, Some(1))
                }
                Some(b'{') => {
                    self.pos += 1;
                    let mut n = String::new();
                    while let Some(b) = self.peek() {
                        if b == b'}' || b == b',' {
                            break;
                        }
                        n.push(b as char);
                        self.pos += 1;
                    }
                    let min: u32 = n.parse().unwrap();
                    if self.peek() == Some(b',') {
                        self.pos += 1;
                        let mut m = String::new();
                        while let Some(b) = self.peek() {
                            if b == b'}' {
                                break;
                            }
                            m.push(b as char);
                            self.pos += 1;
                        }
                        self.pos += 1; // '}'
                        let max = if m.is_empty() {
                            None
                        } else {
                            Some(m.parse().unwrap())
                        };
                        (min, max)
                    } else {
                        self.pos += 1; // '}'
                        (min, Some(min))
                    }
                }
                _ => return atom,
            };
            let greedy = if self.peek() == Some(b'?') {
                self.pos += 1;
                false
            } else {
                true
            };
            RefAst::Rep(Box::new(atom), min, max, greedy)
        }
        fn atom(&mut self) -> RefAst {
            match self.peek().unwrap() {
                b'(' => {
                    self.pos += 1;
                    if self.peek() == Some(b'?') {
                        self.pos += 2; // ?:
                    }
                    let inner = self.alt();
                    assert_eq!(self.peek(), Some(b')'));
                    self.pos += 1;
                    inner
                }
                b'[' => {
                    self.pos += 1;
                    let negate = if self.peek() == Some(b'^') {
                        self.pos += 1;
                        true
                    } else {
                        false
                    };
                    let mut members: Vec<u8> = Vec::new();
                    let mut first = true;
                    loop {
                        let b = self.peek().unwrap();
                        if b == b']' && !first {
                            self.pos += 1;
                            break;
                        }
                        first = false;
                        self.pos += 1;
                        let lo = if b == b'\\' {
                            let e = self.peek().unwrap();
                            self.pos += 1;
                            match e {
                                b'd' => {
                                    members.extend(b'0'..=b'9');
                                    continue;
                                }
                                b'n' => b'\n',
                                b't' => b'\t',
                                other => other,
                            }
                        } else {
                            b
                        };
                        if self.peek() == Some(b'-')
                            && self.bytes.get(self.pos + 1).is_some_and(|&n| n != b']')
                        {
                            self.pos += 1;
                            let hi = self.peek().unwrap();
                            self.pos += 1;
                            for v in lo..=hi {
                                members.push(v);
                            }
                        } else {
                            members.push(lo);
                        }
                    }
                    if negate {
                        let all: Vec<u8> = (0u8..=255).filter(|v| !members.contains(v)).collect();
                        RefAst::Set(all)
                    } else {
                        RefAst::Set(members)
                    }
                }
                b'.' => {
                    self.pos += 1;
                    RefAst::Any
                }
                b'^' => {
                    self.pos += 1;
                    RefAst::Start
                }
                b'$' => {
                    self.pos += 1;
                    RefAst::End
                }
                b'\\' => {
                    self.pos += 1;
                    let e = self.peek().unwrap();
                    self.pos += 1;
                    match e {
                        b'd' => RefAst::Set((b'0'..=b'9').collect()),
                        b'w' => RefAst::Set(
                            (b'0'..=b'9')
                                .chain(b'A'..=b'Z')
                                .chain(b'a'..=b'z')
                                .chain(*b"_")
                                .collect(),
                        ),
                        b's' => RefAst::Set(vec![b' ', b'\t', b'\n', b'\r', 0x0B, 0x0C]),
                        b'n' => RefAst::Byte(b'\n'),
                        b'r' => RefAst::Byte(b'\r'),
                        b't' => RefAst::Byte(b'\t'),
                        other => RefAst::Byte(other),
                    }
                }
                b => {
                    self.pos += 1;
                    RefAst::Byte(b)
                }
            }
        }
    }

    /// The reference matcher's continuation: given the text and
    /// a position after a successful sub-match, return the end
    /// of the whole match when the rest of the pattern accepts.
    type RefK<'a> = dyn FnMut(&[u8], usize, &mut u64) -> Option<usize> + 'a;

    /// Continuation-passing backtracking match: try to match
    /// `ast` at `pos`; on success hand the end position to `k`,
    /// which returns `Some` when the rest of the pattern accepts.
    /// Preference order is depth-first, exactly as a recursive
    /// backtracker explores.
    fn ref_match(
        ast: &RefAst,
        text: &[u8],
        pos: usize,
        budget: &mut u64,
        k: &mut RefK<'_>,
    ) -> Option<usize> {
        *budget = budget.checked_sub(1)?;
        match ast {
            RefAst::Byte(want) => {
                if text.get(pos) == Some(want) {
                    k(text, pos + 1, budget)
                } else {
                    None
                }
            }
            RefAst::Any => match text.get(pos) {
                Some(&b) if b != b'\n' => k(text, pos + 1, budget),
                _ => None,
            },
            RefAst::Set(members) => match text.get(pos) {
                Some(&b) if members.contains(&b) => k(text, pos + 1, budget),
                _ => None,
            },
            RefAst::Start => {
                if pos == 0 || text[pos - 1] == b'\n' {
                    k(text, pos, budget)
                } else {
                    None
                }
            }
            RefAst::End => {
                if pos == text.len() || text[pos] == b'\n' {
                    k(text, pos, budget)
                } else {
                    None
                }
            }
            RefAst::Concat(items) => ref_match_seq(items, 0, text, pos, budget, k),
            RefAst::Alt(branches) => {
                for branch in branches {
                    if let Some(end) = ref_match(branch, text, pos, budget, k) {
                        return Some(end);
                    }
                }
                None
            }
            RefAst::Rep(node, min, max, greedy) => {
                ref_match_rep(node, *min, *max, *greedy, 0, text, pos, budget, k)
            }
        }
    }

    fn ref_match_seq(
        items: &[RefAst],
        idx: usize,
        text: &[u8],
        pos: usize,
        budget: &mut u64,
        k: &mut RefK<'_>,
    ) -> Option<usize> {
        if idx == items.len() {
            return k(text, pos, budget);
        }
        ref_match(&items[idx], text, pos, budget, &mut |text, next, budget| {
            ref_match_seq(items, idx + 1, text, next, budget, k)
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn ref_match_rep(
        node: &RefAst,
        min: u32,
        max: Option<u32>,
        greedy: bool,
        count: u32,
        text: &[u8],
        pos: usize,
        budget: &mut u64,
        k: &mut RefK<'_>,
    ) -> Option<usize> {
        let can_more = max.is_none_or(|m| count < m);
        let must_more = count < min;
        // One repetition that matches the empty string must not
        // loop: the backtracker treats a no-progress repetition
        // as the end of the loop (the usual engine rule).
        let step = |text: &[u8], pos: usize, budget: &mut u64, k: &mut RefK<'_>| {
            ref_match(node, text, pos, budget, &mut |text, next, budget| {
                if next == pos {
                    if count + 1 >= min {
                        k(text, next, budget)
                    } else {
                        None
                    }
                } else {
                    ref_match_rep(node, min, max, greedy, count + 1, text, next, budget, k)
                }
            })
        };
        if greedy {
            if can_more {
                if let Some(end) = step(text, pos, budget, k) {
                    return Some(end);
                }
            }
            if !must_more {
                return k(text, pos, budget);
            }
            None
        } else {
            if !must_more {
                if let Some(end) = k(text, pos, budget) {
                    return Some(end);
                }
            }
            if can_more {
                return step(text, pos, budget, k);
            }
            None
        }
    }

    /// The reference search: earliest start, first preference
    /// match. Returns None when the budget ran out (case skipped,
    /// not a verdict).
    fn ref_search(pattern: &str, text: &[u8]) -> Option<Option<(u64, u64)>> {
        let mut parser = RefParser {
            bytes: pattern.as_bytes(),
            pos: 0,
        };
        let ast = parser.parse();
        let mut budget: u64 = 2_000_000;
        for start in 0..=text.len() {
            let result = ref_match(&ast, text, start, &mut budget, &mut |_, end, _| Some(end));
            match result {
                Some(end) => return Some(Some((start as u64, end as u64))),
                None => {
                    if budget == 0 {
                        return None;
                    }
                }
            }
        }
        Some(None)
    }

    /// Deterministic xorshift for the case generator (the crate
    /// has no rand dependency, by design).
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn gen_pattern(rng: &mut Rng, depth: u32) -> String {
        fn atom(rng: &mut Rng, depth: u32) -> String {
            match rng.below(if depth > 0 { 9 } else { 7 }) {
                0 => "a".to_string(),
                1 => "b".to_string(),
                2 => "c".to_string(),
                3 => ".".to_string(),
                4 => "[ab]".to_string(),
                5 => "[^b]".to_string(),
                6 => "\\d".to_string(),
                7 => format!("({})", gen_pattern(rng, depth - 1)),
                _ => "[a-c]".to_string(),
            }
        }
        let parts = 1 + rng.below(3);
        let mut out = String::new();
        for i in 0..parts {
            if i > 0 && rng.below(4) == 0 {
                out.push('|');
            }
            out.push_str(&atom(rng, depth));
            match rng.below(6) {
                0 => out.push('*'),
                1 => out.push('+'),
                2 => out.push('?'),
                3 => out.push_str(match rng.below(3) {
                    0 => "{1,2}",
                    1 => "{2}",
                    _ => "{0,2}",
                }),
                _ => {}
            }
            if matches!(out.chars().last(), Some('*' | '+' | '?' | '}')) && rng.below(3) == 0 {
                out.push('?'); // lazy marker
            }
        }
        out
    }

    #[test]
    fn equivalence_with_reference_matcher() {
        let mut rng = Rng(0x5EED_1234_5678_9ABC);
        let alphabet = *b"abc\n1";
        let mut checked = 0;
        for case in 0..400 {
            let pattern = gen_pattern(&mut rng, 2);
            let len = rng.below(9) as usize;
            let text: Vec<u8> = (0..len)
                .map(|_| alphabet[rng.below(alphabet.len() as u64) as usize])
                .collect();
            let expected = ref_search(&pattern, &text);
            let Some(expected) = expected else {
                continue; // reference budget exhausted: skip, no verdict
            };
            let engine = re(&pattern);
            let got = engine.find_first_in(&text);
            assert_eq!(
                got, expected,
                "case {case}: pattern {pattern:?} on {text:?}"
            );
            checked += 1;
        }
        assert!(checked > 300, "too many cases skipped: {checked}");
    }

    #[test]
    fn anchors_against_reference() {
        for (pattern, text) in [
            ("^a+", "baaa\naa"),
            ("a+$", "aa\nbb aa"),
            ("^$", "\n"),
            ("^a|b$", "xb"),
            ("^^a", "a"), // stacked anchors are harmless assertions
        ] {
            let expected = ref_search(pattern, text.as_bytes()).expect("budget");
            // The reference does not model stacked '^' specially;
            // it composes assertions, same as the VM.
            assert_eq!(
                re(pattern).find_first_in(text.as_bytes()),
                expected,
                "{pattern} on {text:?}"
            );
        }
    }
}
