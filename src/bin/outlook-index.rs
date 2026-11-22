use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::{Value as Json, json};

use outlook_cli::error::{Error, Result, guarded};
use outlook_cli::index::doctor::{self, StoreSource};
use outlook_cli::index::embed::{self, Embedder};
use outlook_cli::index::progress::{self, Progress};
use outlook_cli::index::{self, attach, extract, model, search, sync, vectors, worker};
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
        /// Messages to open for body text and attachment statistics (default
        /// 200); with --attachments, attachments to extract (default 300).
        #[arg(long)]
        sample: Option<usize>,
        /// Attachment text extraction instead: whole random messages with
        /// attachments, each attachment extracted in a child process (30 s
        /// timeout). Counts per type and skip reason, sizes, timings,
        /// estimated chunks and the SHA-256 dedupe ratio.
        #[arg(long)]
        attachments: bool,
    },
    /// Bring the index up to date with the file (every mail folder): new,
    /// deleted, moved and read/unread changes, then attachment text (see
    /// `scope`), then embed pending chunks of the embedding scope when the
    /// model files are present. Prints counts only. Resumable: an
    /// interrupted sync continues where it stopped.
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
        /// Skip the attachment step (no attachment is listed or read); a
        /// later sync without it catches up.
        #[arg(long)]
        no_attachments: bool,
        /// Also index these well-known folders with their subfolders
        /// (deleted = Deleted Items, junk = Junk E-mail; none = neither, the
        /// default). Stored in the index: later syncs keep the choice.
        /// Messages that leave the index this way are reported as
        /// excluded_removed, outside the mass-deletion limit.
        #[arg(long, value_delimiter = ',', value_parser = ["deleted", "junk", "none"])]
        include_folders: Option<Vec<String>>,
        #[command(flatten)]
        embed: EmbedArgs,
        #[command(flatten)]
        progress: ProgressArgs,
    },
    /// Embed pending chunks of the embedding scope: body chunks newest
    /// message first, then attachment chunks (up to the scope's cap per
    /// content) newest referencing message first. Commits every batch: Ctrl-C loses at most the batches in flight (one
    /// per worker), the next run resumes. Prints counts and rates only.
    Embed {
        /// Start no new batch after this long (90s, 30m, 2h); batches in
        /// flight finish.
        #[arg(long)]
        max_time: Option<String>,
        #[command(flatten)]
        embed: EmbedArgs,
        #[command(flatten)]
        progress: ProgressArgs,
    },
    /// Show the persisted embedding and attachment scopes with counts, or
    /// change them. Search covers every folder; only embeddings are limited.
    /// Vectors and attachment texts that fall out of scope are kept. Folder
    /// specs are counted, not printed.
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
        /// Attachment chunks to embed per distinct content, from its start
        /// (default 30, ~50,000 chars; 0 = none). Keyword search covers the
        /// whole text. Raising it embeds the rest; lowering it keeps vectors.
        #[arg(long)]
        attachment_chunks: Option<u32>,
        /// Attachment types to index (comma-separated); replaces the stored list.
        #[arg(long, value_enum, value_delimiter = ',')]
        attachment_types: Option<Vec<extract::Kind>>,
        /// Largest attachment to read, in MB (default 20).
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=100))]
        attachment_max_mb: Option<u64>,
        /// Reset both scopes to their defaults (everything, 30 attachment chunks; every attachment type, 20 MB).
        #[arg(long, conflicts_with_all = ["embed_since", "embed_until", "folders", "attachment_chunks", "attachment_types", "attachment_max_mb"])]
        clear: bool,
    },
    /// Search the index. Best chunk per message. keyword: every word (FTS5,
    /// accent-insensitive); semantic: nearest by meaning (local model);
    /// hybrid: RRF of any-word keyword and semantic results.
    Search {
        /// Text to find. Operators are treated as words.
        query: String,
        /// Default: hybrid when the index has vectors of its model, else
        /// keyword. semantic/hybrid without vectors fall back to keyword with
        /// a warning.
        #[arg(long, value_enum)]
        mode: Option<search::Mode>,
        /// Model of the query vector. Default: the model of the stored vectors.
        #[arg(long, value_parser = model::NAMES)]
        model: Option<String>,
        /// Directory with the model files. Default: <data dir>/models/<repository name>.
        #[arg(long, env = "OUTLOOK_INDEX_MODEL_DIR")]
        model_dir: Option<String>,
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
    /// Index counts and size, and `running`: the live progress of a sync or
    /// embed (null when none; `stale` when its process is gone). No content,
    /// no names.
    Status {
        /// Print `{"running": ...}` every SECONDS (default 10), one line per
        /// tick, until the run ends (done, stale or none).
        #[arg(long, value_name = "SECONDS", num_args = 0..=1, default_missing_value = "10",
              value_parser = clap::value_parser!(u64).range(1..=3600))]
        watch: Option<u64>,
    },
    /// Consistent copy of the index into a new file (SQLite VACUUM INTO,
    /// safe during a sync), then checked read-only: integrity, schema
    /// version, counts. Restore with every outlook-index stopped: delete
    /// index.sqlite-wal and index.sqlite-shm, then copy the backup to
    /// index.sqlite.
    Backup {
        /// New file to write (an existing one is refused). Default:
        /// backups/index-YYYYMMDD-HHMMSS.sqlite next to the index.
        #[arg(long, value_name = "PATH")]
        to: Option<String>,
    },
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
        #[command(flatten)]
        par: ParArgs,
        /// Passages per batch.
        #[arg(long, default_value_t = embed::DEFAULT_BATCH as u64, value_parser = clap::value_parser!(u64).range(1..=256))]
        batch: u64,
    },
    /// Internal: attachment extraction worker (frames on stdin/stdout, see
    /// src/index/extract.rs).
    #[command(hide = true)]
    ExtractWorker,
    /// Internal: one embedding worker process of --workers (frames on
    /// stdin/stdout, see src/index/worker.rs).
    #[command(hide = true)]
    EmbedWorker {
        #[command(flatten)]
        model: ModelArgs,
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=1024))]
        threads: u64,
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=256))]
        batch: u64,
    },
}

