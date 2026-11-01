//! Command-line parsing and headless command execution.
//!
//! Argument parsing is hand-written: VeloEdit is zero-dependency apart from
//! `windows-sys` (SPEC.md section 1.1), so there is no argument-parsing
//! crate. The recognised interface combines SPEC.md section 7 (file,
//! `--line`, `--mode`, `--read-only`, `--diff`, `--output`, `--headless`,
//! `--help`, `--version`) with the global flags every project in this
//! family carries (`--json`, `--silent`, `--dry-run`, `--verbose`,
//! `--debug`, `--trace`, `--clean`, `--uninstall`).

use std::path::{Path, PathBuf};

use crate::diff;
use crate::encoding::{self, TextEncoding};
use crate::piece_table::PieceTable;
use crate::search::{self, SearchOptions};

/// Full help text, printed by `--help`. Kept as one constant so the exact
/// text is reviewable and testable.
pub const HELP: &str = "\
VeloEdit 0.1.0 — portable binary and large-file editor (Windows 11, 64-bit)

USAGE:
    VeloEdit.exe [OPTIONS] [FILE]
    VeloEdit.exe --diff <A> <B> [--output <REPORT>] [--headless]

OPEN A FILE (GUI arrives in a later build; this foundation build is headless):
    FILE                  File to open
    --line <N>            Open at line N
    --mode <hex|text>     Open in hex or text mode (default: text)
    --read-only           Open without allowing edits

HEADLESS OPERATIONS:
    --diff <A> <B>        Compare two files byte by byte and report the result
    --output <PATH>       Write the diff report JSON to PATH
    --headless            Run without opening a window
    --find <TEXT>         Find TEXT in FILE and print the result as JSON:
                          {\"count\":N,\"offsets\":[...]} — byte offsets, at
                          most 1000 listed (with \"truncated\":true when
                          capped). Exit code is 0 whether or not anything
                          matches. Runs headless; combine with --headless
                          as in: VeloEdit.exe notes.txt --find TODO --headless
    --encoding <NAME>     Encoding used to read FILE for --find:
                          utf8 | utf16le | utf16be | ansi
                          (default: detect from the file's first 64 KB,
                          including any byte-order mark)

CLEANUP:
    --clean               Remove the portable data folder next to the program
    --uninstall           Same as --clean in portable builds
    --dry-run             Show what an operation would do without doing it

GLOBAL FLAGS:
    --json                Print results as JSON
    --silent              Print nothing on success
    --verbose             Print extra detail
    --debug               Enable debug detail (accepted; no effect headless)
    --trace               Enable trace detail (accepted; no effect headless)
    --help                Show this help
    --version             Show the version

EXIT CODES:
    0   Success (for --diff: the files are identical)
    1   An error occurred (bad arguments, unreadable file, failed write)
    2   The GUI was requested but is not part of this foundation build
    3   For --diff: the files differ

VeloEdit works fully offline and writes only to the data folder next to the
program file. It never writes to %APPDATA% or the registry.
";

/// View mode requested on the command line (SPEC.md section 7).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Text,
    Hex,
}

/// Parsed command line. Every field is public so later slices (GUI shell,
/// logging setup) read the same structure the headless runner uses:
/// the command line is one operation surface, not a GUI-only extra.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CliOptions {
    pub file: Option<PathBuf>,
    pub line: Option<u64>,
    pub mode: Mode,
    pub read_only: bool,
    pub diff: Option<(PathBuf, PathBuf)>,
    pub output: Option<PathBuf>,
    pub find: Option<String>,
    pub encoding: Option<TextEncoding>,
    pub headless: bool,
    pub json: bool,
    pub silent: bool,
    pub dry_run: bool,
    pub verbose: bool,
    pub debug: bool,
    pub trace: bool,
    pub clean: bool,
    pub uninstall: bool,
    pub help: bool,
    pub version: bool,
}

/// A command-line problem, reported in plain words with exit code 1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliError {
    pub message: String,
}

