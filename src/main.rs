use std::collections::{BTreeMap, HashSet};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::{Map, Value as Json, json};

use outlook_cli::body::{html_of, text_of, truncate};
use outlook_cli::error::{Error, Result, guarded, with_retry};
use outlook_cli::ndb::Nid;
use outlook_cli::paths::{default_file, native_path};
use outlook_cli::store::{
    Folder, MSGFLAG_HASATTACH, MSGFLAG_READ, Message, RowSummary, Store, is_mail_folder, parse_id, resolve_folders, tag,
};
use outlook_cli::timeutil::{format_ft, now_ft, parse_when};

/// Read e-mails from an Outlook OST/PST file (works while Outlook is running).
///
/// Output is JSON by default, designed to be consumed by scripts and AI agents.
/// Exit codes: 0 ok, 2 usage, 3 not found, 4 I/O, 5 unsupported file, 6 inconsistent data.
#[derive(Parser)]
#[command(name = "outlook-cli", version, max_term_width = 110)]
struct Cli {
    /// OST/PST file. Default: $OUTLOOK_CLI_FILE, else the most recently
    /// modified .ost in %LOCALAPPDATA%\Microsoft\Outlook.
    #[arg(short, long, global = true, env = "OUTLOOK_CLI_FILE")]
    file: Option<String>,

    /// Output format.
    #[arg(long, global = true, value_enum, default_value_t = Format::Json)]
    format: Format,

    /// Pretty-print JSON.
    #[arg(long, global = true)]
    pretty: bool,

    /// Print times in the local time zone instead of UTC.
    #[arg(long, global = true)]
    local_time: bool,

    /// Escape non-ASCII characters in JSON as \uXXXX. Use it when the reader
    /// does not decode UTF-8 (Windows PowerShell 5.1 pipelines, for instance).
    #[arg(long, global = true, env = "OUTLOOK_CLI_ASCII", value_parser = clap::builder::FalseyValueParser::new())]
    ascii: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    /// One JSON document.
    Json,
    /// One JSON object per line (one per message).
    Jsonl,
    /// Human-readable text.
    Text,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show information about the file.
    Info,
    /// List folders with their message counts.
    Folders {
        /// Include system folders outside the visible mailbox tree.
        #[arg(long)]
        all: bool,
    },
    /// List messages matching filters, newest first.
    #[command(visible_aliases = ["ls", "mails"])]
    List {
        #[command(flatten)]
        filter: Filter,
        #[command(flatten)]
        content: Content,
        /// Maximum number of messages (0 = no limit).
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
        /// Oldest first (use with --after to page forward without gaps).
        #[arg(long)]
        oldest_first: bool,
    },
    /// Show one or more messages in full (body, recipients, attachments).
    #[command(visible_alias = "show")]
    Get {
        /// Message ids (the "id" field of `list`), or Internet Message-IDs ("<...@...>").
        #[arg(required = true)]
        ids: Vec<String>,
        /// Include the raw HTML body.
        #[arg(long)]
        html: bool,
        /// Include the Internet transport headers.
        #[arg(long)]
        headers: bool,
        /// Truncate the text body to this many characters (0 = no limit).
        #[arg(long, default_value_t = 0)]
        body_max: usize,
        /// Omit the body.
        #[arg(long)]
        no_body: bool,
    },
    /// Save the attachments of a message to a directory.
    #[command(visible_alias = "save-attachments")]
    Attachments {
        /// Message id.
        id: String,
        /// Destination directory (created if needed).
        #[arg(short, long, default_value = ".")]
        dir: String,
        /// Only these attachment indexes (repeatable).
        #[arg(short, long)]
        index: Vec<usize>,
        /// Also save inline attachments (images embedded in the body).
        #[arg(long)]
        include_inline: bool,
    },
    /// Report messages that arrived since the previous call (state kept in a file).
    ///
    /// The first call records the current messages and reports nothing (or the
    /// newest --initial ones). Robust to sync delays: a message is new because
    /// its id was never seen, not because of its timestamp.
    New {
        /// State file (created/updated). One per consumer.
        #[arg(long)]
        state: String,
        #[command(flatten)]
        filter: Filter,
        #[command(flatten)]
        content: Content,
        /// On the first call, report up to this many existing recent messages.
        #[arg(long, default_value_t = 0)]
        initial: usize,
        /// Messages received before (newest seen - window) are never reported.
        #[arg(long, default_value = "7d")]
        window: String,
    },
    /// Watch for new messages and print each one as a JSON line as it arrives.
    Watch {
        #[command(flatten)]
        filter: Filter,
        #[command(flatten)]
        content: Content,
        /// Polling interval in seconds.
        #[arg(short, long, default_value_t = 5.0)]
        interval: f64,
        /// Also persist seen ids to this state file (resume without loss).
        #[arg(long)]
        state: Option<String>,
        /// Stop after this many messages (0 = run forever).
        #[arg(long, default_value_t = 0)]
        max: usize,
    },
}

