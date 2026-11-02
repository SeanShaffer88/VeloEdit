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
    VeloEdit.exe extract <FILE> --start <N> --length <M> --out <DEST>

OPEN A FILE IN THE GRAPHICAL EDITOR (Windows):
    FILE                  File to open (memory-mapped; huge files open fast)
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
                          capped). Exit code is 0 when at least one match
                          is found, 1 when nothing matches. Runs headless;
                          combine with --headless as in:
                          VeloEdit.exe notes.txt --find TODO --headless
    --regex               With --find: treat the text as a regular
                          expression and report match spans as
                          {\"count\":N,\"spans\":[[START,END],...]}.
                          UTF-8 and ANSI files only (the pattern runs
                          over the file's bytes as-is).
    --encoding <NAME>     Encoding used to read FILE for --find:
                          utf8 | utf16le | utf16be | ansi
                          (default: detect from the file's first 64 KB,
                          including any byte-order mark)
    --save-copy <DEST>    Save a copy of FILE to DEST using the atomic
                          save pipeline (write to a staging file next to
                          DEST, flush, then swap it into place), and
                          print a report; with --json the report is
                          {\"bytes_written\":N,\"staging_removed\":true}.
                          DEST must differ from FILE; FILE itself is
                          never modified. With --output <PATH> the JSON
                          report is also written to PATH.
    extract <FILE>        Copy one byte range of FILE to another file:
    --start <N>           first byte of the range (default 0); <N> and
    --length <M>          <M> are decimal or 0x-hex numbers. --length
    --out <DEST>          defaults to the rest of the file. The range is
                          clamped to the file; a --start beyond the end
                          of the file is an error (exit 2). The bytes are
                          streamed in 4 MB chunks, so huge files are
                          fine. With --json the report is
                          {\"start\":N,\"bytes_written\":M}.
    --split <FILE>        Split FILE into fixed-size parts, streamed:
    --part-size <SIZE>    the part size (required with --split): a
                          whole number of bytes (decimal or 0x-hex) or
                          a number with a K, M or G suffix, like 500M
                          or 4G (K = 1024 bytes, and so on).
    --out-dir <DIR>       Folder the parts are written to (default:
                          the current folder). Parts are named
                          <name>.part001, <name>.part002, ... and
                          joining them in order restores FILE exactly.
                          At most 10000 parts are made. With --json
                          the report is
                          {\"parts\":N,\"part_size\":S,\"out_dir\":\"...\"}.
    --slice \"<SPEC>\"      Extract the ranges SPEC names from FILE to
    --out <DEST>          --out: byte ranges like 0-99 or 0x1000-0x1FFF
                          and line ranges like L1-L20, separated by
                          semicolons, in ascending order (the same
                          grammar as File > Extract Ranges). Line
                          ranges use whole lines, endings included.
                          A bad spec is reported word for word, exit 2.
    --export-patch <P>    Write a VELOPAT1 delta patch of FILE
                          against its on-disk original to P. A
                          headless run has no edits, so the patch has
                          zero hunks; it still round-trips through
                          --apply-patch.
    --apply-patch <P>     Apply the VELOPAT1 patch P to FILE and
    --out <DEST>          write the result to --out. FILE itself is
                          never modified.
    --insert-zeroes <N>   Insert N zero bytes into FILE in place, at
    --at <OFFSET>         --at (default: the end of the file), through
                          the atomic save. N takes the K, M or G
                          suffixes like --part-size.
    --compare <OTHER>     Structural compare: FILE against OTHER in
                          1 MB blocks (SHA-256 per block); prints the
                          differing regions and the first differing
                          byte. Exit 0 when identical, 1 when the
                          block maps differ, 2 on error.

EXPLORER MENU (Windows, opt-in):
    --install-shell       Add an \"Open in VeloEdit\" item to the
                          Explorer right-click menu for all files.
                          It is stored under the current user only
                          (no administrator rights) and is the one
                          registry entry VeloEdit ever writes.
    --uninstall-shell     Remove that menu item again. --clean also
                          removes it.

CLEANUP:
    --clean               Remove the portable data folder next to the program,
                          and the Explorer menu item when one was added
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
    0   Success (for --find: at least one match; for --diff and
        --compare: the files are identical)
    1   For --find: nothing matched. For --diff and --compare: the
        files differ
    2   An error occurred (bad arguments, unreadable file, failed
        write) — also used when the GUI was requested but this
        platform build has no window code

VeloEdit works fully offline and writes only to the data folder next to the
program file. It never writes to %APPDATA%, and the only registry entry it
ever writes is the Explorer menu item — only when you add it with
--install-shell; --uninstall-shell or --clean removes it.
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
    /// `--regex`: the `--find` text is a regular expression
    /// (roadmap item 9, slice R5d). The report then carries match
    /// spans instead of plain offsets.
    pub regex: bool,
    pub encoding: Option<TextEncoding>,
    pub save_copy: Option<PathBuf>,
    /// The `extract` subcommand was given (roadmap item 44): stream
    /// the byte range `extract_start`..+`extract_length` of `file`
    /// to `extract_out`.
    pub extract: bool,
    pub extract_start: Option<u64>,
    pub extract_length: Option<u64>,
    pub extract_out: Option<PathBuf>,
    /// `--split <FILE>` (roadmap item 36): split that file into parts
    /// of `part_size` bytes in `split_out_dir`.
    pub split: Option<PathBuf>,
    pub part_size: Option<u64>,
    pub split_out_dir: Option<PathBuf>,
    /// `--install-shell` / `--uninstall-shell` (roadmap item 38): the
    /// opt-in Explorer context-menu item (Windows registry, HKCU).
    pub install_shell: bool,
    pub uninstall_shell: bool,
    /// `--slice "<spec>" --out <path>` (slice R4c, item 2): extract
    /// the ranges the `crate::slicing` grammar names from `file`.
    pub slice_spec: Option<String>,
    /// Shared `--out <path>` destination for `--slice` and
    /// `--apply-patch` (the `extract` subcommand keeps its own
    /// `extract_out`, set by the same flag).
    pub out: Option<PathBuf>,
    /// `--export-patch <path>` (slice R4c, item 3): write a
    /// VELOPAT1 delta patch of FILE against its on-disk original
    /// (a file compared to itself yields an empty patch).
    pub export_patch: Option<PathBuf>,
    /// `--apply-patch <patch>` (slice R4c, item 3): apply a
    /// VELOPAT1 patch to FILE, writing the result to `--out`
    /// (never over FILE itself).
    pub apply_patch: Option<PathBuf>,
    /// `--insert-zeroes <count>` with `--at <offset>` (slice R4c,
    /// item 4): insert a zero run into FILE in place through the
    /// atomic save; `--at` defaults to the end of the file.
    pub insert_zeroes: Option<u64>,
    pub insert_at: Option<u64>,
    /// `--compare <other>` (slice R4c, item 43): structural
    /// (block-wise) comparison of FILE against another file; exit
    /// 1 when the block maps differ.
    pub compare: Option<PathBuf>,
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

/// A command-line problem, reported in plain words with exit code 2
/// (the standard scheme documented in [`HELP`]).
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

/// Parse a whole number written in decimal or as `0x`/`0X` hex
/// (roadmap item 44: `extract --start` / `--length`, and `--line`).
/// Surrounding spaces are ignored; anything else — signs, empty text,
/// a bare `0x`, values past u64 — is an error, never a guess.
pub fn parse_u64(text: &str) -> Result<u64, CliError> {
    let trimmed = text.trim();
    let parsed = match trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        Some(hex) if !hex.is_empty() => u64::from_str_radix(hex, 16),
        _ => trimmed.parse::<u64>(),
    };
    parsed.map_err(|_| {
        CliError::new(format!(
            "'{text}' is not a whole number (decimal or 0x-hex)"
        ))
    })
}

/// Parse a byte size for `--part-size` (roadmap item 36): a whole
/// number in the [`parse_u64`] forms (decimal or `0x`-hex), optionally
/// followed by one suffix letter — `K`, `M` or `G` (either case) for
/// kibi/mebi/gibibytes (1024-based steps). A size of 0 parses fine
/// here and is refused by the split itself; an overflowing size is an
/// error, never a wraparound.
pub fn parse_size(text: &str) -> Result<u64, CliError> {
    let trimmed = text.trim();
    let (digits, multiplier) = match trimmed.chars().last() {
        Some('K') | Some('k') => (&trimmed[..trimmed.len() - 1], 1024u64),
        Some('M') | Some('m') => (&trimmed[..trimmed.len() - 1], 1024u64 * 1024),
        Some('G') | Some('g') => (&trimmed[..trimmed.len() - 1], 1024u64 * 1024 * 1024),
        _ => (trimmed, 1u64),
    };
    let value = parse_u64(digits).map_err(|_| {
        CliError::new(format!(
            "'{text}' is not a size (a whole number, optionally with a K, M or G suffix)"
        ))
    })?;
    value
        .checked_mul(multiplier)
        .ok_or_else(|| CliError::new(format!("'{text}' is too large to be a size")))
}

/// Parse arguments (without the program name) by hand.
pub fn parse_args(args: &[String]) -> Result<CliOptions, CliError> {
    let mut opts = CliOptions::default();
    // The `extract` subcommand (roadmap item 44) is a leading word,
    // not a flag: `VeloEdit extract FILE --start N --length M --out
    // DEST`. The input file then arrives through the normal file
    // argument, exactly like the other headless operations.
    let args: &[String] = if args.first().map(String::as_str) == Some("extract") {
        opts.extract = true;
        &args[1..]
    } else {
        args
    };
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
            "--regex" => opts.regex = true,
            "--headless" => opts.headless = true,
            "--json" => opts.json = true,
            "--silent" => opts.silent = true,
            "--dry-run" => opts.dry_run = true,
            "--verbose" => opts.verbose = true,
            "--debug" => opts.debug = true,
            "--trace" => opts.trace = true,
            "--clean" => opts.clean = true,
            "--uninstall" => opts.uninstall = true,
            "--install-shell" => opts.install_shell = true,
            "--uninstall-shell" => opts.uninstall_shell = true,
            "--split" => {
                let value = take_value(args, &mut i, inline_value, "--split")?;
                opts.split = Some(PathBuf::from(value));
            }
            "--part-size" => {
                let value = take_value(args, &mut i, inline_value, "--part-size")?;
                opts.part_size = Some(parse_size(&value).map_err(|_| {
                    CliError::new(format!(
                        "--part-size needs a size (a whole number, or one with a K, M or G suffix), got '{value}'"
                    ))
                })?);
            }
            "--out-dir" => {
                let value = take_value(args, &mut i, inline_value, "--out-dir")?;
                opts.split_out_dir = Some(PathBuf::from(value));
            }
            "--line" => {
                let value = take_value(args, &mut i, inline_value, "--line")?;
                opts.line = Some(parse_u64(&value).map_err(|_| {
                    CliError::new(format!("--line needs a whole number, got '{value}'"))
                })?);
            }
            "--start" => {
                let value = take_value(args, &mut i, inline_value, "--start")?;
                opts.extract_start = Some(parse_u64(&value).map_err(|_| {
                    CliError::new(format!(
                        "--start needs a whole number (decimal or 0x-hex), got '{value}'"
                    ))
                })?);
            }
            "--length" => {
                let value = take_value(args, &mut i, inline_value, "--length")?;
                opts.extract_length = Some(parse_u64(&value).map_err(|_| {
                    CliError::new(format!(
                        "--length needs a whole number (decimal or 0x-hex), got '{value}'"
                    ))
                })?);
            }
            "--out" => {
                let value = take_value(args, &mut i, inline_value, "--out")?;
                opts.extract_out = Some(PathBuf::from(value.clone()));
                opts.out = Some(PathBuf::from(value));
            }
            "--slice" => {
                let value = take_value(args, &mut i, inline_value, "--slice")?;
                opts.slice_spec = Some(value);
            }
            "--export-patch" => {
                let value = take_value(args, &mut i, inline_value, "--export-patch")?;
                opts.export_patch = Some(PathBuf::from(value));
            }
            "--apply-patch" => {
                let value = take_value(args, &mut i, inline_value, "--apply-patch")?;
                opts.apply_patch = Some(PathBuf::from(value));
            }
            "--insert-zeroes" => {
                let value = take_value(args, &mut i, inline_value, "--insert-zeroes")?;
                opts.insert_zeroes = Some(parse_size(&value).map_err(|_| {
                    CliError::new(format!(
                        "--insert-zeroes needs a count (a whole number, or one with a K, M or G suffix), got '{value}'"
                    ))
                })?);
            }
            "--at" => {
                let value = take_value(args, &mut i, inline_value, "--at")?;
                opts.insert_at = Some(parse_u64(&value).map_err(|_| {
                    CliError::new(format!(
                        "--at needs a byte offset (decimal or 0x-hex), got '{value}'"
                    ))
                })?);
            }
            "--compare" => {
                let value = take_value(args, &mut i, inline_value, "--compare")?;
                opts.compare = Some(PathBuf::from(value));
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
            "--save-copy" => {
                let value = take_value(args, &mut i, inline_value, "--save-copy")?;
                opts.save_copy = Some(PathBuf::from(value));
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
    // Before anything can print (including argument errors): on
    // Windows, borrow the parent terminal's console for this run —
    // the binary is GUI-subsystem, so it has no console of its own
    // (the call lives in gui_win, the crate's unsafe-sanctioned
    // Windows module). The code-page converter (slice R4c, item
    // 29) is installed at the same point so every later decode —
    // GUI or headless — uses the same Win32 conversion.
    #[cfg(windows)]
    crate::gui_win::attach_parent_console();
    #[cfg(windows)]
    crate::gui_win::install_page_converter();

    let opts = match parse_args(args) {
        Ok(opts) => opts,
        Err(err) => {
            eprintln!("VeloEdit: {}", err.message);
            eprintln!("Run VeloEdit --help for usage.");
            return 2;
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

    if opts.install_shell || opts.uninstall_shell {
        return run_shell_menu(&opts);
    }

    if opts.extract {
        return run_extract(&opts);
    }

    if opts.split.is_some() {
        return run_split(&opts);
    }

    if opts.slice_spec.is_some() {
        return run_slice(&opts);
    }

    if opts.export_patch.is_some() {
        return run_export_patch(&opts);
    }

    if opts.apply_patch.is_some() {
        return run_apply_patch(&opts);
    }

    if opts.insert_zeroes.is_some() {
        return run_insert_zeroes(&opts);
    }

    if opts.compare.is_some() {
        return run_compare(&opts);
    }

    if let Some((path_a, path_b)) = &opts.diff {
        return run_diff(&opts, path_a, path_b);
    }

    if opts.find.is_some() {
        return run_find(&opts);
    }

    if opts.save_copy.is_some() {
        return run_save_copy(&opts);
    }

    // Anything else — a file to open, or no arguments at all — is a request
    // to launch the GUI. On Windows the Win32 + Direct2D shell
    // (crate::gui_win) takes it from here and runs the message loop. Other
    // platforms have no window code in this build and say so plainly
    // instead of failing silently.
    #[cfg(windows)]
    return crate::gui_win::run_gui(&opts);

    #[cfg(not(windows))]
    {
        if !opts.silent {
            println!(
                "VeloEdit: the graphical editor is only built for Windows ({}).",
                env!("CARGO_PKG_VERSION")
            );
            println!(
                "The editing core and headless tools are ready — run VeloEdit --help to see them."
            );
        }
        2
    }
}

/// Headless `--find`: report every offset of the needle in the file as
/// JSON. Exit codes follow the standard scheme (roadmap item 45): 0
/// when at least one match was found, 1 when nothing matched, 2 on
/// any usage or I/O error.
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
        return 2;
    };
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("VeloEdit: could not read {}: {e}", path.display());
            return 2;
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
    let table = PieceTable::from_original(bytes);
    if opts.regex {
        return run_find_regex(opts, &table, enc, needle_text);
    }
    let needle = match encoding::encode_needle(needle_text, enc) {
        Ok(needle) => needle,
        Err(e) => {
            eprintln!(
                "VeloEdit: could not encode the search text as {}: {e}",
                enc.name()
            );
            return 2;
        }
    };
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
            return 2;
        }
    }
    if !opts.silent {
        println!("{json}");
    }
    // The standard scheme: a search that found nothing is a
    // successful run with a "no" answer — exit 1, not an error.
    if total > 0 {
        0
    } else {
        1
    }
}

/// The `--regex` form of headless `--find` (roadmap item 9, slice
/// R5d): the pattern runs over the file's bytes as-is, so only the
/// single-byte encodings are accepted — a UTF-16 file would need
/// the pattern woven into code units, which this tool declines
/// rather than guesses at. The report carries match spans:
/// `{"count":N,"spans":[[START,END],...]}` with the same 1000-entry
/// cap, `--output` handling and exit codes as the plain find.
fn run_find_regex(opts: &CliOptions, table: &PieceTable, enc: TextEncoding, pattern: &str) -> i32 {
    if !matches!(enc, TextEncoding::Utf8 | TextEncoding::Ansi) {
        eprintln!(
            "VeloEdit: --regex searches the file's bytes as-is and works with UTF-8 or ANSI files; this file is {}",
            enc.name()
        );
        return 2;
    }
    let re = match crate::regex::Regex::new(pattern) {
        Ok(re) => re,
        Err(e) => {
            eprintln!("VeloEdit: bad regular expression: {e}");
            return 2;
        }
    };
    let spans = crate::op_find_regex(table, &re);
    const MAX_PRINTED_SPANS: usize = 1000;
    let total = spans.len();
    let truncated = total > MAX_PRINTED_SPANS;
    let mut json = String::from("{\"count\":");
    json.push_str(&total.to_string());
    json.push_str(",\"spans\":[");
    for (idx, (start, end)) in spans.iter().take(MAX_PRINTED_SPANS).enumerate() {
        if idx > 0 {
            json.push(',');
        }
        json.push_str(&format!("[{start},{end}]"));
    }
    json.push(']');
    if truncated {
        json.push_str(",\"truncated\":true");
    }
    json.push('}');
    if let Some(output) = &opts.output {
        if let Err(e) = std::fs::write(output, json.clone() + "\n") {
            eprintln!("VeloEdit: could not write {}: {e}", output.display());
            return 2;
        }
    }
    if !opts.silent {
        println!("{json}");
    }
    if total > 0 {
        0
    } else {
        1
    }
}

/// Load a file as a piece table for the headless `--save-copy` tool:
/// memory-mapped on Windows (the production open path, SPEC.md section
/// 2.1 — no bytes are loaded), read into memory on the host.
#[cfg(windows)]
fn load_table_for_copy(path: &Path) -> Result<PieceTable, String> {
    let mmap = crate::mmap_win::MmapFile::open(path)
        .map_err(|e| format!("could not map {}: {e}", path.display()))?;
    Ok(PieceTable::from_mapped(std::sync::Arc::new(mmap)))
}

/// Load a file as a piece table for the headless `--save-copy` tool
/// (host form; Windows memory-maps instead — see above).
#[cfg(not(windows))]
fn load_table_for_copy(path: &Path) -> Result<PieceTable, String> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    Ok(PieceTable::from_original(bytes))
}

/// Headless `--save-copy`: save a copy of FILE to the destination with
/// the atomic save pipeline (`crate::save::save_atomic`) and report the
/// result; exit 0 on success, 2 on any error.
///
/// The destination must differ from the source: saving a file *over
/// itself* is the editor's save operation (which closes our mapping of
/// the target first — `PieceTable::save_to`), not a copy, so it is
/// refused here with a plain message instead of risking a sharing
/// failure on Windows.
fn run_save_copy(opts: &CliOptions) -> i32 {
    let dest = opts.save_copy.as_deref().unwrap_or_else(|| Path::new(""));
    let Some(path) = &opts.file else {
        eprintln!(
            "VeloEdit: --save-copy needs a file to copy: VeloEdit.exe <FILE> --save-copy <DEST>"
        );
        return 2;
    };
    if dest == path.as_path() {
        eprintln!(
            "VeloEdit: --save-copy destination must differ from the source file ({})",
            path.display()
        );
        return 2;
    }
    let table = match load_table_for_copy(path) {
        Ok(table) => table,
        Err(message) => {
            eprintln!("VeloEdit: {message}");
            return 2;
        }
    };
    let report = match crate::save::save_atomic(&table, dest, &crate::save::SaveOptions::default())
    {
        Ok(report) => report,
        Err(e) => {
            eprintln!("VeloEdit: could not save copy to {}: {e}", dest.display());
            return 2;
        }
    };
    let json = format!(
        "{{\"bytes_written\":{},\"staging_removed\":{}}}",
        report.bytes_written, report.staging_removed
    );
    if let Some(output) = &opts.output {
        if let Err(e) = std::fs::write(output, json.clone() + "\n") {
            eprintln!("VeloEdit: could not write {}: {e}", output.display());
            return 2;
        }
    }
    if !opts.silent {
        if opts.json {
            println!("{json}");
        } else {
            println!(
                "Saved a copy of {} to {} ({} bytes).",
                path.display(),
                dest.display(),
                report.bytes_written
            );
        }
    }
    0
}

/// A `read_at` closure over a plain file (seek + read) — the
/// random-access byte source the patch and compare cores take.
fn file_reader(file: std::fs::File) -> impl FnMut(u64, &mut [u8]) -> usize {
    let mut file = file;
    move |offset: u64, out: &mut [u8]| -> usize {
        use std::io::{Read, Seek, SeekFrom};
        if file.seek(SeekFrom::Start(offset)).is_err() {
            return 0;
        }
        file.read(out).unwrap_or(0)
    }
}

/// Headless `--slice "<spec>" --out <path>` (slice R4c, item 2):
/// extract the ranges the slicing grammar names from FILE,
/// streamed. The spec error text is printed verbatim — it is
/// written for the person typing the spec. Exit 0 on success, 2
/// on any usage, spec or I/O error. Line ranges need a complete
/// line index: the first attempt runs without one, and only an
/// index-required answer builds it and retries (byte-range specs
/// never pay for an index).
fn run_slice(opts: &CliOptions) -> i32 {
    let spec = opts.slice_spec.as_deref().unwrap_or_default();
    let Some(path) = &opts.file else {
        eprintln!(
            "VeloEdit: --slice needs a file: VeloEdit.exe <FILE> --slice \"<spec>\" --out <DEST>"
        );
        return 2;
    };
    let Some(out_path) = &opts.out else {
        eprintln!("VeloEdit: --slice needs a destination: --out <DEST>");
        return 2;
    };
    if out_path == path {
        eprintln!(
            "VeloEdit: --slice --out must differ from the input file ({})",
            path.display()
        );
        return 2;
    }
    let table = match load_table_for_copy(path) {
        Ok(table) => table,
        Err(message) => {
            eprintln!("VeloEdit: {message}");
            return 2;
        }
    };
    let attempt = |index: Option<&crate::line_index::LineIndex>| -> Result<crate::slicing::ExtractReport, crate::slicing::SliceError> {
        let mut out_file =
            std::fs::File::create(out_path).map_err(crate::slicing::SliceError::Io)?;
        let report = crate::slicing::extract_ranges(&table, index, spec, &mut out_file)?;
        std::io::Write::flush(&mut out_file).map_err(crate::slicing::SliceError::Io)?;
        Ok(report)
    };
    let report = match attempt(None) {
        Ok(report) => report,
        Err(crate::slicing::SliceError::IndexRequired) => {
            let index = crate::line_index::LineIndex::build(table.len(), |offset, buf| {
                table.read_into(offset, buf)
            });
            match attempt(Some(&index)) {
                Ok(report) => report,
                Err(e) => {
                    eprintln!("VeloEdit: {e}");
                    return 2;
                }
            }
        }
        Err(e) => {
            eprintln!("VeloEdit: {e}");
            return 2;
        }
    };
    if !opts.silent {
        if opts.json {
            println!(
                "{{\"ranges\":{},\"bytes_written\":{}}}",
                report.ranges, report.bytes_written
            );
        } else {
            println!(
                "Extracted {} range(s), {} byte(s), from {} to {}.",
                report.ranges,
                report.bytes_written,
                path.display(),
                out_path.display()
            );
        }
    }
    0
}

/// Headless `--export-patch <path>` (slice R4c, item 3): write a
/// VELOPAT1 delta patch of FILE against its on-disk original. A
/// headless run has no edits, so the patch honestly has zero
/// hunks — the format round-trips and `--apply-patch` reproduces
/// the file. Exit 0 on success, 2 on error.
fn run_export_patch(opts: &CliOptions) -> i32 {
    let dest = opts
        .export_patch
        .as_deref()
        .unwrap_or_else(|| Path::new(""));
    let Some(path) = &opts.file else {
        eprintln!(
            "VeloEdit: --export-patch needs a file: VeloEdit.exe <FILE> --export-patch <PATCH>"
        );
        return 2;
    };
    let table = match load_table_for_copy(path) {
        Ok(table) => table,
        Err(message) => {
            eprintln!("VeloEdit: {message}");
            return 2;
        }
    };
    let source = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) => {
            eprintln!("VeloEdit: could not read {}: {e}", path.display());
            return 2;
        }
    };
    let patch =
        match crate::patch::build_patch(table.original_file_size, file_reader(source), &table) {
            Ok(patch) => patch,
            Err(e) => {
                eprintln!(
                    "VeloEdit: could not build the patch for {}: {e}",
                    path.display()
                );
                return 2;
            }
        };
    let bytes = patch.to_bytes();
    if let Err(e) = std::fs::write(dest, &bytes) {
        eprintln!("VeloEdit: could not write {}: {e}", dest.display());
        return 2;
    }
    if !opts.silent {
        if opts.json {
            println!(
                "{{\"patch\":\"{}\",\"hunks\":{},\"bytes\":{},\"original_len\":{},\"result_len\":{}}}",
                crate::settings::json_escape(&dest.to_string_lossy()),
                patch.hunks.len(),
                bytes.len(),
                patch.original_len,
                patch.result_len
            );
        } else {
            println!(
                "Wrote a delta patch with {} hunk(s) ({} bytes) to {}.",
                patch.hunks.len(),
                bytes.len(),
                dest.display()
            );
        }
    }
    0
}

/// Headless `--apply-patch <patch> --out <path>` (slice R4c,
/// item 3): apply a VELOPAT1 patch to FILE and write the result
/// to `--out`. FILE itself is never modified — the same rule as
/// the window's Apply Patch. Exit 0 on success, 2 on error
/// (including a patch that does not match the base file).
fn run_apply_patch(opts: &CliOptions) -> i32 {
    let patch_path = opts.apply_patch.as_deref().unwrap_or_else(|| Path::new(""));
    let Some(path) = &opts.file else {
        eprintln!(
            "VeloEdit: --apply-patch needs a base file: VeloEdit.exe <FILE> --apply-patch <PATCH> --out <DEST>"
        );
        return 2;
    };
    let Some(out_path) = &opts.out else {
        eprintln!("VeloEdit: --apply-patch needs a destination: --out <DEST>");
        return 2;
    };
    if out_path == path {
        eprintln!(
            "VeloEdit: --apply-patch --out must differ from the base file ({}); the base is never overwritten",
            path.display()
        );
        return 2;
    }
    let patch_bytes = match std::fs::read(patch_path) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("VeloEdit: could not read {}: {e}", patch_path.display());
            return 2;
        }
    };
    let patch = match crate::patch::Patch::from_bytes(&patch_bytes) {
        Ok(patch) => patch,
        Err(e) => {
            eprintln!(
                "VeloEdit: {} is not a usable patch: {e}",
                patch_path.display()
            );
            return 2;
        }
    };
    let base = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) => {
            eprintln!("VeloEdit: could not read {}: {e}", path.display());
            return 2;
        }
    };
    let base_len = match base.metadata() {
        Ok(md) => md.len(),
        Err(e) => {
            eprintln!("VeloEdit: could not read {}: {e}", path.display());
            return 2;
        }
    };
    let mut out_file = match std::fs::File::create(out_path) {
        Ok(file) => file,
        Err(e) => {
            eprintln!("VeloEdit: could not create {}: {e}", out_path.display());
            return 2;
        }
    };
    let written =
        match crate::patch::apply_patch(base_len, file_reader(base), &patch, &mut out_file) {
            Ok(written) => written,
            Err(e) => {
                eprintln!("VeloEdit: could not apply {}: {e}", patch_path.display());
                return 2;
            }
        };
    if let Err(e) = std::io::Write::flush(&mut out_file) {
        eprintln!("VeloEdit: could not finish {}: {e}", out_path.display());
        return 2;
    }
    if !opts.silent {
        if opts.json {
            println!("{{\"bytes_written\":{written}}}");
        } else {
            println!(
                "Applied {} to {} and wrote {} byte(s) to {}.",
                patch_path.display(),
                path.display(),
                written,
                out_path.display()
            );
        }
    }
    0
}

