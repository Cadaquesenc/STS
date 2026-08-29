// Windows opens a console window behind any GUI binary unless it is told the
// subsystem is a windowed one. Only in release: a debug build wants the console,
// because that is where panics and `eprintln!` go.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::Write;

/// The signature every subcommand shares: the arguments, somewhere to print the
/// answer, somewhere to print the complaint, and the process's exit code.
type Subcommand = fn(&[String], &mut dyn Write, &mut dyn Write) -> i32;

/// The window, unless the first argument names a subcommand.
///
/// The check is against a known list rather than "are there any arguments",
/// because a launch from Finder passes its own — macOS hands a bundled binary a
/// `-psn_...` process serial number — and a desktop app that refused to start
/// when double-clicked would be an odd way to ship a window.
///
/// Every subcommand has the same signature, so the list is a lookup rather than
/// a chain of branches that each remember to flush.
///
/// **The handles are the unlocked ones, and that is the whole point.** Taking
/// `stdout().lock()` and `stderr().lock()` here would hold both process-wide
/// locks for as long as the subcommand runs, and `sts daemon run` runs until
/// somebody signals it. The lock is per process, not per handle: the telemetry
/// pump is a second thread, and `--telemetry -` points its sink at the same
/// stderr this thread would be sitting on. It blocks on its first line, the
/// event queue behind it fills and starts dropping, and the shutdown that joins
/// that thread waits for a thread that is waiting for this one. Nothing is
/// written and nothing exits.
///
/// Unlocked, every `write` takes the lock and gives it straight back. A whole
/// `write_all` or `writeln!` still goes out under one acquisition, so a report
/// and a telemetry line cannot land shredded into each other — the guarantee
/// that was actually wanted here, and the only one that was.
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(first) = args.first() {
        let subcommand: Option<Subcommand> = if sts_lib::backtest::cli::is_subcommand(first) {
            Some(sts_lib::backtest::cli::run)
        } else if sts_lib::daemon::cli::is_subcommand(first) {
            Some(sts_lib::daemon::cli::run)
        } else {
            None
        };
        if let Some(run) = subcommand {
            let mut out = std::io::stdout();
            let mut err = std::io::stderr();
            let code = run(&args, &mut out, &mut err);
            // Flushed before exiting: `process::exit` runs no destructors, so an
            // unflushed report would be a report that was written and never
            // arrived.
            let _ = out.flush();
            let _ = err.flush();
            std::process::exit(code);
        }
    }

    sts_lib::run();
}
