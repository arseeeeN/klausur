//! Built-in helpers for testing real services: files and multipart bodies, server-sent events,
//! base64 and JWT decoding, and regular expressions.

use super::value::{self, map_of, to_text};
use super::{Failure, Shared};
use rhai::{Array, Blob, Dynamic, Engine, EvalAltResult, FnPtr, Map, NativeCallContext};
use std::any::TypeId;
use std::path::Path;
use std::rc::Rc;

type R<T> = Result<T, Box<EvalAltResult>>;

/// A file to send: as a multipart part, or on its own with `send = file(...)`.
#[derive(Debug, Clone)]
pub struct FileRef {
    pub name: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

/// A `multipart/form-data` body built with `multipart(#{ … })`.
#[derive(Debug, Clone)]
pub struct Multipart(pub Vec<(String, Dynamic)>);

impl Multipart {
    /// Encode the parts; returns the body and the content type with its boundary.
    pub fn encode(&self, boundary: &str) -> (Vec<u8>, String) {
        let mut out = Vec::new();
        for (name, value) in &self.0 {
            let values: Vec<Dynamic> = match value.clone().try_cast::<Array>() {
                Some(items) => items,
                None => vec![value.clone()],
            };
            for v in values {
                if v.is_unit() {
                    continue;
                }
                out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
                if let Some(f) = v.read_lock::<FileRef>() {
                    out.extend_from_slice(
                        format!(
                            "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\nContent-Type: {}\r\n\r\n",
                            escape(name),
                            escape(&f.name),
                            f.content_type
                        )
                        .as_bytes(),
                    );
                    out.extend_from_slice(&f.bytes);
                } else {
                    out.extend_from_slice(
                        format!(
                            "Content-Disposition: form-data; name=\"{}\"\r\n\r\n",
                            escape(name)
                        )
                        .as_bytes(),
                    );
                    if v.is_blob() {
                        out.extend_from_slice(&v.cast::<Blob>());
                    } else {
                        out.extend_from_slice(to_text(&v).as_bytes());
                    }
                }
                out.extend_from_slice(b"\r\n");
            }
        }
        out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        (out, format!("multipart/form-data; boundary={boundary}"))
    }
}

fn escape(s: &str) -> String {
    s.replace('"', "%22")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

pub fn guess_content_type(name: &str) -> &'static str {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "json" => "application/json",
        "xml" => "application/xml",
        "html" | "htm" => "text/html",
        "txt" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "zip" => "application/zip",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    }
}

fn bytes_of(v: &Dynamic) -> Vec<u8> {
    if v.is_blob() {
        return v.clone().cast::<Blob>();
    }
    to_text(v).into_bytes()
}

/// `sorted(a)` / `sorted(a, |x, y| …)`: a sorted copy, using Rhai's own `sort`.
fn sorted(ctx: NativeCallContext, args: &mut [&mut Dynamic]) -> R<Array> {
    let mut copy = args[0].clone();
    let mut rest: Vec<Dynamic> = args[1..].iter().map(|d| (**d).clone()).collect();
    let mut call: Vec<&mut Dynamic> = std::iter::once(&mut copy).chain(rest.iter_mut()).collect();
    let _ = ctx.call_native_fn_raw("sort", true, &mut call)?;
    Ok(copy.cast::<Array>())
}

// --- Server-sent events -------------------------------------------------------------------

/// Parse a `text/event-stream` body into `#{ event, data, id }` maps. JSON data is parsed.
pub fn parse_sse(text: &str) -> Array {
    let mut events = Array::new();
    let text = text.replace("\r\n", "\n");
    for block in text.split("\n\n") {
        let mut event = String::from("message");
        let mut data: Vec<&str> = Vec::new();
        let mut id = Dynamic::UNIT;
        for line in block.lines() {
            if line.starts_with(':') {
                continue;
            }
            let (field, val) = line.split_once(':').unwrap_or((line, ""));
            let val = val.strip_prefix(' ').unwrap_or(val);
            match field {
                "event" => event = val.to_string(),
                "data" => data.push(val),
                "id" => id = val.to_string().into(),
                _ => {}
            }
        }
        if data.is_empty() {
            continue;
        }
        let joined = data.join("\n");
        let parsed = match serde_json::from_str::<serde_json::Value>(&joined) {
            Ok(json) => value::from_json(&json),
            Err(_) => joined.into(),
        };
        events.push(map_of([("event", event.into()), ("data", parsed), ("id", id)]).into());
    }
    events
}

// --- base64 -------------------------------------------------------------------------------

const STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub fn base64_encode(bytes: &[u8], url: bool) -> String {
    let alphabet = if url { URL } else { STD };
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(alphabet[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else if !url {
                out.push('=');
            }
        }
    }
    out
}

