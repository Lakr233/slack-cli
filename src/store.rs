//! Find and read Slack desktop's local Redux cache.
//!
//! Read-only: blob files are read straight from Slack's data directory and
//! nothing is written back. Slack renames blob files while it runs, so a
//! file that disappears between listing and reading is skipped.

use crate::decode::decode_bytes;
use chrono::{Local, LocalResult, SecondsFormat, TimeZone};
use indexmap::IndexMap;
use regex::{Captures, Regex};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::UNIX_EPOCH;

/// One output row: a JSON object with keys in a stable order.
pub type Row = Map<String, Value>;

pub const DATA_DIR_ENV: &str = "SLACK_CLI_DATA_DIR";

const CANDIDATE_DATA_DIRS: &[&str] = &[
    // Mac App Store build (sandboxed).
    "~/Library/Containers/com.tinyspeck.slackmacgap/Data/Library/Application Support/Slack",
    // Direct download / Homebrew build.
    "~/Library/Application Support/Slack",
];

const STORE_KEYS: &[&str] = &["bootData", "channels", "members", "messages", "teams"];

/// No usable Slack cache was found; the message says why.
#[derive(Debug)]
pub struct StoreNotFound(pub String);

static EMPTY: LazyLock<Value> = LazyLock::new(|| Value::Object(Map::new()));

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid regex")
}

// ---------------------------------------------------------------------------
// Helpers for loosely shaped Redux data

/// `value[key]` when `value` is an object.
pub fn get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.as_object()?.get(key)
}

/// Follow a path of object keys.
pub fn dig<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(value, |v, key| get(v, key))
}

fn is_blank(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

/// The first value that is present and not null, "", [] or {}.
fn first<'a>(values: &[Option<&'a Value>]) -> Option<&'a Value> {
    values.iter().flatten().copied().find(|v| !is_blank(v))
}

/// JavaScript-style truthiness.
pub fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// The value when truthy, otherwise `fallback`.
fn or(value: Option<&Value>, fallback: Value) -> Value {
    match value {
        Some(v) if truthy(Some(v)) => v.clone(),
        _ => fallback,
    }
}

fn or_empty(value: Option<&Value>) -> Value {
    or(value, Value::String(String::new()))
}

/// A scalar as text; anything else is "".
pub fn text_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

/// Topic and purpose come either as strings or as {"value": ...}.
fn as_text(value: Option<&Value>) -> String {
    let value = match value {
        Some(Value::Object(o)) => o.get("value"),
        other => other,
    };
    value.and_then(Value::as_str).unwrap_or("").to_string()
}

/// Record objects from a dict-by-id or a list slice.
fn records(value: Option<&Value>) -> Vec<&Value> {
    match value {
        Some(Value::Object(o)) => o.values().filter(|v| v.is_object()).collect(),
        Some(Value::Array(a)) => a.iter().filter(|v| v.is_object()).collect(),
        _ => Vec::new(),
    }
}

static TS_PREFIX: LazyLock<Regex> = LazyLock::new(|| regex(r"^\s*(\d+(?:\.\d+)?)"));

/// A Slack timestamp as seconds; 0 when it has none.
pub fn ts_key(ts: Option<&Value>) -> f64 {
    let seconds = match ts {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok().or_else(|| {
            // Client-side ephemeral messages use ts such as "1700000350.xxxx13".
            TS_PREFIX.captures(s).and_then(|c| c[1].parse().ok())
        }),
        _ => None,
    };
    seconds.filter(|s| s.is_finite()).unwrap_or(0.0)
}

fn local_iso(seconds: f64) -> String {
    let whole = seconds.floor();
    let nanos = ((seconds - whole) * 1e9) as u32;
    match Local.timestamp_opt(whole as i64, nanos) {
        LocalResult::Single(t) | LocalResult::Ambiguous(t, _) => {
            t.to_rfc3339_opts(SecondsFormat::Secs, false)
        }
        LocalResult::None => String::new(),
    }
}

/// A Slack timestamp as local ISO 8601 time, or "".
pub fn ts_to_iso(ts: Option<&Value>) -> String {
    let seconds = ts_key(ts);
    if seconds == 0.0 {
        return String::new();
    }
    local_iso(seconds)
}