#[derive(Args)]
struct ModelArgs {
    /// Embedding model. Default: the model of the index, else e5-base.
    #[arg(long, value_parser = model::NAMES)]
    model: Option<String>,
    /// Directory with the model's config.json, tokenizer.json and
    /// model.safetensors. Default: <data dir>/models/multilingual-e5-base
    /// (or -small), next to the default index.
    #[arg(long, env = "OUTLOOK_INDEX_MODEL_DIR")]
    model_dir: Option<String>,
}

impl ModelArgs {
    /// --model, else the model of `index` when it exists (not created), else e5-base.
    fn resolve(&self, index: Option<&std::path::Path>) -> Result<(&'static model::Model, PathBuf)> {
        let stored = match index.filter(|p| p.exists()) {
            Some(p) => vectors::current_model(&index::open_read(p)?)?,
            None => None,
        };
        let m = model::choose(self.model.as_deref(), stored.as_deref())?;
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
    /// Embedding model. Default: the model of the index, else e5-base. Another
    /// model than the one of the stored vectors needs --reembed.
    #[arg(long, value_parser = model::NAMES)]
    model: Option<String>,
    /// Directory with the model files. Default: <data dir>/models/<repository name>.
    #[arg(long, env = "OUTLOOK_INDEX_MODEL_DIR")]
    model_dir: Option<String>,
    #[command(flatten)]
    par: ParArgs,
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
    /// The requested model, else the stored one, else e5-base; refused if
    /// vectors of another model exist and --reembed is not given.
    fn model(&self, c: &rusqlite::Connection) -> Result<&'static model::Model> {
        let m = model::choose(self.model.as_deref(), vectors::current_model(c)?.as_deref())?;
        vectors::check_model(c, m, self.reembed)?;
        Ok(m)
    }
}

#[derive(Args)]
struct ParArgs {
    /// Batches embedded at once. More than 1: that many worker processes,
    /// each with its own copy of the model. Default: (logical CPUs - 2) / 3,
    /// at least 1.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=256))]
    workers: Option<u64>,
    /// Compute threads per worker.
    #[arg(long, default_value_t = embed::DEFAULT_THREADS as u64, value_parser = clap::value_parser!(u64).range(1..=1024))]
    threads: u64,
    /// With worker processes: the longest wait for a worker to load the
    /// model, then for each batch (e.g. 90s, 5m). A worker past it is
    /// killed and the run stops; run again to resume. Default: 180s to
    /// load, 120s per batch.
    #[arg(long, value_name = "DURATION", value_parser = worker_timeout)]
    worker_timeout: Option<Duration>,
}

fn worker_timeout(s: &str) -> std::result::Result<Duration, String> {
    duration(s).filter(|d| !d.is_zero()).ok_or_else(|| "use e.g. 90s, 5m".into())
}

impl ParArgs {
    fn workers(&self) -> usize {
        self.workers.map_or_else(|| embed::default_workers(embed::logical_cpus()), |n| n as usize)
    }

