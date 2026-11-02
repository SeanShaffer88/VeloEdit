//! External tools (roadmap item 49): the protocol core for
//! user-defined external programs — the approved replacement
//! for a plugin engine. A tool is a command the editor runs
//! with document bytes on its standard input, whose standard
//! output comes back as document bytes: no runtime is embedded,
//! nothing is loaded into the process, and the whole contract
//! is this file's configuration shape plus two substitutions.
//!
//! The configuration lives in `data/tools.json`:
//!
//! ```json
//! { "tools": [
//!   { "name": "Sort lines", "command": "sort",
//!     "args": [], "input": "selection", "output": "replace-selection" },
//!   { "name": "Count bytes", "command": "wc", "args": ["-c"],
//!     "input": "range", "output": "discard" }
//! ] }
//! ```
//!
//! - `name` (required, non-empty, unique): the menu label.
//! - `command` (required, non-empty): the program to run.
//! - `args` (optional, default `[]`): argument strings.
//! - `input` (optional, default `"selection"`): what is fed to
//!   the program's standard input — `"selection"` (the current
//!   selection), `"range"` (a byte range chosen when run),
//!   `"file"` (nothing on stdin; the file path is substituted
//!   into the arguments instead), or `"none"`.
//! - `output` (optional, default `"replace-selection"`): what
//!   happens to the program's standard output —
//!   `"replace-selection"` (it replaces the bytes that were
//!   fed in), `"insert"` (inserted at the caret), `"new-file"`
//!   (opened as a new document), or `"discard"` (the tool runs
//!   for its side effects only).
//!
//! Unknown fields are tolerated (skipped), so a file written by
//! a newer version still loads. Every known field is validated
//! and the error names the tool and the field.
//!
//! # Substitution
//!
//! In the command and each argument, `{file}` expands to the
//! document's file path and `{offset}` to the caret offset in
//! decimal — plain substring replacement, applied by
//! [`expand_arg`] / [`expand_args`]; any other `{...}` text is
//! left untouched. Process spawning is deliberately NOT here:
//! it is shell wiring in a later slice. This module is the
//! parse / validate / serialise / substitute core only.
//!
//! Like the other data-file cores, the JSON reading is a small
//! hand-rolled reader for this shape (the crate has no JSON
//! dependency), modelled on `crate::bookmarks`.

use crate::settings::json_escape;

/// Where a tool's input bytes come from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolInput {
    /// The current selection.
    #[default]
    Selection,
    /// A byte range chosen when the tool runs.
    Range,
    /// Nothing on stdin; the file is named via `{file}`.
    File,
    /// Nothing at all.
    None,
}

impl ToolInput {
    pub fn as_str(&self) -> &'static str {
        match self {
            ToolInput::Selection => "selection",
            ToolInput::Range => "range",
            ToolInput::File => "file",
            ToolInput::None => "none",
        }
    }

    fn from_str(value: &str) -> Option<ToolInput> {
        Some(match value {
            "selection" => ToolInput::Selection,
            "range" => ToolInput::Range,
            "file" => ToolInput::File,
            "none" => ToolInput::None,
            _ => return None,
        })
    }
}

/// What happens to a tool's output bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolOutput {
    /// Replace the bytes that were fed in.
    #[default]
    ReplaceSelection,
    /// Insert at the caret.
    Insert,
    /// Open as a new document.
    NewFile,
    /// Throw the output away.
    Discard,
}

impl ToolOutput {
    pub fn as_str(&self) -> &'static str {
        match self {
            ToolOutput::ReplaceSelection => "replace-selection",
            ToolOutput::Insert => "insert",
            ToolOutput::NewFile => "new-file",
            ToolOutput::Discard => "discard",
        }
    }

    fn from_str(value: &str) -> Option<ToolOutput> {
        Some(match value {
            "replace-selection" => ToolOutput::ReplaceSelection,
            "insert" => ToolOutput::Insert,
            "new-file" => ToolOutput::NewFile,
            "discard" => ToolOutput::Discard,
            _ => return None,
        })
    }
}

/// One external tool definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalTool {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub input: ToolInput,
    pub output: ToolOutput,
}

/// A validated `tools.json` document.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolSet {
    pub tools: Vec<ExternalTool>,
}