#[derive(Args, Clone, Default)]
struct Filter {
    /// Folder path, name or id; repeatable. "all" = every mail folder. Default: Inbox.
    #[arg(short = 'F', long = "folder")]
    folders: Vec<String>,
    /// Received at or after (2026-10-15T08:00:00Z, '2026-10-15 08:00' local, 2h, 3d, today).
    #[arg(short, long)]
    since: Option<String>,
    /// Received strictly after this timestamp (pass back the previous "cursor").
    #[arg(long)]
    after: Option<String>,
    /// Received strictly before.
    #[arg(short, long)]
    until: Option<String>,
    /// Sender name or address contains (case-insensitive); repeatable = any.
    #[arg(long)]
    from: Vec<String>,
    /// A recipient (to/cc/bcc) name or address contains; repeatable = any.
    #[arg(long)]
    to: Vec<String>,
    /// Subject contains (case-insensitive).
    #[arg(long)]
    subject: Option<String>,
    /// Only unread messages.
    #[arg(long, conflicts_with = "read")]
    unread: bool,
    /// Only read messages.
    #[arg(long)]
    read: bool,
    /// Only messages with attachments.
    #[arg(long)]
    has_attachments: bool,
    /// Message class prefix (IPM.Note = e-mails, IPM.Schedule = meeting requests...).
    #[arg(long)]
    class: Option<String>,
}

#[derive(Args, Clone, Default)]
struct Content {
    /// Include the text body.
    #[arg(short, long)]
    body: bool,
    /// Truncate the text body to this many characters (0 = no limit).
    #[arg(long, default_value_t = 0)]
    body_max: usize,
    /// Include the recipient list (to/cc/bcc with addresses).
    #[arg(long)]
    recipients: bool,
    /// Include the attachment list.
    #[arg(long)]
    attachments: bool,
    /// Include body, recipients and attachments.
    #[arg(long)]
    full: bool,
}

struct Ctx {
    path: PathBuf,
    format: Format,
    pretty: bool,
    local_time: bool,
}

impl Ctx {
    fn open(&self) -> Result<Store> {
        Store::open(&self.path)
    }

    fn time(&self, ft: Option<i64>) -> Json {
        ft.and_then(|t| format_ft(t, self.local_time)).map(Json::String).unwrap_or(Json::Null)
    }

    fn print(&self, out: &mut dyn Write, v: &Json) -> Result<()> {
        let s = if self.pretty { serde_json::to_string_pretty(v) } else { serde_json::to_string(v) };
        writeln!(out, "{}", s.map_err(|e| Error::Io(e.to_string()))?).map_err(io_err)
    }
}

fn io_err(e: std::io::Error) -> Error {
    Error::Io(e.to_string())
}

/// Writer that escapes non-ASCII characters as JSON \uXXXX sequences. JSON
/// output only has non-ASCII inside strings, so the result stays valid JSON.
struct AsciiEscape<W: Write> {
    inner: W,
    pending: Vec<u8>,
}

impl<W: Write> Write for AsciiEscape<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.pending.extend_from_slice(buf);
        let valid = match std::str::from_utf8(&self.pending) {
            Ok(_) => self.pending.len(),
            Err(e) => e.valid_up_to(),
        };
        let mut out = Vec::with_capacity(valid + 16);
        for c in std::str::from_utf8(&self.pending[..valid]).unwrap().chars() {
            if c.is_ascii() {
                out.push(c as u8);
            } else {
                let mut units = [0u16; 2];
                for u in c.encode_utf16(&mut units) {
                    write!(out, "\\u{u:04x}")?;
                }
            }
        }
        self.inner.write_all(&out)?;
        self.pending.drain(..valid);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

