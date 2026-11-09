//! Command-line interface.

use crate::decode::shape;
use crate::redact::{scrub, scrub_text};
use crate::store::{load_snapshots, select, text_of, truthy, ts_key, Row, Snapshot};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Map, Value};
use std::io::{self, Write};
use unicode_width::UnicodeWidthChar;

/// Where output goes and how it may be styled.
pub struct Term {
    /// stdout is a terminal: color and clipping to `width` are allowed.
    pub tty: bool,
    pub width: usize,
    pub color: bool,
}

impl Term {
    /// Plain output, as when piped.
    pub fn plain() -> Self {
        Term {
            tty: false,
            width: 120,
            color: false,
        }
    }
}

#[derive(Parser)]
#[command(
    name = "slack-cli",
    version,
    about = "Read your own Slack desktop cache. Offline and read-only: no network, no credentials, no Keychain."
)]
struct Cli {
    /// Workspace id, name or domain
    #[arg(long, global = true, value_name = "TEAM")]
    team: Option<String>,
    /// Slack data directory (default: auto-detect; env SLACK_CLI_DATA_DIR also works)
    #[arg(long, global = true, value_name = "PATH")]
    data_dir: Option<String>,
    /// Machine-readable output
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum Kind {
    Public,
    Private,
    Im,
    Mpim,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Public => "public",
            Kind::Private => "private",
            Kind::Im => "im",
            Kind::Mpim => "mpim",
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// List cached workspaces
    Teams,
    /// List channels and DMs
    Channels {
        /// Include channels you are not in
        #[arg(long)]
        all: bool,
        /// Only channels with cached messages
        #[arg(long)]
        cached: bool,
        /// Filter by kind (repeatable)
        #[arg(long, value_enum)]
        kind: Vec<Kind>,
    },
    /// List members
    Members {
        /// Include bots, deactivated users and Slack Connect members
        #[arg(long)]
        all: bool,
        /// Show email addresses
        #[arg(long)]
        emails: bool,
    },
    /// Print a channel's cached messages
    History {
        /// Channel id, #name, or @user for a DM
        channel: String,
        /// Last N messages (0 = all)
        #[arg(short = 'n', long, default_value_t = 50, allow_negative_numbers = true)]
        limit: i64,
        /// Only messages in the thread with this ts
        #[arg(long)]
        thread: Option<String>,
    },
    /// Search cached messages
    Search {
        query: String,
        /// Limit to one channel
        #[arg(long)]
        channel: Option<String>,
        /// Limit to one author (handle, name or id)
        #[arg(long)]
        user: Option<String>,
        /// Max results (0 = all)
        #[arg(
            short = 'n',
            long,
            default_value_t = 100,
            allow_negative_numbers = true
        )]
        limit: i64,
    },
    /// List cached file metadata
    Files {
        /// Max rows (0 = all)
        #[arg(short = 'n', long, default_value_t = 50, allow_negative_numbers = true)]
        limit: i64,
    },
    /// Dump the cache as JSON
    Export {
        /// Keep email addresses
        #[arg(long)]
        emails: bool,
        /// Dump the whole Redux store, scrubbed
        #[arg(long)]
        raw: bool,
        /// No indentation
        #[arg(long)]
        compact: bool,
        /// Write to a file instead of stdout
        #[arg(short = 'o', long, value_name = "FILE")]
        output: Option<String>,
    },
    /// Show the store's structure without values
    Inspect {
        /// Dotted path into the store, e.g. bootData
        key: Option<String>,
        #[arg(long, default_value_t = 2, allow_negative_numbers = true)]
        depth: i64,
    },
}

// ---------------------------------------------------------------------------
// Output helpers

struct Style(bool);

impl Style {
    fn wrap(&self, code: &str, text: &str) -> String {
        if self.0 {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
    fn dim(&self, text: &str) -> String {
        self.wrap("2", text)
    }
    fn bold(&self, text: &str) -> String {
        self.wrap("1", text)
    }
    fn cyan(&self, text: &str) -> String {
        self.wrap("36", text)
    }
    fn yellow(&self, text: &str) -> String {
        self.wrap("33", text)
    }
}

struct Ctx<'a> {
    out: &'a mut dyn Write,
    err: &'a mut dyn Write,
    term: &'a Term,
    style: Style,
    json: bool,
}

impl Ctx<'_> {
    fn line(&mut self, text: &str) -> io::Result<()> {
        writeln!(self.out, "{text}")
    }