    /// Effective (workers, threads); sets this process's compute threads
    /// (worker processes get --threads).
    fn apply(&self) -> (usize, usize) {
        (self.workers(), set_threads(self.threads as usize))
    }
}

#[derive(Args)]
struct ProgressArgs {
    /// Progress on stderr (stdout keeps the summary): auto = bar on a
    /// terminal, else jsonl ({"progress":{...}} lines). Counts only. The
    /// live record is also kept in the index for `status`.
    #[arg(long, value_enum, default_value_t = progress::Mode::Auto)]
    progress: progress::Mode,
    /// Seconds between progress reports; phase changes are always reported.
    #[arg(long, value_name = "SECONDS", default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..=3600))]
    progress_interval: u64,
}

impl ProgressArgs {
    fn start(&self, workers: Option<usize>, model: Option<&'static model::Model>) -> Progress {
        let mode = self.progress.resolve(std::io::stderr().is_terminal());
        let every = Duration::from_secs(self.progress_interval);
        let mut p = Progress::new(mode, every, Box::new(std::io::stderr()), true);
        (p.workers, p.model) = (workers, model.map(|m| m.name));
        p
    }
}

/// Compute threads for candle: its matmul split
/// (`candle_core::utils::get_num_threads`) and rayon's global pool both
/// read RAYON_NUM_THREADS.
fn set_threads(threads: usize) -> usize {
    // SAFETY: no other thread exists yet; candle and rayon read this
    // variable on use.
    unsafe { std::env::set_var("RAYON_NUM_THREADS", threads.to_string()) };
    threads
}

const INCLUDE_NONE: &str = "--include-folders none cannot be combined with deleted or junk";

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
    p: &mut Progress,
) -> Result<Json> {
    let (workers, threads) = a.par.apply();
    // Before the workers start: they inherit it.
    let low = a.priority == Priority::Low && vectors::lower_priority();
    let t = Instant::now();
    let dir = model_dir(m, &a.model_dir)?;
    let mut es = match embedders(m, &dir, workers, threads, a.batch as usize, a.par.worker_timeout) {
        Err(Error::NotFound(_)) if missing_ok => return Ok(json!({ "skipped": MODEL_MISSING })),
        r => r?,
    };
    let load_ms = t.elapsed().as_millis() as u64;
    let o = vectors::Options {
        batch: a.batch as usize,
        max_time: max_time.map(|d| d.saturating_sub(start.elapsed())),
        reembed: a.reembed,
    };
    let mut v = vectors::run_with(c, &mut es.iter_mut().map(|e| e.as_mut()).collect::<Vec<_>>(), &o, p)?;
    let obj = v.as_object_mut().unwrap();
    obj.insert("threads".into(), json!(threads));
    obj.insert("priority".into(), json!(if low { "low" } else { "normal" }));
    obj.insert("load_ms".into(), json!(load_ms));
    Ok(v)
}

/// One worker: the model in this process. More: that many `embed-worker`
/// child processes (one process with more threads scales badly), each
/// loading its own copy of the model.
fn embedders(
    m: &'static model::Model,
    dir: &std::path::Path,
    workers: usize,
    threads: usize,
    batch: usize,
    timeout: Option<Duration>,
) -> Result<Vec<Box<dyn vectors::Embed>>> {
    if workers <= 1 {
        let mut e = Embedder::load(m, dir)?;
        e.batch = batch;
        return Ok(vec![Box::new(e)]);
    }
    let mut cmd = std::process::Command::new(std::env::current_exe().map_err(|e| Error::Io(e.to_string()))?);
    cmd.args(["embed-worker", "--model", m.name, "--threads", &threads.to_string(), "--batch", &batch.to_string()]);
    cmd.arg("--model-dir").arg(dir);
    Ok(worker::Child::spawn(cmd, workers, m, timeout)?
        .into_iter()
        .map(|c| Box::new(c) as Box<dyn vectors::Embed>)
        .collect())
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
    duration(s).ok_or_else(|| Error::Usage("invalid --max-time (use e.g. 90s, 30m, 2h)".into()))
}

fn duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let mult = match unit.trim() {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => 0,
    };
    n.parse::<u64>().ok().filter(|_| mult > 0).map(|n| Duration::from_secs(n * mult))
}

/// `parse_when`, with an error naming the flag, not the value.
fn when(flag: &str, s: &str) -> Result<i64> {
    parse_when(s).map_err(|_| {
        Error::Usage(format!(
            "cannot parse {flag} (use e.g. 2026-10-15T08:00:00Z, '2026-10-15 08:00', 2026-10-15, 2h, 3d, today)"
        ))
    })
}

