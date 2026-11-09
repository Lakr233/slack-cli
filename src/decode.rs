//! Decode the Redux snapshots Slack desktop persists in IndexedDB.
//!
//! Each blob file holds one workspace's whole Redux store, wrapped in layers:
//!
//! 1. Chromium IndexedDB value wrapper: `FF 11 02` means the rest is
//!    snappy-compressed. `FF 11 01` means the value lives in another blob.
//! 2. Snappy, raw format with a varint length prefix.
//! 3. Blink SerializedScriptValue envelope: `FF <blink version>`, and from
//!    version 21 a trailer-offset tag `FE` followed by 12 bytes.
//! 4. V8 ValueSerializer stream: `FF <v8 version>` (15, or 16 from
//!    Electron 45) and then one value.

use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Map, Number, Value};
use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;
use std::sync::LazyLock;

/// A blob that does not match the expected format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DecodeError {}

type Result<T> = std::result::Result<T, DecodeError>;

fn fail<T>(message: impl Into<String>) -> Result<T> {
    Err(DecodeError(message.into()))
}

fn varint(buf: &[u8], mut pos: usize) -> Result<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        let Some(&byte) = buf.get(pos) else {
            return fail("truncated varint");
        };
        pos += 1;
        if shift < 64 {
            result |= u64::from(byte & 0x7F) << shift;
        }
        if byte < 0x80 {
            return Ok((result, pos));
        }
        shift += 7;
        if shift > 70 {
            return fail("varint too long");
        }
    }
}

fn little_endian(data: &[u8], pos: usize, width: usize) -> Result<usize> {
    let Some(bytes) = data.get(pos..pos + width) else {
        return fail("snappy tag runs past the input");
    };
    Ok(bytes
        .iter()
        .rev()
        .fold(0usize, |acc, &b| (acc << 8) | usize::from(b)))
}

// ---------------------------------------------------------------------------
// Snappy