    fn note(&mut self, text: &str) -> io::Result<()> {
        writeln!(self.err, "{text}")
    }

    fn emit_json(&mut self, value: &Value, emails: bool) -> io::Result<()> {
        let text = serde_json::to_string_pretty(&scrub(value, emails)).map_err(io::Error::other)?;
        writeln!(self.out, "{text}")
    }

    /// Print rows as aligned columns; the last column takes leftover width.
    fn table(&mut self, rows: &[Row], columns: &[(&str, &str)], emails: bool) -> io::Result<()> {
        if rows.is_empty() {
            let text = self.style.dim("(nothing cached)");
            return self.line(&text);
        }
        let cells: Vec<Vec<String>> = rows
            .iter()
            .map(|row| {
                columns
                    .iter()
                    .map(|(key, _)| scrub_text(&cell(row.get(*key)), emails))
                    .collect()
            })
            .collect();
        let last = columns.len() - 1;
        let mut widths: Vec<usize> = columns
            .iter()
            .enumerate()
            .map(|(i, (_, header))| {
                cells
                    .iter()
                    .map(|c| width(&c[i]))
                    .max()
                    .unwrap_or(0)
                    .max(width(header))
                    .min(40)
            })
            .collect();
        let used: usize = widths[..last].iter().sum::<usize>() + 2 * last;
        widths[last] = if self.term.tty {
            widths[last]
                .min(self.term.width.saturating_sub(used))
                .max(10)
        } else {
            usize::MAX
        };
        let line = |texts: Vec<String>| -> String {
            texts
                .iter()
                .zip(&widths)
                .enumerate()
                .map(|(i, (t, w))| {
                    if i == last {
                        clip(t, *w)
                    } else {
                        pad(&clip(t, *w), *w)
                    }
                })
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end()
                .to_string()
        };
        let header = line(columns.iter().map(|(_, h)| h.to_string()).collect());
        let header = self.style.bold(&header);
        self.line(&header)?;
        for row in cells {
            self.line(&line(row))?;
        }
        Ok(())
    }

    fn messages(&mut self, rows: &[Row], show_channel: bool) -> io::Result<()> {
        for row in rows {
            if truthy(row.get("gap_before")) {
                let text = self.style.dim("    ··· messages may be missing here ···");
                self.line(&text)?;
            }
            let when: String = text_of(row.get("time"))
                .replace('T', " ")
                .chars()
                .take(16)
                .collect();
            let mut prefix = self.style.dim(&when);
            if show_channel {
                let label = row.get("channel_label").or(row.get("channel"));
                prefix = format!("{prefix} {}", self.style.cyan(&text_of(label)));
            }
            let author = [text_of(row.get("author")), text_of(row.get("user"))]
                .into_iter()
                .find(|s| !s.is_empty())
                .unwrap_or_else(|| "?".to_string());
            let mut extras = Vec::new();
            let ts = text_of(row.get("ts"));
            let thread_ts = text_of(row.get("thread_ts"));
            if !thread_ts.is_empty() && thread_ts != ts {
                extras.push("↳ reply".to_string());
            }
            if truthy(row.get("reply_count")) {
                extras.push(format!("{} replies", cell(row.get("reply_count"))));
            }
            let files = strings(row.get("files"));
            if !files.is_empty() {
                extras.push(format!("files: {}", files.join(", ")));
            }
            if let Some(Value::Array(reactions)) = row.get("reactions") {
                if !reactions.is_empty() {
                    let parts: Vec<String> = reactions
                        .iter()
                        .map(|r| format!(":{}:×{}", cell(r.get("name")), cell(r.get("count"))))
                        .collect();
                    extras.push(parts.join(" "));
                }
            }
            if truthy(row.get("edited")) {
                extras.push("edited".to_string());
            }
            if truthy(row.get("ephemeral")) {
                extras.push("only visible to you".to_string());
            }
            let mut text = scrub_text(&text_of(row.get("text")), false);
            if text.is_empty() {
                let subtype = text_of(row.get("subtype"));
                let what = if subtype.is_empty() {
                    "no text".to_string()
                } else {
                    subtype
                };
                text = self.style.dim(&format!("({what})"));
            }
            let line = format!("{prefix} {}: {text}", self.style.bold(&author));
            self.line(&line)?;
            if !extras.is_empty() {
                let line = format!(
                    "    {}",
                    self.style.yellow(&scrub_text(&extras.join(" · "), false))
                );
                self.line(&line)?;
            }
        }
        Ok(())
    }
}

