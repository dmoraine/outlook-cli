use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::{Value as Json, json};

use outlook_cli::error::{Error, Result};
use outlook_cli::index::doctor::{self, StoreSource};
use outlook_cli::index::embed::{self, Embedder};
use outlook_cli::index::{self, model, search, sync, vectors};
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

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Priority {
    /// Below normal (nice 10 on Linux), so the machine stays usable.
    Low,
    Normal,
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
    /// deleted, moved and read/unread changes, then embed pending chunks of
    /// the embedding scope when the model files are present. Prints counts
    /// only. Resumable: an interrupted sync continues where it stopped.
    Sync {
        /// Stop after this long (90s, 30m, 2h), embedding included; the next
        /// sync resumes.
        #[arg(long)]
        max_time: Option<String>,
        /// Allow deleting more than max(100, 20%) of the indexed messages in
        /// one sync (default: abort with no change).
        #[arg(long)]
        allow_mass_delete: bool,
        /// Index only; skip the embedding step.
        #[arg(long)]
        no_embed: bool,
        #[command(flatten)]
        embed: EmbedArgs,
    },
    /// Embed pending chunks of the embedding scope, newest message first.
    /// Commits every batch: Ctrl-C or --max-time lose at most one batch, the
    /// next run resumes. Prints counts and rates only.
    Embed {
        /// Stop after the batch that ends past this long (90s, 30m, 2h).
        #[arg(long)]
        max_time: Option<String>,
        #[command(flatten)]
        embed: EmbedArgs,
    },
    /// Show the persisted embedding scope with counts, or change it. Search
    /// covers every folder; only embeddings are limited. Vectors that fall
    /// out of scope are kept. Folder specs are counted, not printed.
    Scope {
        /// Embed messages received at or after (2026-01-01, 90d, ...),
        /// stored as an absolute time.
        #[arg(long)]
        embed_since: Option<String>,
        /// Embed messages received strictly before, stored as an absolute time.
        #[arg(long)]
        embed_until: Option<String>,
        /// Embed only this folder (name, or path with its subfolders, as
        /// `search -F`); repeatable = any. Replaces the stored folders.
        #[arg(short = 'F', long = "folder")]
        folders: Vec<String>,
        /// Reset the scope to everything.
        #[arg(long, conflicts_with_all = ["embed_since", "embed_until", "folders"])]
        clear: bool,
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
        /// Folder name, or path with its subfolders; repeatable = any.
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
    /// Embedding model files: list, or download (opt-in network access).
    Model {
        #[command(subcommand)]
        cmd: ModelCmd,
    },
    /// Embedding speed on this CPU, on synthetic text (no mail data).
    BenchEmbed {
        #[command(flatten)]
        model: ModelArgs,
        /// Approximate duration in seconds.
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
        seconds: u64,
        /// Compute threads. Default: logical CPUs - 1 (at least 1).
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=1024))]
        threads: Option<u64>,
        /// Passages per batch.
        #[arg(long, default_value_t = embed::DEFAULT_BATCH as u64, value_parser = clap::value_parser!(u64).range(1..=256))]
        batch: u64,
    },
}

#[derive(Args)]
struct ModelArgs {
    /// Embedding model.
    #[arg(long, default_value = "e5-small", value_parser = model::NAMES)]
    model: String,
    /// Directory with the model's config.json, tokenizer.json and
    /// model.safetensors. Default: <data dir>/models/multilingual-e5-small
    /// (or -base), next to the default index.
    #[arg(long, env = "OUTLOOK_INDEX_MODEL_DIR")]
    model_dir: Option<String>,
}

impl ModelArgs {
    fn resolve(&self) -> Result<(&'static model::Model, PathBuf)> {
        let m = model::get(&self.model)?;
        Ok((m, model_dir(m, &self.model_dir)?))
    }
}

fn model_dir(m: &model::Model, dir: &Option<String>) -> Result<PathBuf> {
    match dir {
        Some(d) => Ok(native_path(d)),
        None => model::default_dir(m),
    }
}

#[derive(Args)]
struct EmbedArgs {
    /// Embedding model. Default: the model of the stored vectors, else e5-small.
    #[arg(long, value_parser = model::NAMES)]
    model: Option<String>,
    /// Directory with the model files. Default: <data dir>/models/<repository name>.
    #[arg(long, env = "OUTLOOK_INDEX_MODEL_DIR")]
    model_dir: Option<String>,
    /// Compute threads. Default: logical CPUs - 1 (at least 1).
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=1024))]
    threads: Option<u64>,
    /// Passages per batch (one model call, one commit).
    #[arg(long, default_value_t = embed::DEFAULT_BATCH as u64, value_parser = clap::value_parser!(u64).range(1..=256))]
    batch: u64,
    /// Drop the vectors of another model and embed again (needed to change model).
    #[arg(long)]
    reembed: bool,
    /// CPU priority while embedding.
    #[arg(long, value_enum, default_value_t = Priority::Low)]
    priority: Priority,
}

impl EmbedArgs {
    /// The requested model, else the stored one, else e5-small; refused if
    /// vectors of another model exist and --reembed is not given.
    fn model(&self, c: &rusqlite::Connection) -> Result<&'static model::Model> {
        let name = match &self.model {
            Some(n) => n.clone(),
            None => vectors::current_model(c)?.unwrap_or_else(|| model::NAMES[0].into()),
        };
        let m = model::get(&name)?;
        vectors::check_model(c, m, self.reembed)?;
        Ok(m)
    }
}

