mod common;

use common::{make_data_dir, sample_store, TempDir};
use regex::Regex;
use serde_json::{json, Value};
use slack_cli::redact::scrub_text;
use slack_cli::{run, Term};
use std::io::Read;
use std::process::{Command, Stdio};

struct Fixture {
    tmp: TempDir,
    data: String,
}

/// Two workspaces: the sample store (T01) and a nearly empty one (T02).
fn fixture() -> Fixture {
    let tmp = TempDir::new();
    let mut other = sample_store();
    other["bootData"] = json!({"team_id": "T02", "user_id": "U01ME"});
    // Each snapshot's `teams` lists the other signed-in workspaces too.
    other["teams"] = json!({"T01": {"id": "T01", "name": "Acme 研发", "domain": "acme"},
                            "T02": {"id": "T02", "name": "Side Project", "domain": "side"}});
    other["messages"] = json!({"C01GEN": {}});
    let data = make_data_dir(&tmp.0, &[sample_store(), other])
        .display()
        .to_string();
    Fixture { tmp, data }
}

impl Fixture {
    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let mut argv = vec!["slack-cli"];
        argv.extend_from_slice(args);
        argv.extend(["--data-dir", &self.data]);
        run_raw(&argv)
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut argv = args.to_vec();
        argv.push("--json");
        serde_json::from_str(&self.run(&argv).1).unwrap()
    }
}

fn run_raw(argv: &[&str]) -> (i32, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(argv.iter().copied(), &mut out, &mut err, &Term::plain()).unwrap();
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

fn ids(rows: &Value) -> Vec<String> {
    rows.as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn teams() {
    let f = fixture();
    let teams = f.json(&["teams"]);
    let t01 = teams
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["team_id"] == "T01")
        .unwrap();
    assert_eq!(teams.as_array().unwrap().len(), 2);
    assert_eq!(t01["team_name"], "Acme 研发");
    assert_eq!(t01["cached_messages"], 10);
    assert_eq!(t01["user_name"], "me");
    assert_eq!(t01["members"], 3); // me, alice, deploybot
    assert_eq!(t01["external_members"], 1);
    let (_, out, _) = f.run(&["teams"]);
    let side = out.lines().find(|l| l.contains("Side Project")).unwrap();
    assert!(
        side.contains(" 0 "),
        "zero counts print as 0, not blank: {side}"
    );
}

#[test]
fn team_selects_only_its_own_workspace() {
    let f = fixture();
    for (query, expected) in [
        ("T02", "T02"),
        (" side ", "T02"),
        ("ACME", "T01"),
        ("Acme 研发", "T01"),
    ] {
        let teams = f.json(&["teams", "--team", query]);
        let got: Vec<&str> = teams
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["team_id"].as_str().unwrap())
            .collect();
        assert_eq!(got, [expected], "{query}");
    }
    assert_eq!(f.run(&["teams", "--team", "elsewhere"]).0, 2);
    assert_eq!(f.run(&["history", "#general", "--team", "side"]).0, 0);
}

#[test]
fn channels_filters_and_labels() {
    let f = fixture();
    let rows = f.json(&["channels", "--team", "acme"]);
    let by_id = |id: &str| {
        rows.as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == id)
            .cloned()
    };
    assert!(by_id("C03OUT").is_none(), "not a member, nothing cached");
    assert_eq!(by_id("D01AL").unwrap()["label"], "@Alice");
    assert_eq!(by_id("C02SEC").unwrap()["kind"], "private");
    assert_eq!(by_id("C01GEN").unwrap()["topic"], "Company-wide");
    assert_eq!(by_id("C01GEN").unwrap()["unread"], 2);
    let all = ids(&f.json(&["channels", "--team", "T01", "--all"]));
    assert!(all.contains(&"C03OUT".to_string()));
    assert!(!all.contains(&"C09NX".to_string()), "placeholder");
    let ims = ids(&f.json(&["channels", "--team", "T01", "--kind", "im"]));
    assert_eq!(ims, ["D01AL"]);
}

