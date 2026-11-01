//! Per-file bookmarks (roadmap item 20): named anchors at byte
//! offsets, persisted between runs.
//!
//! A [`BookmarkStore`] holds one record per file path. Each record
//! is guarded by the file's [`FileStamp`] — its size and
//! modification time — captured when the bookmarks were made: when
//! the store is asked about a file whose current stamp differs
//! from the stored one, the file changed since the bookmarks were
//! set, so the record is dropped instead of pointing at bytes that
//! may no longer mean the same thing.
//!
//! Persistence is `bookmarks.json` in the portable data folder,
//! hand-rolled JSON in the same style as [`crate::settings`]:
//! a strict little writer, a tolerant reader, defaults on any
//! problem (a corrupt file is renamed to `bookmarks.json.bad` so
//! the evidence survives). This module is the pure core; the
//! window's Ctrl+F2 / F2 wiring lands with the GUI slice.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::settings::json_escape;

/// The bookmarks file's name inside the portable data folder.
pub const BOOKMARKS_FILE_NAME: &str = "bookmarks.json";

/// The full path of the bookmarks file inside the portable
/// `data/` folder (see [`crate::settings::data_dir`]).
pub fn bookmarks_path() -> PathBuf {
    crate::settings::data_dir().join(BOOKMARKS_FILE_NAME)
}

/// A file's identity stamp for staleness checks: its size and its
/// modification time (nanoseconds since the Unix epoch; negative
/// for the theoretical pre-1970 file).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStamp {
    pub size: u64,
    pub mtime_nanos: i64,
}

/// Build the [`FileStamp`] for already-read file metadata.
pub fn stamp_of(metadata: &std::fs::Metadata) -> FileStamp {
    let mtime_nanos = metadata.modified().map(system_time_nanos).unwrap_or(0);
    FileStamp {
        size: metadata.len(),
        mtime_nanos,
    }
}

/// Build the [`FileStamp`] for the file at `path`.
pub fn stamp_path(path: &Path) -> std::io::Result<FileStamp> {
    Ok(stamp_of(&std::fs::metadata(path)?))
}

fn system_time_nanos(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos().min(i64::MAX as u128) as i64,
        Err(err) => {
            let d = err.duration();
            -(d.as_nanos().min(i64::MAX as u128) as i64)
        }
    }
}

/// The fixed bookmark colour swatches (slice R4c, item 20
/// expansion): six bright, colourblind-safe colours that read on
/// every built-in theme, chosen from the Okabe-Ito palette. A
/// bookmark with no colour (`None`) draws in the theme accent, as
/// bookmarks always have.
pub const BOOKMARK_SWATCHES: [(&str, (u8, u8, u8)); 6] = [
    ("Orange", (0xE6, 0x9F, 0x00)),
    ("Sky Blue", (0x56, 0xB4, 0xE9)),
    ("Green", (0x00, 0x9E, 0x73)),
    ("Yellow", (0xF0, 0xE4, 0x42)),
    ("Blue", (0x00, 0x72, 0xB2)),
    ("Pink", (0xCC, 0x79, 0xA7)),
];

/// One bookmark: a byte offset in the file, with a display name,
/// an optional colour swatch (an index into [`BOOKMARK_SWATCHES`])
/// and an optional short note (empty = none). Colour and note are
/// the R4c annotation fields: store files written before them
/// load with both defaulted, and the writer only emits them when
/// set, so an un-annotated store stays byte-identical to the old
/// format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bookmark {
    pub offset: u64,
    pub name: String,
    pub color: Option<u8>,
    pub note: String,
}

impl Bookmark {
    /// A plain bookmark with no annotation (the pre-R4c shape).
    pub fn new(offset: u64, name: &str) -> Self {
        Self {
            offset,
            name: name.to_string(),
            color: None,
            note: String::new(),
        }
    }
}

/// The bookmarks of one file, kept sorted by offset at all times.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BookmarkList {
    entries: Vec<Bookmark>,
}