// Subtypes whose `edited` field is Slack bookkeeping, not a user edit.
static SYSTEM_SUBTYPE: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^(huddle|channel_|group_|pinned_|bot_add|bot_remove)"));
static MARKUP: LazyLock<Regex> = LazyLock::new(|| regex(r"<([^<>]+)>"));
static LINK_SCHEME: LazyLock<Regex> = LazyLock::new(|| regex(r"^(mailto:|https?://)"));

/// Readable text from Block Kit blocks, in order.
fn blocks_text(value: Option<&Value>, out: &mut Vec<String>) {
    let Some(value) = value else { return };
    if let Value::Array(items) = value {
        for item in items {
            blocks_text(Some(item), out);
        }
        return;
    }
    let Value::Object(block) = value else { return };
    let kind = block.get("type").and_then(Value::as_str);
    let text = block.get("text");
    let field = |key: &str| {
        block
            .get(key)
            .filter(|v| truthy(Some(v)))
            .map(|v| text_of(Some(v)))
    };
    match (
        kind,
        field("user_id"),
        field("channel_id"),
        field("url"),
        field("name"),
    ) {
        (Some("user"), Some(id), ..) => out.push(format!("<@{id}>")),
        (Some("channel"), _, Some(id), ..) => out.push(format!("<#{id}>")),
        (Some("link"), _, _, Some(url), _) => match text.and_then(Value::as_str) {
            Some(label) if !label.is_empty() => out.push(format!("<{url}|{label}>")),
            _ => out.push(format!("<{url}>")),
        },
        (Some("emoji"), _, _, _, Some(name)) => out.push(format!(":{name}:")),
        _ => match text {
            Some(Value::String(s))
                if matches!(kind, None | Some("text" | "mrkdwn" | "plain_text")) =>
            {
                out.push(s.clone())
            }
            Some(inner @ Value::Object(_)) => blocks_text(Some(inner), out),
            _ => {}
        },
    }
    for key in ["elements", "fields", "blocks"] {
        if let Some(inner) = block.get(key) {
            blocks_text(Some(inner), out);
        }
    }
}

