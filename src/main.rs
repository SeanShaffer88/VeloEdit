//! VeloEdit program entry point.
//!
//! All behaviour lives in the library (`veloedit::cli::run`) so the GUI
//! shell added in a later slice and this headless entry point share one
//! operation surface.

// The Windows (GUI) subsystem: launching the editor must not open a
// console window next to it. The headless tools are unaffected —
// `cli::run` attaches to the parent terminal's console before it
// prints (AttachConsole), so `VeloEdit --help` in a terminal behaves
// exactly like a console program there.
#![windows_subsystem = "windows"]

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(veloedit::cli::run(&args));
}