/// Decode standard or URL-safe base64, with or without padding.
pub fn base64_decode(text: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0;
    for c in text.trim().chars() {
        let v = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            'a'..='z' => c as u32 - 'a' as u32 + 26,
            '0'..='9' => c as u32 - '0' as u32 + 52,
            '+' | '-' => 62,
            '/' | '_' => 63,
            '=' => break,
            c if c.is_whitespace() => continue,
            c => return Err(format!("invalid base64 character {c:?}")),
        };
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

fn decode_text(text: &str) -> R<String> {
    let bytes = base64_decode(text).map_err(|e| Failure::new(e).into_err())?;
    String::from_utf8(bytes).map_err(|_| {
        Failure::new("base64 data isn't UTF-8 text; use base64_decode_blob").into_err()
    })
}

fn json_part(part: &str, what: &str) -> R<Dynamic> {
    let text = decode_text(part)?;
    let json: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| Failure::new(format!("the JWT {what} isn't JSON: {e}")).into_err())?;
    Ok(value::from_json(&json))
}

/// Decode a JWT without verifying it: `#{ header, claims, signature }`.
pub fn jwt_decode(token: &str) -> R<Map> {
    let token = token.trim().strip_prefix("Bearer ").unwrap_or(token.trim());
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(
            Failure::new(format!("a JWT has 3 parts, this has {}", parts.len())).into_err(),
        );
    }
    Ok(map_of([
        ("header", json_part(parts[0], "header")?),
        ("claims", json_part(parts[1], "claims")?),
        ("signature", parts[2].to_string().into()),
    ]))
}

// --- Regular expressions --------------------------------------------------------------------

fn regex(pattern: &str) -> R<regex::Regex> {
    regex::Regex::new(pattern).map_err(|e| Failure::new(format!("invalid regex: {e}")).into_err())
}

fn captures_map(re: &regex::Regex, caps: &regex::Captures) -> Dynamic {
    let names: Vec<Option<&str>> = re.capture_names().collect();
    let has_names = names.iter().any(Option::is_some);
    if has_names {
        let mut m = Map::new();
        for (i, name) in names.iter().enumerate() {
            let v = caps
                .get(i)
                .map_or(Dynamic::UNIT, |c| c.as_str().to_string().into());
            m.insert(name.map_or_else(|| i.to_string(), str::to_string).into(), v);
        }
        return m.into();
    }
    let groups: Array = (0..caps.len())
        .map(|i| {
            caps.get(i)
                .map_or(Dynamic::UNIT, |c| c.as_str().to_string().into())
        })
        .collect();
    groups.into()
}