/// Headless `--insert-zeroes <count>` (slice R4c, item 4): insert
/// a run of zero bytes into FILE at `--at <offset>` (default: the
/// end of the file), in place through the atomic save — the same
/// operation the window's Insert Zeroes runs, so the result is
/// one atomic swap, never a half-written file. Exit 0 on success,
/// 2 on any error.
fn run_insert_zeroes(opts: &CliOptions) -> i32 {
    let count = opts.insert_zeroes.unwrap_or(0);
    let Some(path) = &opts.file else {
        eprintln!(
            "VeloEdit: --insert-zeroes needs a file: VeloEdit.exe <FILE> --insert-zeroes <COUNT>"
        );
        return 2;
    };
    if count == 0 {
        eprintln!("VeloEdit: --insert-zeroes needs a count greater than zero");
        return 2;
    }
    let mut table = match load_table_for_copy(path) {
        Ok(table) => table,
        Err(message) => {
            eprintln!("VeloEdit: {message}");
            return 2;
        }
    };
    let at = opts.insert_at.unwrap_or(table.len());
    if at > table.len() {
        eprintln!(
            "VeloEdit: --at {at} is beyond the end of {} ({} bytes)",
            path.display(),
            table.len()
        );
        return 2;
    }
    let mut history = crate::undo::UndoStack::new();
    crate::op_insert_zeroes(&mut table, &mut history, at, count);
    let report = match table.save_to(path, &mut history) {
        Ok(report) => report,
        Err(e) => {
            eprintln!("VeloEdit: could not save {}: {e}", path.display());
            return 2;
        }
    };
    if !opts.silent {
        if opts.json {
            println!(
                "{{\"inserted\":{count},\"at\":{at},\"bytes_written\":{}}}",
                report.bytes_written
            );
        } else {
            println!(
                "Inserted {count} zero byte(s) at byte {at} in {} (now {} bytes).",
                path.display(),
                report.bytes_written
            );
        }
    }
    0
}