/// Decompress raw snappy data (not the framing format).
pub fn snappy_decompress(data: &[u8]) -> Result<Vec<u8>> {
    let (expected, mut pos) = varint(data, 0)?;
    let expected = usize::try_from(expected).unwrap_or(usize::MAX);
    let mut out: Vec<u8> = Vec::with_capacity(expected.min(1 << 28));
    while pos < data.len() {
        let tag = data[pos];
        pos += 1;
        let (length, offset) = match tag & 3 {
            0 => {
                let mut length = usize::from(tag >> 2);
                if length >= 60 {
                    let extra = length - 59;
                    length = little_endian(data, pos, extra)?;
                    pos += extra;
                }
                length += 1;
                let Some(literal) = data.get(pos..pos + length) else {
                    return fail("snappy literal runs past the input");
                };
                out.extend_from_slice(literal);
                pos += length;
                continue;
            }
            1 => {
                let length = 4 + usize::from((tag >> 2) & 7);
                let offset = (usize::from(tag >> 5) << 8) | little_endian(data, pos, 1)?;
                pos += 1;
                (length, offset)
            }
            2 => {
                let offset = little_endian(data, pos, 2)?;
                pos += 2;
                (usize::from(tag >> 2) + 1, offset)
            }
            _ => {
                let offset = little_endian(data, pos, 4)?;
                pos += 4;
                (usize::from(tag >> 2) + 1, offset)
            }
        };
        if offset == 0 || offset > out.len() {
            return fail("snappy copy offset out of range");
        }
        // Byte by byte, so overlapping copies repeat the last `offset` bytes.
        let start = out.len() - offset;
        for i in 0..length {
            let byte = out[start + i];
            out.push(byte);
        }
    }
    if out.len() != expected {
        return fail(format!(
            "snappy output is {} bytes, header says {expected}",
            out.len()
        ));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Outer layers

/// Strip Chromium's IndexedDB value wrapper, decompressing if needed.
pub fn unwrap_idb_value(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() >= 3 && data[0] == 0xFF && data[1] == 0x11 {
        return match data[2] {
            0x02 => snappy_decompress(&data[3..]),
            0x01 => fail("value only points at an external blob"),
            mode => fail(format!("unknown IndexedDB wrapper mode 0x{mode:02x}")),
        };
    }
    Ok(data.to_vec())
}

/// The offset where the V8 ValueSerializer stream begins.
pub fn v8_stream_start(data: &[u8]) -> Result<usize> {
    if data.first() != Some(&0xFF) {
        return fail("missing serializer version header");
    }
    let (_, mut pos) = varint(data, 1)?;
    if data.get(pos) == Some(&0xFE) {
        // Blink v21+: trailer offset (uint64) and trailer size (uint32).
        pos += 1 + 8 + 4;
    }
    if data.get(pos) == Some(&0xFF) {
        return Ok(pos);
    }
    // The first header was V8's own: there is no Blink envelope.
    Ok(0)
}

// ---------------------------------------------------------------------------
// V8 ValueSerializer

/// Decoded values keep shared and cyclic references until they are turned
/// into JSON, where a cycle becomes the string "[circular]".
#[derive(Clone)]
enum V {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Bytes(usize),
    List(Rc<RefCell<Vec<V>>>),
    Map(Rc<RefCell<IndexMap<String, V>>>),
}

const MAX_DEPTH: usize = 4000;

struct Deserializer<'a> {
    data: &'a [u8],
    pos: usize,
    version: u64,
    objects: Vec<Option<V>>,
    depth: usize,
}

impl<'a> Deserializer<'a> {
    fn new(data: &'a [u8], pos: usize) -> Self {
        Self {
            data,
            pos,
            version: 0,
            objects: Vec::new(),
            depth: 0,
        }
    }

    fn byte(&mut self) -> Result<u8> {
        let Some(&byte) = self.data.get(self.pos) else {
            return fail("unexpected end of V8 stream");
        };
        self.pos += 1;
        Ok(byte)
    }

    fn varint(&mut self) -> Result<u64> {
        let (value, pos) = varint(self.data, self.pos)?;
        self.pos = pos;
        Ok(value)
    }

    fn length(&mut self) -> Result<usize> {
        Ok(usize::try_from(self.varint()?).unwrap_or(usize::MAX))
    }

    fn bytes(&mut self, length: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(length)
            .filter(|&end| end <= self.data.len());
        let Some(end) = end else {
            return fail("V8 value runs past the input");
        };
        let chunk = &self.data[self.pos..end];
        self.pos = end;
        Ok(chunk)
    }

    fn double(&mut self) -> Result<f64> {
        let raw: [u8; 8] = self.bytes(8)?.try_into().expect("8 bytes");
        Ok(f64::from_le_bytes(raw))
    }

    fn tag(&mut self) -> Result<u8> {
        loop {
            let tag = self.byte()?;
            if tag != 0x00 {
                return Ok(tag); // 0x00 is padding
            }
        }
    }

    fn peek_tag(&self) -> Result<u8> {
        let mut pos = self.pos;
        while self.data.get(pos) == Some(&0x00) {
            pos += 1;
        }
        match self.data.get(pos) {
            Some(&tag) => Ok(tag),
            None => fail("unexpected end of V8 stream"),
        }
    }

    fn consume_tag(&mut self, expected: u8) -> Result<bool> {
        if self.peek_tag()? == expected {
            self.tag()?;
            return Ok(true);
        }
        Ok(false)
    }

    fn new_id(&mut self) -> usize {
        self.objects.push(None);
        self.objects.len() - 1
    }

    fn set(&mut self, id: usize, value: V) {
        self.objects[id] = Some(value);
    }

    fn remember(&mut self, value: V) -> V {
        let id = self.new_id();
        self.set(id, value.clone());
        value
    }

    fn read_value(&mut self) -> Result<V> {
        if self.data.get(self.pos) == Some(&0xFF) {
            self.pos += 1;
            self.version = self.varint()?;
        }
        self.read_object()
    }

    fn read_object(&mut self) -> Result<V> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return fail("V8 value nested too deeply");
        }
        let mut value = self.read_object_internal()?;
        if let V::Bytes(length) = value {
            if self.pos < self.data.len() && self.peek_tag()? == b'V' {
                self.tag()?;
                value = self.read_view(length)?;
            }
        }
        self.depth -= 1;
        Ok(value)
    }

    fn read_object_internal(&mut self) -> Result<V> {
        let mut tag = self.tag()?;
        while tag == b'?' {
            self.varint()?; // verify object count
            tag = self.tag()?;
        }
        Ok(match tag {
            b'_' | b'0' | b'-' => V::Null,
            b'T' => V::Bool(true),
            b'F' => V::Bool(false),
            b'I' => {
                let raw = self.varint()?;
                V::Int(((raw >> 1) as i64) ^ -((raw & 1) as i64))
            }
            b'U' => V::Int(self.varint()? as i64),
            b'N' => clean_double(self.double()?),
            b'Z' => self.read_bigint()?,
            b'"' => {
                let length = self.length()?;
                V::Str(self.bytes(length)?.iter().map(|&b| char::from(b)).collect())
            }
            b'S' => {
                let length = self.length()?;
                V::Str(String::from_utf8_lossy(self.bytes(length)?).into_owned())
            }
            b'c' => {
                let length = self.length()?;
                let units: Vec<u16> = self
                    .bytes(length)?
                    .chunks_exact(2)
                    .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                    .collect();
                V::Str(String::from_utf16_lossy(&units))
            }
            b'^' => {
                let id = self.length()?;
                match self.objects.get(id) {
                    Some(Some(value)) => value.clone(),
                    _ => return fail(format!("reference to unknown object {id}")),
                }
            }
            b'o' => self.read_js_object()?,
            b'A' => self.read_dense_array()?,
            b'a' => self.read_sparse_array()?,
            b'D' | b'n' => {
                let value = clean_double(self.double()?);
                self.remember(value)
            }
            b'y' | b'x' => self.remember(V::Bool(tag == b'y')),
            b'z' => {
                let value = self.read_bigint()?;
                self.remember(value)
            }
            b's' => {
                let id = self.new_id();
                let value = self.read_object()?;
                self.set(id, value.clone());
                value
            }
            b'R' => {
                let id = self.new_id();
                let pattern = self.read_object()?;
                let flags = self.varint()?;
                let mut map = IndexMap::new();
                map.insert("__regexp__".to_string(), pattern);
                map.insert("flags".to_string(), V::Int(flags as i64));
                let value = V::Map(Rc::new(RefCell::new(map)));
                self.set(id, value.clone());
                value
            }
            b';' => self.read_map()?,
            b'\'' => self.read_set()?,
            b'B' => {
                let id = self.new_id();
                let length = self.length()?;
                self.bytes(length)?;
                self.set(id, V::Bytes(length));
                V::Bytes(length)
            }
            b'~' => {
                let id = self.new_id();
                let length = self.length()?;
                self.varint()?; // max byte length
                self.bytes(length)?;
                self.set(id, V::Bytes(length));
                V::Bytes(length)
            }
            b'r' => self.read_error()?,
            _ => {
                return fail(format!(
                    "unsupported V8 tag 0x{tag:02x} ({:?}) at offset {}",
                    char::from(tag),
                    self.pos - 1
                ))
            }
        })
    }

    fn read_bigint(&mut self) -> Result<V> {
        let bitfield = self.varint()?;
        let length = usize::try_from(bitfield >> 1).unwrap_or(usize::MAX);
        let digits = self.bytes(length)?;
        let negative = bitfield & 1 == 1;
        if digits.iter().skip(8).all(|&b| b == 0) {
            let mut raw = [0u8; 8];
            for (slot, &b) in raw.iter_mut().zip(digits) {
                *slot = b;
            }
            let magnitude = u64::from_le_bytes(raw);
            if let Ok(value) = i64::try_from(magnitude) {
                return Ok(V::Int(if negative { -value } else { value }));
            }
        }
        let magnitude = digits
            .iter()
            .rev()
            .fold(0f64, |acc, &b| acc * 256.0 + f64::from(b));
        Ok(clean_double(if negative { -magnitude } else { magnitude }))
    }

    fn read_properties(
        &mut self,
        target: &Rc<RefCell<IndexMap<String, V>>>,
        end: u8,
    ) -> Result<()> {
        while !self.consume_tag(end)? {
            let key = self.read_object()?;
            let value = self.read_object()?;
            target.borrow_mut().insert(property_key(&key), value);
        }
        Ok(())
    }

    fn read_js_object(&mut self) -> Result<V> {
        let id = self.new_id();
        let map = Rc::new(RefCell::new(IndexMap::new()));
        self.set(id, V::Map(map.clone()));
        self.read_properties(&map, b'{')?;
        self.varint()?; // property count
        Ok(V::Map(map))
    }

    fn read_dense_array(&mut self) -> Result<V> {
        let id = self.new_id();
        let length = self.length()?;
        let items = Rc::new(RefCell::new(Vec::new()));
        self.set(id, V::List(items.clone()));
        for _ in 0..length {
            let item = if self.consume_tag(b'-')? {
                V::Null
            } else {
                self.read_object()?
            };
            items.borrow_mut().push(item);
        }
        let extra = Rc::new(RefCell::new(IndexMap::new()));
        self.read_properties(&extra, b'$')?;
        self.varint()?; // property count
        self.varint()?; // length
        Ok(V::List(items))
    }

    fn read_sparse_array(&mut self) -> Result<V> {
        let id = self.new_id();
        let length = self.length()?;
        let entries = Rc::new(RefCell::new(IndexMap::new()));
        self.set(id, V::Map(entries.clone()));
        self.read_properties(&entries, b'@')?;
        self.varint()?; // property count
        self.varint()?; // length
        let numeric = entries
            .borrow()
            .keys()
            .all(|k| !k.is_empty() && k.bytes().all(|b| b.is_ascii_digit()));
        if length <= 100_000 && numeric {
            let mut items = vec![V::Null; length];
            for (key, value) in entries.borrow().iter() {
                if let Ok(index) = key.parse::<usize>() {
                    if index < length {
                        items[index] = value.clone();
                    }
                }
            }
            let list = V::List(Rc::new(RefCell::new(items)));
            self.set(id, list.clone());
            return Ok(list);
        }
        Ok(V::Map(entries))
    }

    fn read_map(&mut self) -> Result<V> {
        let id = self.new_id();
        let map = Rc::new(RefCell::new(IndexMap::new()));
        self.set(id, V::Map(map.clone()));
        while !self.consume_tag(b':')? {
            let key = self.read_object()?;
            let value = self.read_object()?;
            map.borrow_mut().insert(property_key(&key), value);
        }
        self.varint()?; // length (keys + values)
        Ok(V::Map(map))
    }

    fn read_set(&mut self) -> Result<V> {
        let id = self.new_id();
        let items = Rc::new(RefCell::new(Vec::new()));
        self.set(id, V::List(items.clone()));
        while !self.consume_tag(b',')? {
            let item = self.read_object()?;
            items.borrow_mut().push(item);
        }
        self.varint()?;
        Ok(V::List(items))
    }

    fn read_view(&mut self, buffer_length: usize) -> Result<V> {
        let id = self.new_id();
        self.varint()?; // sub tag
        let offset = self.length()?;
        let length = self.length()?;
        if self.version >= 14 {
            self.varint()?; // flags
        }
        let value = V::Bytes(length.min(buffer_length.saturating_sub(offset)));
        self.set(id, value.clone());
        Ok(value)
    }

    fn read_error(&mut self) -> Result<V> {
        let id = self.new_id();
        let mut error = IndexMap::new();
        error.insert("__error__".to_string(), V::Bool(true));
        loop {
            let tag = self.tag()?;
            match tag {
                b'.' => break,
                b'E' | b'K' | b'R' | b'F' | b'S' | b'T' | b'U' => {
                    error.insert("kind".to_string(), V::Str(char::from(tag).to_string()));
                }
                b'm' => {
                    let value = self.read_object()?;
                    error.insert("message".to_string(), value);
                }
                b's' => {
                    let value = self.read_object()?;
                    error.insert("stack".to_string(), value);
                }
                b'c' => {
                    let value = self.read_object()?;
                    error.insert("cause".to_string(), value);
                }
                _ => return fail(format!("unknown error sub-tag {:?}", char::from(tag))),
            }
        }
        let value = V::Map(Rc::new(RefCell::new(error)));
        self.set(id, value.clone());
        Ok(value)
    }
}

