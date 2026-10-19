//! The capture engine as a program of its own, for testing before the app exists:
//! `hdrcast-capture list-outputs --json`, `hdrcast-capture capture --selftest`, ...

use hdrcast_capture::exit_codes;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = hdrcast_capture::cli_main(&args).unwrap_or_else(|| {
        // Not an engine mode: where the app would start its window, this program only explains itself.
        let asked_for_help = matches!(
            args.first().map(String::as_str),
            Some("--help" | "-h" | "/?" | "help")
        );
        let usage = hdrcast_capture::cli::usage();
        if asked_for_help {
            print!("{usage}");
            return exit_codes::OK;
        }
        if let Some(verb) = args.first() {
            eprintln!("unknown command: {verb}");
        }
        eprint!("{usage}");
        exit_codes::BAD_ARGUMENTS
    });
    std::process::exit(code);
}