/// Headless `--compare <other>` (slice R4c, item 43): the
/// structural diff — compare FILE against another file block by
/// block (1 MB blocks, SHA-256 per block) and print the differing
/// regions. Exit 0 when the files are identical, 1 when they
/// differ, 2 on error — the `--diff` scheme, at block level.
fn run_compare(opts: &CliOptions) -> i32 {
    let other = opts.compare.as_deref().unwrap_or_else(|| Path::new(""));
    let Some(path) = &opts.file else {
        eprintln!("VeloEdit: --compare needs a file: VeloEdit.exe <FILE> --compare <OTHER>");
        return 2;
    };
    let table_a = match load_table_for_copy(path) {
        Ok(table) => table,
        Err(message) => {
            eprintln!("VeloEdit: {message}");
            return 2;
        }
    };
    let table_b = match load_table_for_copy(other) {
        Ok(table) => table,
        Err(message) => {
            eprintln!("VeloEdit: {message}");
            return 2;
        }
    };
    let report = crate::op_structural_diff(
        table_a.len(),
        |offset, buf| table_a.read_into(offset, buf),
        table_b.len(),
        |offset, buf| table_b.read_into(offset, buf),
        crate::structdiff::DEFAULT_BLOCK_SIZE,
    );
    if !opts.silent {
        if opts.json {
            let mut json = String::from("{\"equal\":");
            json.push_str(if report.equal() { "true" } else { "false" });
            json.push_str(",\"first_divergence\":");
            match report.first_divergence {
                Some(offset) => json.push_str(&offset.to_string()),
                None => json.push_str("null"),
            }
            json.push_str(",\"regions\":[");
            for (i, region) in report.regions.iter().enumerate() {
                if i > 0 {
                    json.push(',');
                }
                json.push_str(&format!(
                    "{{\"offset\":{},\"length\":{}}}",
                    region.offset, region.length
                ));
            }
            json.push_str(&format!(
                "],\"len_a\":{},\"len_b\":{},\"block_size\":{}}}",
                report.len_a, report.len_b, report.block_size
            ));
            println!("{json}");
        } else if report.equal() {
            println!("Files are identical ({} bytes each).", report.len_a);
        } else {
            println!(
                "Files differ in {} region(s); first difference at byte {}.",
                report.regions.len(),
                report.first_divergence.unwrap_or(0)
            );
            for region in &report.regions {
                println!("  byte {}: {} byte(s) differ", region.offset, region.length);
            }
        }
    }
    if report.equal() {
        0
    } else {
        1
    }
}