#[test]
fn mpim_label_from_membership() {
    let f = fixture();
    let rows = f.json(&["channels", "--team", "T01"]);
    let mpim = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "C05MP")
        .unwrap();
    assert_eq!(
        (mpim["kind"].as_str(), mpim["label"].as_str()),
        (Some("mpim"), Some("@Alice, @Olive"))
    );
}

#[test]
fn ephemeral_notice_keeps_its_time() {
    let f = fixture();
    let (_, out, _) = f.run(&["history", "@alice", "--team", "T01"]);
    let lines: Vec<&str> = out.lines().collect();
    let notice = lines
        .iter()
        .position(|l| l.contains("not a valid command"))
        .unwrap();
    assert!(notice > lines.iter().position(|l| l.contains("ping me")).unwrap());
    assert!(Regex::new(r"^\d{4}-\d\d-\d\d \d\d:\d\d ")
        .unwrap()
        .is_match(lines[notice]));
    assert!(lines[notice + 1].contains("only visible to you"));
}

#[test]
fn history_renders_mentions_and_scrubs() {
    let f = fixture();
    let (code, out, err) = f.run(&["history", "#general", "--team", "T01"]);
    assert_eq!(code, 0);
    assert!(out.contains("hello @me, see #secret-plans and docs (https://x.dev)"));
    assert!(out.contains("deploybot"));
    assert!(!out.contains("xoxb-999"));
    assert!(out.contains("2 replies"));
    assert!(err.contains("cached messages"));
}

#[test]
fn history_dm_by_user_and_limit() {
    let f = fixture();
    let data = f.json(&["history", "@alice", "--team", "T01"]);
    assert_eq!(data["channel"]["id"], "D01AL");
    assert!(data["messages"][0]["text"]
        .as_str()
        .unwrap()
        .contains("[email]"));
    let data = f.json(&["history", "C01GEN", "-n", "1", "--team", "T01"]);
    assert_eq!(data["messages"].as_array().unwrap().len(), 1);
    assert_eq!(data["messages"][0]["ts"], "1700000200.000300");
}

#[test]
fn history_real_store_shapes() {
    let f = fixture();
    let (_, out, _) = f.run(&["history", "#general", "--team", "T01"]);
    assert!(out.contains(":tada:×3"), "reactions[_rxn_key]");
    assert!(out.contains("files: log.txt"), "file id -> files slice");
    assert!(out.contains("deploybot:"), "bots[bot_id].name");
    assert!(
        !out.contains("thread reply"),
        "_hidden_reply stays in its thread"
    );
    let (_, out, _) = f.run(&[
        "history",
        "#general",
        "--thread",
        "1700000000.000100",
        "--team",
        "T01",
    ]);
    assert!(out.contains("thread reply"));
    assert!(out.contains("↳ reply"));
}

#[test]
fn history_thread_hints() {
    let f = fixture();
    let (code, _, err) = f.run(&[
        "history",
        "#general",
        "--thread",
        "1700000000.000100",
        "--team",
        "T01",
    ]);
    assert_eq!(code, 0);
    assert!(err.contains("1 of 2 replies are not cached"), "{err}");
    assert!(!err.contains("Older"), "no channel-level hint");
    let (code, _, err) = f.run(&["history", "#general", "--thread", "1.0", "--team", "T01"]);
    assert_eq!(code, 1);
    assert!(err.contains("No cached message in thread"));
}