fn attachments_text(value: Option<&Value>) -> String {
    records(value)
        .into_iter()
        .filter_map(|item| {
            first(&[
                get(item, "text"),
                get(item, "fallback"),
                get(item, "title"),
                get(item, "pretext"),
            ])
            .and_then(Value::as_str)
            .map(str::to_string)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Messages are usually {ts: msg}; accept lists and one extra level.
fn message_dicts<'a>(container: Option<&'a Value>, depth: usize, out: &mut Vec<&'a Value>) {
    for item in records(container) {
        if truthy(get(item, "isNonExistent")) {
            continue; // placeholder Slack keeps for ids it has not fetched
        }
        let has = |key: &str| get(item, key).is_some();
        if has("ts") && (has("text") || has("type") || has("user")) {
            out.push(item);
        } else if depth < 2 {
            message_dicts(Some(item), depth + 1, out);
        }
    }
}

/// Which cached messages Slack holds as contiguous runs of history.
pub struct HistoryState {
    slice_of: HashMap<String, usize>,
    pub reached_start: bool,
    pub reached_end: bool,
}

// ---------------------------------------------------------------------------
// Snapshot model

/// One workspace's decoded Redux store.
pub struct Snapshot {
    pub store: Value,
    pub source: String,
    pub mtime: f64,
    team_id: String,
    user_id: String,
    member_index: IndexMap<String, Value>,
    channel_index: IndexMap<String, Value>,
}

impl Snapshot {
    pub fn new(store: Value, source: String, mtime: f64) -> Self {
        let mut snapshot = Snapshot {
            store,
            source,
            mtime,
            team_id: String::new(),
            user_id: String::new(),
            member_index: IndexMap::new(),
            channel_index: IndexMap::new(),
        };
        snapshot.team_id = snapshot.find_team_id();
        snapshot.user_id = snapshot.find_user_id();
        snapshot.member_index = snapshot.build_member_index();
        snapshot.channel_index = snapshot.build_channel_index();
        snapshot
    }

    fn slice(&self, key: &str) -> Option<&Value> {
        get(&self.store, key)
    }

    fn boot(&self) -> &Value {
        self.slice("bootData")
            .filter(|v| v.is_object())
            .unwrap_or(&EMPTY)
    }

    // -- identity -----------------------------------------------------------

    fn find_team_id(&self) -> String {
        let boot = self.boot();
        // Real snapshots carry it in selfTeamIds; `teams` also lists the
        // other signed-in workspaces, so its first entry is only a last resort.
        let teams = records(self.slice("teams"));
        text_of(first(&[
            get(boot, "team_id"),
            dig(boot, &["team", "id"]),
            dig(boot, &["client", "team_id"]),
            dig(&self.store, &["selfTeamIds", "teamId"]),
            dig(&self.store, &["selfTeamIds", "defaultWorkspaceId"]),
            self.slice("selfTeamId"),
            teams.first().and_then(|t| get(t, "id")),
        ]))
    }

    fn find_user_id(&self) -> String {
        let boot = self.boot();
        text_of(first(&[
            get(boot, "user_id"),
            dig(boot, &["self", "id"]),
            dig(boot, &["client", "user_id"]),
            self.slice("selfId"),
        ]))
    }

    pub fn team_id(&self) -> &str {
        &self.team_id
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    fn team_record(&self) -> &Value {
        let teams = records(self.slice("teams"));
        if !self.team_id.is_empty() {
            if let Some(team) = teams.iter().find(|t| text_of(get(t, "id")) == self.team_id) {
                return team;
            }
        }
        if let Some(team) = get(self.boot(), "team").filter(|t| t.is_object()) {
            return team;
        }
        teams.first().copied().unwrap_or(&EMPTY)
    }

    pub fn team_name(&self) -> String {
        let team = self.team_record();
        let name = text_of(first(&[get(team, "name"), get(team, "domain")]));
        if name.is_empty() {
            self.team_id.clone()
        } else {
            name
        }
    }

    pub fn team_domain(&self) -> String {
        text_of(get(self.team_record(), "domain"))
    }

    /// Only this snapshot's own workspace: `teams` also lists every other
    /// signed-in workspace and Slack Connect partners.
    pub fn matches_team(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        [self.team_id.clone(), self.team_name(), self.team_domain()]
            .iter()
            .any(|n| !n.is_empty() && n.to_lowercase() == query)
    }

    // -- members ------------------------------------------------------------

    fn build_member_index(&self) -> IndexMap<String, Value> {
        records(self.slice("members"))
            .into_iter()
            .filter(|raw| truthy(get(raw, "id")) && !truthy(get(raw, "isNonExistent")))
            .map(|raw| (text_of(get(raw, "id")), raw.clone()))
            .collect()
    }

    fn profile(raw: &Value) -> &Value {
        get(raw, "profile")
            .filter(|p| p.is_object())
            .unwrap_or(&EMPTY)
    }

    pub fn member_name(&self, user_id: &str) -> String {
        if user_id.is_empty() {
            return String::new();
        }
        let Some(raw) = self.member_index.get(user_id) else {
            return user_id.to_string();
        };
        let profile = Self::profile(raw);
        let name = text_of(first(&[
            get(profile, "display_name"),
            get(profile, "real_name"),
            get(raw, "real_name"),
            get(raw, "name"),
        ]));
        if name.is_empty() {
            user_id.to_string()
        } else {
            name
        }
    }

    /// Slack Connect members from another organisation.
    pub fn is_external(&self, raw: &Value) -> bool {
        let team = text_of(get(raw, "team_id"));
        truthy(get(raw, "isExternal"))
            || (!team.is_empty() && !self.team_id.is_empty() && team != self.team_id)
    }

    /// Ids of members whose id, handle, real or display name match.
    pub fn resolve_user_ids(&self, query: &str) -> HashSet<String> {
        let wanted = query.trim().trim_start_matches('@').to_lowercase();
        let mut ids = HashSet::new();
        for (uid, raw) in &self.member_index {
            let profile = Self::profile(raw);
            let names = [
                get(raw, "name"),
                get(raw, "real_name"),
                get(profile, "display_name"),
                get(profile, "real_name"),
            ];
            let matches = uid.to_lowercase() == wanted
                || names
                    .iter()
                    .any(|n| truthy(*n) && text_of(*n).to_lowercase() == wanted);
            if matches {
                ids.insert(uid.clone());
            }
        }
        ids
    }

    pub fn members(&self, include_email: bool) -> Vec<Row> {
        let mut rows: Vec<Row> = self
            .member_index
            .values()
            .map(|raw| {
                let profile = Self::profile(raw);
                let mut row = Row::new();
                row.insert("id".into(), get(raw, "id").cloned().unwrap_or(Value::Null));
                row.insert("team_id".into(), or_empty(get(raw, "team_id")));
                row.insert("is_external".into(), self.is_external(raw).into());
                row.insert("name".into(), or_empty(get(raw, "name")));
                row.insert(
                    "real_name".into(),
                    first(&[get(raw, "real_name"), get(profile, "real_name")])
                        .cloned()
                        .unwrap_or_else(|| "".into()),
                );
                row.insert(
                    "display_name".into(),
                    or_empty(get(profile, "display_name")),
                );
                row.insert("title".into(), or_empty(get(profile, "title")));
                row.insert("tz".into(), or_empty(get(raw, "tz")));
                row.insert("is_bot".into(), truthy(get(raw, "is_bot")).into());
                row.insert("deleted".into(), truthy(get(raw, "deleted")).into());
                if include_email {
                    row.insert(
                        "email".into(),
                        first(&[get(profile, "email"), get(raw, "_email_normalized_lc")])
                            .cloned()
                            .unwrap_or_else(|| "".into()),
                    );
                }
                row
            })
            .collect();
        let flag = |row: &Row, key: &str| truthy(row.get(key));
        rows.sort_by_key(|r| {
            (
                flag(r, "deleted"),
                flag(r, "is_external"),
                flag(r, "is_bot"),
                text_of(r.get("name")).to_lowercase(),
            )
        });
        rows
    }

    // -- channels -----------------------------------------------------------

    fn build_channel_index(&self) -> IndexMap<String, Value> {
        let mut index: IndexMap<String, Value> = records(self.slice("channels"))
            .into_iter()
            // Skip placeholders Slack keeps for ids it has not fetched.
            .filter(|raw| truthy(get(raw, "id")) && !truthy(get(raw, "isNonExistent")))
            .map(|raw| (text_of(get(raw, "id")), raw.clone()))
            .collect();
        // Channels that only appear in the message cache still count.
        if let Some(Value::Object(messages)) = self.slice("messages") {
            for cid in messages.keys() {
                index.entry(cid.clone()).or_insert_with(|| {
                    let mut stub = Map::new();
                    stub.insert("id".into(), Value::String(cid.clone()));
                    Value::Object(stub)
                });
            }
        }
        index
    }

    pub fn channel_index(&self) -> &IndexMap<String, Value> {
        &self.channel_index
    }

    pub fn channel_kind(&self, raw: &Value) -> &'static str {
        if truthy(get(raw, "is_im")) || text_of(get(raw, "id")).starts_with('D') {
            "im"
        } else if truthy(get(raw, "is_mpim")) {
            "mpim"
        } else if truthy(get(raw, "is_private")) || truthy(get(raw, "is_group")) {
            "private"
        } else {
            "public"
        }
    }

    pub fn channel_label(&self, raw: &Value) -> String {
        let kind = self.channel_kind(raw);
        let id = text_of(get(raw, "id"));
        if kind == "im" {
            return match get(raw, "user").filter(|u| truthy(Some(u))) {
                Some(user) => format!("@{}", self.member_name(&text_of(Some(user)))),
                None => id,
            };
        }
        if kind == "mpim" {
            let others: Vec<String> = self
                .channel_members(raw)
                .into_iter()
                .filter(|m| *m != self.user_id)
                .map(|m| format!("@{}", self.member_name(&m)))
                .collect();
            if !others.is_empty() {
                return others.join(", ");
            }
        }
        let name = text_of(first(&[get(raw, "name"), get(raw, "name_normalized")]));
        if name.is_empty() {
            id
        } else {
            format!("#{name}")
        }
    }

    /// Member ids: the channel's own list, else the membership slice.
    pub fn channel_members(&self, raw: &Value) -> Vec<String> {
        if let Some(Value::Array(members)) = get(raw, "members") {
            return members
                .iter()
                .filter_map(|m| m.as_str().map(str::to_string))
                .collect();
        }
        let cid = text_of(get(raw, "id"));
        match dig(&self.store, &["membership", &cid]) {
            Some(Value::Object(known)) => known
                .iter()
                .filter(|(_, state)| match state {
                    Value::Object(s) => s.get("isMember").map_or(true, |m| truthy(Some(m))),
                    _ => true,
                })
                .map(|(uid, _)| uid.clone())
                .collect(),
            _ => Vec::new(),
        }
    }

    pub fn channel_row(&self, raw: &Value) -> Row {
        let cid = text_of(get(raw, "id"));
        let kind = self.channel_kind(raw);
        let mut row = Row::new();
        row.insert("id".into(), get(raw, "id").cloned().unwrap_or(Value::Null));
        row.insert("name".into(), or_empty(get(raw, "name")));
        row.insert("label".into(), self.channel_label(raw).into());
        row.insert("kind".into(), kind.into());
        row.insert(
            "is_member".into(),
            (truthy(get(raw, "is_member")) || matches!(kind, "im" | "mpim")).into(),
        );
        row.insert("is_archived".into(), truthy(get(raw, "is_archived")).into());
        row.insert("topic".into(), as_text(get(raw, "topic")).into());
        row.insert("purpose".into(), as_text(get(raw, "purpose")).into());
        row.insert("im_user".into(), or_empty(get(raw, "user")));
        row.insert(
            "cached_messages".into(),
            self.raw_messages(&cid).len().into(),
        );
        row.insert("unread".into(), self.unread_count(&cid).into());
        row
    }

    pub fn channels(&self) -> Vec<Row> {
        let mut rows: Vec<Row> = self
            .channel_index
            .values()
            .map(|raw| self.channel_row(raw))
            .collect();
        let order = |kind: &str| match kind {
            "public" => 0,
            "private" => 1,
            "mpim" => 2,
            _ => 3,
        };
        rows.sort_by_key(|r| {
            (
                order(&text_of(r.get("kind"))),
                text_of(r.get("label")).to_lowercase(),
            )
        });
        rows
    }

    pub fn resolve_channel(&self, query: &str) -> Option<&Value> {
        let q = query.trim();
        if let Some(raw) = self.channel_index.get(q) {
            return Some(raw);
        }
        let bare = q.trim_start_matches('#').to_lowercase();
        if !bare.is_empty() {
            let by_name = self
                .channel_index
                .values()
                .find(|raw| text_of(get(raw, "name")).to_lowercase() == bare);
            if by_name.is_some() {
                return by_name;
            }
        }
        let by_label = self
            .channel_index
            .values()
            .find(|raw| self.channel_label(raw).to_lowercase() == q.to_lowercase());
        if by_label.is_some() {
            return by_label;
        }
        if q.starts_with('@') {
            let ids = self.resolve_user_ids(q);
            return self.channel_index.values().find(|raw| {
                self.channel_kind(raw) == "im" && ids.contains(&text_of(get(raw, "user")))
            });
        }
        None
    }

    pub fn unread_count(&self, cid: &str) -> i64 {
        let value = dig(&self.store, &["unreadChannelCounts", cid]);
        let value = match value {
            Some(v @ Value::Object(_)) => first(&[
                get(v, "unreadCnt"),
                get(v, "unread_count"),
                get(v, "count"),
                get(v, "unreads"),
            ]),
            other => other,
        };
        value.and_then(Value::as_i64).unwrap_or(0)
    }

    // -- messages -----------------------------------------------------------

    pub fn raw_messages(&self, cid: &str) -> Vec<&Value> {
        let mut found = Vec::new();
        if cid.is_empty() {
            return found;
        }
        message_dicts(dig(&self.store, &["messages", cid]), 0, &mut found);
        found.sort_by(|a, b| ts_key(get(a, "ts")).total_cmp(&ts_key(get(b, "ts"))));
        found
    }

    pub fn history_state(&self, cid: &str) -> Option<HistoryState> {
        let raw = dig(&self.store, &["channelHistory", cid]).filter(|v| v.is_object())?;
        let mut slice_of = HashMap::new();
        for (i, piece) in records(get(raw, "slices")).into_iter().enumerate() {
            if let Some(Value::Array(stamps)) = get(piece, "timestamps") {
                for ts in stamps {
                    slice_of.entry(text_of(Some(ts))).or_insert(i);
                }
            }
        }
        Some(HistoryState {
            slice_of,
            reached_start: truthy(get(raw, "reachedStart")),
            reached_end: truthy(get(raw, "reachedEnd")),
        })
    }

    /// Set `gap_before` where messages may be missing before a row.
    ///
    /// Rows must be one channel's visible messages, oldest first. Two
    /// neighbours are contiguous only when both sit in the same slice.
    pub fn mark_gaps(&self, cid: &str, rows: &mut [Row]) -> Option<HistoryState> {
        let state = self.history_state(cid);
        let mut previous: Option<usize> = None;
        for (i, row) in rows.iter_mut().enumerate() {
            let here = state
                .as_ref()
                .and_then(|s| s.slice_of.get(&text_of(row.get("ts"))).copied());
            let gap = state.is_some()
                && i > 0
                && (here.is_none() || previous.is_none() || here != previous);
            row.insert("gap_before".into(), gap.into());
            previous = here;
        }
        state
    }

    /// Message files are either full objects or ids into the files slice.
    fn message_files(&self, raw: &Value) -> Vec<String> {
        let Some(Value::Array(items)) = get(raw, "files") else {
            return Vec::new();
        };
        items
            .iter()
            .filter_map(|item| {
                let item = match item {
                    Value::String(id) => match dig(&self.store, &["files", id]) {
                        Some(file) if truthy(Some(file)) => file.clone(),
                        _ => {
                            let mut stub = Map::new();
                            stub.insert("id".into(), Value::String(id.clone()));
                            Value::Object(stub)
                        }
                    },
                    other => other.clone(),
                };
                let name = text_of(first(&[
                    get(&item, "name"),
                    get(&item, "title"),
                    get(&item, "id"),
                ]));
                (!name.is_empty()).then_some(name)
            })
            .collect()
    }

    /// Reactions live inline or in the top-level slice under `_rxn_key`.
    fn message_reactions(&self, raw: &Value) -> Vec<Value> {
        let mut found = get(raw, "reactions");
        if !truthy(found) && truthy(get(raw, "_rxn_key")) {
            found = dig(&self.store, &["reactions", &text_of(get(raw, "_rxn_key"))]);
        }
        records(found)
            .into_iter()
            .map(|r| {
                let count = match get(r, "count") {
                    Some(c) => c.clone(),
                    None => get(r, "users")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len)
                        .into(),
                };
                let mut row = Map::new();
                row.insert(
                    "name".into(),
                    get(r, "name").cloned().unwrap_or(Value::Null),
                );
                row.insert("count".into(), count);
                Value::Object(row)
            })
            .collect()
    }

    fn bot_name(&self, bot_id: &str) -> String {
        if bot_id.is_empty() {
            return String::new();
        }
        text_of(dig(&self.store, &["bots", bot_id, "name"]))
    }

    pub fn message_author(&self, raw: &Value) -> String {
        let uid = text_of(get(raw, "user").filter(|u| truthy(Some(u))));
        let bot_id = get(raw, "bot_id").filter(|b| truthy(Some(b)));
        // Integrations such as Email post as USLACKBOT with their own bot id;
        // Slack shows the integration's name.
        let integration = uid == "USLACKBOT" && bot_id.is_some();
        if !uid.is_empty() && !integration {
            return self.member_name(&uid);
        }
        [
            text_of(first(&[get(raw, "username")])),
            text_of(first(&[dig(raw, &["bot_profile", "name"])])),
            self.bot_name(&text_of(bot_id)),
            self.member_name(&uid),
            text_of(bot_id),
        ]
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or_default()
    }

    pub fn message_row(&self, cid: &str, raw: &Value) -> Row {
        let text = get(raw, "text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // Some messages carry everything in blocks or attachments.
        let shown = if !text.is_empty() {
            text.clone()
        } else {
            let mut parts = Vec::new();
            blocks_text(get(raw, "blocks"), &mut parts);
            let joined = parts.concat().trim().to_string();
            if joined.is_empty() {
                attachments_text(get(raw, "attachments"))
            } else {
                joined
            }
        };
        let subtype = or_empty(get(raw, "subtype"));
        let edited =
            truthy(get(raw, "edited")) && !SYSTEM_SUBTYPE.is_match(&text_of(Some(&subtype)));
        let user = or(get(raw, "user"), or_empty(get(raw, "bot_id")));
        let mut row = Row::new();
        row.insert("channel".into(), cid.into());
        row.insert("ts".into(), get(raw, "ts").cloned().unwrap_or(Value::Null));
        row.insert("time".into(), ts_to_iso(get(raw, "ts")).into());
        row.insert("user".into(), user);
        row.insert("author".into(), self.message_author(raw).into());
        row.insert("subtype".into(), subtype);
        row.insert("text".into(), self.render_text(&shown).into());
        row.insert("raw_text".into(), text.into());
        row.insert("thread_ts".into(), or_empty(get(raw, "thread_ts")));
        row.insert("reply_count".into(), or(get(raw, "reply_count"), 0.into()));
        // Thread replies not sent to the channel; Slack hides them there.
        row.insert(
            "hidden_reply".into(),
            truthy(get(raw, "_hidden_reply")).into(),
        );
        // "Only visible to you" notices, such as Slackbot's replies.
        row.insert("ephemeral".into(), truthy(get(raw, "is_ephemeral")).into());
        row.insert("files".into(), self.message_files(raw).into());
        row.insert("reactions".into(), self.message_reactions(raw).into());
        row.insert("edited".into(), edited.into());
        row
    }

    pub fn messages(&self, cid: &str) -> Vec<Row> {
        self.raw_messages(cid)
            .into_iter()
            .map(|raw| self.message_row(cid, raw))
            .collect()
    }

    pub fn all_messages(&self) -> Vec<Row> {
        match self.slice("messages") {
            Some(Value::Object(messages)) => {
                messages.keys().flat_map(|cid| self.messages(cid)).collect()
            }
            _ => Vec::new(),
        }
    }

    /// Turn Slack mrkdwn references into readable text.
    pub fn render_text(&self, text: &str) -> String {
        let rendered = MARKUP.replace_all(text, |c: &Captures| {
            let body = &c[1];
            let (target, label) = body.split_once('|').unwrap_or((body, ""));
            if let Some(uid) = target.strip_prefix('@') {
                let name = if label.is_empty() {
                    self.member_name(uid)
                } else {
                    label.to_string()
                };
                return format!("@{name}");
            }
            if let Some(cid) = target.strip_prefix('#') {
                if !label.is_empty() {
                    return format!("#{label}");
                }
                return match self.channel_index.get(cid) {
                    Some(raw) => self.channel_label(raw),
                    None => target.to_string(),
                };
            }
            if target.starts_with("!subteam^") {
                return if label.is_empty() {
                    "@group".to_string()
                } else {
                    label.to_string()
                };
            }
            if let Some(special) = target.strip_prefix('!') {
                let name = if label.is_empty() {
                    special.split('^').next().unwrap_or("")
                } else {
                    label
                };
                return format!("@{name}");
            }
            let bare = LINK_SCHEME.replace(target, "");
            let bare = bare.trim_end_matches('/');
            if !label.is_empty() && label != target && label != bare {
                return format!("{label} ({target})");
            }
            if label.is_empty() {
                target.to_string()
            } else {
                label.to_string()
            }
        });
        rendered
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&")
    }

    // -- files --------------------------------------------------------------

    pub fn files(&self) -> Vec<Row> {
        let mut rows: Vec<Row> = records(self.slice("files"))
            .into_iter()
            .map(|raw| {
                let mut row = Row::new();
                row.insert("id".into(), get(raw, "id").cloned().unwrap_or(Value::Null));
                row.insert("name".into(), or_empty(get(raw, "name")));
                row.insert("title".into(), or_empty(get(raw, "title")));
                row.insert("mimetype".into(), or_empty(get(raw, "mimetype")));
                row.insert("size".into(), or(get(raw, "size"), 0.into()));
                row.insert("user".into(), or_empty(get(raw, "user")));
                row.insert(
                    "author".into(),
                    self.member_name(&text_of(get(raw, "user"))).into(),
                );
                row.insert("created".into(), ts_to_iso(get(raw, "created")).into());
                row.insert("permalink".into(), or_empty(get(raw, "permalink")));
                row
            })
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(text_of(r.get("created"))));
        rows
    }

    pub fn summary(&self) -> Row {
        let external = self
            .member_index
            .values()
            .filter(|raw| self.is_external(raw))
            .count();
        let cached: Vec<usize> = self
            .channel_index
            .keys()
            .map(|cid| self.raw_messages(cid).len())
            .collect();
        let mut row = Row::new();
        row.insert("team_id".into(), self.team_id.clone().into());
        row.insert("team_name".into(), self.team_name().into());
        row.insert("domain".into(), self.team_domain().into());
        row.insert("user_id".into(), self.user_id.clone().into());
        row.insert("user_name".into(), self.member_name(&self.user_id).into());
        row.insert("channels".into(), self.channel_index.len().into());
        row.insert(
            "members".into(),
            (self.member_index.len() - external).into(),
        );
        row.insert("external_members".into(), external.into());
        row.insert(
            "cached_channels".into(),
            cached.iter().filter(|&&n| n > 0).count().into(),
        );
        row.insert(
            "cached_messages".into(),
            cached.iter().sum::<usize>().into(),
        );
        row.insert("snapshot_time".into(), local_iso(self.mtime).into());
        row.insert("source".into(), self.source.clone().into());
        row
    }
}

// ---------------------------------------------------------------------------
// Loading

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => Path::new(&home).join(rest),
        _ => PathBuf::from(path),
    }
}