/// Headless `extract` subcommand (roadmap item 44): stream one byte
/// range of FILE to `--out`, in 4 MB chunks, and report the result.
/// Exit 0 on success, 2 on any usage or I/O error. A `--start` beyond
/// the end of the file is an error and no output file is created; a
/// `--length` running past the end is clamped to what exists. The
/// input is opened the same way `--save-copy` opens it (memory-mapped
/// on Windows), so carving a range out of a huge file loads nothing.
fn run_extract(opts: &CliOptions) -> i32 {
    let Some(path) = &opts.file else {
        eprintln!(
            "VeloEdit: extract needs a file: VeloEdit.exe extract <FILE> --start <N> --length <M> --out <DEST>"
        );
        return 2;
    };
    let Some(out_path) = &opts.extract_out else {
        eprintln!("VeloEdit: extract needs a destination: --out <DEST>");
        return 2;
    };
    if out_path == path {
        eprintln!(
            "VeloEdit: extract --out must differ from the input file ({})",
            path.display()
        );
        return 2;
    }
    let table = match load_table_for_copy(path) {
        Ok(table) => table,
        Err(message) => {
            eprintln!("VeloEdit: {message}");
            return 2;
        }
    };
    let start = opts.extract_start.unwrap_or(0);
    if start > table.len() {
        eprintln!(
            "VeloEdit: --start {start} is beyond the end of {} ({} bytes)",
            path.display(),
            table.len()
        );
        return 2;
    }
    let length = opts.extract_length.unwrap_or(table.len() - start);
    let mut out_file = match std::fs::File::create(out_path) {
        Ok(file) => file,
        Err(e) => {
            eprintln!("VeloEdit: could not create {}: {e}", out_path.display());
            return 2;
        }
    };
    let written = match crate::save::write_table_range(&table, start, length, &mut out_file) {
        Ok(written) => written,
        Err(e) => {
            eprintln!("VeloEdit: could not write {}: {e}", out_path.display());
            return 2;
        }
    };
    if let Err(e) = std::io::Write::flush(&mut out_file) {
        eprintln!("VeloEdit: could not finish {}: {e}", out_path.display());
        return 2;
    }
    if !opts.silent {
        if opts.json {
            println!("{{\"start\":{start},\"bytes_written\":{written}}}");
        } else {
            println!(
                "Extracted {written} byte(s) from {} starting at byte {start} to {}.",
                path.display(),
                out_path.display()
            );
        }
    }
    0
}