#[test]
fn history_log_channel() {
    let f = fixture();
    let (code, out, err) = f.run(&["history", "#log", "--team", "T01"]);
    assert_eq!(code, 0);
    let lines: Vec<&str> = out.lines().collect();
    assert!(out.contains("Email: "), "not Slackbot");
    assert!(
        out.contains("files: Shot 8.59.48@2x.png"),
        "not taken for an email"
    );
    let typo = lines.iter().position(|l| l.contains("fixed typo")).unwrap();
    assert!(lines[typo + 1].contains("edited"));
    let huddle = lines
        .iter()
        .position(|l| l.contains("发起了一个抱团"))
        .unwrap();
    assert!(
        lines[huddle].contains("@Alice 发起了一个抱团"),
        "text from blocks"
    );
    assert!(!lines.get(huddle + 1).is_some_and(|l| l.contains("edited")));
    assert!(out.contains("[email] signed up"), "mailto label once");
    assert!(!out.contains("mailto"));
    assert!(out.contains("build 42 passed"), "attachments fallback");
    assert_eq!(out.matches("messages may be missing here").count(), 2);
    assert!(err.contains("Older and newer messages may be missing"));
    let data = f.json(&["history", "#log", "--team", "T01"]);
    let gaps: Vec<bool> = data["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["gap_before"] == true)
        .collect();
    assert_eq!(gaps, [false, false, true, true, false]);
    assert_eq!(
        (data["reached_start"].clone(), data["reached_end"].clone()),
        (json!(false), json!(false))
    );
    assert_eq!(data["messages"][0]["files"], json!(["Shot 8.59.48@2x.png"]));
    let (_, _, err) = f.run(&["history", "#general", "--team", "T01"]);
    assert!(!err.contains("may be missing"));
}

#[test]
fn history_ambiguous_across_teams() {
    let f = fixture();
    let (code, _, err) = f.run(&["history", "general"]);
    assert_eq!(code, 1);
    assert!(err.contains("--team"));
}

#[test]
fn search() {
    let f = fixture();
    let hits = f.json(&["search", "中文"]);
    assert_eq!(hits.as_array().unwrap().len(), 1);
    assert_eq!(hits[0]["channel_label"], "#general");
    assert_eq!(f.run(&["search", "nothing-like-this"]).0, 1);
    // Ephemeral notices have a non-numeric ts and still sort.
    let hits = f.json(&["search", "valid command"]);
    assert_eq!(hits.as_array().unwrap().len(), 1);
}

#[test]
fn search_user_and_channel() {
    let f = fixture();
    for who in ["alice", "@alice", "Alice Liddell", "U02AL", "Alice"] {
        let hits = f.json(&["search", "", "--user", who, "--team", "T01"]);
        assert!(
            hits.as_array()
                .unwrap()
                .iter()
                .all(|h| h["user"] == "U02AL"),
            "{who}"
        );
        assert!(!hits.as_array().unwrap().is_empty(), "{who}");
    }
    let hits = f.json(&["search", "", "--user", "Email", "--team", "T01"]);
    assert_eq!(hits.as_array().unwrap().len(), 1);
    let (code, _, err) = f.run(&["search", "x", "--channel", "#nope"]);
    assert_eq!(code, 1);
    assert!(err.contains("No cached channel matches"));
    let (code, out, _) = f.run(&["search", "nothing-like-this", "--json"]);
    assert_eq!(
        (code, serde_json::from_str::<Value>(&out).unwrap()),
        (1, json!([]))
    );
}

#[test]
fn members_external_and_placeholders() {
    let f = fixture();
    let (_, out, _) = f.run(&["members", "--team", "T01", "--json"]);
    assert!(!out.contains("U09EXT"));
    assert!(!out.contains("UTBD"));
    let all = f.json(&["members", "--all", "--team", "T01"]);
    let olive = all
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "U09EXT")
        .unwrap();
    assert_eq!(
        (olive["is_external"].clone(), olive["team_id"].clone()),
        (json!(true), json!("T77EXT"))
    );
}

#[test]
fn members_hide_email_by_default() {
    let f = fixture();
    let (_, out, _) = f.run(&["members", "--team", "T01", "--json"]);
    assert!(!out.contains("example.com"));
    assert!(!out.contains("deploybot"));
    let (_, out, _) = f.run(&["members", "--team", "T01", "--emails", "--json"]);
    assert!(out.contains("alice@example.com"));
}