/// Test hook of the error boundary: OUTLOOK_INDEX_TEST_PANIC="<point> <text>" panics at `point`.
fn test_panic(point: &str) {
    if let Some(v) = std::env::var("OUTLOOK_INDEX_TEST_PANIC").ok().filter(|v| v.split(' ').next() == Some(point)) {
        panic!("{v}");
    }
}

/// Test hook of the worker timeouts: OUTLOOK_INDEX_TEST_WORKER="<mode> <ok>"
/// serves a fake model (no files) that answers `ok` batches, then hangs
/// (`hang`), sends half a reply and hangs (`partial`) or exits (`exit`);
/// `never_ready` never reports ready.
fn test_worker(hook: &str, m: &'static model::Model, out: &mut dyn Write) -> Result<()> {
    struct Fake(&'static model::Model, String, usize);
    impl vectors::Embed for Fake {
        fn model(&self) -> &'static model::Model {
            self.0
        }
        fn passages(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            if self.2 == 0 {
                match self.1.as_str() {
                    "exit" => std::process::exit(1),
                    "partial" => {
                        let mut o = std::io::stdout().lock();
                        let _ = o.write_all(&[9, 0, 0, 0, 0]).and_then(|_| o.flush());
                    }
                    _ => {}
                }
                loop {
                    std::thread::sleep(Duration::from_secs(60));
                }
            }
            self.2 -= 1;
            Ok(texts.iter().map(|_| (0..self.0.dims).map(|i| f32::from(u8::from(i == 0))).collect()).collect())
        }
    }
    let mut w = hook.split(' ');
    let mode = w.next().unwrap_or_default().to_string();
    if mode == "never_ready" {
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }
    let ok = w.next().and_then(|n| n.parse().ok()).unwrap_or(0);
    worker::serve(Ok(Fake(m, mode, ok)), &mut std::io::stdin().lock(), &mut { out })
}

fn existing(p: PathBuf) -> Result<PathBuf> {
    if p.exists() { Ok(p) } else { Err(Error::NotFound("no index yet; run `outlook-index sync`".into())) }
}

