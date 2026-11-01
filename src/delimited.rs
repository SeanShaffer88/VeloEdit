//! Delimiter grid core (roadmap R4): detect which delimiter a
//! delimited text file uses and split lines into column byte
//! ranges — the data behind a CSV/TSV grid mode that aligns fields
//! into columns and can sort or filter by one column.
//!
//! # Quoting
//!
//! CSV quoting applies to the **comma** delimiter only: a field
//! whose first byte is `"` is quoted — the delimiters inside it do
//! not split, a doubled `""` is an escaped quote and stays part of
//! the field, and the returned range covers the field's *content*
//! (without the surrounding quotes, escapes still doubled, exactly
//! as stored). Tab, semicolon and pipe files are split plainly:
//! real-world TSV/SSV/PSV data does not quote, and treating a
//! leading `"` there as syntax would corrupt ranges for text that
//! merely starts with a quote character. An unterminated quote
//! runs to the end of the line (the field is what it is; the grid
//! shows it rather than refusing the line).
//!
//! Empty fields are real fields: a trailing delimiter yields a
//! final empty field, and an empty line is one empty field.

/// A candidate delimiter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delimiter {
    Comma,
    Tab,
    Semicolon,
    Pipe,
}

impl Delimiter {
    /// The delimiter byte.
    pub fn byte(self) -> u8 {
        match self {
            Delimiter::Comma => b',',
            Delimiter::Tab => b'\t',
            Delimiter::Semicolon => b';',
            Delimiter::Pipe => b'|',
        }
    }

    /// All candidates, in tie-break order.
    pub const ALL: [Delimiter; 4] = [
        Delimiter::Comma,
        Delimiter::Tab,
        Delimiter::Semicolon,
        Delimiter::Pipe,
    ];
}

/// Split `line` (one line, without its line ending) into field
/// content ranges `(start, end)` as byte offsets within the line,
/// under `delim`'s rules (see the module docs for quoting).
pub fn split_line(line: &[u8], delim: Delimiter) -> Vec<(u64, u64)> {
    let byte = delim.byte();
    let quoting = delim == Delimiter::Comma;
    let mut fields = Vec::new();
    let mut pos = 0usize;
    loop {
        if quoting && line.get(pos) == Some(&b'"') {
            // Quoted field: content runs to the closing quote,
            // with "" as an escaped quote inside.
            let content_start = pos + 1;
            let mut i = content_start;
            let content_end = loop {
                match line.get(i) {
                    None => break line.len(), // unterminated: to EOL
                    Some(b'"') if line.get(i + 1) == Some(&b'"') => i += 2,
                    Some(b'"') => break i,
                    Some(_) => i += 1,
                }
            };
            fields.push((content_start as u64, content_end as u64));
            // Resume after the closing quote and its delimiter.
            pos = content_end + 1; // past the closing quote (or EOL+1)
            match line.get(pos) {
                None => break,
                Some(&b) if b == byte => {
                    pos += 1;
                    if pos > line.len() {
                        break;
                    }
                    // A delimiter right at the end means one last
                    // empty field follows.
                    if pos == line.len() {
                        fields.push((pos as u64, pos as u64));
                        break;
                    }
                }
                Some(_) => {
                    // Junk between the closing quote and the next
                    // delimiter: the field is malformed; treat the
                    // rest up to the delimiter as part of it by
                    // resuming the plain scan from here.
                    let (end, next) = scan_plain_field(line, pos, byte);
                    let last = fields.last_mut().expect("field pushed");
                    last.1 = end;
                    match next {
                        Some(n) => {
                            pos = n;
                            if pos == line.len() {
                                fields.push((pos as u64, pos as u64));
                                break;
                            }
                        }
                        None => break,
                    }
                }
            }
        } else {
            let (end, next) = scan_plain_field(line, pos, byte);
            fields.push((pos as u64, end));
            match next {
                Some(n) => {
                    pos = n;
                    if pos == line.len() {
                        fields.push((pos as u64, pos as u64));
                        break;
                    }
                }
                None => break,
            }
        }
    }
    fields
}

/// Scan a plain (unquoted) field starting at `pos`: returns the
/// field's end offset and the offset just past the delimiter that
/// ended it (`None` when the field runs to the end of the line).
fn scan_plain_field(line: &[u8], pos: usize, byte: u8) -> (u64, Option<usize>) {
    let mut i = pos;
    while i < line.len() {
        if line[i] == byte {
            return (i as u64, Some(i + 1));
        }
        i += 1;
    }
    (line.len() as u64, None)
}

