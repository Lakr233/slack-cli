//! Build synthetic Slack IndexedDB blobs for tests.
#![allow(dead_code)]

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

pub fn varint(mut n: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let byte = (n & 0x7F) as u8;
        n >>= 7;
        if n == 0 {
            out.push(byte);
            return out;
        }
        out.push(byte | 0x80);
    }
}

/// Tiny V8 ValueSerializer writer, enough for plain JSON-like data.
pub fn v8_encode(value: &Value) -> Vec<u8> {
    let mut out = vec![0xFF, 0x0F];
    write_value(&mut out, value);
    out
}

fn write_value(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => out.push(b'0'),
        Value::Bool(true) => out.push(b'T'),
        Value::Bool(false) => out.push(b'F'),
        Value::Number(n) => match n.as_i64().filter(|i| i32::try_from(*i).is_ok()) {
            Some(i) => {
                out.push(b'I');
                out.extend(varint(((i << 1) ^ (i >> 63)) as u64));
            }
            None => {
                out.push(b'N');
                out.extend(n.as_f64().unwrap().to_le_bytes());
            }
        },
        Value::String(s) => {
            if s.chars().all(|c| (c as u32) < 256) {
                out.push(b'"');
                out.extend(varint(s.chars().count() as u64));
                out.extend(s.chars().map(|c| c as u8));
            } else {
                let raw: Vec<u8> = s.encode_utf16().flat_map(u16::to_le_bytes).collect();
                let header = varint(raw.len() as u64);
                if (out.len() + 1 + header.len()) % 2 == 1 {
                    out.push(0x00); // V8 aligns two-byte strings
                }
                out.push(b'c');
                out.extend(header);
                out.extend(raw);
            }
        }
        Value::Object(map) => {
            out.push(b'o');
            for (key, item) in map {
                write_value(out, &Value::String(key.clone()));
                write_value(out, item);
            }
            out.push(b'{');
            out.extend(varint(map.len() as u64));
        }
        Value::Array(items) => {
            out.push(b'A');
            out.extend(varint(items.len() as u64));
            for item in items {
                write_value(out, item);
            }
            out.push(b'$');
            out.extend(varint(0));
            out.extend(varint(items.len() as u64));
        }
    }
}

fn literal(chunk: &[u8], out: &mut Vec<u8>) {
    let n = chunk.len() - 1;
    if n < 60 {
        out.push((n << 2) as u8);
    } else if n < 256 {
        out.extend([60 << 2, n as u8]);
    } else {
        out.push(61 << 2);
        out.extend((n as u16).to_le_bytes());
    }
    out.extend_from_slice(chunk);
}

/// Valid snappy: literals, plus overlapping copies for byte runs.
pub fn snappy_compress(data: &[u8]) -> Vec<u8> {
    let mut out = varint(data.len() as u64);
    let emit = |from: usize, to: usize, out: &mut Vec<u8>| {
        let mut pos = from;
        while pos < to {
            let end = to.min(pos + 65536);
            literal(&data[pos..end], out);
            pos = end;
        }
    };
    let (mut i, mut pending) = (0, 0);
    while i < data.len() {
        let mut run = 1;
        while i + run < data.len() && data[i + run] == data[i] && run < 64 {
            run += 1;
        }
        if run < 6 {
            i += 1;
            continue;
        }
        // Pending bytes plus the run's first byte, then a copy at offset 1.
        emit(pending, i + 1, &mut out);
        let length = run - 1;
        out.push((((length - 1) << 2) | 2) as u8);
        out.extend(1u16.to_le_bytes());
        i += run;
        pending = i;
    }
    emit(pending, data.len(), &mut out);
    out
}

pub fn blob_bytes(store: &Value) -> Vec<u8> {
    let mut blink = vec![0xFF, 0x15, 0xFE];
    blink.extend([0u8; 12]);
    blink.extend(v8_encode(store));
    let mut out = vec![0xFF, 0x11, 0x02];
    out.extend(snappy_compress(&blink));
    out
}

