use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value as Json, json};

use outlook_cli::error::{Error, Result};
use outlook_cli::index::doctor::{self, StoreSource};
use outlook_cli::index::{self, search, sync};
use outlook_cli::paths::{default_file, native_path};
use outlook_cli::timeutil::parse_when;

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

    /// Print times in the local time zone instead of UTC.
    #[arg(long, global = true)]
    local_time: bool,

    /// Escape non-ASCII characters in JSON as \uXXXX.
    #[arg(long, global = true, env = "OUTLOOK_CLI_ASCII", value_parser = clap::builder::FalseyValueParser::new())]
    ascii: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    /// One JSON document {count, hits}.
    Json,
    /// One JSON object per line (one per hit).
    Jsonl,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Mode {
    Keyword,
    Semantic,
    Hybrid,
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
    /// Bring the index up to date with the file (every mail folder): new,
    /// deleted, moved and read/unread changes. Prints counts only.
    /// Resumable: an interrupted sync continues where it stopped.
    Sync {
        /// Stop after this long (90s, 30m, 2h); the next sync resumes.
        #[arg(long)]
        max_time: Option<String>,
    },
    /// Keyword search (FTS5, accent-insensitive). Best chunk per message.
    Search {
        /// Words to find (all required). Operators are treated as words.
        query: String,
        /// semantic and hybrid need embeddings (not available yet).
        #[arg(long, value_enum, default_value_t = Mode::Keyword)]
        mode: Mode,
        /// Maximum number of hits.
        #[arg(short = 'n', long, default_value_t = 10)]
        limit: usize,
        /// Folder path or name; repeatable = any.
        #[arg(short = 'F', long = "folder")]
        folders: Vec<String>,
        /// Received at or after (2026-10-15, '2026-10-15 08:00', 3d, today...).
        #[arg(short, long)]
        since: Option<String>,
        /// Received strictly before.
        #[arg(short, long)]
        until: Option<String>,
        /// Sender name or address contains (not a date); repeatable = any.
        #[arg(long)]
        from: Vec<String>,
        /// A To/Cc recipient contains (not a date; see --until); repeatable = any.
        #[arg(long)]
        to: Vec<String>,
        /// Only messages with attachments.
        #[arg(long)]
        has_attachments: bool,
        /// Excerpt length in characters (max 2000).
        #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..=2000))]
        excerpt_max: u64,
        /// Output format.
        #[arg(long, value_enum, default_value_t = Format::Json)]
        format: Format,
    },
    /// Index counts and size. No content, no names.
    Status,
}

/// "90s", "30m", "2h".
fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let mult = match unit.trim() {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => 0,
    };
    match n.parse::<u64>() {
        Ok(n) if mult > 0 => Ok(Duration::from_secs(n * mult)),
        _ => Err(Error::Usage(format!("invalid duration '{s}' (use e.g. 90s, 30m, 2h)"))),
    }
}

fn existing(p: PathBuf) -> Result<PathBuf> {
    if p.exists() { Ok(p) } else { Err(Error::NotFound("no index yet; run `outlook-index sync`".into())) }
}

fn run(cli: Cli, out: &mut dyn Write) -> Result<()> {
    let file = || match &cli.file {
        Some(f) => Ok(native_path(f)),
        None => default_file(),
    };
    let index_path = match &cli.index {
        Some(p) => native_path(p),
        None => index::default_path()?,
    };
    if index::under_onedrive(&index_path) {
        let msg = "the index is under a OneDrive folder; move it with --index or OUTLOOK_INDEX_DB";
        eprintln!("{}", json!({ "warning": { "kind": "onedrive", "message": msg } }));
    }
    let mut lines = None;
    let v = match cli.cmd {
        // doctor output gets pasted back: its errors carry no path or content.
        Cmd::Doctor { sample } => {
            let f = file().map_err(doctor::sanitize)?;
            doctor::run(&f, &index_path, sample).map_err(doctor::sanitize)?
        }
        Cmd::Sync { ref max_time } => {
            let opts =
                sync::Options { max_time: max_time.as_deref().map(parse_duration).transpose()?, ..Default::default() };
            let f = file()?;
            let mut c = index::open(&index_path)?;
            let _lock = sync::lock(&index_path)?;
            sync::sync(&mut c, &mut StoreSource::open(&f)?, &opts)?
        }
        Cmd::Search { query, mode, limit, folders, since, until, from, to, has_attachments, excerpt_max, format } => {
            if mode != Mode::Keyword {
                return Err(Error::Usage(
                    "semantic and hybrid search need embeddings (not available yet); use --mode keyword".into(),
                ));
            }
            let t = |o: Option<String>| o.as_deref().map(parse_when).transpose();
            let q = search::Query {
                text: query,
                limit,
                folders,
                since: t(since)?,
                until: t(until)?,
                from,
                to,
                has_attachments,
                excerpt_max: excerpt_max as usize,
                local_time: cli.local_time,
            };
            let hits = search::search(&index::open(&existing(index_path)?)?, &q)?;
            if format == Format::Jsonl {
                lines = Some(hits);
                Json::Null
            } else {
                json!({ "count": hits.len(), "hits": hits })
            }
        }
        Cmd::Status => search::status(&index::open(&existing(index_path)?)?)?,
    };
    let print = |v: &Json, pretty: bool| {
        let s = if pretty { serde_json::to_string_pretty(v) } else { serde_json::to_string(v) };
        let s = s.map_err(|e| Error::Io(e.to_string()))?;
        let s = if cli.ascii { ascii(&s) } else { s };
        Ok::<_, Error>(s)
    };
    let io = |e: std::io::Error| Error::Io(e.to_string());
    match lines {
        Some(hits) => {
            for h in &hits {
                writeln!(out, "{}", print(h, false)?).map_err(io)?;
            }
            Ok(())
        }
        None => writeln!(out, "{}", print(&v, cli.pretty)?).map_err(io),
    }
}

/// Non-ASCII as JSON \uXXXX escapes. JSON has non-ASCII only inside
/// strings, so the result stays valid JSON.
fn ascii(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii() {
            o.push(c);
        } else {
            for u in c.encode_utf16(&mut [0; 2]) {
                o.push_str(&format!("\\u{u:04x}"));
            }
        }
    }
    o
}

fn main() {
    // Panics are reported as JSON errors (see `guarded`), not on stderr.
    let cli = Cli::parse();
    std::panic::set_hook(Box::new(|_| {}));
    let r = outlook_cli::error::guarded(|| run(cli, &mut std::io::stdout().lock()));
    if let Err(e) = r {
        let v = json!({ "error": { "kind": e.kind(), "message": e.to_string() } });
        eprintln!("{v}");
        std::process::exit(e.exit_code());
    }
}
