//! Converting between Rhai values, JSON and text.

use rhai::{Dynamic, Map};

/// Text inserted verbatim by `raw(x)`: no URL encoding, no JSON quoting.
#[derive(Debug, Clone)]
pub struct RawText(pub String);

/// A type name on the right-hand side of `is` (`x is string`).
#[derive(Debug, Clone)]
pub struct TypeTag(pub String);

pub const TYPE_NAMES: &[&str] = &[
    "string", "int", "float", "number", "bool", "array", "map", "blob",
];

pub fn type_matches(value: &Dynamic, tag: &str) -> bool {
    match tag {
        "string" => value.is_string() || value.is_char(),
        "int" => value.is_int(),
        "float" => value.is_float(),
        "number" => value.is_int() || value.is_float(),
        "bool" => value.is_bool(),
        "array" => value.is_array(),
        "map" => value.is_map(),
        "blob" => value.is_blob(),
        "null" => value.is_unit(),
        _ => false,
    }
}

/// A friendly type name for messages.
pub fn type_name(value: &Dynamic) -> String {
    if value.is_unit() {
        return "null".into();
    }
    if value.is_string() {
        return "string".into();
    }
    if value.is_int() {
        return "int".into();
    }
    if value.is_float() {
        return "float".into();
    }
    value
        .type_name()
        .rsplit("::")
        .next()
        .unwrap_or("value")
        .to_string()
}

pub fn to_json(value: &Dynamic) -> Result<serde_json::Value, String> {
    if let Some(raw) = value.read_lock::<RawText>() {
        return serde_json::from_str(&raw.0)
            .map_err(|e| format!("raw(...) value isn't valid JSON: {e}"));
    }
    if value.is_char() {
        return Ok(serde_json::Value::String(value.to_string()));
    }
    rhai::serde::from_dynamic::<serde_json::Value>(value)
        .map_err(|e| format!("can't convert a {} to JSON: {e}", type_name(value)))
}

pub fn from_json(value: &serde_json::Value) -> Dynamic {
    rhai::serde::to_dynamic(value).unwrap_or(Dynamic::UNIT)
}

/// Render a value for insertion into text (URLs, headers, raw bodies).
pub fn to_text(value: &Dynamic) -> String {
    if let Some(raw) = value.read_lock::<RawText>() {
        return raw.0.clone();
    }
    if value.is_unit() {
        return String::new();
    }
    if value.is_array() || value.is_map() {
        return to_json(value)
            .map(|v| v.to_string())
            .unwrap_or_else(|_| value.to_string());
    }
    value.to_string()
}

/// Render a value for humans: JSON-like, strings quoted.
pub fn display(value: &Dynamic) -> String {
    if value.is_string() {
        return serde_json::Value::String(value.to_string()).to_string();
    }
    if value.is_unit() {
        return "null".into();
    }
    if (value.is_array() || value.is_map())
        && let Ok(json) = to_json(value)
    {
        return json.to_string();
    }
    if value.is_blob() {
        return format!("<{} bytes>", value.clone().cast::<rhai::Blob>().len());
    }
    if value.is::<rhai::FnPtr>() {
        return format!(
            "<function {}>",
            value.clone().cast::<rhai::FnPtr>().fn_name()
        );
    }
    value.to_string()
}

/// Like [`display`], but strings are shown without quotes (CLI case filters, args).
pub fn display_plain(value: &Dynamic) -> String {
    if value.is_string() {
        value.to_string()
    } else {
        display(value)
    }
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}… ({} more bytes)", s.len() - cut.len())
}

pub fn map_of(pairs: impl IntoIterator<Item = (impl Into<rhai::Identifier>, Dynamic)>) -> Map {
    pairs.into_iter().map(|(k, v)| (k.into(), v)).collect()
}
