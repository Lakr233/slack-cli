//! Scrub credentials and, unless asked, email addresses from output.

use regex::{Captures, Regex};
use serde_json::{Map, Value};
use std::sync::LazyLock;

pub const REDACTED: &str = "[redacted]";

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid regex")
}

static SECRET_KEY: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?i)(^|_)(token|tokens|secret|password|passwd|cookie|cookies|api_key|apikey|session_key|xoxc|xoxd|xoxs)($|_)",
    )
});
static SECRET_VALUE: LazyLock<Regex> = LazyLock::new(|| regex(r"xox[a-z]-[A-Za-z0-9%\-]+"));
static EMAIL_KEY: LazyLock<Regex> = LazyLock::new(|| regex(r"(?i)(^|_)email(s)?($|_)"));
static EMAIL_VALUE: LazyLock<Regex> =
    LazyLock::new(|| regex(r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}\b"));
// Retina file names such as "Shot 8.59.48@2x.png" are not addresses.
static RETINA: LazyLock<Regex> = LazyLock::new(|| regex(r"^[0-9]+x\."));
const FILE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "heic", "svg", "pdf", "mov", "mp4", "zip", "txt", "log",
];

fn is_email(candidate: &str) -> bool {
    let Some((_, domain)) = candidate.split_once('@') else {
        return false;
    };
    let tld = domain.rsplit('.').next().unwrap_or("");
    !RETINA.is_match(domain) && !FILE_EXTENSIONS.contains(&tld)
}

/// Mask tokens, and email addresses unless `emails` is set.
pub fn scrub_text(text: &str, emails: bool) -> String {
    let text = SECRET_VALUE.replace_all(text, |c: &Captures| format!("{}{REDACTED}", &c[0][..5]));
    if emails {
        return text.into_owned();
    }
    EMAIL_VALUE
        .replace_all(&text, |c: &Captures| {
            if is_email(&c[0]) {
                "[email]".to_string()
            } else {
                c[0].to_string()
            }
        })
        .into_owned()
}

fn is_unset(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => true,
        Value::String(s) => s.is_empty(),
        Value::Number(n) => n.as_f64() == Some(0.0),
        _ => false,
    }
}

/// A copy of `value` with secrets removed.
pub fn scrub(value: &Value, emails: bool) -> Value {
    match value {
        Value::String(s) => Value::String(scrub_text(s, emails)),
        Value::Array(items) => Value::Array(items.iter().map(|v| scrub(v, emails)).collect()),
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, item) in map {
                let clean = if SECRET_KEY.is_match(key) && !is_unset(item) {
                    Value::String(REDACTED.to_string())
                } else if !emails
                    && EMAIL_KEY.is_match(key)
                    && item.as_str().is_some_and(|s| !s.is_empty())
                {
                    Value::String("[email]".to_string())
                } else {
                    scrub(item, emails)
                };
                out.insert(key.clone(), clean);
            }
            Value::Object(out)
        }
        _ => value.clone(),
    }
}