impl CliError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Parse arguments (without the program name) by hand.
pub fn parse_args(args: &[String]) -> Result<CliOptions, CliError> {
    let mut opts = CliOptions::default();
    let mut i = 0;
    let mut positional_only = false;
    while i < args.len() {
        let arg = &args[i];
        if positional_only {
            set_file(&mut opts, arg)?;
            i += 1;
            continue;
        }
        // Support --name=value for the value-taking flags.
        let (flag, inline_value) = match arg.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value.to_string())),
            _ => (arg.as_str(), None),
        };
        match flag {
            "--" => positional_only = true,
            "--help" => opts.help = true,
            "--version" => opts.version = true,
            "--read-only" => opts.read_only = true,
            "--headless" => opts.headless = true,
            "--json" => opts.json = true,
            "--silent" => opts.silent = true,
            "--dry-run" => opts.dry_run = true,
            "--verbose" => opts.verbose = true,
            "--debug" => opts.debug = true,
            "--trace" => opts.trace = true,
            "--clean" => opts.clean = true,
            "--uninstall" => opts.uninstall = true,
            "--line" => {
                let value = take_value(args, &mut i, inline_value, "--line")?;
                opts.line = Some(value.parse::<u64>().map_err(|_| {
                    CliError::new(format!("--line needs a whole number, got '{value}'"))
                })?);
            }
            "--mode" => {
                let value = take_value(args, &mut i, inline_value, "--mode")?;
                opts.mode = match value.as_str() {
                    "hex" => Mode::Hex,
                    "text" => Mode::Text,
                    other => {
                        return Err(CliError::new(format!(
                            "--mode must be 'hex' or 'text', got '{other}'"
                        )))
                    }
                };
            }
            "--output" => {
                let value = take_value(args, &mut i, inline_value, "--output")?;
                opts.output = Some(PathBuf::from(value));
            }
            "--find" => {
                let value = take_value(args, &mut i, inline_value, "--find")?;
                opts.find = Some(value);
            }
            "--encoding" => {
                let value = take_value(args, &mut i, inline_value, "--encoding")?;
                opts.encoding = Some(match value.as_str() {
                    "utf8" => TextEncoding::Utf8,
                    "utf16le" => TextEncoding::Utf16Le,
                    "utf16be" => TextEncoding::Utf16Be,
                    "ansi" => TextEncoding::Ansi,
                    other => {
                        return Err(CliError::new(format!(
                        "--encoding must be 'utf8', 'utf16le', 'utf16be' or 'ansi', got '{other}'"
                    )))
                    }
                });
            }
            "--diff" => {
                if inline_value.is_some() {
                    return Err(CliError::new("--diff takes two file arguments: --diff A B"));
                }
                let a = take_value(args, &mut i, None, "--diff")?;
                let b = take_value(args, &mut i, None, "--diff")?;
                opts.diff = Some((PathBuf::from(a), PathBuf::from(b)));
            }
            other if other.starts_with("--") => {
                return Err(CliError::new(format!("unknown option '{other}'")));
            }
            other => set_file(&mut opts, other)?,
        }
        i += 1;
    }
    Ok(opts)
}

fn set_file(opts: &mut CliOptions, value: &str) -> Result<(), CliError> {
    if opts.file.is_some() {
        return Err(CliError::new(format!(
            "only one file can be opened; '{value}' is extra"
        )));
    }
    opts.file = Some(PathBuf::from(value));
    Ok(())
}

/// Take a flag value: the inline `=value` when present, otherwise the next
/// argument (advancing `i` past it).
fn take_value(
    args: &[String],
    i: &mut usize,
    inline_value: Option<String>,
    flag: &str,
) -> Result<String, CliError> {
    if let Some(value) = inline_value {
        return Ok(value);
    }
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| CliError::new(format!("{flag} needs a value but none was given")))
}