impl BookmarkList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Iterate the bookmarks in ascending offset order.
    pub fn iter(&self) -> impl Iterator<Item = &Bookmark> {
        self.entries.iter()
    }

    pub fn contains(&self, offset: u64) -> bool {
        self.entries.iter().any(|b| b.offset == offset)
    }

    /// Toggle a bookmark at `offset`: removes the existing one and
    /// returns `false`, or inserts a new one (keeping the sort
    /// order) and returns `true`.
    pub fn toggle(&mut self, offset: u64, name: &str) -> bool {
        match self.entries.iter().position(|b| b.offset == offset) {
            Some(index) => {
                self.entries.remove(index);
                false
            }
            None => {
                let at = self
                    .entries
                    .iter()
                    .position(|b| b.offset > offset)
                    .unwrap_or(self.entries.len());
                self.entries.insert(at, Bookmark::new(offset, name));
                true
            }
        }
    }

    /// The next bookmark strictly after `pos`, wrapping to the
    /// first bookmark when `pos` is at or past the last one.
    /// `None` when the list is empty.
    pub fn next_after(&self, pos: u64) -> Option<u64> {
        if self.entries.is_empty() {
            return None;
        }
        self.entries
            .iter()
            .find(|b| b.offset > pos)
            .or_else(|| self.entries.first())
            .map(|b| b.offset)
    }

    /// The previous bookmark strictly before `pos`, wrapping to
    /// the last bookmark when `pos` is at or before the first one.
    /// `None` when the list is empty.
    pub fn prev_before(&self, pos: u64) -> Option<u64> {
        if self.entries.is_empty() {
            return None;
        }
        self.entries
            .iter()
            .rev()
            .find(|b| b.offset < pos)
            .or_else(|| self.entries.last())
            .map(|b| b.offset)
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// The bookmark at exactly `offset`, if one sits there.
    pub fn get(&self, offset: u64) -> Option<&Bookmark> {
        self.entries.iter().find(|b| b.offset == offset)
    }

    /// Insert `bookmark` keeping the sort order, replacing any
    /// bookmark already at its offset (annotations included).
    pub fn add(&mut self, bookmark: Bookmark) {
        if let Some(index) = self
            .entries
            .iter()
            .position(|b| b.offset == bookmark.offset)
        {
            self.entries[index] = bookmark;
            return;
        }
        let at = self
            .entries
            .iter()
            .position(|b| b.offset > bookmark.offset)
            .unwrap_or(self.entries.len());
        self.entries.insert(at, bookmark);
    }

    /// Set the annotation (colour swatch index, note) of the
    /// bookmark at `offset`. A colour index outside
    /// [`BOOKMARK_SWATCHES`] is stored as `None` (no colour)
    /// rather than kept to break a later draw. Returns whether a
    /// bookmark sits at `offset`.
    pub fn set_annotation(&mut self, offset: u64, color: Option<u8>, note: &str) -> bool {
        let Some(bookmark) = self.entries.iter_mut().find(|b| b.offset == offset) else {
            return false;
        };
        bookmark.color = color.filter(|&c| (c as usize) < BOOKMARK_SWATCHES.len());
        bookmark.note = note.to_string();
        true
    }
}

/// One file's record inside the store.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileRecord {
    path: String,
    stamp: FileStamp,
    list: BookmarkList,
}

/// All persisted bookmarks, keyed by file path (the path string as
/// the caller passes it — callers use the file's full path).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BookmarkStore {
    records: Vec<FileRecord>,
}