// ---------------------------------------------------------------- filters

#[derive(Clone)]
struct Compiled {
    since: Option<i64>,
    after: Option<i64>,
    until: Option<i64>,
    from: Vec<String>,
    to: Vec<String>,
    subject: Option<String>,
    unread: bool,
    read: bool,
    attach: bool,
    class: Option<String>,
}

impl Filter {
    fn compile(&self) -> Result<Compiled> {
        let t = |o: &Option<String>| o.as_deref().map(parse_when).transpose();
        let low = |v: &[String]| v.iter().map(|s| s.to_lowercase()).collect();
        Ok(Compiled {
            since: t(&self.since)?,
            after: t(&self.after)?,
            until: t(&self.until)?,
            from: low(&self.from),
            to: low(&self.to),
            subject: self.subject.as_ref().map(|s| s.to_lowercase()),
            unread: self.unread,
            read: self.read,
            attach: self.has_attachments,
            class: self.class.as_ref().map(|s| s.to_lowercase()),
        })
    }
}

impl Compiled {
    /// Filters answerable from the folder's contents table.
    fn row_ok(&self, r: &RowSummary) -> bool {
        if self.since.is_some_and(|t| r.received < t)
            || self.after.is_some_and(|t| r.received <= t)
            || self.until.is_some_and(|t| r.received >= t)
            || (self.unread && r.flags & MSGFLAG_READ != 0)
            || (self.read && r.flags & MSGFLAG_READ == 0)
            || (self.attach && r.flags & MSGFLAG_HASATTACH == 0)
        {
            return false;
        }
        if let Some(c) = &self.class
            && !r.class.get().unwrap_or_default().to_lowercase().starts_with(c.as_str())
        {
            return false;
        }
        if let Some(s) = &self.subject
            && !r.subject.get().unwrap_or_default().to_lowercase().contains(s.as_str())
        {
            return false;
        }
        true
    }