/// Report produced by a `--clean` / `--uninstall` run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleanReport {
    pub data_dir: PathBuf,
    pub existed: bool,
    pub dry_run: bool,
    pub entries_found: usize,
}

/// Remove the portable `data/` folder that sits next to the program file.
///
/// Safety rules: the only path ever removed is `<exe_dir>/data` — the
/// function builds that path itself, checks the target really is named
/// `data` and really is a directory directly inside `exe_dir`, and refuses
/// anything else. With `dry_run` it only counts what would be removed.
/// Nothing outside that one folder is ever deleted.
pub fn clean_data_dir(exe_dir: &Path, dry_run: bool) -> Result<CleanReport, String> {
    if exe_dir.as_os_str().is_empty() {
        return Err("could not determine the program folder".to_string());
    }
    let data_dir = exe_dir.join("data");
    if data_dir.file_name().map(|n| n != "data").unwrap_or(true)
        || data_dir.parent() != Some(exe_dir)
    {
        return Err(format!(
            "refusing to clean unexpected path {}",
            data_dir.display()
        ));
    }
    if !data_dir.exists() {
        return Ok(CleanReport {
            data_dir,
            existed: false,
            dry_run,
            entries_found: 0,
        });
    }
    if !data_dir.is_dir() {
        return Err(format!(
            "refusing to remove {} because it is not a folder",
            data_dir.display()
        ));
    }
    let entries_found = std::fs::read_dir(&data_dir)
        .map(|entries| entries.count())
        .map_err(|e| format!("could not read {}: {e}", data_dir.display()))?;
    if !dry_run {
        std::fs::remove_dir_all(&data_dir)
            .map_err(|e| format!("could not remove {}: {e}", data_dir.display()))?;
    }
    Ok(CleanReport {
        data_dir,
        existed: true,
        dry_run,
        entries_found,
    })
}

/// Run the program for already-collected arguments (without the program
/// name) and return the process exit code documented in [`HELP`].
pub fn run(args: &[String]) -> i32 {
    let opts = match parse_args(args) {
        Ok(opts) => opts,
        Err(err) => {
            eprintln!("VeloEdit: {}", err.message);
            eprintln!("Run VeloEdit --help for usage.");
            return 1;
        }
    };

    if opts.help {
        print!("{HELP}");
        return 0;
    }
    if opts.version {
        println!("VeloEdit {}", env!("CARGO_PKG_VERSION"));
        return 0;
    }

    if opts.clean || opts.uninstall {
        return run_clean(&opts);
    }

    if let Some((path_a, path_b)) = &opts.diff {
        return run_diff(&opts, path_a, path_b);
    }

    if opts.find.is_some() {
        return run_find(&opts);
    }

    // Anything else — a file to open, or no arguments at all — is a request
    // to launch the GUI. This foundation build contains no window code yet:
    // the GUI arrives in a later slice, after the interface design is
    // confirmed. Say so plainly instead of failing silently.
    if !opts.silent {
        println!(
            "VeloEdit: the graphical editor is not part of this foundation build ({}).",
            env!("CARGO_PKG_VERSION")
        );
        println!(
            "The editing core and headless tools are ready — run VeloEdit --help to see them."
        );
    }
    2
}