/// Detect the delimiter from a sample of lines by consistency
/// scoring: for each candidate, the modal (most common) field
/// count over the sample — which must be at least 2 for the
/// candidate to count at all — and how many lines have that count
/// (the score). The best score wins; ties break toward the higher
/// modal count, then toward the candidate order (comma, tab,
/// semicolon, pipe). `None` for an empty sample or when no
/// candidate reaches two columns consistently.
pub fn detect_delimiter(lines: &[&[u8]]) -> Option<Delimiter> {
    if lines.is_empty() {
        return None;
    }
    let mut best: Option<(Delimiter, usize, usize)> = None; // (delim, score, modal)
    for candidate in Delimiter::ALL {
        // Histogram of field counts for this candidate.
        let mut counts: Vec<(usize, usize)> = Vec::new(); // (field count, lines)
        for line in lines {
            let n = split_line(line, candidate).len();
            match counts.iter_mut().find(|(c, _)| *c == n) {
                Some(entry) => entry.1 += 1,
                None => counts.push((n, 1)),
            }
        }
        let (modal, score) = counts
            .iter()
            .max_by_key(|(count, lines)| (*lines, *count))
            .map(|&(count, lines)| (count, lines))
            .unwrap_or((0, 0));
        if modal < 2 {
            continue;
        }
        let better = match best {
            None => true,
            Some((_, best_score, best_modal)) => {
                score > best_score || (score == best_score && modal > best_modal)
            }
        };
        if better {
            best = Some((candidate, score, modal));
        }
    }
    best.map(|(delim, _, _)| delim)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(line: &[u8], delim: Delimiter) -> Vec<String> {
        split_line(line, delim)
            .iter()
            .map(|&(s, e)| String::from_utf8_lossy(&line[s as usize..e as usize]).into_owned())
            .collect()
    }

    #[test]
    fn plain_splits_and_empty_fields() {
        assert_eq!(fields(b"a,b,c", Delimiter::Comma), ["a", "b", "c"]);
        assert_eq!(fields(b"a,b,", Delimiter::Comma), ["a", "b", ""]);
        assert_eq!(fields(b"", Delimiter::Comma), [""]);
        assert_eq!(fields(b",,", Delimiter::Comma), ["", "", ""]);
        assert_eq!(fields(b"a\tb\tc", Delimiter::Tab), ["a", "b", "c"]);
        assert_eq!(fields(b"a;b", Delimiter::Semicolon), ["a", "b"]);
        assert_eq!(fields(b"a|b", Delimiter::Pipe), ["a", "b"]);
        // No delimiter present: one field, the whole line.
        assert_eq!(fields(b"plain", Delimiter::Comma), ["plain"]);
    }

    #[test]
    fn comma_quoting_rules() {
        // A quoted field keeps its commas; the range is the content.
        assert_eq!(fields(b"\"a,b\",c", Delimiter::Comma), ["a,b", "c"]);
        // Doubled quotes are escapes and stay in the stored bytes.
        assert_eq!(
            fields(b"\"say \"\"hi\"\"\",x", Delimiter::Comma),
            ["say \"\"hi\"\"", "x"]
        );
        // Empty quoted field, and a quoted field at the end.
        assert_eq!(fields(b"\"\",y", Delimiter::Comma), ["", "y"]);
        assert_eq!(fields(b"a,\"b\"", Delimiter::Comma), ["a", "b"]);
        // Unterminated quote runs to the end of the line.
        assert_eq!(fields(b"\"abc,def", Delimiter::Comma), ["abc,def"]);
        // A quote that is not the field's first byte is plain text.
        assert_eq!(fields(b"a\"b,c", Delimiter::Comma), ["a\"b", "c"]);
    }

    #[test]
    fn quoting_applies_to_comma_only() {
        // The same bytes under tab rules: the quote is plain text
        // and the comma does not split (tab does).
        assert_eq!(fields(b"\"a,b\"\tc", Delimiter::Tab), ["\"a,b\"", "c"]);
        // Under semicolon rules quoting is not recognised either:
        // the quotes stay part of the field content (only ';'
        // splits, so the comma inside is inert).
        assert_eq!(fields(b"\"a,b\";c", Delimiter::Semicolon), ["\"a,b\"", "c"]);
    }

    #[test]
    fn detection_picks_the_consistent_delimiter() {
        let csv: Vec<&[u8]> = vec![b"a,b,c", b"1,2,3", b"x,y,z", b"p,q"];
        assert_eq!(detect_delimiter(&csv), Some(Delimiter::Comma));
        let tsv: Vec<&[u8]> = vec![b"a\tb\tc", b"1\t2\t3", b"x\ty\tz"];
        assert_eq!(detect_delimiter(&tsv), Some(Delimiter::Tab));
        let ssv: Vec<&[u8]> = vec![b"a;b;c", b"1;2;3"];
        assert_eq!(detect_delimiter(&ssv), Some(Delimiter::Semicolon));
        let psv: Vec<&[u8]> = vec![b"a|b", b"1|2", b"x|y"];
        assert_eq!(detect_delimiter(&psv), Some(Delimiter::Pipe));
        // Semicolons inside CSV text do not beat the comma: the
        // comma count is consistent, the semicolon count is not.
        let mixed: Vec<&[u8]> = vec![b"a,b;c,d", b"1,2;3,4", b"x,y;z,w"];
        assert_eq!(detect_delimiter(&mixed), Some(Delimiter::Comma));
        // One column everywhere: no delimiter.
        let plain: Vec<&[u8]> = vec![b"just text", b"more text"];
        assert_eq!(detect_delimiter(&plain), None);
        assert_eq!(detect_delimiter(&[]), None);
    }
}
