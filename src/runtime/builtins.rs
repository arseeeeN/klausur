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

/// An `application/x-www-form-urlencoded` body built with `form(#{ … })`, like an HTML form
/// posts it. Arrays become repeated fields, `()` leaves a field out.
#[derive(Debug, Clone)]
pub struct Form(pub Vec<(String, String)>);

impl Form {
    fn new(fields: Map) -> R<Form> {
        let mut pairs = Vec::new();
        for (name, value) in fields {
            let values = match value.clone().try_cast::<Array>() {
                Some(items) => items,
                None => vec![value],
            };
            for v in values {
                if v.is_unit() {
                    continue;
                }
                if v.is::<FileRef>() || v.is_blob() {
                    return Err(Failure::new(format!(
                        "form field `{name}` is a file: a form can only send text, files need `multipart(…)`"
                    ))
                    .into_err());
                }
                pairs.push((name.to_string(), to_text(&v)));
            }
        }
        Ok(Form(pairs))
    }

    pub fn encode(&self) -> String {
        let mut out = url::form_urlencoded::Serializer::new(String::new());
        for (name, value) in &self.0 {
            out.append_pair(name, value);
        }
        out.finish()
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

/// `watch_file(path)`: remembers how long a file is, so `.lines()` / `.text()` return what was
/// appended since, e.g. to check a service's log for new errors during a test.
#[derive(Clone)]
pub struct FileWatch {
    path: std::path::PathBuf,
    start: u64,
}

impl FileWatch {
    fn appended(&self) -> R<String> {
        let bytes = match std::fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
            Err(e) => {
                return Err(
                    Failure::new(format!("can't read {}: {e}", self.path.display())).into_err(),
                );
            }
        };
        // A file that got shorter was rotated or truncated: all of it is new.
        let start = if (bytes.len() as u64) < self.start {
            0
        } else {
            self.start as usize
        };
        Ok(String::from_utf8_lossy(&bytes[start..]).into_owned())
    }
}

/// The local time in a strftime format; an invalid pattern is an error, not a panic.
fn local_time(format: &str) -> R<String> {
    use chrono::format::{Item, StrftimeItems};
    let items: Vec<Item> = StrftimeItems::new(format).collect();
    if items.iter().any(|i| matches!(i, Item::Error)) {
        return Err(Failure::new(format!(
            "`{format}` isn't a valid date format; use strftime patterns such as \"%Y-%m-%d %H:%M\""
        ))
        .into_err());
    }
    Ok(chrono::Local::now()
        .format_with_items(items.into_iter())
        .to_string())
}

pub(crate) fn bytes_of(v: &Dynamic) -> Vec<u8> {
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
    engine
        .register_type_with_name::<Form>("Form")
        .register_fn("to_string", |f: &mut Form| f.encode());

    let s = shared.clone();
    engine.register_fn(
        "file",
        move |ncx: NativeCallContext, path: &str| -> R<FileRef> {
            let full = s.at_call(&ncx, || s.resolve_path(path));
            let bytes = std::fs::read(&full).map_err(|e| {
                Failure::new(format!("can't read {}: {e}", full.display())).into_err()
            })?;
            let name = full
                .file_name()
                .map_or_else(|| path.to_string(), |n| n.to_string_lossy().into_owned());
            Ok(FileRef {
                content_type: guess_content_type(&name).into(),
                name,
                bytes,
            })
        },
    );
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
    engine.register_fn("form", Form::new);
    let s = shared.clone();
    engine.register_fn(
        "read_blob",
        move |ncx: NativeCallContext, path: &str| -> R<Blob> {
            let full = s.at_call(&ncx, || s.resolve_path(path));
            std::fs::read(&full)
                .map_err(|e| Failure::new(format!("can't read {}: {e}", full.display())).into_err())
        },
    );

    // Writing files, e.g. into a folder a service watches. Both return the absolute path.
    let s = shared.clone();
    engine.register_fn(
        "write_file",
        move |ncx: NativeCallContext, path: &str, content: Dynamic| -> R<String> {
            let full = s.at_call(&ncx, || s.resolve_path(path));
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
    // `write_file(path, content, undo: true)`: the undo restores what was there before, or
    // removes the file if it's new, so a fixture can't destroy a file that already existed.
    let s = shared.clone();
    engine.register_fn(
        "write_file__kw",
        move |ncx: NativeCallContext, opts: rhai::Map| -> R<String> {
            let mut opts = opts;
            let (Some(path), Some(content)) = (opts.remove("_0"), opts.remove("_1")) else {
                return Err(Failure::new("`write_file` needs a path and content").into_err());
            };
            let undo = match opts.remove("undo") {
                None => false,
                Some(v) => v.as_bool().map_err(|_| {
                    Failure::new("`write_file`'s `undo` takes true or false").into_err()
                })?,
            };
            if let Some(k) = opts.keys().next() {
                return Err(
                    Failure::new(format!("`write_file` takes `undo`, not `{k}`")).into_err(),
                );
            }
            let full = s.at_call(&ncx, || s.resolve_path(&path.to_string()));
            let before = std::fs::read(&full).ok();
            // The directories this creates, innermost first, for the undo to remove again.
            let created: Vec<std::path::PathBuf> = full
                .ancestors()
                .skip(1)
                .take_while(|d| !d.as_os_str().is_empty() && !d.exists())
                .map(|d| d.to_path_buf())
                .collect();
            if let Some(dir) = full.parent() {
                std::fs::create_dir_all(dir).map_err(|e| {
                    Failure::new(format!("can't create {}: {e}", dir.display())).into_err()
                })?;
            }
            std::fs::write(&full, bytes_of(&content)).map_err(|e| {
                Failure::new(format!("can't write {}: {e}", full.display())).into_err()
            })?;
            let full = dunce::canonicalize(&full).unwrap_or(full);
            if undo {
                let target = full.clone();
                let label = match before {
                    Some(_) => format!("restore {}", full.display()),
                    None => format!("remove {}", full.display()),
                };
                let restore: Rc<dyn Fn() -> Result<(), String>> = Rc::new(move || {
                    let result = match &before {
                        Some(bytes) => std::fs::write(&target, bytes),
                        None => match std::fs::remove_file(&target) {
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                            r => r,
                        },
                    };
                    result.map_err(|e| format!("can't restore {}: {e}", target.display()))?;
                    // Directories made for the file go too, unless something else is in them now.
                    for dir in &created {
                        if std::fs::remove_dir(dir).is_err() {
                            break;
                        }
                    }
                    Ok(())
                });
                let span = ncx
                    .call_source()
                    .and_then(|src| s.map_pos(src, ncx.call_position()));
                super::undo::register_native(&s, label, restore, span);
            }
            Ok(full.to_string_lossy().into_owned())
        },
    );
    // `symlink(target, link)`: `link` resolves like every path, `target` is stored as written
    // (relative targets are relative to the link's folder). With `undo: true` the undo removes
    // the link, never what it points to, and the folders made for it.
    let s = shared.clone();
    engine.register_fn(
        "symlink",
        move |ncx: NativeCallContext, target: &str, link: &str| -> R<String> {
            symlink_fn(&s, &ncx, target, link, false)
        },
    );
    let s = shared.clone();
    engine.register_fn(
        "symlink__kw",
        move |ncx: NativeCallContext, opts: rhai::Map| -> R<String> {
            let mut opts = opts;
            let (Some(target), Some(link)) = (opts.remove("_0"), opts.remove("_1")) else {
                return Err(Failure::new("`symlink` needs a target and a link").into_err());
            };
            let undo = match opts.remove("undo") {
                None => false,
                Some(v) => v.as_bool().map_err(|_| {
                    Failure::new("`symlink`'s `undo` takes true or false").into_err()
                })?,
            };
            if let Some(k) = opts.keys().next() {
                return Err(Failure::new(format!("`symlink` takes `undo`, not `{k}`")).into_err());
            }
            symlink_fn(&s, &ncx, &target.to_string(), &link.to_string(), undo)
        },
    );
    let s = shared.clone();
    engine.register_fn("is_symlink", move |ncx: NativeCallContext, path: &str| {
        s.at_call(&ncx, || s.resolve_path(path))
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink())
    });

    // Local date and time: `now()` is ISO 8601 with the offset, `now("%d.%m.%Y")` uses strftime
    // patterns, `today()` is `2026-11-05`.
    engine.register_fn("now", || {
        chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
    });
    engine.register_fn("now", |format: &str| -> R<String> { local_time(format) });
    engine.register_fn("today", || {
        chrono::Local::now().format("%Y-%m-%d").to_string()
    });
    engine.register_type_with_name::<FileWatch>("file_watch");
    let s = shared.clone();
    engine.register_fn("watch_file", move |ncx: NativeCallContext, path: &str| {
        let path = s.at_call(&ncx, || s.resolve_path(path));
        let start = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        FileWatch { path, start }
    });
    engine.register_fn("text", |w: &mut FileWatch| w.appended());
    // Only complete lines: a writer in the middle of a line isn't done with it.
    engine.register_fn("lines", |w: &mut FileWatch| -> R<Array> {
        let text = w.appended()?;
        let complete = match text.rfind('\n') {
            Some(i) => &text[..=i],
            None => "",
        };
        Ok(complete
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| Dynamic::from(l.to_string()))
            .collect())
    });
    let s = shared.clone();
    engine.register_fn(
        "remove_file",
        move |ncx: NativeCallContext, path: &str| -> R<bool> {
            let full = s.at_call(&ncx, || s.resolve_path(path));
            match remove_link_or_file(&full) {
                Ok(()) => Ok(true),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(e) => {
                    Err(Failure::new(format!("can't remove {}: {e}", full.display())).into_err())
                }
            }
        },
    );
    let s = shared.clone();
    engine.register_fn("file_exists", move |ncx: NativeCallContext, path: &str| {
        s.at_call(&ncx, || s.resolve_path(path)).exists()
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

    // `last_index_of`, the counterpart of Rhai's `index_of`: a character position for strings,
    // an element for arrays (a value, or the last one a closure accepts); -1 when there's none.
    engine.register_fn("last_index_of", |s: &str, find: &str| -> i64 {
        s.rfind(find)
            .map_or(-1, |byte| s[..byte].chars().count() as i64)
    });
    engine.register_fn(
        "last_index_of",
        |ctx: NativeCallContext, a: &mut Array, find: Dynamic| -> R<i64> {
            let f = find.read_lock::<FnPtr>().map(|f| f.clone());
            for (i, item) in a.iter().enumerate().rev() {
                let found = match &f {
                    Some(f) => f
                        .call_within_context::<Dynamic>(&ctx, (item.clone(),))?
                        .as_bool()
                        .unwrap_or(false),
                    // Like `index_of`: values without an `==` between them aren't equal.
                    None => ctx
                        .call_native_fn::<bool>("==", (item.clone(), find.clone()))
                        .unwrap_or(false),
                };
                if found {
                    return Ok(i as i64);
                }
            }
            Ok(-1)
        },
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

fn symlink_fn(
    s: &Rc<Shared>,
    ncx: &NativeCallContext,
    target: &str,
    link: &str,
    undo: bool,
) -> R<String> {
    let full = s.at_call(ncx, || s.resolve_path(link));
    if full.symlink_metadata().is_ok() {
        return Err(Failure::new(format!(
            "can't create the symlink {}: something is there already",
            full.display()
        ))
        .into_err());
    }
    // The directories this creates, innermost first, for the undo to remove again.
    let created: Vec<std::path::PathBuf> = full
        .ancestors()
        .skip(1)
        .take_while(|d| !d.as_os_str().is_empty() && !d.exists())
        .map(|d| d.to_path_buf())
        .collect();
    if let Some(dir) = full.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| Failure::new(format!("can't create {}: {e}", dir.display())).into_err())?;
    }
    make_symlink(target, &full).map_err(|e| {
        Failure::new(format!(
            "can't create the symlink {} → {target}: {e}",
            full.display()
        ))
        .into_err()
    })?;
    // Not canonicalized: that would follow the link.
    let full = match (full.parent(), full.file_name()) {
        (Some(dir), Some(name)) => dunce::canonicalize(dir)
            .map(|d| d.join(name))
            .unwrap_or(full),
        _ => full,
    };
    if undo {
        let link = full.clone();
        let restore: Rc<dyn Fn() -> Result<(), String>> = Rc::new(move || {
            match remove_link_or_file(&link) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                r => r.map_err(|e| format!("can't remove {}: {e}", link.display()))?,
            }
            for dir in &created {
                if std::fs::remove_dir(dir).is_err() {
                    break;
                }
            }
            Ok(())
        });
        let span = ncx
            .call_source()
            .and_then(|src| s.map_pos(src, ncx.call_position()));
        let label = format!("remove the symlink {}", full.display());
        super::undo::register_native(s, label, restore, span);
    }
    Ok(full.to_string_lossy().into_owned())
}

#[cfg(unix)]
fn make_symlink(target: &str, link: &std::path::Path) -> Result<(), String> {
    std::os::unix::fs::symlink(target, link).map_err(|e| e.to_string())
}

#[cfg(windows)]
fn make_symlink(target: &str, link: &std::path::Path) -> Result<(), String> {
    // Windows has separate links for files and folders. A relative target is relative to the
    // link's folder; a target that doesn't exist (yet) gets a file link.
    let resolved = match link.parent() {
        Some(dir) => dir.join(target),
        None => std::path::PathBuf::from(target),
    };
    let result = if resolved.is_dir() {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    };
    result.map_err(|e| {
        // ERROR_PRIVILEGE_NOT_HELD
        if e.raw_os_error() == Some(1314) {
            "creating symlinks on Windows needs Developer Mode or admin rights".to_string()
        } else {
            e.to_string()
        }
    })
}

#[cfg(not(any(unix, windows)))]
fn make_symlink(_: &str, _: &std::path::Path) -> Result<(), String> {
    Err("symlinks aren't supported on this platform".into())
}

/// Remove a file or a symlink. On Windows a link to a folder is removed as a folder.
fn remove_link_or_file(path: &std::path::Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e)
            if cfg!(windows)
                && path
                    .symlink_metadata()
                    .is_ok_and(|m| m.file_type().is_symlink()) =>
        {
            std::fs::remove_dir(path).map_err(|_| e)
        }
        r => r,
    }
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
    fn form_encodes_like_an_html_form() {
        let mut fields = Map::new();
        fields.insert("user".into(), "a b&c=d".into());
        fields.insert(
            "tags".into(),
            Dynamic::from_array(vec!["x".into(), "ü".into()]),
        );
        fields.insert("left_out".into(), Dynamic::UNIT);
        fields.insert("n".into(), Dynamic::from_int(3));
        let form = Form::new(fields).unwrap();
        assert_eq!(form.encode(), "n=3&tags=x&tags=%C3%BC&user=a+b%26c%3Dd");
    }

    #[test]
    fn form_rejects_files() {
        let mut fields = Map::new();
        fields.insert(
            "doc".into(),
            Dynamic::from(FileRef {
                name: "a.pdf".into(),
                content_type: "application/pdf".into(),
                bytes: vec![],
            }),
        );
        let err = match *Form::new(fields).unwrap_err() {
            EvalAltResult::ErrorRuntime(v, _) => v.cast::<Failure>().message,
            other => other.to_string(),
        };
        assert!(err.contains("form field `doc` is a file"), "{err}");
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