    /// Filters that need the message itself.
    fn message_ok(&self, m: &Message) -> Result<bool> {
        if !self.from.is_empty() {
            let h = m.from_haystack();
            if !self.from.iter().any(|f| h.contains(f.as_str())) {
                return Ok(false);
            }
        }
        if !self.to.is_empty() {
            let mut h = String::new();
            for t in [tag::DISPLAY_TO, tag::DISPLAY_CC] {
                h.push_str(&m.pc.str(t).unwrap_or_default());
                h.push(' ');
            }
            for r in m.recipients()? {
                h.push_str(&format!("{} {} ", r.addr.name.unwrap_or_default(), r.addr.email.unwrap_or_default()));
            }
            let h = h.to_lowercase();
            if !self.to.iter().any(|f| h.contains(f.as_str())) {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

#[derive(Clone, Copy)]
struct Candidate {
    received: i64,
    nid: Nid,
    folder: Nid,
}

fn scan(store: &Store, folders: &[Folder], f: &Compiled) -> Result<Vec<Candidate>> {
    Ok(scan_all(store, folders, f)?.into_iter().filter(|c| c.1).map(|c| c.0).collect())
}

/// Every message of the folders, flagged with whether it passes the row filters.
fn scan_all(store: &Store, folders: &[Folder], f: &Compiled) -> Result<Vec<(Candidate, bool)>> {
    let mut out = Vec::new();
    for folder in folders {
        store.scan_folder(folder.nid, &mut |r| {
            out.push((Candidate { received: r.received, nid: r.nid, folder: r.folder }, f.row_ok(r)));
            true
        })?;
    }
    Ok(out)
}

// ---------------------------------------------------------------- output

struct Want {
    body: bool,
    body_max: usize,
    recipients: bool,
    attachments: bool,
    html: bool,
    headers: bool,
}

impl Content {
    fn want(&self) -> Want {
        Want {
            body: self.body || self.full,
            body_max: self.body_max,
            recipients: self.recipients || self.full,
            attachments: self.attachments || self.full,
            html: false,
            headers: false,
        }
    }
}

fn message_json(ctx: &Ctx, m: &Message, folder: Option<&str>, want: &Want) -> Result<Json> {
    let p = &m.pc;
    let flags = m.flags();
    let mut o = Map::new();
    o.insert("id".into(), json!(m.nid));
    if let Some(f) = folder {
        o.insert("folder".into(), json!(f));
    }
    o.insert("received".into(), ctx.time(m.received()));
    o.insert("sent".into(), ctx.time(p.time(tag::CLIENT_SUBMIT_TIME)));
    o.insert("subject".into(), json!(m.subject()));
    o.insert("from".into(), serde_json::to_value(m.from()).unwrap());
    o.insert("to".into(), json!(p.str(tag::DISPLAY_TO)));
    if let Some(cc) = p.str(tag::DISPLAY_CC) {
        o.insert("cc".into(), json!(cc));
    }
    o.insert("unread".into(), json!(flags & MSGFLAG_READ == 0));
    o.insert("has_attachments".into(), json!(flags & MSGFLAG_HASATTACH != 0));
    let importance = match p.int(tag::IMPORTANCE) {
        Some(0) => "low",
        Some(2) => "high",
        _ => "normal",
    };
    o.insert("importance".into(), json!(importance));
    o.insert("class".into(), json!(p.str(tag::MESSAGE_CLASS)));
    o.insert("size".into(), json!(p.int(tag::MESSAGE_SIZE)));
    o.insert("message_id".into(), json!(p.str(tag::INTERNET_MESSAGE_ID)));
    if let Some(r) = p.str(tag::IN_REPLY_TO) {
        o.insert("in_reply_to".into(), json!(r));
    }
    if !want.body
        && let Some(pv) = p.str(tag::PREVIEW)
    {
        o.insert("preview".into(), json!(pv.split_whitespace().collect::<Vec<_>>().join(" ")));
    }
    if want.recipients {
        o.insert("recipients".into(), serde_json::to_value(m.recipients()?).unwrap());
    }
    if want.attachments {
        o.insert("attachments".into(), serde_json::to_value(m.attachments()?).unwrap());
    }
    if want.body {
        let (text, cut) = truncate(text_of(m).unwrap_or_default(), want.body_max);
        o.insert("body".into(), json!(text));
        if cut {
            o.insert("body_truncated".into(), json!(true));
        }
    }
    if want.html {
        o.insert("html".into(), json!(html_of(m)));
    }
    if want.headers {
        o.insert("headers".into(), json!(p.str(tag::TRANSPORT_HEADERS)));
    }
    Ok(Json::Object(o))
}

fn text_line(v: &Json) -> String {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let from = v.get("from").map(|f| {
        let n = f.get("name").and_then(|x| x.as_str()).unwrap_or("");
        let e = f.get("email").and_then(|x| x.as_str()).unwrap_or("");
        if n.is_empty() { e.to_string() } else { format!("{n} <{e}>") }
    });
    let unread = if v.get("unread").and_then(|x| x.as_bool()).unwrap_or(false) { "*" } else { " " };
    let att = if v.get("has_attachments").and_then(|x| x.as_bool()).unwrap_or(false) { "@" } else { " " };
    let mut line = format!(
        "{unread}{att} {:>8}  {}  {}  {}",
        v.get("id").map(|x| x.to_string()).unwrap_or_default(),
        s("received"),
        from.unwrap_or_default(),
        s("subject")
    );
    if let Some(b) = v.get("body").and_then(|x| x.as_str()) {
        line.push_str("\n\n");
        line.push_str(b);
        line.push_str("\n\n----");
    }
    line
}

fn emit_messages(ctx: &Ctx, out: &mut dyn Write, msgs: &[Json], extra: Map<String, Json>) -> Result<()> {
    match ctx.format {
        Format::Json => {
            let mut o = extra;
            o.insert("count".into(), json!(msgs.len()));
            o.insert("messages".into(), Json::Array(msgs.to_vec()));
            ctx.print(out, &Json::Object(o))
        }
        Format::Jsonl => {
            for m in msgs {
                writeln!(out, "{}", m).map_err(io_err)?;
            }
            Ok(())
        }
        Format::Text => {
            for m in msgs {
                writeln!(out, "{}", text_line(m)).map_err(io_err)?;
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- commands

fn cmd_info(ctx: &Ctx, out: &mut dyn Write) -> Result<()> {
    let v = with_retry(|| info_json(ctx))?;
    if ctx.format == Format::Text {
        writeln!(out, "{}", serde_json::to_string_pretty(&v).unwrap()).map_err(io_err)
    } else {
        ctx.print(out, &v)
    }
}

fn info_json(ctx: &Ctx) -> Result<Json> {
    let store = ctx.open()?;
    let folders = store.folders(false)?;
    let inbox = store.inbox(&folders);
    Ok(json!({
        "file": ctx.path.display().to_string(),
        "size": store.ndb.file_len(),
        "kind": if store.ndb.is_ost { "ost" } else { "pst" },
        "format": store.ndb.format.name(),
        "folders": folders.len(),
        "messages": folders.iter().filter(|f| is_mail_folder(f)).map(|f| f.count).sum::<i64>(),
        "inbox": inbox.and_then(|n| folders.iter().find(|f| f.nid == n)).map(|f| json!({
            "id": f.nid, "path": f.path, "count": f.count, "unread": f.unread
        })),
    }))
}

fn cmd_folders(ctx: &Ctx, out: &mut dyn Write, all: bool) -> Result<()> {
    let (folders, inbox) = with_retry(|| {
        let store = ctx.open()?;
        Ok((store.folders(all)?, store.inbox(&store.folders(false)?)))
    })?;
    let rows: Vec<Json> = folders
        .iter()
        .map(|f| {
            let mut o = json!({
                "id": f.nid, "path": f.path, "name": f.name, "class": f.class,
                "count": f.count, "unread": f.unread,
            });
            if Some(f.nid) == inbox {
                o["well_known"] = json!("inbox");
            }
            o
        })
        .collect();
    match ctx.format {
        Format::Json => ctx.print(out, &json!({ "folders": rows })),
        Format::Jsonl => rows.iter().try_for_each(|r| writeln!(out, "{r}").map_err(io_err)),
        Format::Text => folders.iter().try_for_each(|f| {
            writeln!(out, "{:>8}  {:>6} {:>6}  {}", f.nid, f.count, f.unread, f.path).map_err(io_err)
        }),
    }
}

fn select(store: &Store, cands: &[Candidate], f: &Compiled, limit: usize) -> Result<Vec<Candidate>> {
    let mut out = Vec::new();
    for c in cands {
        if limit > 0 && out.len() >= limit {
            break;
        }
        let m = match store.message(c.nid) {
            Ok(m) => m,
            Err(Error::NotFound(_)) => continue, // deleted since the table was read
            Err(e) => return Err(e),
        };
        if f.message_ok(&m)? {
            out.push(*c);
        }
    }
    Ok(out)
}

fn render(ctx: &Ctx, store: &Store, sel: &[Candidate], folders: &[Folder], want: &Want) -> Result<Vec<Json>> {
    let names: BTreeMap<Nid, &str> = folders.iter().map(|f| (f.nid, f.path.as_str())).collect();
    sel.iter()
        .filter_map(|c| match store.message(c.nid) {
            Ok(m) => Some(message_json(ctx, &m, names.get(&c.folder).copied(), want)),
            Err(Error::NotFound(_)) => None,
            Err(e) => Some(Err(e)),
        })
        .collect()
}

/// Render and write messages one at a time, so memory does not grow with
/// the result size. A message read while Outlook rewrites it is retried on a
/// freshly mapped file.
#[allow(clippy::too_many_arguments)]
fn stream(
    ctx: &Ctx,
    out: &mut dyn Write,
    mut store: Store,
    sel: &[Candidate],
    folders: &[Folder],
    want: &Want,
    extra: Map<String, Json>,
) -> Result<()> {
    let names: BTreeMap<Nid, String> = folders.iter().map(|f| (f.nid, f.path.clone())).collect();
    let envelope = ctx.format == Format::Json && !ctx.pretty;
    let mut collected = Vec::new();
    if envelope {
        let mut head = serde_json::to_string(&Json::Object(extra.clone())).unwrap();
        head.pop();
        write!(out, "{head}{}\"messages\":[", if extra.is_empty() { "" } else { "," }).map_err(io_err)?;
    }
    let mut n = 0;
    for c in sel {
        let mut attempt = 0;
        let v = loop {
            let r = guarded(|| {
                store.message(c.nid).and_then(|m| message_json(ctx, &m, names.get(&c.folder).map(|s| s.as_str()), want))
            });
            match r {
                Err(Error::Corrupt(_)) if attempt < 5 => {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(50 * attempt));
                    store = ctx.open()?;
                }
                Err(Error::NotFound(_)) => break None, // deleted meanwhile
                r => break Some(r?),
            }
        };
        let Some(v) = v else { continue };
        match ctx.format {
            Format::Json if !envelope => collected.push(v),
            Format::Json => write!(out, "{}{v}", if n > 0 { "," } else { "" }).map_err(io_err)?,
            Format::Jsonl => writeln!(out, "{v}").map_err(io_err)?,
            Format::Text => writeln!(out, "{}", text_line(&v)).map_err(io_err)?,
        }
        n += 1;
    }
    if envelope {
        writeln!(out, "],\"count\":{n}}}").map_err(io_err)?;
    } else if ctx.format == Format::Json {
        emit_messages(ctx, out, &collected, extra)?;
    }
    Ok(())
}

fn cmd_list(ctx: &Ctx, out: &mut dyn Write, filter: &Filter, content: &Content, limit: usize, asc: bool) -> Result<()> {
    let f = filter.compile()?;
    let want = content.want();
    let (store, folders, sel, cursor) = with_retry(|| {
        let store = ctx.open()?;
        let folders = resolve_folders(&store, &filter.folders)?;
        let mut cands = scan(&store, &folders, &f)?;
        if asc {
            cands.sort_unstable_by_key(|c| (c.received, c.nid));
        } else {
            cands.sort_unstable_by_key(|c| std::cmp::Reverse((c.received, c.nid)));
        }
        let sel = select(&store, &cands, &f, limit)?;
        let cursor = sel.iter().map(|c| c.received).max().or(f.after).or(f.since);
        Ok((store, folders, sel, cursor))
    })?;
    let mut extra = Map::new();
    // Pass back as --after to get only newer messages next time.
    extra.insert("cursor".into(), ctx.time(cursor));
    stream(ctx, out, store, &sel, &folders, &want, extra)
}

fn find_by_message_id(store: &Store, mid: &str) -> Result<Option<(Nid, Nid)>> {
    let mut found = None;
    for folder in store.folders(false)? {
        store.scan_folder(folder.nid, &mut |r| {
            if r.message_id.get().is_some_and(|m| m == mid) {
                found = Some((r.nid, r.folder));
                return false;
            }
            true
        })?;
        if found.is_some() {
            break;
        }
    }
    Ok(found)
}

fn cmd_get(ctx: &Ctx, out: &mut dyn Write, ids: &[String], want: Want) -> Result<()> {
    let msgs = with_retry(|| {
        let store = ctx.open()?;
        let folders = store.folders(false)?;
        let names: BTreeMap<Nid, &str> = folders.iter().map(|f| (f.nid, f.path.as_str())).collect();
        let mut v = Vec::new();
        for id in ids {
            let nid = if id.contains('@') {
                let mid = if id.starts_with('<') { id.clone() } else { format!("<{id}>") };
                find_by_message_id(&store, &mid)?.ok_or_else(|| Error::NotFound(format!("message {id}")))?.0
            } else {
                parse_id(id)?
            };
            let m = store.message(nid)?;
            v.push(message_json(ctx, &m, names.get(&m.folder).copied(), &want)?);
        }
        Ok(v)
    })?;
    if ctx.format == Format::Json && msgs.len() == 1 {
        return ctx.print(out, &msgs[0]);
    }
    emit_messages(ctx, out, &msgs, Map::new())
}

fn safe_name(name: &str) -> String {
    let s: String = name.chars().map(|c| if "\\/:*?\"<>|".contains(c) || c.is_control() { '_' } else { c }).collect();
    let s = s.trim().trim_matches('.').to_string();
    if s.is_empty() { "attachment".into() } else { s }
}

fn cmd_attachments(ctx: &Ctx, out: &mut dyn Write, id: &str, dir: &str, index: &[usize], inline: bool) -> Result<()> {
    let nid = parse_id(id)?;
    let dir = native_path(dir);
    std::fs::create_dir_all(&dir).map_err(|e| Error::Io(format!("{}: {e}", dir.display())))?;
    let saved = with_retry(|| {
        let store = ctx.open()?;
        let m = store.message(nid)?;
        let mut saved = Vec::new();
        for a in m.attachments()? {
            if (!index.is_empty() && !index.contains(&a.index)) || (index.is_empty() && a.inline && !inline) {
                continue;
            }
            let entry = match m.attachment_data(&a) {
                Ok(data) => {
                    let base = safe_name(a.name.as_deref().unwrap_or("attachment"));
                    let mut path = dir.join(&base);
                    let mut n = 1;
                    while path.exists() {
                        let p = Path::new(&base);
                        let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                        let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
                        path = dir.join(format!("{stem} ({n}){ext}"));
                        n += 1;
                    }
                    std::fs::write(&path, &data).map_err(|e| Error::Io(format!("{}: {e}", path.display())))?;
                    json!({"index": a.index, "name": a.name, "path": path.display().to_string(), "bytes": data.len()})
                }
                Err(Error::NotFound(msg)) => json!({"index": a.index, "name": a.name, "error": msg}),
                Err(e) => return Err(e),
            };
            saved.push(entry);
        }
        Ok(saved)
    })?;
    ctx.print(out, &json!({ "id": nid, "saved": saved }))
}

/// Seen-id state for `new` and `watch`.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct State {
    /// Newest received time seen (FILETIME).
    newest: i64,
    /// Ids of messages received within the window before `newest`.
    seen: Vec<Nid>,
}

impl State {
    fn load(path: &Path) -> Result<Option<State>> {
        match std::fs::read(path) {
            Ok(b) => serde_json::from_slice(&b)
                .map(Some)
                .map_err(|e| Error::Usage(format!("bad state file {}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::Io(format!("{}: {e}", path.display()))),
        }
    }

    fn save(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec(self).unwrap())
            .map_err(|e| Error::Io(format!("{}: {e}", tmp.display())))?;
        std::fs::rename(&tmp, path).map_err(|e| Error::Io(format!("{}: {e}", path.display())))
    }
}

/// Messages not seen before, oldest first; updates `seen` with everything
/// currently present within the window.
fn diff_new(
    store: &Store,
    folders: &[Folder],
    f: &Compiled,
    state: &mut State,
    window: i64,
    first: bool,
) -> Result<Vec<Candidate>> {
    let mut cands = scan_all(store, folders, f)?;
    let floor = state.newest - window;
    let seen: HashSet<Nid> = state.seen.iter().copied().collect();
    cands.sort_unstable_by_key(|(c, _)| (c.received, c.nid));
    let fresh: Vec<Candidate> = if first {
        Vec::new()
    } else {
        cands.iter().filter(|(c, ok)| *ok && c.received >= floor && !seen.contains(&c.nid)).map(|c| c.0).collect()
    };
    let newest = cands.iter().map(|c| c.0.received).max().unwrap_or(0).max(state.newest);
    state.newest = newest;
    // Everything present is seen, whether or not it passed the filters.
    let mut keep: Vec<Nid> = cands.iter().filter(|(c, _)| c.received >= newest - window).map(|c| c.0.nid).collect();
    keep.sort_unstable();
    keep.dedup();
    state.seen = keep;
    // Content filters (from/to) only decide what is reported, not what is seen.
    select(store, &fresh, f, 0)
}

fn parse_window(s: &str) -> Result<i64> {
    Ok(now_ft() - parse_when(s)?)
}

fn cmd_new(
    ctx: &Ctx,
    out: &mut dyn Write,
    state_path: &str,
    filter: &Filter,
    content: &Content,
    initial: usize,
    window: &str,
) -> Result<()> {
    let f = filter.compile()?;
    let want = content.want();
    let window = parse_window(window)?;
    let path = native_path(state_path);
    let loaded = State::load(&path)?;
    let first = loaded.is_none();
    let loaded = loaded.unwrap_or_default();
    let (store, folders, sel, state) = with_retry(|| {
        let mut state = State { newest: loaded.newest, seen: loaded.seen.clone() };
        let store = ctx.open()?;
        let folders = resolve_folders(&store, &filter.folders)?;
        let mut sel = diff_new(&store, &folders, &f, &mut state, window, first)?;
        if first && initial > 0 {
            let mut c = scan(&store, &folders, &f)?;
            c.sort_unstable_by_key(|c| std::cmp::Reverse((c.received, c.nid)));
            sel = select(&store, &c, &f, initial)?;
            sel.reverse();
        }
        Ok((store, folders, sel, state))
    })?;
    let mut extra = Map::new();
    extra.insert("initialized".into(), json!(first));
    stream(ctx, out, store, &sel, &folders, &want, extra)?;
    // Saved only once the messages were written out.
    state.save(&path)
}

fn cmd_watch(
    ctx: &Ctx,
    out: &mut dyn Write,
    filter: &Filter,
    content: &Content,
    interval: f64,
    state_path: Option<&str>,
    max: usize,
) -> Result<()> {
    let f = filter.compile()?;
    let want = content.want();
    let path = state_path.map(native_path);
    let loaded = match &path {
        Some(p) => State::load(p)?,
        None => None,
    };
    let mut first = loaded.is_none();
    let mut st = loaded.unwrap_or_default();
    let window = 7 * 86_400 * 10_000_000;
    let mut last_rev = None;
    let mut emitted = 0usize;
    let mut fails = 0;
    loop {
        let res = guarded(|| -> Result<Vec<Json>> {
            let store = ctx.open()?;
            if last_rev == Some(store.ndb.revision) {
                return Ok(Vec::new());
            }
            let folders = resolve_folders(&store, &filter.folders)?;
            let mut tmp = State { newest: st.newest, seen: st.seen.clone() };
            let sel = diff_new(&store, &folders, &f, &mut tmp, window, first)?;
            let v = render(ctx, &store, &sel, &folders, &want)?;
            st = tmp;
            last_rev = Some(store.ndb.revision);
            Ok(v)
        });
        match res {
            Ok(v) => {
                fails = 0;
                if (first || !v.is_empty())
                    && let Some(p) = &path
                {
                    st.save(p)?;
                }
                first = false;
                for m in v {
                    match ctx.format {
                        Format::Text => writeln!(out, "{}", text_line(&m)),
                        _ => writeln!(out, "{m}"),
                    }
                    .map_err(io_err)?;
                    emitted += 1;
                    if max > 0 && emitted >= max {
                        out.flush().map_err(io_err)?;
                        return Ok(());
                    }
                }
                out.flush().map_err(io_err)?;
            }
            // Transient: Outlook was writing; try again next round.
            Err(Error::Corrupt(_)) if fails < 20 => fails += 1,
            Err(e) => return Err(e),
        }
        std::thread::sleep(Duration::from_secs_f64(interval.max(0.2)));
    }
}

fn run(cli: Cli) -> Result<()> {
    let path = match &cli.file {
        Some(f) => native_path(f),
        None => default_file()?,
    };
    let ctx = Ctx { path, format: cli.format, pretty: cli.pretty, local_time: cli.local_time };
    let stdout = BufWriter::new(std::io::stdout().lock());
    let mut out: Box<dyn Write> = if cli.ascii && cli.format != Format::Text {
        Box::new(AsciiEscape { inner: stdout, pending: Vec::new() })
    } else {
        Box::new(stdout)
    };
    match &cli.cmd {
        Cmd::Info => cmd_info(&ctx, &mut out),
        Cmd::Folders { all } => cmd_folders(&ctx, &mut out, *all),
        Cmd::List { filter, content, limit, oldest_first } => {
            cmd_list(&ctx, &mut out, filter, content, *limit, *oldest_first)
        }
        Cmd::Get { ids, html, headers, body_max, no_body } => {
            let want = Want {
                body: !no_body,
                body_max: *body_max,
                recipients: true,
                attachments: true,
                html: *html,
                headers: *headers,
            };
            cmd_get(&ctx, &mut out, ids, want)
        }
        Cmd::Attachments { id, dir, index, include_inline } => {
            cmd_attachments(&ctx, &mut out, id, dir, index, *include_inline)
        }
        Cmd::New { state, filter, content, initial, window } => {
            cmd_new(&ctx, &mut out, state, filter, content, *initial, window)
        }
        Cmd::Watch { filter, content, interval, state, max } => {
            cmd_watch(&ctx, &mut out, filter, content, *interval, state.as_deref(), *max)
        }
    }?;
    out.flush().map_err(io_err)
}

fn main() {
    // Panics are reported as JSON errors (see `guarded`), not on stderr.
    let cli = Cli::parse();
    std::panic::set_hook(Box::new(|_| {}));
    if let Err(e) = run(cli) {
        let v = json!({ "error": { "kind": e.kind(), "message": e.to_string() } });
        eprintln!("{v}");
        std::process::exit(e.exit_code());
    }
}