#[test]
fn placeholder_messages_are_skipped() {
    let f = fixture();
    let (_, out, _) = f.run(&["history", "#general", "-n", "0", "--team", "T01", "--json"]);
    assert!(!out.contains("1700000250.000000"));
}

#[test]
fn files_newest_first() {
    let f = fixture();
    let files = f.json(&["files", "--team", "T01"]);
    assert_eq!(ids(&files), ["F2", "F1"]);
    assert_eq!(files[1]["author"], "deploybot");
}

#[test]
fn export_never_leaks_tokens() {
    let f = fixture();
    for extra in [&[][..], &["--raw"], &["--raw", "--compact"]] {
        let mut args = vec!["export"];
        args.extend_from_slice(extra);
        let (_, out, _) = f.run(&args);
        assert!(!out.contains("xoxc-1234"));
        assert!(!out.contains("xoxb-999"));
        assert!(!out.contains("alice@example.com"));
        serde_json::from_str::<Value>(&out).unwrap();
    }
    let (_, out, _) = f.run(&["export", "--raw", "--team", "T01"]);
    let data: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(data[0]["store"]["bootData"]["api_token"], "[redacted]");
}

#[test]
fn export_to_file() {
    let f = fixture();
    let path = f.tmp.0.join("out.json");
    let (code, out, err) = f.run(&["export", "-o", path.to_str().unwrap()]);
    assert_eq!((code, out.as_str()), (0, ""));
    assert!(err.contains("Wrote"));
    let data: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(data.as_array().unwrap().len(), 2);
}

#[test]
fn inspect() {
    let f = fixture();
    let (_, out, _) = f.run(&["inspect", "--team", "T01", "--depth", "3"]);
    assert!(out.contains("messages"));
    assert!(!out.contains("hello"), "structure only, no values");
    let (code, _, err) = f.run(&["inspect", "nope.zz", "--team", "T01"]);
    assert_eq!(code, 1);
    assert!(err.contains("No key"));
}

#[test]
fn scrub_keeps_retina_file_names() {
    assert_eq!(
        scrub_text("Shot 8.59.48@2x.png icon@3x.jpeg", false),
        "Shot 8.59.48@2x.png icon@3x.jpeg"
    );
    assert_eq!(
        scrub_text("mail a@b.co or x.y@mail.example.com", false),
        "mail [email] or [email]"
    );
    assert_eq!(scrub_text("t xoxc-12-ab%3D", false), "t xoxc-[redacted]");
}

#[test]
fn data_dir_may_point_at_indexeddb() {
    let f = fixture();
    let dir = format!("{}/IndexedDB", f.data);
    assert_eq!(run_raw(&["slack-cli", "teams", "--data-dir", &dir]).0, 0);
}

#[test]
fn global_options_go_anywhere() {
    let f = fixture();
    let (code, out, _) = run_raw(&["slack-cli", "--data-dir", &f.data, "--json", "teams"]);
    assert_eq!(code, 0);
    serde_json::from_str::<Value>(&out).unwrap();
}

#[test]
fn missing_data_dir() {
    let f = fixture();
    let dir = format!("{}/nope", f.data);
    let (code, _, err) = run_raw(&["slack-cli", "teams", "--data-dir", &dir]);
    assert_eq!(code, 2);
    assert!(err.contains("No Slack IndexedDB cache"));
}

#[test]
fn closed_pipe_is_quiet() {
    let f = fixture();
    let mut child = Command::new(env!("CARGO_BIN_EXE_slack-cli"))
        .args(["export", "--data-dir", &f.data])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take()); // reader goes away before the CLI writes
    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    let status = child.wait().unwrap();
    assert!(
        !err.contains("panicked") && !err.contains("Broken pipe"),
        "{err}"
    );
    assert_eq!(status.code(), Some(0));
}