pub fn candidate_data_dirs(explicit: Option<&str>) -> Vec<PathBuf> {
    let from_env = std::env::var(DATA_DIR_ENV).ok().filter(|s| !s.is_empty());
    match explicit.map(str::to_string).or(from_env) {
        Some(dir) => vec![expand_home(&dir)],
        None => CANDIDATE_DATA_DIRS.iter().map(|p| expand_home(p)).collect(),
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// IndexedDB blob directories for app.slack.com under a data dir.
pub fn find_blob_dirs(data_dir: &Path) -> Vec<PathBuf> {
    let name = file_name(data_dir);
    if name.ends_with(".indexeddb.blob") {
        return if data_dir.is_dir() {
            vec![data_dir.to_path_buf()]
        } else {
            Vec::new()
        };
    }
    let indexed_db = if name == "IndexedDB" {
        data_dir.to_path_buf()
    } else {
        data_dir.join("IndexedDB")
    };
    let Ok(entries) = fs::read_dir(&indexed_db) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let name = file_name(p);
            p.is_dir() && name.ends_with(".indexeddb.blob") && name.contains("slack")
        })
        .collect();
    dirs.sort();
    dirs
}

fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            walk_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn looks_like_store(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|o| STORE_KEYS.iter().filter(|k| o.contains_key(**k)).count() >= 2)
}