fn run(cli: Cli, out: &mut dyn Write) -> Result<()> {
    if let Cmd::EmbedWorker { ref model, threads, batch } = cli.cmd {
        set_threads(threads as usize);
        if let Ok(hook) = std::env::var("OUTLOOK_INDEX_TEST_WORKER") {
            return test_worker(&hook, model.resolve(None)?.0, out);
        }
        // A load panic goes back as an error frame.
        let e = guarded(|| {
            test_panic("embed-worker");
            let (m, dir) = model.resolve(None)?;
            let mut e = Embedder::load(m, &dir)?;
            e.batch = batch as usize;
            Ok(e)
        });
        return worker::serve(e, &mut std::io::stdin().lock(), &mut { out });
    }
    if let Cmd::ExtractWorker = cli.cmd {
        return extract::serve(&mut std::io::stdin().lock(), &mut { out });
    }
    test_panic("run");
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
        Cmd::Doctor { sample, attachments } => {
            let f = file().map_err(doctor::sanitize)?;
            if attachments {
                let exe = std::env::current_exe().map_err(|e| doctor::sanitize(Error::Io(e.to_string())))?;
                let mut cmd = std::process::Command::new(exe);
                cmd.arg("extract-worker");
                let mut x = extract::Isolated::new(cmd, extract::TIMEOUT, extract::Limits::default());
                doctor::run_attachments(&f, sample.unwrap_or(300), &mut x).map_err(doctor::sanitize)?
            } else {
                doctor::run(&f, &index_path, sample.unwrap_or(200)).map_err(doctor::sanitize)?
            }
        }
        Cmd::Sync {
            ref max_time,
            allow_mass_delete,
            no_embed,
            no_attachments,
            ref include_folders,
            ref embed,
            ref progress,
        } => {
            let start = Instant::now();
            let max_time = max_time.as_deref().map(parse_duration).transpose()?;
            if include_folders.as_ref().is_some_and(|v| v.len() > 1 && v.iter().any(|x| x == "none")) {
                return Err(Error::Usage(INCLUDE_NONE.into()));
            }
            let include = include_folders.as_ref().map(|v| sync::Include {
                deleted: v.iter().any(|x| x == "deleted"),
                junk: v.iter().any(|x| x == "junk"),
            });
            let opts = sync::Options { max_time, allow_mass_delete, include, ..Default::default() };
            let f = file()?;
            let (mut c, _lock) = index::open_write(&index_path)?;
            // Model choice is checked before indexing.
            let m = if no_embed { None } else { Some(embed.model(&c)?) };
            // Each attachment is parsed in a child process (30 s timeout, restarted after a failure).
            let mut cmd = std::process::Command::new(std::env::current_exe().map_err(|e| Error::Io(e.to_string()))?);
            cmd.arg("extract-worker");
            let limits = extract::Limits { max_bytes: attach::scope(&c)?.max_bytes as usize, ..Default::default() };
            let mut x = extract::Isolated::new(cmd, extract::TIMEOUT, limits);
            let mut extract = |b: &[u8], n: Option<&str>, m: Option<&str>| x.extract(b, n, m);
            let extract: Option<&mut doctor::ExtractFn> = if no_attachments { None } else { Some(&mut extract) };
            let mut p = progress.start(m.map(|_| embed.par.workers()), m);
            let mut v = sync::sync_with_progress(&mut c, &mut StoreSource::open(&f)?, extract, &opts, &mut p)?;
            let e = match m {
                None => json!({ "skipped": "--no-embed" }),
                Some(_) if max_time.is_some_and(|t| start.elapsed() >= t) => {
                    json!({ "skipped": "max-time reached; the next sync or embed resumes" })
                }
                Some(m) => embed_step(&mut c, embed, m, start, max_time, true, &mut p)?,
            };
            p.finish(Some(&c))?;
            v.as_object_mut().unwrap().insert("embedding".into(), e);
            v
        }
        Cmd::Embed { ref max_time, ref embed, ref progress } => {
            let start = Instant::now();
            let max_time = max_time.as_deref().map(parse_duration).transpose()?;
            let (mut c, _lock) = index::open_write(&existing(index_path)?)?;
            let m = embed.model(&c)?;
            let mut p = progress.start(Some(embed.par.workers()), Some(m));
            let v = embed_step(&mut c, embed, m, start, max_time, false, &mut p)?;
            p.finish(Some(&c))?;
            v
        }
        Cmd::Scope {
            embed_since,
            embed_until,
            folders,
            attachment_chunks,
            attachment_types,
            attachment_max_mb,
            clear,
        } => {
            let embedding = clear
                || embed_since.is_some()
                || embed_until.is_some()
                || !folders.is_empty()
                || attachment_chunks.is_some();
            let attachments = clear || attachment_types.is_some() || attachment_max_mb.is_some();
            let mut requeued = None;
            let (c, _lock) = if embedding || attachments {
                let (c, lock) = index::open_write(&index_path)?;
                if embedding {
                    let mut s = if clear { vectors::Scope::default() } else { vectors::scope(&c)? };
                    if let Some(t) = embed_since {
                        s.since = Some(when("--embed-since", &t)?);
                        s.since_input = Some(t);
                    }
                    if let Some(t) = embed_until {
                        s.until = Some(when("--embed-until", &t)?);
                        s.until_input = Some(t);
                    }
                    if !folders.is_empty() {
                        s.folders = folders;
                    }
                    if let Some(n) = attachment_chunks {
                        s.attachment_chunks = n;
                    }
                    vectors::set_scope(&c, &s)?;
                }
                if attachments {
                    let mut a = if clear { attach::Scope::default() } else { attach::scope(&c)? };
                    if let Some(t) = attachment_types {
                        a.types = t;
                    }
                    if let Some(mb) = attachment_max_mb {
                        a.max_bytes = mb << 20;
                    }
                    requeued = Some(attach::set_scope(&c, &a)?);
                }
                (c, Some(lock))
            } else {
                let c = index::open_read(&index_path)?;
                index::require_current(&c)?;
                (c, None)
            };
            let mut v = vectors::counts(&c)?;
            v["attachments"] = attach::counts(&c)?;
            if let Some(n) = requeued {
                // Messages whose skipped attachments the next sync reads again.
                v["attachments"]["requeued_messages"] = json!(n);
            }
            v
        }
        Cmd::Search {
            query,
            mode,
            ref model,
            ref model_dir,
            limit,
            folders,
            since,
            until,
            from,
            to,
            has_attachments,
            excerpt_max,
            format,
        } => {
            let t = |o: Option<String>, flag| o.as_deref().map(|s| when(flag, s)).transpose();
            let (since, until) = (t(since, "--since")?, t(until, "--until")?);
            search::check_text(&query)?;
            let c = index::open_read(&index_path)?;
            index::require_current(&c)?;
            let (mut run_mode, m, warn) = search::plan(&c, mode, model.as_deref())?;
            let warning =
                |kind: &str, msg: &str| eprintln!("{}", json!({ "warning": { "kind": kind, "message": msg } }));
            if let Some(w) = warn {
                warning("keyword_fallback", &w);
            }
            let mut vector = None;
            if let Some(m) = m {
                set_threads(embed::DEFAULT_THREADS);
                match Embedder::load(m, &self::model_dir(m, model_dir)?) {
                    Ok(e) => vector = Some(e.embed_queries(&[&query])?.remove(0)),
                    // Default mode stays usable without the model files.
                    Err(Error::NotFound(_)) if mode.is_none() => {
                        warning("keyword_fallback", &format!("{MODEL_MISSING}; keyword search used"));
                        run_mode = search::Mode::Keyword;
                    }
                    Err(e) => return Err(e),
                }
            }
            let q = search::Query {
                text: query,
                limit,
                folders,
                since,
                until,
                from,
                to,
                has_attachments,
                excerpt_max: excerpt_max as usize,
                local_time: cli.local_time,
                mode: run_mode,
                vector,
                model: m,
            };
            // One snapshot from here, the model loaded before: plan again in it.
            let snapshot = c.unchecked_transaction().map_err(|e| Error::Io(format!("index database: {e}")))?;
            if run_mode != search::Mode::Keyword
                && search::plan(&snapshot, mode, model.as_deref())?.1.map(|m| m.name) != m.map(|m| m.name)
            {
                return Err(Error::Corrupt("the index vectors changed during the search; retry".into()));
            }
            let hits = search::search(&snapshot, &q)?;
            if format == Format::Jsonl {
                lines = Some(hits);
                Json::Null
            } else {
                json!({ "count": hits.len(), "mode": run_mode, "hits": hits })
            }
        }
        Cmd::EmbedWorker { .. } | Cmd::ExtractWorker => unreachable!("served above"),
        Cmd::Status { watch } => {
            let path = index_path;
            let c = index::open_read(&path)?;
            if let Some(secs) = watch {
                // jsonl whatever --pretty: one line per tick.
                loop {
                    let r = progress::live(&c, sync::held(&path))?;
                    let end = r.as_ref().is_none_or(|r| r["phase"] == "done" || r["stale"] == true);
                    writeln!(out, "{}", render(&json!({ "running": r }), false, cli.ascii)?).map_err(io)?;
                    out.flush().map_err(io)?;
                    if end {
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_secs(secs));
                }
            }
            let mut v = search::status(&c)?;
            v["running"] = progress::running(&c, sync::held(&path))?;
            v["permissions_warning"] = json!(index::permissions_warning(&path));
            v
        }
        Cmd::Backup { ref to } => index::backup(&index_path, to.as_deref().map(native_path).as_deref())?,
        Cmd::Model { cmd: ModelCmd::List } => model::list(model::default_dir)?,
        Cmd::Model { cmd: ModelCmd::Fetch { ref model } } => {
            let (m, dir) = model.resolve(Some(&index_path))?;
            model::fetch(m, &dir, |f| {
                let p = json!({ "kind": "download", "model": m.name, "file": f.name, "bytes": f.size });
                eprintln!("{}", json!({ "progress": p }))
            })?
        }
        // Same default as sync: what the index will embed with.
        Cmd::BenchEmbed { ref model, seconds, ref par, batch } => {
            let (m, dir) = model.resolve(Some(&index_path))?;
            let (workers, threads) = par.apply();
            let (t, time) = (Instant::now(), Duration::from_secs(seconds));
            if workers <= 1 {
                let mut e = Embedder::load(m, &dir)?;
                e.batch = batch as usize;
                embed::bench(&e, t.elapsed(), time, threads)?
            } else {
                let mut es = embedders(m, &dir, workers, threads, batch as usize, par.worker_timeout)?;
                let mut es: Vec<&mut dyn vectors::Embed> = es.iter_mut().map(|e| e.as_mut()).collect();
                worker::bench(&mut es, t.elapsed(), time, batch as usize, threads)?
            }
        }
    };
    match lines {
        Some(hits) => {
            for h in &hits {
                writeln!(out, "{}", render(h, false, cli.ascii)?).map_err(io)?;
            }
            Ok(())
        }
        None => writeln!(out, "{}", render(&v, cli.pretty, cli.ascii)?).map_err(io),
    }
}