/// JavaScript property keys are strings.
fn property_key(key: &V) -> String {
    match key {
        V::Str(s) => s.clone(),
        V::Int(i) => i.to_string(),
        V::Float(f) => f.to_string(),
        V::Bool(b) => b.to_string(),
        V::Null => "null".to_string(),
        V::Bytes(_) | V::List(_) | V::Map(_) => "[object]".to_string(),
    }
}

fn clean_double(value: f64) -> V {
    if !value.is_finite() {
        return V::Null;
    }
    if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        return V::Int(value as i64);
    }
    V::Float(value)
}

fn into_json(value: &V, stack: &mut Vec<*const ()>) -> Value {
    match value {
        V::Null => Value::Null,
        V::Bool(b) => Value::Bool(*b),
        V::Int(i) => Value::from(*i),
        V::Float(f) => Number::from_f64(*f).map_or(Value::Null, Value::Number),
        V::Str(s) => Value::String(s.clone()),
        V::Bytes(n) => {
            let mut map = Map::new();
            map.insert(BYTES_KEY.to_string(), Value::from(*n));
            Value::Object(map)
        }
        V::List(items) => {
            let ptr = Rc::as_ptr(items) as *const ();
            if stack.contains(&ptr) {
                return Value::String("[circular]".to_string());
            }
            stack.push(ptr);
            let out = items.borrow().iter().map(|v| into_json(v, stack)).collect();
            stack.pop();
            Value::Array(out)
        }
        V::Map(map) => {
            let ptr = Rc::as_ptr(map) as *const ();
            if stack.contains(&ptr) {
                return Value::String("[circular]".to_string());
            }
            stack.push(ptr);
            let out: Map<String, Value> = map
                .borrow()
                .iter()
                .map(|(k, v)| (k.clone(), into_json(v, stack)))
                .collect();
            stack.pop();
            Value::Object(out)
        }
    }
}