/// Headless `--split` (roadmap item 36): split the file into parts
/// of `--part-size` bytes in `--out-dir` (default: the current
/// folder), streamed by [`crate::save::split_file`]. Exit 0 on
/// success, 2 on any usage or I/O error.
fn run_split(opts: &CliOptions) -> i32 {
    let path = opts.split.as_deref().unwrap_or_else(|| Path::new(""));
    let Some(part_size) = opts.part_size else {
        eprintln!(
            "VeloEdit: --split needs a part size: --part-size <SIZE> (for example 500M or 4G)"
        );
        return 2;
    };
    let out_dir = opts
        .split_out_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("."));
    let parts = match crate::save::split_file(path, part_size, &out_dir) {
        Ok(parts) => parts,
        Err(e) => {
            eprintln!("VeloEdit: could not split {}: {e}", path.display());
            return 2;
        }
    };
    if !opts.silent {
        if opts.json {
            println!(
                "{{\"parts\":{},\"part_size\":{},\"out_dir\":\"{}\"}}",
                parts.len(),
                part_size,
                crate::settings::json_escape(&out_dir.to_string_lossy())
            );
        } else {
            println!(
                "Split {} into {} part(s) of at most {} bytes in {}.",
                path.display(),
                parts.len(),
                part_size,
                out_dir.display()
            );
        }
    }
    0
}