impl ToolSet {
    /// Parse and validate `tools.json` text. The root may be an
    /// object with a `"tools"` array (the canonical form) or a
    /// bare array. Errors name the offending tool and field.
    pub fn from_json(text: &str) -> Result<ToolSet, String> {
        let mut parser = Parser::new(text);
        parser.skip_ws();
        let tools = match parser.peek() {
            Some('[') => parse_tool_array(&mut parser)?,
            Some('{') => {
                parser.expect('{')?;
                let mut tools = None;
                loop {
                    parser.skip_ws();
                    if parser.eat('}') {
                        break;
                    }
                    let key = parser.string()?;
                    parser.expect(':')?;
                    if key == "tools" {
                        tools = Some(parse_tool_array(&mut parser)?);
                    } else {
                        parser.skip_value()?;
                    }
                    parser.skip_ws();
                    if parser.eat(',') {
                        continue;
                    }
                    parser.expect('}')?;
                    break;
                }
                tools.ok_or_else(|| "the document has no \"tools\" array".to_string())?
            }
            _ => {
                return Err(
                    "the document must be an object with a \"tools\" array, or a bare array"
                        .to_string(),
                )
            }
        };
        parser.skip_ws();
        if parser.peek().is_some() {
            return Err("extra text follows the document".to_string());
        }
        // Names are the tools' identities: duplicates would make
        // menu entries and error reports ambiguous.
        for i in 0..tools.len() {
            for j in i + 1..tools.len() {
                if tools[i].name == tools[j].name {
                    return Err(format!(
                        "tools[{j}].name: {:?} is already used by tools[{i}]",
                        tools[j].name
                    ));
                }
            }
        }
        Ok(ToolSet { tools })
    }

    /// Render the canonical `tools.json` text.
    pub fn to_json(&self) -> String {
        let mut out = String::from("{\n  \"tools\": [");
        for (i, tool) in self.tools.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("\n    { ");
            out.push_str(&format!("\"name\": \"{}\"", json_escape(&tool.name)));
            out.push_str(&format!(
                ", \"command\": \"{}\"",
                json_escape(&tool.command)
            ));
            out.push_str(", \"args\": [");
            for (j, arg) in tool.args.iter().enumerate() {
                if j > 0 {
                    out.push_str(", ");
                }
                out.push_str(&format!("\"{}\"", json_escape(arg)));
            }
            out.push(']');
            out.push_str(&format!(", \"input\": \"{}\"", tool.input.as_str()));
            out.push_str(&format!(", \"output\": \"{}\"", tool.output.as_str()));
            out.push_str(" }");
        }
        if !self.tools.is_empty() {
            out.push('\n');
        }
        out.push_str("  ]\n}\n");
        out
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<&ExternalTool> {
        self.tools.iter().find(|t| t.name == name)
    }
}

fn parse_tool_array(parser: &mut Parser) -> Result<Vec<ExternalTool>, String> {
    parser.expect('[')?;
    let mut tools = Vec::new();
    loop {
        parser.skip_ws();
        if parser.eat(']') {
            break;
        }
        let index = tools.len();
        tools.push(parse_tool(parser, index)?);
        parser.skip_ws();
        if parser.eat(',') {
            continue;
        }
        parser.expect(']')?;
        break;
    }
    Ok(tools)
}