impl BookmarkStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// The record for `path` when its stored stamp matches
    /// `stamp`; a record with a different stamp is stale — it is
    /// removed and `None` returned (see the module docs).
    fn fresh_record(&mut self, path: &str, stamp: FileStamp) -> Option<&mut FileRecord> {
        let index = self.records.iter().position(|r| r.path == path)?;
        if self.records[index].stamp != stamp {
            self.records.remove(index);
            return None;
        }
        Some(&mut self.records[index])
    }

    /// The record for `path`, dropping a stale one and starting a
    /// fresh record stamped `stamp` when none (or only a stale
    /// one) exists.
    fn record_for(&mut self, path: &str, stamp: FileStamp) -> &mut FileRecord {
        if let Some(index) = self.records.iter().position(|r| r.path == path) {
            if self.records[index].stamp != stamp {
                self.records[index].stamp = stamp;
                self.records[index].list.clear();
            }
            return &mut self.records[index];
        }
        self.records.push(FileRecord {
            path: path.to_string(),
            stamp,
            list: BookmarkList::new(),
        });
        self.records.last_mut().expect("just pushed")
    }

    /// The bookmarks stored for `path` — empty when none are
    /// stored or the stored record's stamp no longer matches
    /// `stamp` (in which case the stale record is dropped).
    pub fn bookmarks(&mut self, path: &str, stamp: FileStamp) -> &[Bookmark] {
        match self.fresh_record(path, stamp) {
            Some(record) => &record.list.entries,
            None => &[],
        }
    }

    /// Toggle a bookmark in `path`'s record (created or re-stamped
    /// to `stamp` first). Returns `true` when the bookmark now
    /// exists.
    pub fn toggle(&mut self, path: &str, stamp: FileStamp, offset: u64, name: &str) -> bool {
        self.record_for(path, stamp).list.toggle(offset, name)
    }

    /// Next bookmark after `pos` in `path`'s record (wrap-around;
    /// see [`BookmarkList::next_after`]). Stale records count as
    /// empty and are dropped.
    pub fn next_after(&mut self, path: &str, stamp: FileStamp, pos: u64) -> Option<u64> {
        self.fresh_record(path, stamp)
            .and_then(|record| record.list.next_after(pos))
    }

    /// Previous bookmark before `pos` in `path`'s record
    /// (wrap-around; see [`BookmarkList::prev_before`]).
    pub fn prev_before(&mut self, path: &str, stamp: FileStamp, pos: u64) -> Option<u64> {
        self.fresh_record(path, stamp)
            .and_then(|record| record.list.prev_before(pos))
    }

    /// Drop `path`'s record entirely (used or not).
    pub fn forget(&mut self, path: &str) {
        self.records.retain(|r| r.path != path);
    }

    /// Set the annotation of the bookmark at `offset` in `path`'s
    /// record (see [`BookmarkList::set_annotation`]). Stale
    /// records count as empty. Returns whether the bookmark
    /// exists.
    pub fn set_annotation(
        &mut self,
        path: &str,
        stamp: FileStamp,
        offset: u64,
        color: Option<u8>,
        note: &str,
    ) -> bool {
        match self.fresh_record(path, stamp) {
            Some(record) => record.list.set_annotation(offset, color, note),
            None => false,
        }
    }

    // -- Persistence ---------------------------------------------------

    /// Render the store as the `bookmarks.json` text.
    pub fn to_json(&self) -> String {
        let mut out = String::from("{\n  \"version\": 1,\n  \"files\": [");
        for (i, record) in self.records.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("\n    {");
            out.push_str(&format!(
                "\n      \"path\": \"{}\",",
                json_escape(&record.path)
            ));
            out.push_str(&format!("\n      \"size\": {},", record.stamp.size));
            out.push_str(&format!(
                "\n      \"mtime_nanos\": {},",
                record.stamp.mtime_nanos
            ));
            out.push_str("\n      \"bookmarks\": [");
            for (j, bookmark) in record.list.entries.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                // The annotation fields are emitted only when set,
                // so a store without annotations keeps the exact
                // pre-R4c byte shape.
                let mut line = format!(
                    "\n        {{ \"offset\": {}, \"name\": \"{}\"",
                    bookmark.offset,
                    json_escape(&bookmark.name)
                );
                if let Some(color) = bookmark.color {
                    line.push_str(&format!(", \"color\": {color}"));
                }
                if !bookmark.note.is_empty() {
                    line.push_str(&format!(", \"note\": \"{}\"", json_escape(&bookmark.note)));
                }
                line.push_str(" }");
                out.push_str(&line);
            }
            if !record.list.entries.is_empty() {
                out.push_str("\n      ");
            }
            out.push_str("]\n    }");
        }
        if !self.records.is_empty() {
            out.push_str("\n  ");
        }
        out.push_str("]\n}\n");
        out
    }

    /// Parse the `bookmarks.json` text. Tolerates whitespace and
    /// key order, skips unknown keys, and re-sorts each record's
    /// bookmarks by offset.
    pub fn from_json(text: &str) -> Result<BookmarkStore, String> {
        let mut parser = Parser::new(text);
        let mut store = BookmarkStore::new();
        parser.expect('{')?;
        loop {
            parser.skip_ws();
            if parser.eat('}') {
                break;
            }
            let key = parser.string()?;
            parser.expect(':')?;
            match key.as_str() {
                "files" => store.records = parser.file_records()?,
                _ => parser.skip_value()?,
            }
            parser.skip_ws();
            if parser.eat(',') {
                continue;
            }
            parser.expect('}')?;
            break;
        }
        Ok(store)
    }

    /// Load the store from `path`, mirroring
    /// [`crate::settings::Settings::load_from`]: a missing file is
    /// an empty store; a corrupt file is renamed to
    /// `<path>.bad` and the store starts empty.
    pub fn load_from(path: &Path) -> BookmarkStore {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(_) => return BookmarkStore::new(),
        };
        match Self::from_json(&text) {
            Ok(store) => store,
            Err(_) => {
                let bad = path.with_extension("json.bad");
                let _ = std::fs::rename(path, &bad);
                BookmarkStore::new()
            }
        }
    }

    /// Save the store to `path` through a sibling temp file
    /// renamed over the target, like
    /// [`crate::settings::Settings::save_to`].
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, self.to_json())?;
        match std::fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    std::fs::write(path, self.to_json())
                } else {
                    Err(e)
                }
            }
        }
    }
}