/// Binary values (ArrayBuffers and views) decode to `{"__bytes__": length}`.
pub const BYTES_KEY: &str = "__bytes__";

/// Decode one blob's raw bytes into JSON values.
pub fn decode_bytes(data: &[u8]) -> Result<Value> {
    let payload = unwrap_idb_value(data)?;
    let start = v8_stream_start(&payload)?;
    let mut de = Deserializer::new(&payload, start);
    let value = de.read_value()?;
    drop(de);
    Ok(into_json(&value, &mut Vec::new()))
}

// Slack ids (C0123ABC, U0…), message ts and keys such as "message-<ts>-<cid>".
static ID_LIKE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d{3,}|^[A-Z][A-Z0-9]{7,}$").expect("valid regex"));

/// Describe a decoded value's structure without printing contents.
pub fn shape(value: &Value, depth: i64) -> String {
    shape_at(value, depth, 0)
}

fn shape_at(value: &Value, depth: i64, indent: usize) -> String {
    let pad = "  ".repeat(indent);
    match value {
        Value::Object(map) => {
            if map.len() == 1 {
                if let Some(n) = map.get(BYTES_KEY) {
                    return format!("bytes({n})");
                }
            }
            if depth <= 0 || map.is_empty() {
                return format!("dict[{}]", map.len());
            }
            let mut lines = vec![format!("dict[{}]", map.len())];
            let keys: Vec<&String> = map.keys().collect();
            let sample_ids = keys.len() > 12
                && map.values().take(12).all(Value::is_object)
                && keys.iter().filter(|k| ID_LIKE.is_match(k)).count() * 2 > keys.len();
            let shown = keys.len().min(if sample_ids { 3 } else { 60 });
            for key in &keys[..shown] {
                let label = if sample_ids { "<id>" } else { key.as_str() };
                let inner = shape_at(&map[key.as_str()], depth - 1, indent + 1);
                lines.push(format!("{pad}  {label}: {inner}"));
            }
            if keys.len() > shown {
                lines.push(format!("{pad}  … {} more keys", keys.len() - shown));
            }
            lines.join("\n")
        }
        Value::Array(items) => {
            if depth <= 0 || items.is_empty() {
                return format!("list[{}]", items.len());
            }
            format!(
                "list[{}] of {}",
                items.len(),
                shape_at(&items[0], depth - 1, indent + 1)
            )
        }
        Value::String(s) => format!("str({})", s.chars().count()),
        Value::Number(n) if n.is_f64() => "float".to_string(),
        Value::Number(_) => "int".to_string(),
        Value::Bool(_) => "bool".to_string(),
        Value::Null => "null".to_string(),
    }
}