fn width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(1).max(1)).sum()
}

/// Collapse whitespace and cut to `limit` columns with an ellipsis.
fn clip(text: &str, limit: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if limit == 0 || width(&text) <= limit {
        return text;
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = width(c.encode_utf8(&mut [0; 4]));
        if used + w > limit - 1 {
            break;
        }
        out.push(c);
        used += w;
    }
    out + "…"
}

fn pad(text: &str, limit: usize) -> String {
    format!("{text}{}", " ".repeat(limit.saturating_sub(width(text))))
}

fn cell(value: Option<&Value>) -> String {
    match value {
        Some(Value::Bool(true)) => "yes".to_string(),
        None | Some(Value::Null) | Some(Value::Bool(false)) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(other) => other.to_string(),
    }
}

fn strings(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items.iter().map(|v| text_of(Some(v))).collect(),
        _ => Vec::new(),
    }
}

fn rows_value(rows: Vec<Row>) -> Value {
    Value::Array(rows.into_iter().map(Value::Object).collect())
}

/// Prefix rows with their workspace when several are shown.
fn with_team(snapshot: &Snapshot, rows: Vec<Row>, many: bool) -> Vec<Row> {
    if !many {
        return rows;
    }
    rows.into_iter()
        .map(|row| {
            let mut out = Map::new();
            out.insert("team".into(), snapshot.team_name().into());
            out.extend(row);
            out
        })
        .collect()
}

fn keep_last<T>(rows: &mut Vec<T>, limit: i64) {
    if let Ok(limit) = usize::try_from(limit) {
        if limit > 0 && rows.len() > limit {
            rows.drain(..rows.len() - limit);
        }
    }
}

fn no_channel(ctx: &mut Ctx, query: &str) -> io::Result<i32> {
    ctx.note(&format!(
        "No cached channel matches '{query}'. Try `slack-cli channels --all` to list known channels."
    ))?;
    Ok(1)
}

// ---------------------------------------------------------------------------
// Commands

fn cmd_teams(ctx: &mut Ctx, snapshots: &[Snapshot]) -> io::Result<i32> {
    let rows: Vec<Row> = snapshots.iter().map(Snapshot::summary).collect();
    if ctx.json {
        ctx.emit_json(&rows_value(rows), false)?;
        return Ok(0);
    }
    let columns = [
        ("team_id", "TEAM"),
        ("team_name", "NAME"),
        ("domain", "DOMAIN"),
        ("user_name", "ME"),
        ("channels", "CHANNELS"),
        ("members", "MEMBERS"),
        ("cached_messages", "CACHED MSGS"),
        ("snapshot_time", "SNAPSHOT"),
    ];
    ctx.table(&rows, &columns, false)?;
    Ok(0)
}

fn cmd_channels(
    ctx: &mut Ctx,
    snapshots: &[Snapshot],
    all: bool,
    cached: bool,
    kinds: &[Kind],
) -> io::Result<i32> {
    let many = snapshots.len() > 1;
    let mut rows = Vec::new();
    for snap in snapshots {
        let channels: Vec<Row> = snap
            .channels()
            .into_iter()
            .filter(|c| {
                let has_cache = truthy(c.get("cached_messages"));
                (all || truthy(c.get("is_member")) || has_cache)
                    && (!cached || has_cache)
                    && (kinds.is_empty()
                        || kinds.iter().any(|k| text_of(c.get("kind")) == k.as_str()))
            })
            .collect();
        rows.extend(with_team(snap, channels, many));
    }
    if ctx.json {
        ctx.emit_json(&rows_value(rows), false)?;
        return Ok(0);
    }
    let mut columns = vec![
        ("id", "ID"),
        ("label", "CHANNEL"),
        ("kind", "KIND"),
        ("cached_messages", "CACHED"),
        ("unread", "UNREAD"),
        ("topic", "TOPIC"),
    ];
    if many {
        columns.insert(0, ("team", "TEAM"));
    }
    ctx.table(&rows, &columns, false)?;
    Ok(0)
}

