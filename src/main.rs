//! VeloEdit program entry point.
//!
//! All behaviour lives in the library (`veloedit::cli::run`) so the GUI
//! shell added in a later slice and this headless entry point share one
//! operation surface.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(veloedit::cli::run(&args));
}