/// A minimal JSON reader for the bookmarks shape — deliberately
/// not a general JSON parser; it reads what [`BookmarkStore::to_json`]
/// writes, tolerates whitespace and key order, and rejects the rest.
/// Modeled on the settings reader in `crate::settings`.
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

    /// A JSON integer as i64 (the only number shape the writer
    /// emits; sizes and offsets then range-check into u64).
    fn integer(&mut self) -> Result<i64, String> {
        self.skip_ws();
        let start = self.pos;
        if self.peek() == Some('-') {
            self.pos += 1;
        }
        while matches!(self.peek(), Some('0'..='9')) {
            self.pos += 1;
        }
        if self.pos == start || (self.pos == start + 1 && self.chars[start] == '-') {
            return Err(format!("expected an integer at character {start}"));
        }
        self.chars[start..self.pos]
            .iter()
            .collect::<String>()
            .parse::<i64>()
            .map_err(|_| "integer out of range".to_string())
    }

    fn unsigned(&mut self) -> Result<u64, String> {
        let value = self.integer()?;
        u64::try_from(value).map_err(|_| "negative value where unsigned expected".to_string())
    }

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
                // A literal (number, true, false, null): consume
                // the bare token.
                while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || c == '-' || c == '+' || c == '.')
                {
                    self.pos += 1;
                }
            }
            None => return Err("unexpected end of input".to_string()),
        }
        Ok(())
    }

    fn file_records(&mut self) -> Result<Vec<FileRecord>, String> {
        let mut records = Vec::new();
        self.expect('[')?;
        loop {
            self.skip_ws();
            if self.eat(']') {
                break;
            }
            records.push(self.file_record()?);
            self.skip_ws();
            if self.eat(',') {
                continue;
            }
            self.expect(']')?;
            break;
        }
        Ok(records)
    }

    fn file_record(&mut self) -> Result<FileRecord, String> {
        let mut record = FileRecord {
            path: String::new(),
            stamp: FileStamp {
                size: 0,
                mtime_nanos: 0,
            },
            list: BookmarkList::new(),
        };
        self.expect('{')?;
        loop {
            self.skip_ws();
            if self.eat('}') {
                break;
            }
            let key = self.string()?;
            self.expect(':')?;
            match key.as_str() {
                "path" => record.path = self.string()?,
                "size" => record.stamp.size = self.unsigned()?,
                "mtime_nanos" => record.stamp.mtime_nanos = self.integer()?,
                "bookmarks" => record.list = self.bookmark_list()?,
                _ => self.skip_value()?,
            }
            self.skip_ws();
            if self.eat(',') {
                continue;
            }
            self.expect('}')?;
            break;
        }
        Ok(record)
    }

    fn bookmark_list(&mut self) -> Result<BookmarkList, String> {
        let mut list = BookmarkList::new();
        self.expect('[')?;
        loop {
            self.skip_ws();
            if self.eat(']') {
                break;
            }
            let bookmark = self.bookmark()?;
            // Insert through toggle's sorted path, but force-add:
            // parsed entries are data, not user toggles.
            list.entries.push(bookmark);
            self.skip_ws();
            if self.eat(',') {
                continue;
            }
            self.expect(']')?;
            break;
        }
        list.entries.sort_by_key(|b| b.offset);
        Ok(list)
    }

    fn bookmark(&mut self) -> Result<Bookmark, String> {
        let mut bookmark = Bookmark::new(0, "");
        self.expect('{')?;
        loop {
            self.skip_ws();
            if self.eat('}') {
                break;
            }
            let key = self.string()?;
            self.expect(':')?;
            match key.as_str() {
                "offset" => bookmark.offset = self.unsigned()?,
                "name" => bookmark.name = self.string()?,
                // Annotation fields (R4c): absent in old files,
                // where they default; an out-of-range colour is
                // dropped, never kept to break a later draw.
                "color" => {
                    let n = self.unsigned()?;
                    bookmark.color = if (n as usize) < BOOKMARK_SWATCHES.len() {
                        Some(n as u8)
                    } else {
                        None
                    };
                }
                "note" => bookmark.note = self.string()?,
                _ => self.skip_value()?,
            }
            self.skip_ws();
            if self.eat(',') {
                continue;
            }
            self.expect('}')?;
            break;
        }
        Ok(bookmark)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_stamp(size: u64) -> FileStamp {
        FileStamp {
            size,
            mtime_nanos: 1_700_000_000_000_000_000,
        }
    }

    #[test]
    fn toggle_inserts_sorted_and_removes() {
        let mut list = BookmarkList::new();
        assert!(list.toggle(500, "later"));
        assert!(list.toggle(100, "first"));
        assert!(list.toggle(300, "middle"));
        let offsets: Vec<u64> = list.iter().map(|b| b.offset).collect();
        assert_eq!(offsets, vec![100, 300, 500]);
        assert_eq!(list.iter().nth(1).unwrap().name, "middle");
        assert!(list.contains(300));
        // Toggling an existing offset removes it.
        assert!(!list.toggle(300, "ignored"));
        let offsets: Vec<u64> = list.iter().map(|b| b.offset).collect();
        assert_eq!(offsets, vec![100, 500]);
        assert!(!list.contains(300));
    }

    #[test]
    fn next_and_prev_wrap_around() {
        let mut list = BookmarkList::new();
        assert_eq!(list.next_after(0), None);
        assert_eq!(list.prev_before(0), None);
        for offset in [100, 300, 500] {
            list.toggle(offset, "");
        }
        assert_eq!(list.next_after(0), Some(100));
        assert_eq!(list.next_after(100), Some(300));
        assert_eq!(list.next_after(499), Some(500));
        // At/past the last bookmark, next wraps to the first.
        assert_eq!(list.next_after(500), Some(100));
        assert_eq!(list.next_after(999_999), Some(100));
        assert_eq!(list.prev_before(501), Some(500));
        assert_eq!(list.prev_before(500), Some(300));
        assert_eq!(list.prev_before(101), Some(100));
        // At/before the first bookmark, prev wraps to the last.
        assert_eq!(list.prev_before(100), Some(500));
        assert_eq!(list.prev_before(0), Some(500));
    }

    // (R4c) Store files written before annotations existed load
    // with colour/note defaulted, and a store without annotations
    // serialises exactly as it did before.
    #[test]
    fn old_store_files_load_with_default_annotations() {
        let old = "{\n  \"version\": 1,\n  \"files\": [\n    {\n      \"path\": \"C:/a.txt\",\n      \
                   \"size\": 10,\n      \"mtime_nanos\": 1700000000000000000,\n      \"bookmarks\": [\n        \
                   { \"offset\": 5, \"name\": \"Bookmark 1\" }\n      ]\n    }\n  ]\n}\n";
        let stamp = make_stamp(10);
        let mut store = BookmarkStore::from_json(old).expect("old-format store parses");
        let entries = store.bookmarks("C:/a.txt", stamp);
        let bookmark = entries
            .iter()
            .find(|b| b.offset == 5)
            .expect("the bookmark survived");
        assert_eq!(bookmark.color, None);
        assert_eq!(bookmark.note, "");
        // And re-serialising that record keeps the old byte shape:
        // no annotation keys appear.
        let out = store.to_json();
        assert!(!out.contains("\"color\""), "{out}");
        assert!(!out.contains("\"note\""), "{out}");
    }

    // (R4c) Annotations round-trip through the store JSON, and the
    // swatch index is validated on the way in from both sides.
    #[test]
    fn annotations_round_trip_and_validate() {
        let mut store = BookmarkStore::default();
        let stamp = make_stamp(10);
        store.toggle("C:/a.txt", stamp, 5, "Bookmark 1");
        store.toggle("C:/a.txt", stamp, 9, "Bookmark 2");
        assert!(store.set_annotation("C:/a.txt", stamp, 5, Some(2), "watch this spot"));
        // Out-of-range swatch index degrades to "no colour".
        assert!(store.set_annotation("C:/a.txt", stamp, 9, Some(200), ""));
        assert!(!store.set_annotation("C:/a.txt", stamp, 999, Some(0), "nowhere"));
        let json = store.to_json();
        assert!(json.contains("\"color\": 2"), "{json}");
        assert!(json.contains("\"note\": \"watch this spot\""), "{json}");
        let mut back = BookmarkStore::from_json(&json).expect("annotated store parses");
        let entries = back.bookmarks("C:/a.txt", stamp);
        let first = entries.iter().find(|b| b.offset == 5).unwrap();
        assert_eq!(first.color, Some(2));
        assert_eq!(first.note, "watch this spot");
        assert_eq!(entries.iter().find(|b| b.offset == 9).unwrap().color, None);
        // A hand-edited out-of-range colour in the file is dropped
        // on load, not kept.
        let edited = json.replace("\"color\": 2", "\"color\": 77");
        let mut back = BookmarkStore::from_json(&edited).expect("edited store parses");
        let entries = back.bookmarks("C:/a.txt", stamp);
        assert_eq!(entries.iter().find(|b| b.offset == 5).unwrap().color, None);
    }

    #[test]
    fn store_round_trip_through_a_real_file() {
        let dir =
            std::env::temp_dir().join(format!("veloedit-bookmarks-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bookmarks.json");

        let mut store = BookmarkStore::new();
        let stamp = make_stamp(4096);
        assert!(store.toggle(r"C:\logs\big.log", stamp, 8192, "error spike"));
        assert!(store.toggle(r"C:\logs\big.log", stamp, 128, "start"));
        assert!(store.toggle(r"D:\dumps\core.bin", make_stamp(99), 0, ""));
        store.save_to(&path).unwrap();

        let mut loaded = BookmarkStore::load_from(&path);
        let marks = loaded.bookmarks(r"C:\logs\big.log", stamp);
        assert_eq!(marks.len(), 2);
        assert_eq!(marks[0].offset, 128);
        assert_eq!(marks[0].name, "start");
        assert_eq!(marks[1].offset, 8192);
        assert_eq!(
            loaded.next_after(r"C:\logs\big.log", stamp, 128),
            Some(8192)
        );
        assert_eq!(
            loaded.bookmarks(r"D:\dumps\core.bin", make_stamp(99))[0].offset,
            0
        );
        // JSON text round trip is equally faithful (names with
        // quotes and backslashes survive).
        let mut tricky = BookmarkStore::new();
        tricky.toggle("weird \"quoted\" path", make_stamp(4096), 7, "na\\me");
        let back = BookmarkStore::from_json(&tricky.to_json()).unwrap();
        assert_eq!(back, tricky);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stale_stamp_drops_the_record() {
        let mut store = BookmarkStore::new();
        let old = make_stamp(1000);
        let new = make_stamp(2000); // the file grew: old bookmarks are stale
        store.toggle("file.log", old, 10, "a");
        store.toggle("file.log", old, 20, "b");
        assert_eq!(store.bookmarks("file.log", old).len(), 2);
        // Asked with the new stamp: nothing, and the record is gone
        // even for the old stamp afterwards.
        assert!(store.bookmarks("file.log", new).is_empty());
        assert!(store.bookmarks("file.log", old).is_empty());
        assert_eq!(store.next_after("file.log", old, 0), None);
        // Toggling under the new stamp starts a fresh record.
        assert!(store.toggle("file.log", new, 5, "fresh"));
        assert_eq!(store.bookmarks("file.log", new).len(), 1);
        assert_eq!(store.bookmarks("file.log", new)[0].name, "fresh");
    }

    #[test]
    fn missing_and_corrupt_files_load_empty() {
        let dir =
            std::env::temp_dir().join(format!("veloedit-bookmarks-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bookmarks.json");
        assert_eq!(
            BookmarkStore::load_from(&path),
            BookmarkStore::new(),
            "missing file is an empty store"
        );
        std::fs::write(&path, b"{ this is not json").unwrap();
        assert_eq!(BookmarkStore::load_from(&path), BookmarkStore::new());
        // The corrupt file was renamed aside, like settings does.
        assert!(dir.join("bookmarks.json.bad").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
