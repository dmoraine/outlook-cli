use std::io::Write;

use clap::{Parser, Subcommand};
use serde_json::json;

use outlook_cli::error::{Error, Result};
use outlook_cli::index::{self, doctor};
use outlook_cli::paths::{default_file, native_path};

/// Local index of an Outlook OST/PST file (preview). Read-only on the file.
///
/// Output is JSON. Exit codes: 0 ok, 2 usage, 3 not found, 4 I/O, 5 unsupported file, 6 inconsistent data.
#[derive(Parser)]
#[command(name = "outlook-index", version, max_term_width = 110)]
struct Cli {
    /// OST/PST file. Default: $OUTLOOK_CLI_FILE, else the most recently
    /// modified .ost in %LOCALAPPDATA%\Microsoft\Outlook.
    #[arg(short, long, global = true, env = "OUTLOOK_CLI_FILE")]
    file: Option<String>,

    /// Index database. Default: %LOCALAPPDATA%\outlook-cli\index.sqlite
    /// (Windows), $XDG_DATA_HOME/outlook-cli/index.sqlite (Linux).
    #[arg(long, global = true, env = "OUTLOOK_INDEX_DB")]
    index: Option<String>,

    /// Pretty-print JSON.
    #[arg(long, global = true)]
    pretty: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Aggregate-only diagnostics of the mailbox and this machine. Prints
    /// counts, sizes and timings only: no mail content, no names.
    Doctor {
        /// Messages to open for body text and attachment statistics.
        #[arg(long, default_value_t = 200)]
        sample: usize,
    },
}

fn run(cli: Cli) -> Result<()> {
    let file = match &cli.file {
        Some(f) => native_path(f),
        None => default_file()?,
    };
    let index_path = match &cli.index {
        Some(p) => native_path(p),
        None => index::default_path()?,
    };
    if index::under_onedrive(&index_path) {
        let msg = "the index is under a OneDrive folder; move it with --index or OUTLOOK_INDEX_DB";
        eprintln!("{}", json!({ "warning": { "kind": "onedrive", "message": msg } }));
    }
    let v = match cli.cmd {
        Cmd::Doctor { sample } => doctor::run(&file, &index_path, sample)?,
    };
    let s = if cli.pretty { serde_json::to_string_pretty(&v) } else { serde_json::to_string(&v) };
    let mut out = std::io::stdout().lock();
    writeln!(out, "{}", s.map_err(|e| Error::Io(e.to_string()))?).map_err(|e| Error::Io(e.to_string()))
}

fn main() {
    // Panics are reported as JSON errors (see `guarded`), not on stderr.
    let cli = Cli::parse();
    std::panic::set_hook(Box::new(|_| {}));
    // doctor output gets pasted back: the error message never carries a path or content.
    if let Err(e) = run(cli).map_err(doctor::sanitize) {
        let v = json!({ "error": { "kind": e.kind(), "message": e.to_string() } });
        eprintln!("{v}");
        std::process::exit(e.exit_code());
    }
}