fn parse_tool(parser: &mut Parser, index: usize) -> Result<ExternalTool, String> {
    parser.skip_ws();
    if parser.peek() != Some('{') {
        return Err(format!("tools[{index}]: each tool must be an object"));
    }
    parser.expect('{')?;
    let mut name: Option<String> = None;
    let mut command: Option<String> = None;
    let mut args: Vec<String> = Vec::new();
    let mut input = ToolInput::default();
    let mut output = ToolOutput::default();
    loop {
        parser.skip_ws();
        if parser.eat('}') {
            break;
        }
        let key = parser.string()?;
        parser.expect(':')?;
        match key.as_str() {
            "name" => {
                name = Some(
                    parser
                        .string()
                        .map_err(|e| format!("tools[{index}].name: {e}"))?,
                );
            }
            "command" => {
                command = Some(
                    parser
                        .string()
                        .map_err(|e| format!("tools[{index}].command: {e}"))?,
                );
            }
            "args" => {
                args =
                    parse_string_array(parser).map_err(|e| format!("tools[{index}].args: {e}"))?;
            }
            "input" => {
                let value = parser
                    .string()
                    .map_err(|e| format!("tools[{index}].input: {e}"))?;
                input = ToolInput::from_str(&value).ok_or_else(|| {
                    format!(
                        "tools[{index}].input: unknown value {value:?} (expected \"selection\", \"range\", \"file\" or \"none\")"
                    )
                })?;
            }
            "output" => {
                let value = parser
                    .string()
                    .map_err(|e| format!("tools[{index}].output: {e}"))?;
                output = ToolOutput::from_str(&value).ok_or_else(|| {
                    format!(
                        "tools[{index}].output: unknown value {value:?} (expected \"replace-selection\", \"insert\", \"new-file\" or \"discard\")"
                    )
                })?;
            }
            _ => parser.skip_value()?,
        }
        parser.skip_ws();
        if parser.eat(',') {
            continue;
        }
        parser.expect('}')?;
        break;
    }
    let name = name.ok_or_else(|| format!("tools[{index}]: missing \"name\""))?;
    if name.trim().is_empty() {
        return Err(format!("tools[{index}].name: the name must not be empty"));
    }
    let command = command.ok_or_else(|| format!("tools[{index}]: missing \"command\""))?;
    if command.trim().is_empty() {
        return Err(format!(
            "tools[{index}].command: the command must not be empty"
        ));
    }
    Ok(ExternalTool {
        name,
        command,
        args,
        input,
        output,
    })
}

fn parse_string_array(parser: &mut Parser) -> Result<Vec<String>, String> {
    parser.expect('[')?;
    let mut out = Vec::new();
    loop {
        parser.skip_ws();
        if parser.eat(']') {
            break;
        }
        out.push(parser.string()?);
        parser.skip_ws();
        if parser.eat(',') {
            continue;
        }
        parser.expect(']')?;
        break;
    }
    Ok(out)
}

/// Expand the `{file}` and `{offset}` placeholders in one
/// command or argument string: plain substring replacement; any
/// other `{...}` text is left exactly as written.
pub fn expand_arg(template: &str, file: &str, offset: u64) -> String {
    template
        .replace("{file}", file)
        .replace("{offset}", &offset.to_string())
}

/// Expand placeholders in a whole argument list.
pub fn expand_args(args: &[String], file: &str, offset: u64) -> Vec<String> {
    args.iter()
        .map(|arg| expand_arg(arg, file, offset))
        .collect()
}

/// Build the single command-line string `CreateProcessW` wants
/// from a program and its arguments, quoting each piece by the
/// standard Windows rules (the ones `CommandLineToArgvW` parses
/// back): a piece with no spaces or quotes passes through; any
/// other piece is wrapped in double quotes, with backslashes
/// doubled where they precede a quote or the closing quote, and
/// embedded quotes backslash-escaped.
pub fn quote_command_line(program: &str, args: &[String]) -> String {
    let mut out = quote_arg(program);
    for arg in args {
        out.push(' ');
        out.push_str(&quote_arg(arg));
    }
    out
}

fn quote_arg(arg: &str) -> String {
    if !arg.is_empty()
        && !arg
            .chars()
            .any(|c| c == ' ' || c == '\t' || c == '"' || c == '\n' || c == '\x0B')
    {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                for _ in 0..backslashes * 2 + 1 {
                    out.push('\\');
                }
                backslashes = 0;
                out.push('"');
            }
            _ => {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push(c);
            }
        }
    }
    for _ in 0..backslashes * 2 {
        out.push('\\');
    }
    out.push('"');
    out
}