fn io(e: std::io::Error) -> Error {
    Error::Io(e.to_string())
}

fn render(v: &Json, pretty: bool, ascii_only: bool) -> Result<String> {
    let s = if pretty { serde_json::to_string_pretty(v) } else { serde_json::to_string(v) };
    let s = s.map_err(|e| Error::Io(e.to_string()))?;
    Ok(if ascii_only { ascii(&s) } else { s })
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

/// Messages printed as they are: fixed words, flag names, numbers and model
/// identifiers only (compared as `template`s). Any other error gets the
/// fixed text of its kind: messages may hold paths, folder or attachment
/// names, query text or, from a reader or a worker, mail content.
const PUBLIC: &[&str] = &[
    "no index yet; run `outlook-index sync`",
    "another sync is running on this index",
    "index database: database is locked",
    "no data directory found; pass --index or set OUTLOOK_INDEX_DB",
    "no data directory found; pass --model-dir or set OUTLOOK_INDEX_MODEL_DIR",
    "no .ost file found; pass --file or set OUTLOOK_CLI_FILE",
    "index schema 7 is newer than this tool; upgrade outlook-index",
    "index schema 4 needs an upgrade; run `outlook-index sync`",
    "the index vectors changed during the search; retry",
    "the backup destination already exists; choose a new path",
    "the backup path must be valid Unicode",
    "the backup failed its integrity check",
    INCLUDE_NONE,
    "invalid --max-time (use e.g. 90s, 30m, 2h)",
    "cannot parse --since (use e.g. 2026-10-15T08:00:00Z, '2026-10-15 08:00', 2026-10-15, 2h, 3d, today)",
    "cannot parse --until (use e.g. 2026-10-15T08:00:00Z, '2026-10-15 08:00', 2026-10-15, 2h, 3d, today)",
    "cannot parse --embed-since (use e.g. 2026-10-15T08:00:00Z, '2026-10-15 08:00', 2026-10-15, 2h, 3d, today)",
    "cannot parse --embed-until (use e.g. 2026-10-15T08:00:00Z, '2026-10-15 08:00', 2026-10-15, 2h, 3d, today)",
    "--embed-since must be before --embed-until",
    "at least one attachment type is needed",
    "attachment max size must be 1 byte to 100 MB",
    "folder filter 1 of 2 matches no folder in the index (run `outlook-index sync`)",
    "the query has no word to search for",
    "semantic and hybrid search need a query vector and its model",
    "the index vectors are e5-small; search with --model e5-small, or embed again with --model e5-base --reembed",
    "the index holds 1 vectors of e5-small; pass --reembed to replace them with e5-base vectors",
    "sync would delete 1 of 2 indexed messages (limit 1); nothing changed. Pass --allow-mass-delete if this is expected",
    "model file config.json of e5-base; run `outlook-index model fetch --model e5-base` or pass --model-dir",
    "model file config.json does not match intfloat/multilingual-e5-base revision d128750597153bb5987e10b1c3493a34e5a4502a",
    "curl not found; download the model files manually and pass --model-dir",
    "embedding worker exited",
    "embedding worker timed out; run again to resume",
    "embedding worker stopped",
    "worker panicked",
    "truncated frame header (1 of 4 bytes)",
    "truncated frame (1 of 2 bytes)",
    "frame of 1 bytes",
    "worker reply of 1 bytes for 2 vectors",
    "1 vectors for 2 passages",
    "query vector of 1 dimensions, expected 2",
    "stored vector of 1 bytes, expected 2",
];

/// `m` with model identifiers as `@`, then every number as `#`.
fn template(m: &str) -> String {
    let mut ids: Vec<&str> = model::MODELS
        .iter()
        .flat_map(|m| [m.repo, m.revision, m.name].into_iter().chain(m.files.iter().map(|f| f.name)))
        .collect();
    ids.sort_by_key(|s| std::cmp::Reverse(s.len()));
    let s = ids.iter().fold(m.to_string(), |s, id| s.replace(id, "@"));
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        if !c.is_ascii_digit() {
            o.push(c);
        } else if !o.ends_with('#') {
            o.push('#');
        }
    }
    o
}