fn cmd_members(ctx: &mut Ctx, snapshots: &[Snapshot], all: bool, emails: bool) -> io::Result<i32> {
    let many = snapshots.len() > 1;
    let mut rows = Vec::new();
    for snap in snapshots {
        let members: Vec<Row> = snap
            .members(emails)
            .into_iter()
            .filter(|m| {
                all || !(truthy(m.get("deleted"))
                    || truthy(m.get("is_bot"))
                    || truthy(m.get("is_external")))
            })
            .collect();
        rows.extend(with_team(snap, members, many));
    }
    if ctx.json {
        ctx.emit_json(&rows_value(rows), emails)?;
        return Ok(0);
    }
    let mut columns = vec![
        ("id", "ID"),
        ("name", "HANDLE"),
        ("real_name", "NAME"),
        ("display_name", "DISPLAY"),
        ("title", "TITLE"),
    ];
    if all {
        columns.insert(1, ("is_external", "EXTERNAL"));
    }
    if emails {
        columns.push(("email", "EMAIL"));
    }
    if many {
        columns.insert(0, ("team", "TEAM"));
    }
    ctx.table(&rows, &columns, emails)?;
    Ok(0)
}

/// Compare the root's reply_count with the replies actually cached.
fn thread_hint(rows: &[Row], thread_ts: &str) -> String {
    let Some(root) = rows.iter().find(|r| text_of(r.get("ts")) == thread_ts) else {
        return " The thread's first message is not cached.".to_string();
    };
    let cached = rows
        .iter()
        .filter(|r| text_of(r.get("thread_ts")) == thread_ts && text_of(r.get("ts")) != thread_ts)
        .count() as i64;
    let total = root.get("reply_count").and_then(Value::as_i64).unwrap_or(0);
    let missing = total - cached;
    if missing > 0 {
        format!(" {missing} of {total} replies are not cached.")
    } else {
        String::new()
    }
}

fn cmd_history(
    ctx: &mut Ctx,
    snapshots: &[Snapshot],
    query: &str,
    limit: i64,
    thread: Option<&str>,
) -> io::Result<i32> {
    let matches: Vec<(&Snapshot, &Value)> = snapshots
        .iter()
        .filter_map(|s| s.resolve_channel(query).map(|c| (s, c)))
        .collect();
    let (snap, channel) = match matches.as_slice() {
        [] => return no_channel(ctx, query),
        [one] => *one,
        many => {
            let names: Vec<String> = many.iter().map(|(s, _)| s.team_name()).collect();
            ctx.note(&format!(
                "'{query}' exists in several workspaces ({}); pick one with --team.",
                names.join(", ")
            ))?;
            return Ok(1);
        }
    };
    let cid = text_of(channel.get("id"));
    let label = snap.channel_label(channel);
    let mut rows = snap.messages(&cid);
    match thread {
        Some(t) => rows.retain(|r| text_of(r.get("thread_ts")) == t || text_of(r.get("ts")) == t),
        // Match the channel view: replies stay in their thread.
        None => rows.retain(|r| !truthy(r.get("hidden_reply"))),
    }
    let state = if thread.is_none() {
        snap.mark_gaps(&cid, &mut rows)
    } else {
        None
    };
    let total = rows.len();
    keep_last(&mut rows, limit);
    let mut code = 0;
    if let (Some(t), 0) = (thread, total) {
        ctx.note(&format!("No cached message in thread '{t}' of {label}."))?;
        code = 1;
    }
    if ctx.json {
        let mut payload = Map::new();
        payload.insert("team".into(), snap.team_name().into());
        payload.insert("channel".into(), Value::Object(snap.channel_row(channel)));
        payload.insert("cached_total".into(), total.into());
        payload.insert(
            "reached_start".into(),
            state.as_ref().is_some_and(|s| s.reached_start).into(),
        );
        payload.insert(
            "reached_end".into(),
            state.as_ref().is_some_and(|s| s.reached_end).into(),
        );
        payload.insert("messages".into(), rows_value(rows));
        ctx.emit_json(&Value::Object(payload), false)?;
        return Ok(code);
    }
    if code != 0 {
        return Ok(code);
    }
    let header = ctx.style.bold(&format!("{label}  ({})", snap.team_name()));
    ctx.line(&header)?;
    ctx.messages(&rows, false)?;
    let hint = match thread {
        Some(t) => thread_hint(&snap.messages(&cid), t),
        None => {
            let mut missing = Vec::new();
            if !state.as_ref().is_some_and(|s| s.reached_start) {
                missing.push("older");
            }
            if state.as_ref().is_some_and(|s| !s.reached_end) {
                missing.push("newer");
            }
            if missing.is_empty() {
                String::new()
            } else {
                let which = missing.join(" and ");
                format!(
                    " {}{} messages may be missing.",
                    which[..1].to_uppercase(),
                    &which[1..]
                )
            }
        }
    };
    let note = ctx.style.dim(&format!(
        "Showing {} of {total} cached messages from Slack's local cache.{hint}",
        rows.len()
    ));
    ctx.note(&note)?;
    Ok(0)
}