pub fn register(engine: &mut Engine, shared: &Rc<Shared>) {
    engine
        .register_type_with_name::<FileRef>("File")
        .register_get("name", |f: &mut FileRef| f.name.clone())
        .register_get_set(
            "content_type",
            |f: &mut FileRef| f.content_type.clone(),
            |f: &mut FileRef, ct: String| f.content_type = ct,
        )
        .register_get("size", |f: &mut FileRef| f.bytes.len() as i64)
        .register_fn("to_string", |f: &mut FileRef| {
            format!("<file {} ({} bytes)>", f.name, f.bytes.len())
        });
    engine
        .register_type_with_name::<Multipart>("Multipart")
        .register_fn("to_string", |m: &mut Multipart| {
            format!("<multipart with {} fields>", m.0.len())
        });

    let s = shared.clone();
    engine.register_fn("file", move |path: &str| -> R<FileRef> {
        let full = s.resolve_path(path);
        let bytes = std::fs::read(&full)
            .map_err(|e| Failure::new(format!("can't read {}: {e}", full.display())).into_err())?;
        let name = full
            .file_name()
            .map_or_else(|| path.to_string(), |n| n.to_string_lossy().into_owned());
        Ok(FileRef {
            content_type: guess_content_type(&name).into(),
            name,
            bytes,
        })
    });
    engine.register_fn("file_from", |name: &str, content: Dynamic| FileRef {
        name: name.to_string(),
        content_type: guess_content_type(name).into(),
        bytes: bytes_of(&content),
    });
    engine.register_fn(
        "file_from",
        |name: &str, content: Dynamic, content_type: &str| FileRef {
            name: name.to_string(),
            content_type: content_type.to_string(),
            bytes: bytes_of(&content),
        },
    );
    engine.register_fn("multipart", |fields: Map| {
        let mut parts: Vec<(String, Dynamic)> = fields
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        // Plain fields first, then files: some servers expect metadata before the payload.
        parts.sort_by_key(|(_, v)| v.is::<FileRef>());
        Multipart(parts)
    });
    let s = shared.clone();
    engine.register_fn("read_blob", move |path: &str| -> R<Blob> {
        let full = s.resolve_path(path);
        std::fs::read(&full)
            .map_err(|e| Failure::new(format!("can't read {}: {e}", full.display())).into_err())
    });

    // Writing files, e.g. into a folder a service watches. Both return the absolute path.
    let s = shared.clone();
    engine.register_fn(
        "write_file",
        move |path: &str, content: Dynamic| -> R<String> {
            let full = s.resolve_path(path);
            if let Some(dir) = full.parent() {
                std::fs::create_dir_all(dir).map_err(|e| {
                    Failure::new(format!("can't create {}: {e}", dir.display())).into_err()
                })?;
            }
            std::fs::write(&full, bytes_of(&content)).map_err(|e| {
                Failure::new(format!("can't write {}: {e}", full.display())).into_err()
            })?;
            let full = dunce::canonicalize(&full).unwrap_or(full);
            Ok(full.to_string_lossy().into_owned())
        },
    );
    let s = shared.clone();
    engine.register_fn("remove_file", move |path: &str| -> R<bool> {
        let full = s.resolve_path(path);
        match std::fs::remove_file(&full) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(Failure::new(format!("can't remove {}: {e}", full.display())).into_err()),
        }
    });
    let s = shared.clone();
    engine.register_fn("file_exists", move |path: &str| {
        s.resolve_path(path).exists()
    });

    // Rhai's `trim`, `sort` and `reverse` change the value in place and return `()`, so
    // `let t = s.trim()` silently gives `()`. These return the changed copy instead.
    engine.register_fn("trimmed", |s: &str| s.trim().to_string());
    engine.register_fn("reversed", |mut a: Array| {
        a.reverse();
        a
    });
    engine.register_raw_fn("sorted", [TypeId::of::<Array>()], sorted);
    engine.register_raw_fn(
        "sorted",
        [TypeId::of::<Array>(), TypeId::of::<FnPtr>()],
        sorted,
    );

    engine.register_fn("sse_parse", |text: &str| parse_sse(text));

    engine.register_fn("base64_encode", |v: Dynamic| {
        base64_encode(&bytes_of(&v), false)
    });
    engine.register_fn("base64url_encode", |v: Dynamic| {
        base64_encode(&bytes_of(&v), true)
    });
    engine.register_fn("base64_decode", |text: &str| decode_text(text));
    engine.register_fn("base64_decode_blob", |text: &str| -> R<Blob> {
        base64_decode(text).map_err(|e| Failure::new(e).into_err())
    });
    engine.register_fn("jwt_decode", |token: &str| jwt_decode(token));

    engine.register_fn("regex_match", |text: &str, pattern: &str| -> R<bool> {
        Ok(regex(pattern)?.is_match(text))
    });
    engine.register_fn("regex_find", |text: &str, pattern: &str| -> R<Dynamic> {
        Ok(regex(pattern)?
            .find(text)
            .map_or(Dynamic::UNIT, |m| m.as_str().to_string().into()))
    });
    engine.register_fn("regex_find_all", |text: &str, pattern: &str| -> R<Array> {
        Ok(regex(pattern)?
            .find_iter(text)
            .map(|m| m.as_str().to_string().into())
            .collect())
    });
    engine.register_fn(
        "regex_captures",
        |text: &str, pattern: &str| -> R<Dynamic> {
            let re = regex(pattern)?;
            Ok(re
                .captures(text)
                .map_or(Dynamic::UNIT, |c| captures_map(&re, &c)))
        },
    );
    engine.register_fn(
        "regex_replace",
        |text: &str, pattern: &str, with: &str| -> R<String> {
            Ok(regex(pattern)?.replace_all(text, with).into_owned())
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips() {
        for input in ["", "f", "fo", "foo", "foob", "fooba", "foobar", "ÿ€ü"] {
            let enc = base64_encode(input.as_bytes(), false);
            assert_eq!(base64_decode(&enc).unwrap(), input.as_bytes());
            let enc = base64_encode(input.as_bytes(), true);
            assert_eq!(base64_decode(&enc).unwrap(), input.as_bytes());
        }
        assert_eq!(base64_encode(b"foobar", false), "Zm9vYmFy");
        assert_eq!(base64_encode(b"fo", false), "Zm8=");
        assert_eq!(base64_encode(b"fo", true), "Zm8");
    }

    #[test]
    fn sse_events_are_parsed() {
        let text = "event:config\ndata:{\"a\":1}\n\n: comment\nevent: chat\ndata: hello\ndata: world\nid: 7\n\n";
        let events = parse_sse(text);
        assert_eq!(events.len(), 2);
        let first = events[0].clone().cast::<Map>();
        assert_eq!(first["event"].to_string(), "config");
        assert!(first["data"].is_map());
        let second = events[1].clone().cast::<Map>();
        assert_eq!(second["data"].to_string(), "hello\nworld");
        assert_eq!(second["id"].to_string(), "7");
    }

    #[test]
    fn multipart_encodes_fields_and_files() {
        let m = Multipart(vec![
            ("url".into(), "x://1".into()),
            (
                "file".into(),
                Dynamic::from(FileRef {
                    name: "a.md".into(),
                    content_type: "text/markdown".into(),
                    bytes: b"# hi".to_vec(),
                }),
            ),
        ]);
        let (body, ct) = m.encode("B");
        assert_eq!(ct, "multipart/form-data; boundary=B");
        let body = String::from_utf8(body).unwrap();
        assert_eq!(
            body,
            "--B\r\nContent-Disposition: form-data; name=\"url\"\r\n\r\nx://1\r\n--B\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.md\"\r\nContent-Type: text/markdown\r\n\r\n# hi\r\n--B--\r\n"
        );
    }
}