/// The public message of `e`: as is when listed in PUBLIC, else fixed per kind.
fn public(e: &Error) -> String {
    let (Error::Io(m)
    | Error::Format(m)
    | Error::Corrupt(m)
    | Error::NotFound(m)
    | Error::Usage(m)
    | Error::UpgradePending(m)) = e;
    let t = template(m);
    if PUBLIC.iter().any(|p| template(p) == t) {
        return e.to_string();
    }
    match e {
        Error::Usage(_) => "invalid usage; see `outlook-index <command> --help` (details: OUTLOOK_INDEX_DEBUG=1)",
        Error::NotFound(_) => {
            "not found: a required item is missing (the mailbox file, the index file, the model files or a folder; \
             details: OUTLOOK_INDEX_DEBUG=1)"
        }
        Error::Io(_) => {
            "I/O error: cannot read or write a file (the mailbox file, the index file, the model files or the output; \
             details: OUTLOOK_INDEX_DEBUG=1)"
        }
        Error::Format(_) => {
            "unsupported file: not a supported mailbox, index or model file (details: OUTLOOK_INDEX_DEBUG=1)"
        }
        Error::Corrupt(_) => {
            "inconsistent data: the mailbox file changed during the read; retry (details: OUTLOOK_INDEX_DEBUG=1)"
        }
        Error::UpgradePending(_) => "the index schema needs an upgrade; run `outlook-index sync`",
    }
    .into()
}