/// `--install-shell` / `--uninstall-shell` (roadmap item 38): the
/// opt-in Explorer context-menu item lives in the Windows registry
/// (current user only); the calls themselves are in
/// [`crate::gui_win`], the crate's Windows module. Exit 0 on success
/// (including "was not installed" for a removal), 2 on error — and on
/// any non-Windows build, where there is no Explorer menu to touch.
fn run_shell_menu(opts: &CliOptions) -> i32 {
    #[cfg(windows)]
    {
        let result = if opts.install_shell {
            crate::gui_win::shell_menu_install().map(|_| ())
        } else {
            crate::gui_win::shell_menu_uninstall().map(|_| ())
        };
        match result {
            Ok(()) => {
                if !opts.silent {
                    println!(
                        "{}",
                        if opts.install_shell {
                            "Added \"Open in VeloEdit\" to the Explorer menu for all files."
                        } else {
                            "Removed \"Open in VeloEdit\" from the Explorer menu."
                        }
                    );
                }
                0
            }
            Err(message) => {
                eprintln!("VeloEdit: {message}");
                2
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = opts;
        eprintln!("VeloEdit: the Explorer menu item is a Windows feature; this build has no registry to write to.");
        2
    }
}

fn run_clean(opts: &CliOptions) -> i32 {
    let exe_dir = match std::env::current_exe() {
        Ok(mut path) => {
            path.pop();
            path
        }
        Err(e) => {
            eprintln!("VeloEdit: could not find the program folder: {e}");
            return 2;
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
            // Roadmap item 38: --clean also removes the Explorer menu
            // item when one was installed — a portable cleanup should
            // leave nothing behind, in the data folder or the registry.
            // A dry run only previews, so it removes nothing here.
            #[cfg(windows)]
            if !opts.dry_run {
                match crate::gui_win::shell_menu_uninstall() {
                    Ok(true) => {
                        if !opts.silent {
                            println!("Removed the Explorer menu item.");
                        }
                    }
                    Ok(false) => {}
                    Err(message) => {
                        eprintln!("VeloEdit: {message}");
                        return 2;
                    }
                }
            }
            0
        }
        Err(message) => {
            eprintln!("VeloEdit: {message}");
            2
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
            return 2;
        }
    };
    if let Some(output) = &opts.output {
        if let Err(e) = std::fs::write(output, report.to_json() + "\n") {
            eprintln!("VeloEdit: could not write {}: {e}", output.display());
            return 2;
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
        1
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

    // On Windows a bare file request opens the GUI (covered by the
    // Windows build, not by host tests); everywhere else it exits 2.
    #[cfg(not(windows))]
    #[test]
    fn gui_request_exits_two_without_a_gui_build() {
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
        assert_eq!(run(&args), 1);
        let written = std::fs::read_to_string(&out).unwrap();
        assert!(written.contains("\"equal\":false"));

        // Missing input file is an error, exit 2.
        let missing: Vec<String> = vec![
            "--diff".into(),
            dir.join("nope.bin").to_string_lossy().into_owned(),
            b.to_string_lossy().into_owned(),
        ];
        assert_eq!(run(&missing), 2);
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
    fn regex_find_run() {
        let dir =
            std::env::temp_dir().join(format!("veloedit-regex-find-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("nums.txt");
        std::fs::write(&file, b"a1 b22 c333\n").unwrap();
        let path = file.to_string_lossy().into_owned();
        // The flag parses alongside --find.
        let opts = parse(&["x.txt", "--find", "[0-9]+", "--regex"]).unwrap();
        assert!(opts.regex);
        // Matches: exit 0. No match: exit 1. A pattern that does
        // not compile and a UTF-16 file are usage errors: exit 2.
        let args: Vec<String> = vec![
            path.clone(),
            "--find".into(),
            "[0-9]+".into(),
            "--regex".into(),
        ];
        assert_eq!(run(&args), 0);
        let args: Vec<String> = vec![
            path.clone(),
            "--find".into(),
            "z{2,}".into(),
            "--regex".into(),
        ];
        assert_eq!(run(&args), 1);
        let args: Vec<String> = vec![
            path.clone(),
            "--find".into(),
            "(unclosed".into(),
            "--regex".into(),
        ];
        assert_eq!(run(&args), 2);
        let args: Vec<String> = vec![
            path.clone(),
            "--find".into(),
            "[0-9]+".into(),
            "--regex".into(),
            "--encoding=utf16le".into(),
        ];
        assert_eq!(run(&args), 2);
        std::fs::remove_dir_all(&dir).unwrap();
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
        // No matches: exit 1 (the search ran fine; the answer is "no").
        let args: Vec<String> = vec![path.clone(), "--find".into(), "absent".into()];
        assert_eq!(run(&args), 1);
        // Forced encoding works too.
        let args: Vec<String> = vec![
            path.clone(),
            "--find".into(),
            "TODO".into(),
            "--encoding".into(),
            "utf8".into(),
        ];
        assert_eq!(run(&args), 0);
        // --find without a file is an error, exit 2.
        let args: Vec<String> = vec!["--find".into(), "TODO".into()];
        assert_eq!(run(&args), 2);
        // A missing file is an error, exit 2.
        let args: Vec<String> = vec![
            dir.join("nope.txt").to_string_lossy().into_owned(),
            "--find".into(),
            "TODO".into(),
        ];
        assert_eq!(run(&args), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parse_u64_accepts_decimal_and_hex() {
        assert_eq!(parse_u64("0").unwrap(), 0);
        assert_eq!(parse_u64("42").unwrap(), 42);
        assert_eq!(parse_u64("0x1000").unwrap(), 4096);
        assert_eq!(parse_u64("0Xff").unwrap(), 255);
        assert_eq!(parse_u64(" 17 ").unwrap(), 17);
        assert!(parse_u64("abc").is_err());
        assert!(parse_u64("0x").is_err());
        assert!(parse_u64("-1").is_err());
        assert!(parse_u64("").is_err());
        // One past u64::MAX overflows: an error, never a wraparound.
        assert!(parse_u64("18446744073709551616").is_err());
    }

    #[test]
    fn parse_extract_subcommand() {
        let opts = parse(&[
            "extract", "big.bin", "--start", "0x1000", "--length", "4096", "--out", "part.bin",
        ])
        .unwrap();
        assert!(opts.extract);
        assert_eq!(opts.file, Some(PathBuf::from("big.bin")));
        assert_eq!(opts.extract_start, Some(4096));
        assert_eq!(opts.extract_length, Some(4096));
        assert_eq!(opts.extract_out, Some(PathBuf::from("part.bin")));
        // A plain file open is not an extract.
        let opts = parse(&["big.bin"]).unwrap();
        assert!(!opts.extract);
        // A bad number is a parse error (exit 2 through run()).
        assert!(parse(&["extract", "big.bin", "--start", "zz"]).is_err());
    }

    #[test]
    fn extract_run_carves_and_clamps_ranges() {
        let dir =
            std::env::temp_dir().join(format!("veloedit-extract-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.bin");
        std::fs::write(&src, b"0123456789ABCDEF").unwrap();
        let out = dir.join("slice.bin");
        let src_s = src.to_string_lossy().into_owned();
        let out_s = out.to_string_lossy().into_owned();

        // A plain decimal range lands byte-exact.
        let args: Vec<String> = vec![
            "extract".into(),
            src_s.clone(),
            "--start".into(),
            "4".into(),
            "--length".into(),
            "6".into(),
            "--out".into(),
            out_s.clone(),
        ];
        assert_eq!(run(&args), 0);
        assert_eq!(std::fs::read(&out).unwrap(), b"456789");

        // A hex start, with the length clamped to the end of the file.
        let args: Vec<String> = vec![
            "extract".into(),
            src_s.clone(),
            "--start".into(),
            "0xE".into(),
            "--length".into(),
            "100".into(),
            "--out".into(),
            out_s.clone(),
        ];
        assert_eq!(run(&args), 0);
        assert_eq!(std::fs::read(&out).unwrap(), b"EF");

        // Defaults: no --start/--length copies the whole file.
        let args: Vec<String> = vec![
            "extract".into(),
            src_s.clone(),
            "--out".into(),
            out_s.clone(),
        ];
        assert_eq!(run(&args), 0);
        assert_eq!(std::fs::read(&out).unwrap(), b"0123456789ABCDEF");

        // A start beyond the end: exit 2, and no output file appears.
        let never = dir.join("never.bin");
        let args: Vec<String> = vec![
            "extract".into(),
            src_s.clone(),
            "--start".into(),
            "17".into(),
            "--out".into(),
            never.to_string_lossy().into_owned(),
        ];
        assert_eq!(run(&args), 2);
        assert!(!never.exists());

        // A missing --out and a missing input file are both exit 2.
        let args: Vec<String> = vec!["extract".into(), src_s.clone()];
        assert_eq!(run(&args), 2);
        let args: Vec<String> = vec![
            "extract".into(),
            dir.join("nope.bin").to_string_lossy().into_owned(),
            "--out".into(),
            out_s.clone(),
        ];
        assert_eq!(run(&args), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parse_size_accepts_plain_and_suffixed_sizes() {
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert_eq!(parse_size("0x1000").unwrap(), 4096);
        assert_eq!(parse_size("4K").unwrap(), 4 * 1024);
        assert_eq!(parse_size("500M").unwrap(), 500 * 1024 * 1024);
        assert_eq!(parse_size("4G").unwrap(), 4 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("2g").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_size(" 10M ").unwrap(), 10 * 1024 * 1024);
        assert!(parse_size("").is_err());
        assert!(parse_size("M").is_err());
        assert!(parse_size("10T").is_err());
        assert!(parse_size("1.5M").is_err());
        // 2^54 gibibytes overflows u64: an error, not a wraparound.
        assert!(parse_size("18014398509481984G").is_err());
    }

    #[test]
    fn parse_split_and_shell_flags() {
        let opts = parse(&[
            "--split",
            "big.bin",
            "--part-size",
            "500M",
            "--out-dir",
            "parts",
        ])
        .unwrap();
        assert_eq!(opts.split, Some(PathBuf::from("big.bin")));
        assert_eq!(opts.part_size, Some(500 * 1024 * 1024));
        assert_eq!(opts.split_out_dir, Some(PathBuf::from("parts")));
        let opts = parse(&["--part-size=4G", "--split=big.bin"]).unwrap();
        assert_eq!(opts.part_size, Some(4 * 1024 * 1024 * 1024));
        assert!(parse(&["--part-size", "lots"]).is_err());
        assert!(parse(&["--split"]).is_err());
        let opts = parse(&["--install-shell"]).unwrap();
        assert!(opts.install_shell && !opts.uninstall_shell);
        let opts = parse(&["--uninstall-shell"]).unwrap();
        assert!(opts.uninstall_shell && !opts.install_shell);
    }

    #[test]
    fn split_run_writes_parts_and_reports_errors() {
        let dir = std::env::temp_dir().join(format!("veloedit-split-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.bin");
        std::fs::write(&src, vec![3u8; 250]).unwrap();
        let out = dir.join("parts");
        let args: Vec<String> = vec![
            "--split".into(),
            src.to_string_lossy().into_owned(),
            "--part-size".into(),
            "100".into(),
            "--out-dir".into(),
            out.to_string_lossy().into_owned(),
        ];
        assert_eq!(run(&args), 0);
        let mut names: Vec<String> = std::fs::read_dir(&out)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["src.bin.part001", "src.bin.part002", "src.bin.part003"]
        );
        // A missing --part-size, a zero size and a missing file are
        // all exit 2.
        let args: Vec<String> = vec!["--split".into(), src.to_string_lossy().into_owned()];
        assert_eq!(run(&args), 2);
        let args: Vec<String> = vec![
            "--split".into(),
            src.to_string_lossy().into_owned(),
            "--part-size".into(),
            "0".into(),
        ];
        assert_eq!(run(&args), 2);
        let args: Vec<String> = vec![
            "--split".into(),
            dir.join("nope.bin").to_string_lossy().into_owned(),
            "--part-size".into(),
            "100".into(),
        ];
        assert_eq!(run(&args), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parse_save_copy() {
        let opts = parse(&["in.bin", "--save-copy", "out.bin", "--headless"]).unwrap();
        assert_eq!(opts.save_copy, Some(PathBuf::from("out.bin")));
        let opts = parse(&["--save-copy=out.bin", "in.bin"]).unwrap();
        assert_eq!(opts.save_copy, Some(PathBuf::from("out.bin")));
        assert!(parse(&["--save-copy"]).is_err());
    }

    #[test]
    fn save_copy_run_writes_copy_and_report() {
        let dir = std::env::temp_dir().join(format!("veloedit-copy-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.bin");
        let dest = dir.join("dest.bin");
        let out = dir.join("report.json");
        std::fs::write(&src, b"copy me exactly").unwrap();
        let args: Vec<String> = vec![
            src.to_string_lossy().into_owned(),
            "--save-copy".into(),
            dest.to_string_lossy().into_owned(),
            "--json".into(),
            "--output".into(),
            out.to_string_lossy().into_owned(),
        ];
        assert_eq!(run(&args), 0);
        // The copy holds the bytes; the source is untouched.
        assert_eq!(std::fs::read(&dest).unwrap(), b"copy me exactly");
        assert_eq!(std::fs::read(&src).unwrap(), b"copy me exactly");
        let written = std::fs::read_to_string(&out).unwrap();
        assert!(written.contains("\"bytes_written\":15"));
        assert!(written.contains("\"staging_removed\":true"));
        // No staging file is left behind.
        assert!(!crate::save::staging_path_for(&dest).exists());

        // Destination equal to the source is refused, exit 2.
        let args: Vec<String> = vec![
            src.to_string_lossy().into_owned(),
            "--save-copy".into(),
            src.to_string_lossy().into_owned(),
        ];
        assert_eq!(run(&args), 2);
        // --save-copy without a file is an error, exit 2.
        let args: Vec<String> = vec!["--save-copy".into(), dest.to_string_lossy().into_owned()];
        assert_eq!(run(&args), 2);
        // A missing source file is an error, exit 2.
        let args: Vec<String> = vec![
            dir.join("nope.bin").to_string_lossy().into_owned(),
            "--save-copy".into(),
            dest.to_string_lossy().into_owned(),
        ];
        assert_eq!(run(&args), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parse_r4c_operations() {
        let opts = parse(&["f.bin", "--slice", "0-99;L1-L2", "--out", "part.bin"]).unwrap();
        assert_eq!(opts.slice_spec, Some("0-99;L1-L2".to_string()));
        assert_eq!(opts.out, Some(PathBuf::from("part.bin")));
        let opts = parse(&["f.bin", "--export-patch", "p.velopat"]).unwrap();
        assert_eq!(opts.export_patch, Some(PathBuf::from("p.velopat")));
        let opts = parse(&["f.bin", "--apply-patch=p.velopat", "--out=o.bin"]).unwrap();
        assert_eq!(opts.apply_patch, Some(PathBuf::from("p.velopat")));
        assert_eq!(opts.out, Some(PathBuf::from("o.bin")));
        let opts = parse(&["f.bin", "--insert-zeroes", "4K", "--at", "0x10"]).unwrap();
        assert_eq!(opts.insert_zeroes, Some(4096));
        assert_eq!(opts.insert_at, Some(16));
        let opts = parse(&["f.bin", "--compare", "g.bin"]).unwrap();
        assert_eq!(opts.compare, Some(PathBuf::from("g.bin")));
        assert!(parse(&["f.bin", "--insert-zeroes", "lots"]).is_err());
        assert!(parse(&["f.bin", "--at", "-1"]).is_err());
        assert!(parse(&["f.bin", "--slice"]).is_err());
    }

    #[test]
    fn slice_run_extracts_byte_and_line_ranges() {
        let dir = std::env::temp_dir().join(format!("veloedit-slice-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.txt");
        std::fs::write(&src, b"alpha\nbeta\ngamma\n").unwrap();
        let out = dir.join("part.bin");
        let src_s = src.to_string_lossy().into_owned();
        let out_s = out.to_string_lossy().into_owned();

        // Byte range, both ends inclusive.
        let args: Vec<String> = vec![
            src_s.clone(),
            "--slice".into(),
            "0-4".into(),
            "--out".into(),
            out_s.clone(),
        ];
        assert_eq!(run(&args), 0);
        assert_eq!(std::fs::read(&out).unwrap(), b"alpha");

        // Line range (needs the index retry path).
        let args: Vec<String> = vec![
            src_s.clone(),
            "--slice".into(),
            "L2-L3".into(),
            "--out".into(),
            out_s.clone(),
        ];
        assert_eq!(run(&args), 0);
        assert_eq!(std::fs::read(&out).unwrap(), b"beta\ngamma\n");

        // A bad spec is an error (exit 2), reported word for word.
        let args: Vec<String> = vec![
            src_s.clone(),
            "--slice".into(),
            "banana".into(),
            "--out".into(),
            out_s.clone(),
        ];
        assert_eq!(run(&args), 2);
        // Missing --out / missing file / out == input are exit 2.
        let args: Vec<String> = vec![src_s.clone(), "--slice".into(), "0-1".into()];
        assert_eq!(run(&args), 2);
        let args: Vec<String> = vec![
            src_s.clone(),
            "--slice".into(),
            "0-1".into(),
            "--out".into(),
            src_s.clone(),
        ];
        assert_eq!(run(&args), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn patch_round_trip_through_the_cli() {
        let dir = std::env::temp_dir().join(format!("veloedit-patch-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.bin");
        std::fs::write(&src, b"patch me gently").unwrap();
        let patch = dir.join("delta.velopat");
        let out = dir.join("applied.bin");
        let src_s = src.to_string_lossy().into_owned();

        // Export from an unedited file: a valid empty patch.
        let args: Vec<String> = vec![
            src_s.clone(),
            "--export-patch".into(),
            patch.to_string_lossy().into_owned(),
        ];
        assert_eq!(run(&args), 0);
        let patch_bytes = std::fs::read(&patch).unwrap();
        assert!(patch_bytes.starts_with(b"VELOPAT1"));

        // Applying it reproduces the base byte for byte.
        let args: Vec<String> = vec![
            src_s.clone(),
            "--apply-patch".into(),
            patch.to_string_lossy().into_owned(),
            "--out".into(),
            out.to_string_lossy().into_owned(),
        ];
        assert_eq!(run(&args), 0);
        assert_eq!(std::fs::read(&out).unwrap(), b"patch me gently");
        // The base was never modified, and applying over the base
        // is refused.
        assert_eq!(std::fs::read(&src).unwrap(), b"patch me gently");
        let args: Vec<String> = vec![
            src_s.clone(),
            "--apply-patch".into(),
            patch.to_string_lossy().into_owned(),
            "--out".into(),
            src_s.clone(),
        ];
        assert_eq!(run(&args), 2);
        // A non-patch file is refused, exit 2.
        let junk = dir.join("junk.velopat");
        std::fs::write(&junk, b"not a patch at all").unwrap();
        let args: Vec<String> = vec![
            src_s.clone(),
            "--apply-patch".into(),
            junk.to_string_lossy().into_owned(),
            "--out".into(),
            out.to_string_lossy().into_owned(),
        ];
        assert_eq!(run(&args), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn insert_zeroes_run_edits_in_place_atomically() {
        let dir = std::env::temp_dir().join(format!("veloedit-zeroes-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.bin");
        std::fs::write(&src, b"ABCD").unwrap();
        let src_s = src.to_string_lossy().into_owned();

        // Default position: the end of the file.
        let args: Vec<String> = vec![src_s.clone(), "--insert-zeroes".into(), "3".into()];
        assert_eq!(run(&args), 0);
        assert_eq!(std::fs::read(&src).unwrap(), b"ABCD\0\0\0");

        // At an offset, with a size suffix.
        let args: Vec<String> = vec![
            src_s.clone(),
            "--insert-zeroes".into(),
            "2".into(),
            "--at".into(),
            "2".into(),
        ];
        assert_eq!(run(&args), 0);
        assert_eq!(std::fs::read(&src).unwrap(), b"AB\0\0CD\0\0\0");

        // Past-the-end --at and a zero count are errors, exit 2.
        let args: Vec<String> = vec![
            src_s.clone(),
            "--insert-zeroes".into(),
            "1".into(),
            "--at".into(),
            "99".into(),
        ];
        assert_eq!(run(&args), 2);
        let args: Vec<String> = vec![src_s.clone(), "--insert-zeroes".into(), "0".into()];
        assert_eq!(run(&args), 2);
        // No staging file is left behind by the atomic save.
        assert!(!crate::save::staging_path_for(&src).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn compare_run_reports_block_regions() {
        let dir =
            std::env::temp_dir().join(format!("veloedit-compare-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.bin");
        let b = dir.join("b.bin");
        std::fs::write(&a, vec![7u8; 5000]).unwrap();
        std::fs::write(&b, vec![7u8; 5000]).unwrap();
        let a_s = a.to_string_lossy().into_owned();
        let b_s = b.to_string_lossy().into_owned();
        let args: Vec<String> = vec![a_s.clone(), "--compare".into(), b_s.clone()];
        assert_eq!(run(&args), 0);
        // One byte different: exit 1.
        let mut changed = vec![7u8; 5000];
        changed[4999] = 8;
        std::fs::write(&b, changed).unwrap();
        assert_eq!(run(&args), 1);
        // A missing other file is an error, exit 2.
        let args: Vec<String> = vec![
            a_s.clone(),
            "--compare".into(),
            dir.join("nope.bin").to_string_lossy().into_owned(),
        ];
        assert_eq!(run(&args), 2);
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
