mod common;

use common::{blob_bytes, sample_store, snappy_compress, v8_encode};
use serde_json::json;
use slack_cli::decode::{decode_bytes, shape, snappy_decompress};
use std::process::Command;

#[test]
fn snappy_roundtrip_with_overlapping_copies() {
    let mut data = b"abc".to_vec();
    data.extend([b'z'; 500]);
    data.extend("中文".as_bytes());
    data.extend([b'q'; 70]);
    data.extend(b"end");
    assert_eq!(snappy_decompress(&snappy_compress(&data)).unwrap(), data);
}

#[test]
fn snappy_spec_vector() {
    // "abcd" literal + copy(len 8, offset 4) -> "abcdabcdabcd"
    let data = [12, 3 << 2, b'a', b'b', b'c', b'd', (4 << 2) | 1, 4];
    assert_eq!(snappy_decompress(&data).unwrap(), b"abcdabcdabcd");
}

#[test]
fn snappy_rejects_bad_offsets() {
    assert!(snappy_decompress(&[4, 1, 0]).is_err());
}

#[test]
fn full_blob() {
    let store = sample_store();
    assert_eq!(decode_bytes(&blob_bytes(&store)).unwrap(), store);
}

#[test]
fn uncompressed_v8_stream() {
    let value = json!({"a": [1, "é", "中"]});
    assert_eq!(decode_bytes(&v8_encode(&value)).unwrap(), value);
}

#[test]
fn v8_format_16() {
    // Slack 4.53 (Electron 45) writes FF 10; older builds write FF 0F.
    let value = json!({"a": [1, "中"]});
    let mut payload = v8_encode(&value);
    payload[1] = 0x10;
    assert_eq!(decode_bytes(&payload).unwrap(), value);
}

#[test]
fn garbage_is_an_error_not_a_panic() {
    for data in [
        &b""[..],
        b"\xff",
        b"\xff\x11\x02\x05",
        b"\xff\x0fo\"\x05ab",
        b"\xff\x0f\x01",
    ] {
        assert!(decode_bytes(data).is_err(), "{data:?}");
    }
}

#[test]
fn against_real_v8() {
    let script = "const v8=require('v8');const s={k:1};const o={a:s,b:s,m:new Map([[1,'x']]),\
                  set:new Set(['y']),h:[1,,3],u:'é中\u{1F600}',big:2**40,f:-1.25,\
                  d:new Date(5),bi:-10n,n:null,und:undefined};o.self=o;\
                  process.stdout.write(v8.serialize(o))";
    let Ok(output) = Command::new("node").args(["-e", script]).output() else {
        eprintln!("node not installed; skipping");
        return;
    };
    let value = decode_bytes(&output.stdout).unwrap();
    assert_eq!(value["self"], "[circular]");
    assert_eq!(value["a"], json!({"k": 1}));
    assert_eq!(value["a"], value["b"]);
    assert_eq!(value["m"], json!({"1": "x"}));
    assert_eq!(value["set"], json!(["y"]));
    assert_eq!(value["h"], json!([1, null, 3]));
    assert_eq!(value["u"], "é中\u{1F600}");
    assert_eq!(value["big"], 1u64 << 40);
    assert_eq!(value["f"], -1.25);
    assert_eq!(value["d"], 5);
    assert_eq!(value["bi"], -10);
    assert_eq!(value["n"], json!(null));
}

#[test]
fn shape_names_top_level_slices_and_folds_ids() {
    let names = [
        "bootData",
        "channels",
        "members",
        "messages",
        "teams",
        "files",
        "bots",
        "reactions",
        "prefs",
        "userPrefs",
        "teamPrefs",
        "selfTeamIds",
        "unreadCounts",
    ];
    let store: serde_json::Map<_, _> = names
        .iter()
        .map(|n| (n.to_string(), json!({"x": 1})))
        .collect();
    assert!(shape(&store.into(), 1).contains("selfTeamIds:"));
    let by_id: serde_json::Map<_, _> = (0..20)
        .map(|i| (format!("C0{i:09}"), json!({"x": 1})))
        .collect();
    assert!(shape(&by_id.into(), 1).contains("<id>:"));
}