/// Parser errors: the clap error kind, and for value errors the argument
/// as defined here and its allowed values; never the user's tokens.
fn cli_error(e: &clap::Error) -> String {
    use clap::error::{ContextKind, ContextValue, ErrorKind as K};
    let mut m = e.kind().as_str().unwrap_or("invalid command line").to_string();
    let defined = [
        K::InvalidValue,
        K::ValueValidation,
        K::MissingRequiredArgument,
        K::ArgumentConflict,
        K::WrongNumberOfValues,
        K::TooManyValues,
        K::TooFewValues,
        K::NoEquals,
    ];
    if defined.contains(&e.kind()) {
        let s = |v: &ContextValue| match v {
            ContextValue::String(s) => s.clone(),
            ContextValue::Strings(v) => v.join(", "),
            _ => String::new(),
        };
        if let Some(a) = e.get(ContextKind::InvalidArg) {
            m += &format!(": {}", s(a));
        }
        if let Some(v) = e.get(ContextKind::ValidValue).map(s).filter(|v| !v.is_empty()) {
            m += &format!(" (allowed: {v})");
        }
    }
    m + "; see --help"
}

/// The only error output: `{"error":{kind,message}}` on stderr, plus the raw
/// text as `debug` with OUTLOOK_INDEX_DEBUG=1 (may hold paths and mail content).
fn fail(e: &Error, message: String, raw: String) -> ! {
    let mut v = json!({ "error": { "kind": e.kind(), "message": message } });
    if std::env::var_os("OUTLOOK_INDEX_DEBUG").is_some_and(|d| d == "1") {
        v["error"]["debug"] = json!(raw);
    }
    eprintln!("{v}");
    std::process::exit(e.exit_code())
}

/// Location (file name and line) of the last panic.
static PANIC_AT: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

fn main() {
    use clap::error::ErrorKind as K;
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e)
            if matches!(e.kind(), K::DisplayHelp | K::DisplayVersion | K::DisplayHelpOnMissingArgumentOrSubcommand) =>
        {
            e.exit()
        }
        Err(e) => fail(&Error::Usage(String::new()), cli_error(&e), e.render().to_string()),
    };
    // Panics are reported as JSON errors with their code location only, not on stderr.
    std::panic::set_hook(Box::new(|i| {
        if let (Some(l), Ok(mut at)) = (i.location(), PANIC_AT.lock()) {
            *at = format!("{}:{}", l.file().rsplit(['/', '\\']).next().unwrap_or_default(), l.line());
        }
    }));
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(cli, &mut std::io::stdout().lock()))) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => fail(&e, public(&e), e.to_string()),
        Err(p) => {
            let raw = p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()));
            let at = PANIC_AT.lock().map(|a| a.clone()).unwrap_or_default();
            fail(&Error::Corrupt(String::new()), format!("internal error at {at}"), raw.unwrap_or_default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Errors carrying mailbox-derived text (attachment and folder names,
    /// paths, reader and worker messages) get the fixed text of their kind;
    /// listed messages with only numbers and model names pass.
    #[test]
    fn public_messages() {
        let m = || "SECRET_Q3 report.pdf in /home/SECRET_USER/alice.ost: SECRET_BODY 42".to_string();
        for e in [Error::Io(m()), Error::Format(m()), Error::Corrupt(m()), Error::NotFound(m()), Error::Usage(m())] {
            let p = public(&e);
            assert!(!p.contains("SECRET") && !p.contains("42"), "{p}");
        }
        for e in [
            Error::NotFound("no index yet; run `outlook-index sync`".into()),
            Error::Usage("the index holds 12 vectors of e5-small; pass --reembed to replace them with e5-base vectors".into()),
            Error::Usage("sync would delete 300 of 1000 indexed messages (limit 200); nothing changed. Pass --allow-mass-delete if this is expected".into()),
        ] {
            assert_eq!(public(&e), e.to_string());
        }
        // A listed message with other text in a number slot is not listed.
        let e = Error::NotFound(
            "folder filter SECRET of 2 matches no folder in the index (run `outlook-index sync`)".into(),
        );
        assert!(!public(&e).contains("SECRET"));
    }
}