fn cmd_search(
    ctx: &mut Ctx,
    snapshots: &[Snapshot],
    query: &str,
    channel: Option<&str>,
    user: Option<&str>,
    limit: i64,
) -> io::Result<i32> {
    let needle = query.to_lowercase();
    let mut hits: Vec<Row> = Vec::new();
    let mut channel_found = false;
    for snap in snapshots {
        let channel_filter = match channel {
            Some(q) => match snap.resolve_channel(q) {
                Some(raw) => {
                    channel_found = true;
                    Some(text_of(raw.get("id")))
                }
                None => continue,
            },
            None => None,
        };
        let wanted = user.map(|u| u.trim().trim_start_matches('@').to_lowercase());
        let user_ids = user.map(|u| snap.resolve_user_ids(u)).unwrap_or_default();
        for mut row in snap.all_messages() {
            let cid = text_of(row.get("channel"));
            if channel_filter.as_ref().is_some_and(|f| *f != cid) {
                continue;
            }
            if let Some(wanted) = &wanted {
                let by_id = user_ids.contains(&text_of(row.get("user")));
                if !by_id && text_of(row.get("author")).to_lowercase() != *wanted {
                    continue;
                }
            }
            let haystack = format!(
                "{}\n{}",
                text_of(row.get("text")),
                strings(row.get("files")).join(" ")
            );
            if haystack.to_lowercase().contains(&needle) {
                let label = match snap.channel_index().get(&cid) {
                    Some(raw) => snap.channel_label(raw),
                    None => cid.clone(),
                };
                row.insert("channel_label".into(), label.into());
                row.insert("team".into(), snap.team_name().into());
                hits.push(row);
            }
        }
    }
    hits.sort_by(|a, b| ts_key(a.get("ts")).total_cmp(&ts_key(b.get("ts"))));
    keep_last(&mut hits, limit);
    if let (Some(q), false) = (channel, channel_found) {
        return no_channel(ctx, q);
    }
    let found = !hits.is_empty();
    if ctx.json {
        ctx.emit_json(&rows_value(hits), false)?;
        return Ok(if found { 0 } else { 1 });
    }
    if !found {
        ctx.note("No cached message matches.")?;
        return Ok(1);
    }
    ctx.messages(&hits, true)?;
    let note = ctx
        .style
        .dim(&format!("{} matches in the local cache.", hits.len()));
    ctx.note(&note)?;
    Ok(0)
}

fn cmd_files(ctx: &mut Ctx, snapshots: &[Snapshot], limit: i64) -> io::Result<i32> {
    let many = snapshots.len() > 1;
    let mut rows: Vec<Row> = snapshots
        .iter()
        .flat_map(|s| with_team(s, s.files(), many))
        .collect();
    if let Ok(limit) = usize::try_from(limit) {
        if limit > 0 {
            rows.truncate(limit);
        }
    }
    if ctx.json {
        ctx.emit_json(&rows_value(rows), false)?;
        return Ok(0);
    }
    let mut columns = vec![
        ("id", "ID"),
        ("created", "CREATED"),
        ("author", "BY"),
        ("mimetype", "TYPE"),
        ("name", "NAME"),
    ];
    if many {
        columns.insert(0, ("team", "TEAM"));
    }
    ctx.table(&rows, &columns, false)?;
    Ok(0)
}