/// Headless `--find`: report every offset of the needle in the file as
/// JSON and exit 0 (also when nothing matches — a search that finds
/// nothing is a successful search).
///
/// This headless tool path reads the file with `std::fs::read` and builds
/// an in-memory piece table over the bytes. That is deliberately *not*
/// how the editor opens files: the GUI path memory-maps the file and
/// never loads it (SPEC.md section 2.1). For a command-line search tool
/// the simple read is the honest trade; the search itself streams over
/// the piece table without copying the document again.
fn run_find(opts: &CliOptions) -> i32 {
    let needle_text = opts.find.as_deref().unwrap_or_default();
    let Some(path) = &opts.file else {
        eprintln!("VeloEdit: --find needs a file to search: VeloEdit.exe <FILE> --find <TEXT>");
        return 1;
    };
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("VeloEdit: could not read {}: {e}", path.display());
            return 1;
        }
    };
    // Encoding: forced by --encoding, otherwise detected from the first
    // 64 KB (detection skips nothing — reported offsets are absolute byte
    // offsets in the file, including for UTF-16 files).
    let sample_len = bytes.len().min(64 * 1024);
    let enc = match opts.encoding {
        Some(enc) => enc,
        None => encoding::detect(&bytes[..sample_len]).encoding,
    };
    if opts.verbose && !opts.silent {
        eprintln!("VeloEdit: searching {} as {}", path.display(), enc.name());
    }
    let needle = match encoding::encode_needle(needle_text, enc) {
        Ok(needle) => needle,
        Err(e) => {
            eprintln!(
                "VeloEdit: could not encode the search text as {}: {e}",
                enc.name()
            );
            return 1;
        }
    };
    let table = PieceTable::from_original(bytes);
    let offsets = search::find_all(&table, &needle, &SearchOptions::default());

    const MAX_PRINTED_OFFSETS: usize = 1000;
    let total = offsets.len();
    let truncated = total > MAX_PRINTED_OFFSETS;
    let mut json = String::from("{\"count\":");
    json.push_str(&total.to_string());
    json.push_str(",\"offsets\":[");
    for (idx, offset) in offsets.iter().take(MAX_PRINTED_OFFSETS).enumerate() {
        if idx > 0 {
            json.push(',');
        }
        json.push_str(&offset.to_string());
    }
    json.push(']');
    if truncated {
        json.push_str(",\"truncated\":true");
    }
    json.push('}');

    if let Some(output) = &opts.output {
        if let Err(e) = std::fs::write(output, json.clone() + "\n") {
            eprintln!("VeloEdit: could not write {}: {e}", output.display());
            return 1;
        }
    }
    if !opts.silent {
        println!("{json}");
    }
    0
}

fn run_clean(opts: &CliOptions) -> i32 {
    let exe_dir = match std::env::current_exe() {
        Ok(mut path) => {
            path.pop();
            path
        }
        Err(e) => {
            eprintln!("VeloEdit: could not find the program folder: {e}");
            return 1;
        }
    };
    match clean_data_dir(&exe_dir, opts.dry_run) {
        Ok(report) => {
            if !opts.silent {
                if !report.existed {
                    println!(
                        "Nothing to clean: no data folder at {}",
                        report.data_dir.display()
                    );
                } else if report.dry_run {
                    println!(
                        "Dry run: would remove {} ({} item(s) inside). Nothing was deleted.",
                        report.data_dir.display(),
                        report.entries_found
                    );
                } else {
                    println!("Removed {}", report.data_dir.display());
                }
            }
            0
        }
        Err(message) => {
            eprintln!("VeloEdit: {message}");
            1
        }
    }
}