/// A minimal JSON reader for the tools shape — deliberately not
/// a general JSON parser; modelled on the bookmarks reader in
/// `crate::bookmarks`.
struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn new(text: &str) -> Self {
        Self {
            chars: text.chars().collect(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.pos += 1;
        Some(ch)
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\n' | '\r')) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, want: char) -> bool {
        self.skip_ws();
        if self.peek() == Some(want) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, want: char) -> Result<(), String> {
        if self.eat(want) {
            Ok(())
        } else {
            Err(format!("expected {want:?} at character {}", self.pos))
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect('"')?;
        let mut out = String::new();
        loop {
            let ch = self.bump().ok_or("unterminated string")?;
            match ch {
                '"' => return Ok(out),
                '\\' => {
                    let esc = self.bump().ok_or("short escape")?;
                    match esc {
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        '/' => out.push('/'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'b' => out.push('\u{0008}'),
                        'f' => out.push('\u{000C}'),
                        'u' => {
                            let mut value: u32 = 0;
                            for _ in 0..4 {
                                let digit = self.bump().ok_or("short \\u escape")?;
                                value = value * 16 + digit.to_digit(16).ok_or("bad \\u escape")?;
                            }
                            out.push(char::from_u32(value).ok_or("bad \\u value")?);
                        }
                        _ => return Err("unknown escape".to_string()),
                    }
                }
                ch => out.push(ch),
            }
        }
    }

    /// Skip any JSON value (used for unknown fields).
    fn skip_value(&mut self) -> Result<(), String> {
        self.skip_ws();
        match self.peek() {
            Some('"') => {
                self.string()?;
            }
            Some('{') => {
                self.pos += 1;
                loop {
                    self.skip_ws();
                    if self.eat('}') {
                        break;
                    }
                    self.string()?;
                    self.expect(':')?;
                    self.skip_value()?;
                    self.skip_ws();
                    if self.eat(',') {
                        continue;
                    }
                    self.expect('}')?;
                    break;
                }
            }
            Some('[') => {
                self.pos += 1;
                loop {
                    self.skip_ws();
                    if self.eat(']') {
                        break;
                    }
                    self.skip_value()?;
                    self.skip_ws();
                    if self.eat(',') {
                        continue;
                    }
                    self.expect(']')?;
                    break;
                }
            }
            Some(_) => {
                // A literal: number, true, false or null — read
                // to the next structural character.
                while matches!(self.peek(), Some(c) if !matches!(c, ',' | '}' | ']' | ' ' | '\t' | '\n' | '\r'))
                {
                    self.pos += 1;
                }
            }
            None => return Err("unexpected end of input".to_string()),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{ "tools": [
        { "name": "Sort lines", "command": "sort", "args": [],
          "input": "selection", "output": "replace-selection" },
        { "name": "Count bytes", "command": "wc", "args": ["-c"],
          "input": "range", "output": "discard" },
        { "name": "Minimal", "command": "cat" }
    ] }"#;

    #[test]
    fn parses_the_documented_shape_with_defaults() {
        let set = ToolSet::from_json(SAMPLE).unwrap();
        assert_eq!(set.tools.len(), 3);
        let sort = &set.tools[0];
        assert_eq!(sort.command, "sort");
        assert_eq!(sort.input, ToolInput::Selection);
        assert_eq!(sort.output, ToolOutput::ReplaceSelection);
        let wc = &set.tools[1];
        assert_eq!(wc.args, vec!["-c".to_string()]);
        assert_eq!(wc.input, ToolInput::Range);
        assert_eq!(wc.output, ToolOutput::Discard);
        // Missing args/input/output take the defaults.
        let minimal = &set.tools[2];
        assert_eq!(minimal.args, Vec::<String>::new());
        assert_eq!(minimal.input, ToolInput::Selection);
        assert_eq!(minimal.output, ToolOutput::ReplaceSelection);
        assert_eq!(set.get("Count bytes"), Some(wc));
        assert_eq!(set.get("nope"), None);
    }

    #[test]
    fn bare_array_root_is_accepted() {
        let set = ToolSet::from_json(r#"[ { "name": "A", "command": "cat" } ]"#).unwrap();
        assert_eq!(set.tools.len(), 1);
        assert_eq!(set.tools[0].name, "A");
    }

    #[test]
    fn round_trip_through_to_json() {
        let set = ToolSet::from_json(SAMPLE).unwrap();
        let text = set.to_json();
        let back = ToolSet::from_json(&text).unwrap();
        assert_eq!(set, back);
        // The empty set renders and re-reads too.
        let empty = ToolSet::default();
        assert_eq!(ToolSet::from_json(&empty.to_json()).unwrap(), empty);
    }

    #[test]
    fn unknown_fields_are_tolerated() {
        let text = r#"{ "version": 3, "tools": [
            { "name": "A", "command": "cat", "hotkey": "Ctrl+1",
              "future": { "nested": [1, true, null] } }
        ], "later": "yes" }"#;
        let set = ToolSet::from_json(text).unwrap();
        assert_eq!(set.tools.len(), 1);
    }

    #[test]
    fn every_validation_failure_is_named() {
        let cases: &[(&str, &str)] = &[
            (r#"42"#, "must be an object"),
            (r#"{ "other": [] }"#, "no \"tools\" array"),
            (r#"[ 42 ]"#, "tools[0]: each tool must be an object"),
            (r#"[ { "command": "cat" } ]"#, "tools[0]: missing \"name\""),
            (
                r#"[ { "name": " ", "command": "cat" } ]"#,
                "tools[0].name: the name must not be empty",
            ),
            (r#"[ { "name": "A" } ]"#, "tools[0]: missing \"command\""),
            (
                r#"[ { "name": "A", "command": "" } ]"#,
                "tools[0].command: the command must not be empty",
            ),
            (r#"[ { "name": "A", "command": 7 } ]"#, "tools[0].command"),
            (
                r#"[ { "name": "A", "command": "cat", "args": "x" } ]"#,
                "tools[0].args",
            ),
            (
                r#"[ { "name": "A", "command": "cat", "args": [1] } ]"#,
                "tools[0].args",
            ),
            (
                r#"[ { "name": "A", "command": "cat", "input": "sel" } ]"#,
                "tools[0].input: unknown value",
            ),
            (
                r#"[ { "name": "A", "command": "cat", "output": "replace" } ]"#,
                "tools[0].output: unknown value",
            ),
            (
                r#"[ { "name": "A", "command": "cat" }, { "name": "A", "command": "dog" } ]"#,
                "already used by tools[0]",
            ),
            (
                r#"[ { "name": "A", "command": "cat" } ] trailing"#,
                "extra text",
            ),
        ];
        for (text, fragment) in cases {
            let err = ToolSet::from_json(text).unwrap_err();
            assert!(
                err.contains(fragment),
                "input {text:?}: error {err:?} should mention {fragment:?}"
            );
        }
    }

    #[test]
    fn substitution_expands_file_and_offset_only() {
        let out = expand_args(
            &[
                "--at".to_string(),
                "{offset}".to_string(),
                "{file}:{offset}".to_string(),
                "{line}".to_string(),
                "plain".to_string(),
            ],
            "C:\\docs\\big.log",
            4096,
        );
        assert_eq!(
            out,
            vec![
                "--at".to_string(),
                "4096".to_string(),
                "C:\\docs\\big.log:4096".to_string(),
                "{line}".to_string(),
                "plain".to_string(),
            ]
        );
        assert_eq!(expand_arg("{file}{file}", "a.txt", 0), "a.txta.txt");
        assert_eq!(expand_arg("nothing", "a.txt", 3), "nothing");
    }

    #[test]
    fn command_line_quoting_follows_the_windows_rules() {
        assert_eq!(quote_command_line("sort", &[]), "sort");
        assert_eq!(
            quote_command_line("my tool.exe", &["a b".to_string()]),
            "\"my tool.exe\" \"a b\""
        );
        assert_eq!(quote_command_line("cat", &["".to_string()]), "cat \"\"");
        // Embedded quotes are escaped; a trailing backslash inside
        // quotes is doubled; an interior one is not.
        assert_eq!(
            quote_command_line("x", &["say \"hi\"".to_string()]),
            "x \"say \\\"hi\\\"\""
        );
        assert_eq!(
            quote_command_line("x", &["C:\\my dir\\".to_string()]),
            "x \"C:\\my dir\\\\\""
        );
        // A trailing backslash alone needs no quotes at all.
        assert_eq!(
            quote_command_line("x", &["C:\\dir\\".to_string()]),
            "x C:\\dir\\"
        );
        assert_eq!(
            quote_command_line("x", &["C:\\dir".to_string()]),
            "x C:\\dir"
        );
    }
}