/// Compute threads for candle (rayon reads RAYON_NUM_THREADS on first use).
fn set_threads(threads: Option<u64>) -> usize {
    let threads = threads.map_or_else(embed::default_threads, |n| n as usize);
    // SAFETY: no other thread exists yet; candle and rayon read this
    // variable on first use.
    unsafe { std::env::set_var("RAYON_NUM_THREADS", threads.to_string()) };
    threads
}

const MODEL_MISSING: &str = "model files missing; run outlook-index model fetch";

/// Load the model and embed within `max_time` from `start`. Missing model
/// files: Ok(skipped) when `missing_ok`, else the not_found error.
fn embed_step(
    c: &mut rusqlite::Connection,
    a: &EmbedArgs,
    m: &'static model::Model,
    start: Instant,
    max_time: Option<Duration>,
    missing_ok: bool,
) -> Result<Json> {
    let threads = set_threads(a.threads);
    let low = a.priority == Priority::Low && vectors::lower_priority();
    let t = Instant::now();
    let mut e = match Embedder::load(m, &model_dir(m, &a.model_dir)?) {
        Err(Error::NotFound(_)) if missing_ok => return Ok(json!({ "skipped": MODEL_MISSING })),
        r => r?,
    };
    e.batch = a.batch as usize;
    let load_ms = t.elapsed().as_millis() as u64;
    let o = vectors::Options {
        batch: a.batch as usize,
        max_time: max_time.map(|d| d.saturating_sub(start.elapsed())),
        reembed: a.reembed,
    };
    let mut v = vectors::run(c, &mut e, &o)?;
    let obj = v.as_object_mut().unwrap();
    obj.insert("threads".into(), json!(threads));
    obj.insert("priority".into(), json!(if low { "low" } else { "normal" }));
    obj.insert("load_ms".into(), json!(load_ms));
    Ok(v)
}

#[derive(Subcommand)]
enum ModelCmd {
    /// Known models, pinned revisions, and whether their files are in the
    /// default directories (sizes only; `fetch` checks the hashes).
    List,
    /// Download the model files over HTTPS from huggingface.co at the pinned
    /// revision (needs curl) and check their SHA-256. Resumes an interrupted
    /// download; files already correct are kept. The only network access.
    Fetch {
        #[command(flatten)]
        model: ModelArgs,
    },
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
        Cmd::Sync { ref max_time, allow_mass_delete, no_embed, ref embed } => {
            let start = Instant::now();
            let max_time = max_time.as_deref().map(parse_duration).transpose()?;
            let opts = sync::Options { max_time, allow_mass_delete, ..Default::default() };
            let f = file()?;
            let mut c = index::open(&index_path)?;
            let _lock = sync::lock(&index_path)?;
            // Model choice is checked before indexing.
            let m = if no_embed { None } else { Some(embed.model(&c)?) };
            let mut v = sync::sync(&mut c, &mut StoreSource::open(&f)?, &opts)?;
            let e = match m {
                None => json!({ "skipped": "--no-embed" }),
                Some(_) if max_time.is_some_and(|t| start.elapsed() >= t) => {
                    json!({ "skipped": "max-time reached; the next sync or embed resumes" })
                }
                Some(m) => embed_step(&mut c, embed, m, start, max_time, true)?,
            };
            v.as_object_mut().unwrap().insert("embedding".into(), e);
            v
        }
        Cmd::Embed { ref max_time, ref embed } => {
            let start = Instant::now();
            let max_time = max_time.as_deref().map(parse_duration).transpose()?;
            let mut c = index::open(&existing(index_path.clone())?)?;
            let _lock = sync::lock(&index_path)?;
            let m = embed.model(&c)?;
            embed_step(&mut c, embed, m, start, max_time, false)?
        }
        Cmd::Scope { embed_since, embed_until, folders, clear } => {
            let c = if clear || embed_since.is_some() || embed_until.is_some() || !folders.is_empty() {
                let c = index::open(&index_path)?;
                let _lock = sync::lock(&index_path)?;
                let mut s = if clear { vectors::Scope::default() } else { vectors::scope(&c)? };
                if let Some(t) = embed_since {
                    s.since = Some(parse_when(&t)?);
                    s.since_input = Some(t);
                }
                if let Some(t) = embed_until {
                    s.until = Some(parse_when(&t)?);
                    s.until_input = Some(t);
                }
                if !folders.is_empty() {
                    s.folders = folders;
                }
                vectors::set_scope(&c, &s)?;
                c
            } else {
                index::open(&existing(index_path)?)?
            };
            vectors::counts(&c)?
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
        Cmd::Model { cmd: ModelCmd::List } => model::list(model::default_dir)?,
        Cmd::Model { cmd: ModelCmd::Fetch { ref model } } => {
            let (m, dir) = model.resolve()?;
            model::fetch(m, &dir, |f| {
                eprintln!("{}", json!({ "progress": { "kind": "download", "file": f.name, "bytes": f.size } }))
            })?
        }
        Cmd::BenchEmbed { ref model, seconds, threads, batch } => {
            let (m, dir) = model.resolve()?;
            let threads = set_threads(threads);
            let t = std::time::Instant::now();
            let mut e = Embedder::load(m, &dir)?;
            e.batch = batch as usize;
            embed::bench(&e, t.elapsed(), Duration::from_secs(seconds), threads)?
        }
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