fn run_diff(opts: &CliOptions, path_a: &Path, path_b: &Path) -> i32 {
    let report = match diff::diff_files(path_a, path_b) {
        Ok(report) => report,
        Err(e) => {
            eprintln!(
                "VeloEdit: could not compare {} and {}: {e}",
                path_a.display(),
                path_b.display()
            );
            return 1;
        }
    };
    if let Some(output) = &opts.output {
        if let Err(e) = std::fs::write(output, report.to_json() + "\n") {
            eprintln!("VeloEdit: could not write {}: {e}", output.display());
            return 1;
        }
        if opts.verbose && !opts.silent {
            println!("Wrote {}", output.display());
        }
    }
    if !opts.silent {
        if opts.json {
            println!("{}", report.to_json());
        } else if report.equal {
            println!("Files are identical ({} bytes each).", report.len_a);
        } else {
            match report.first_diff_offset {
                Some(offset) => println!(
                    "Files differ: first difference at byte {offset}; {} differing byte(s) in the shared range; sizes {} and {} bytes.",
                    report.diff_byte_count, report.len_a, report.len_b
                ),
                None => println!(
                    "Files differ: sizes {} and {} bytes.",
                    report.len_a, report.len_b
                ),
            }
        }
    }
    if report.equal {
        0
    } else {
        3
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<CliOptions, CliError> {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        parse_args(&owned)
    }

    #[test]
    fn parse_plain_file() {
        let opts = parse(&["dump.bin"]).unwrap();
        assert_eq!(opts.file, Some(PathBuf::from("dump.bin")));
        assert_eq!(opts.mode, Mode::Text);
        assert!(!opts.read_only);
    }

    #[test]
    fn parse_spec_examples() {
        // SPEC.md section 7: GUI at a line offset.
        let opts = parse(&["C:\\Logs\\dump.bin", "--line", "10420"]).unwrap();
        assert_eq!(opts.line, Some(10420));
        // SPEC.md section 7: hex mode, read-only.
        let opts = parse(&["core.dmp", "--mode", "hex", "--read-only"]).unwrap();
        assert_eq!(opts.mode, Mode::Hex);
        assert!(opts.read_only);
        // SPEC.md section 7: headless diff with output.
        let opts = parse(&[
            "--diff",
            "A.bin",
            "B.bin",
            "--output",
            "report.json",
            "--headless",
        ])
        .unwrap();
        assert_eq!(
            opts.diff,
            Some((PathBuf::from("A.bin"), PathBuf::from("B.bin")))
        );
        assert_eq!(opts.output, Some(PathBuf::from("report.json")));
        assert!(opts.headless);
    }

    #[test]
    fn parse_equals_forms_and_globals() {
        let opts = parse(&[
            "f.bin",
            "--line=42",
            "--mode=hex",
            "--json",
            "--silent",
            "--dry-run",
            "--verbose",
            "--debug",
            "--trace",
            "--clean",
            "--uninstall",
        ])
        .unwrap();
        assert_eq!(opts.line, Some(42));
        assert_eq!(opts.mode, Mode::Hex);
        assert!(opts.json && opts.silent && opts.dry_run && opts.verbose);
        assert!(opts.debug && opts.trace && opts.clean && opts.uninstall);
    }

    #[test]
    fn parse_errors_are_plain() {
        assert!(parse(&["--bogus"]).is_err());
        assert!(parse(&["--line"]).is_err());
        assert!(parse(&["--line", "abc"]).is_err());
        assert!(parse(&["--mode", "binary"]).is_err());
        assert!(parse(&["--diff", "only-one"]).is_err());
        assert!(parse(&["a.bin", "b.bin"]).is_err());
        let err = parse(&["--bogus"]).unwrap_err();
        assert!(err.message.contains("--bogus"));
    }

    #[test]
    fn help_and_version_exit_zero() {
        assert_eq!(run(&["--help".to_string()]), 0);
        assert_eq!(run(&["--version".to_string()]), 0);
    }

    #[test]
    fn gui_request_exits_two_in_foundation_build() {
        assert_eq!(run(&["somefile.bin".to_string()]), 2);
        assert_eq!(run(&[]), 2);
    }

    #[test]
    fn diff_run_exit_codes_and_output_file() {
        let dir = std::env::temp_dir().join(format!("veloedit-cli-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.bin");
        let b = dir.join("b.bin");
        let out = dir.join("report.json");
        std::fs::write(&a, b"identical").unwrap();
        std::fs::write(&b, b"identical").unwrap();
        let args: Vec<String> = vec![
            "--diff".into(),
            a.to_string_lossy().into_owned(),
            b.to_string_lossy().into_owned(),
            "--output".into(),
            out.to_string_lossy().into_owned(),
            "--headless".into(),
        ];
        assert_eq!(run(&args), 0);
        let written = std::fs::read_to_string(&out).unwrap();
        assert!(written.contains("\"equal\":true"));

        std::fs::write(&b, b"different!").unwrap();
        assert_eq!(run(&args), 3);
        let written = std::fs::read_to_string(&out).unwrap();
        assert!(written.contains("\"equal\":false"));

        // Missing input file is an error, exit 1.
        let missing: Vec<String> = vec![
            "--diff".into(),
            dir.join("nope.bin").to_string_lossy().into_owned(),
            b.to_string_lossy().into_owned(),
        ];
        assert_eq!(run(&missing), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parse_find_and_encoding() {
        let opts = parse(&["notes.txt", "--find", "TODO", "--headless"]).unwrap();
        assert_eq!(opts.find, Some("TODO".to_string()));
        assert_eq!(opts.encoding, None);
        assert!(opts.headless);
        let opts = parse(&["--find=TODO", "--encoding=utf16le", "notes.txt"]).unwrap();
        assert_eq!(opts.find, Some("TODO".to_string()));
        assert_eq!(opts.encoding, Some(TextEncoding::Utf16Le));
        assert!(parse(&["--encoding", "utf-8", "notes.txt"]).is_err());
        assert!(parse(&["--find"]).is_err());
    }

    #[test]
    fn find_run_exit_codes() {
        let dir = std::env::temp_dir().join(format!("veloedit-find-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("notes.txt");
        std::fs::write(&file, b"TODO: one\nTODO: two\n").unwrap();
        let path = file.to_string_lossy().into_owned();

        // Matches found: exit 0.
        let args: Vec<String> = vec![
            path.clone(),
            "--find".into(),
            "TODO".into(),
            "--headless".into(),
        ];
        assert_eq!(run(&args), 0);
        // No matches: still exit 0 (a successful search).
        let args: Vec<String> = vec![path.clone(), "--find".into(), "absent".into()];
        assert_eq!(run(&args), 0);
        // Forced encoding works too.
        let args: Vec<String> = vec![
            path.clone(),
            "--find".into(),
            "TODO".into(),
            "--encoding".into(),
            "utf8".into(),
        ];
        assert_eq!(run(&args), 0);
        // --find without a file is an error, exit 1.
        let args: Vec<String> = vec!["--find".into(), "TODO".into()];
        assert_eq!(run(&args), 1);
        // A missing file is an error, exit 1.
        let args: Vec<String> = vec![
            dir.join("nope.txt").to_string_lossy().into_owned(),
            "--find".into(),
            "TODO".into(),
        ];
        assert_eq!(run(&args), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clean_only_removes_the_data_folder() {
        let exe_dir =
            std::env::temp_dir().join(format!("veloedit-clean-test-{}", std::process::id()));
        let data_dir = exe_dir.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(data_dir.join("settings.txt"), b"x").unwrap();
        let sibling = exe_dir.join("keep-me.txt");
        std::fs::write(&sibling, b"important").unwrap();

        // Dry run reports but deletes nothing.
        let report = clean_data_dir(&exe_dir, true).unwrap();
        assert!(report.existed);
        assert_eq!(report.entries_found, 1);
        assert!(data_dir.exists());

        // Real run removes data/ only.
        let report = clean_data_dir(&exe_dir, false).unwrap();
        assert!(report.existed);
        assert!(!data_dir.exists());
        assert!(sibling.exists());

        // Second run: nothing to clean is still a success.
        let report = clean_data_dir(&exe_dir, false).unwrap();
        assert!(!report.existed);
        std::fs::remove_dir_all(&exe_dir).unwrap();
    }

    #[test]
    fn clean_refuses_a_non_folder_data_path() {
        let exe_dir =
            std::env::temp_dir().join(format!("veloedit-clean-file-{}", std::process::id()));
        std::fs::create_dir_all(&exe_dir).unwrap();
        std::fs::write(exe_dir.join("data"), b"i am a file, not a folder").unwrap();
        assert!(clean_data_dir(&exe_dir, false).is_err());
        // The file must still be there: refusal never deletes.
        assert!(exe_dir.join("data").exists());
        std::fs::remove_dir_all(&exe_dir).unwrap();
    }
}