fn try_decode(path: &Path) -> Option<Snapshot> {
    let meta = fs::metadata(path).ok()?;
    if meta.len() < 16 {
        return None;
    }
    // The file may be renamed away by Slack at any moment; skip it then.
    let data = fs::read(path).ok()?;
    let value = decode_bytes(&data).ok()?;
    if !looks_like_store(&value) {
        return None;
    }
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0.0, |d| d.as_secs_f64());
    Some(Snapshot::new(value, path.display().to_string(), mtime))
}

/// Decode the newest snapshot of every workspace that can be found.
pub fn load_snapshots(data_dir: Option<&str>) -> Result<Vec<Snapshot>, StoreNotFound> {
    let searched = candidate_data_dirs(data_dir);
    let blob_dirs: Vec<PathBuf> = searched.iter().flat_map(|d| find_blob_dirs(d)).collect();
    if blob_dirs.is_empty() {
        let where_ = searched
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n  ");
        return Err(StoreNotFound(format!(
            "No Slack IndexedDB cache found. Looked in:\n  {where_}\n\
             Open the Slack desktop app and sign in first, or pass --data-dir."
        )));
    }
    let mut best: IndexMap<(String, String), Snapshot> = IndexMap::new();
    for blob_dir in &blob_dirs {
        let mut files = Vec::new();
        walk_files(blob_dir, &mut files);
        for path in files {
            let Some(snapshot) = try_decode(&path) else {
                continue;
            };
            let key = (snapshot.team_id.clone(), snapshot.user_id.clone());
            if best.get(&key).map_or(true, |b| snapshot.mtime > b.mtime) {
                best.insert(key, snapshot);
            }
        }
    }
    if best.is_empty() {
        return Err(StoreNotFound(
            "Found Slack's IndexedDB directory but no Redux snapshot in it. \
             Open Slack once so it writes its cache, then try again."
                .to_string(),
        ));
    }
    let mut snapshots: Vec<Snapshot> = best.into_values().collect();
    snapshots.sort_by_key(|s| s.team_name().to_lowercase());
    Ok(snapshots)
}

/// Keep only the snapshots whose workspace matches `team`.
pub fn select(
    snapshots: Vec<Snapshot>,
    team: Option<&str>,
) -> Result<Vec<Snapshot>, StoreNotFound> {
    let Some(team) = team else {
        return Ok(snapshots);
    };
    let known = snapshots
        .iter()
        .map(|s| format!("{} ({})", s.team_name(), s.team_id))
        .collect::<Vec<_>>()
        .join(", ");
    let chosen: Vec<Snapshot> = snapshots
        .into_iter()
        .filter(|s| s.matches_team(team))
        .collect();
    if chosen.is_empty() {
        return Err(StoreNotFound(format!(
            "No cached workspace matches '{team}'. Known: {known}"
        )));
    }
    Ok(chosen)
}