/// A small store in the shape real Slack desktop snapshots have.
///
/// Shapes were checked against real snapshots; all values are made up.
pub fn sample_store() -> Value {
    json!({
        // Real bootData has user_id but no team id; that is in selfTeamIds.
        "bootData": {"user_id": "U01ME", "api_token": "xoxc-1234-should-never-print"},
        "selfTeamIds": {"teamId": "T01", "defaultWorkspaceId": "T01", "internalWorkspaceIds": ["T01"]},
        // `teams` also lists the other signed-in workspaces, in any order.
        "teams": {
            "T09ELSE": {"id": "T09ELSE", "name": "Elsewhere", "domain": "elsewhere", "_other_account": true},
            "T01": {"id": "T01", "name": "Acme 研发", "domain": "acme"}
        },
        "members": {
            "U01ME": {"id": "U01ME", "name": "me", "real_name": "Me Myself", "is_self": true,
                      "profile": {"display_name": "me", "email": "me@example.com", "phone": ""}},
            "U02AL": {"id": "U02AL", "name": "alice", "real_name": "Alice Liddell",
                      "profile": {"display_name": "Alice", "real_name": "Alice Liddell",
                                  "title": "Engineer", "email": "alice@example.com"},
                      "_email_normalized_lc": "alice@example.com", "tz": "Asia/Tokyo"},
            "U03BOT": {"id": "U03BOT", "name": "deploybot", "is_bot": true, "profile": {}},
            // Slack keeps placeholders for ids it has not fetched yet.
            "UTBD": {"id": "UTBD", "name": "", "isNonExistent": true, "profile": {}},
            // Slack Connect member from another organisation.
            "U09EXT": {"id": "U09EXT", "name": "olive", "team_id": "T77EXT", "isExternal": true,
                       "profile": {"display_name": "Olive", "real_name": "Olive Out"}}
        },
        "bots": {"B1": {"id": "B1", "name": "deploybot", "app_id": "A1"},
                 "B2": {"id": "B2", "name": "Email", "app_id": "A2"}},
        "channels": {
            "C01GEN": {"id": "C01GEN", "name": "general", "is_member": true, "is_channel": true,
                       "topic": {"value": "Company-wide", "creator": "U01ME", "last_set": 0},
                       "purpose": "All hands"},
            "C02SEC": {"id": "C02SEC", "name": "secret-plans", "is_private": true,
                       "is_channel": true, "is_member": true},
            "C03OUT": {"id": "C03OUT", "name": "random", "is_member": false},
            "D01AL": {"id": "D01AL", "is_im": true, "user": "U02AL"},
            "C04LOG": {"id": "C04LOG", "name": "log", "is_member": true, "is_channel": true},
            // Real mpim records have no `members`; the membership slice has them.
            "C05MP": {"id": "C05MP", "name": "mpdm-me--alice--olive-1", "is_mpim": true,
                      "is_member": true, "members": null},
            // Placeholder for an id Slack has not fetched (no name, all flags false).
            "C09NX": {"id": "C09NX", "isNonExistent": true, "is_channel": false, "is_im": false,
                      "is_mpim": false, "is_member": false, "_name_lc": ""}
        },
        "membership": {
            "C05MP": {"U01ME": {"isKnown": true, "isMember": true},
                      "U02AL": {"isKnown": true, "isMember": true},
                      "U09EXT": {"isKnown": true, "isMember": true}}
        },
        "messages": {
            "C01GEN": {
                "1700000000.000100": {"ts": "1700000000.000100", "type": "message", "user": "U02AL",
                    "text": "hello <@U01ME>, see <#C02SEC> and <https://x.dev|docs>",
                    "reply_count": 2, "thread_ts": "1700000000.000100",
                    "_rxn_key": "message-1700000000.000100-C01GEN", "_hidden_reply": false},
                "1700000100.000200": {"ts": "1700000100.000200", "type": "message", "user": "U01ME",
                    "text": "thread reply 中文 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "thread_ts": "1700000000.000100", "_hidden_reply": true},
                // Bot message without a username: the name is in `bots`.
                // Files are ids into the top-level files slice.
                "1700000200.000300": {"ts": "1700000200.000300", "type": "message",
                    "subtype": "bot_message", "bot_id": "B1", "text": "deployed xoxb-999-abc",
                    "files": ["F1"], "_rxn_key": "message-1700000200.000300-C01GEN"},
                "1700000250.000000": {"ts": "1700000250.000000", "id": "x", "channel": "C01GEN",
                    "files": null, "isNonExistent": true}
            },
            "C04LOG": {
                // Email integration: posts as USLACKBOT with its own bot id.
                "1700001000.000001": {"ts": "1700001000.000001", "type": "message",
                    "user": "USLACKBOT", "bot_id": "B2", "text": "", "files": ["F2"]},
                "1700001100.000001": {"ts": "1700001100.000001", "type": "message", "user": "U02AL",
                    "text": "fixed typo", "edited": {"user": "U02AL", "ts": "1700001150.000000"}},
                // Huddle notices: text only in blocks; `edited` is bookkeeping.
                "1700001200.000001": {"ts": "1700001200.000001", "type": "message",
                    "subtype": "huddle_thread", "user": "USLACKBOT", "text": "",
                    "edited": {"user": "USLACKBOT", "ts": "1"},
                    "blocks": [{"type": "rich_text", "elements": [
                        {"type": "rich_text_section", "elements": [
                            {"type": "user", "user_id": "U02AL"},
                            {"type": "text", "text": " 发起了一个抱团"}]}]}]},
                "1700001300.000001": {"ts": "1700001300.000001", "type": "message",
                    "subtype": "bot_message", "bot_id": "B1", "username": "signup",
                    "text": "<mailto:bob@example.org|bob@example.org> signed up"},
                "1700001400.000001": {"ts": "1700001400.000001", "type": "message",
                    "subtype": "bot_message", "bot_id": "B1", "text": "",
                    "attachments": [{"fallback": "build 42 passed"}]}
            },
            "D01AL": {
                "1700000300.000400": {"ts": "1700000300.000400", "type": "message", "user": "U02AL",
                    "text": "ping me at alice@example.com"},
                // Client-side "only visible to you" notice: ts is not a number.
                "1700000350.xxxx13": {"ts": "1700000350.xxxx13", "type": "message",
                    "user": "USLACKBOT", "subtype": null, "is_ephemeral": true,
                    "_ephemeral_type": "x", "text": "*/foo* is not a valid command."}
            }
        },
        "reactions": {
            "message-1700000000.000100-C01GEN": [
                {"name": "tada", "users": ["U01ME", "U02AL", "U03BOT"], "count": 3, "baseName": "tada"}
            ]
        },
        // Contiguous runs of history Slack holds; anything else is scattered.
        "channelHistory": {
            "C04LOG": {"reachedStart": false, "reachedEnd": false, "slices": [
                {"start": "1700001000.000001", "end": "1700001100.000001",
                 "timestamps": ["1700001000.000001", "1700001100.000001"]},
                {"start": "1700001300.000001", "end": "1700001400.000001",
                 "timestamps": ["1700001300.000001", "1700001400.000001"]}
            ]},
            "C01GEN": {"reachedStart": true, "reachedEnd": true, "slices": [
                {"start": "1700000000.000100", "end": "1700000200.000300",
                 "timestamps": ["1700000000.000100", "1700000200.000300"]}
            ]}
        },
        "unreadChannelCounts": {
            "C01GEN": {"unreadCnt": 2, "unreadHighlightCnt": 0, "unreadVIPMentionCnt": 0,
                       "unreadVIPThreadTs": []}
        },
        "files": {
            "F1": {"id": "F1", "name": "log.txt", "mimetype": "text/plain", "user": "U03BOT",
                   "created": 1700000200},
            "F2": {"id": "F2", "name": "Shot 8.59.48@2x.png", "mimetype": "image/png",
                   "user": "U02AL", "created": 1700001000}
        }
    })
}

/// A temporary directory removed on drop.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new() -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("slack-cli-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn make_data_dir(root: &Path, stores: &[Value]) -> PathBuf {
    let blob_dir = root.join("IndexedDB/https_app.slack.com_0.indexeddb.blob/2/00");
    std::fs::create_dir_all(&blob_dir).unwrap();
    std::fs::write(
        blob_dir.join("junk"),
        b"\x00\x01not a blob at all, just noise",
    )
    .unwrap();
    for (i, store) in stores.iter().enumerate() {
        std::fs::write(blob_dir.join(format!("{:x}", i + 10)), blob_bytes(store)).unwrap();
    }
    root.to_path_buf()
}