fn cmd_export(
    ctx: &mut Ctx,
    snapshots: &[Snapshot],
    emails: bool,
    raw: bool,
    compact: bool,
    output: Option<&str>,
) -> io::Result<i32> {
    let payload: Vec<Value> = snapshots
        .iter()
        .map(|snap| {
            let mut entry = Map::new();
            entry.insert("summary".into(), Value::Object(snap.summary()));
            if raw {
                entry.insert("store".into(), snap.store.clone());
            } else {
                entry.insert("channels".into(), rows_value(snap.channels()));
                entry.insert("members".into(), rows_value(snap.members(emails)));
                let messages: Map<String, Value> = snap
                    .channel_index()
                    .keys()
                    .filter(|cid| !snap.raw_messages(cid).is_empty())
                    .map(|cid| (cid.clone(), rows_value(snap.messages(cid))))
                    .collect();
                entry.insert("messages".into(), Value::Object(messages));
                entry.insert("files".into(), rows_value(snap.files()));
            }
            Value::Object(entry)
        })
        .collect();
    let data = scrub(&Value::Array(payload), emails);
    let text = if compact {
        serde_json::to_string(&data)
    } else {
        serde_json::to_string_pretty(&data)
    }
    .map_err(io::Error::other)?;
    match output {
        Some(path) => {
            std::fs::write(path, text + "\n")?;
            ctx.note(&format!("Wrote {path}"))?;
        }
        None => ctx.line(&text)?,
    }
    Ok(0)
}

fn cmd_inspect(
    ctx: &mut Ctx,
    snapshots: &[Snapshot],
    key: Option<&str>,
    depth: i64,
) -> io::Result<i32> {
    for snap in snapshots {
        let header = ctx
            .style
            .bold(&format!("{} ({})", snap.team_name(), snap.team_id()));
        ctx.line(&header)?;
        let mut value = &snap.store;
        if let Some(key) = key {
            for part in key.split('.') {
                match value.as_object().and_then(|o| o.get(part)) {
                    Some(inner) => value = inner,
                    None => {
                        ctx.note(&format!("No key '{key}' in this store."))?;
                        return Ok(1);
                    }
                }
            }
        }
        ctx.line(&shape(value, depth))?;
    }
    Ok(0)
}

/// Run the CLI with `args` (including the program name).
///
/// Returns the exit code. An `Err` is an output failure, such as a closed pipe.
pub fn run<I, S>(args: I, out: &mut dyn Write, err: &mut dyn Write, term: &Term) -> io::Result<i32>
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            let text = e.render().to_string();
            if e.use_stderr() {
                write!(err, "{text}")?;
            } else {
                write!(out, "{text}")?;
            }
            return Ok(e.exit_code());
        }
    };
    let snapshots = match load_snapshots(cli.data_dir.as_deref())
        .and_then(|s| select(s, cli.team.as_deref()))
    {
        Ok(snapshots) => snapshots,
        Err(e) => {
            writeln!(err, "{}", e.0)?;
            return Ok(2);
        }
    };
    let mut ctx = Ctx {
        out,
        err,
        term,
        style: Style(term.color && !cli.json),
        json: cli.json,
    };
    let snaps = snapshots.as_slice();
    match &cli.command {
        Command::Teams => cmd_teams(&mut ctx, snaps),
        Command::Channels { all, cached, kind } => {
            cmd_channels(&mut ctx, snaps, *all, *cached, kind)
        }
        Command::Members { all, emails } => cmd_members(&mut ctx, snaps, *all, *emails),
        Command::History {
            channel,
            limit,
            thread,
        } => cmd_history(&mut ctx, snaps, channel, *limit, thread.as_deref()),
        Command::Search {
            query,
            channel,
            user,
            limit,
        } => cmd_search(
            &mut ctx,
            snaps,
            query,
            channel.as_deref(),
            user.as_deref(),
            *limit,
        ),
        Command::Files { limit } => cmd_files(&mut ctx, snaps, *limit),
        Command::Export {
            emails,
            raw,
            compact,
            output,
        } => cmd_export(&mut ctx, snaps, *emails, *raw, *compact, output.as_deref()),
        Command::Inspect { key, depth } => cmd_inspect(&mut ctx, snaps, key.as_deref(), *depth),
    }
}
