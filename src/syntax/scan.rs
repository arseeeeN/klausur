//! Low-level scanning: comments, strings, bracket matching and statement ends.
//!
//! klausur files mix three kinds of text: Rhai code, request text (`connection`)
//! and JSON templates (`send`). Each has its own rules for where a block ends.

pub type ScanResult<T> = Result<T, ScanError>;

#[derive(Debug, Clone)]
pub struct ScanError {
    pub pos: usize,
    pub message: String,
}

fn err<T>(pos: usize, message: impl Into<String>) -> ScanResult<T> {
    Err(ScanError {
        pos,
        message: message.into(),
    })
}

fn byte(src: &str, pos: usize) -> Option<u8> {
    src.as_bytes().get(pos).copied()
}

pub fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

pub fn is_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Identifier starting exactly at `pos`, if any.
pub fn ident_at(src: &str, pos: usize) -> Option<(usize, usize)> {
    let bytes = src.as_bytes();
    if !bytes.get(pos).is_some_and(|&c| is_ident_start(c)) {
        return None;
    }
    let mut end = pos;
    while bytes.get(end).is_some_and(|&c| is_ident_char(c)) {
        end += 1;
    }
    Some((pos, end))
}

/// Skip whitespace and `//` / `/* */` comments (code mode).
pub fn skip_trivia(src: &str, mut pos: usize) -> ScanResult<usize> {
    let bytes = src.as_bytes();
    loop {
        while bytes.get(pos).is_some_and(|c| c.is_ascii_whitespace()) {
            pos += 1;
        }
        if src[pos..].starts_with("//") {
            pos = src[pos..].find('\n').map_or(src.len(), |i| pos + i);
        } else if src[pos..].starts_with("/*") {
            pos = skip_block_comment(src, pos)?;
        } else {
            return Ok(pos);
        }
    }
}

/// Like [`skip_trivia`] but stops at a newline.
pub fn skip_inline_trivia(src: &str, mut pos: usize) -> ScanResult<usize> {
    let bytes = src.as_bytes();
    loop {
        while bytes
            .get(pos)
            .is_some_and(|&c| c == b' ' || c == b'\t' || c == b'\r')
        {
            pos += 1;
        }
        if src[pos..].starts_with("//") {
            pos = src[pos..].find('\n').map_or(src.len(), |i| pos + i);
        } else if src[pos..].starts_with("/*") {
            pos = skip_block_comment(src, pos)?;
        } else {
            return Ok(pos);
        }
    }
}

fn skip_block_comment(src: &str, start: usize) -> ScanResult<usize> {
    let mut depth = 0;
    let mut pos = start;
    while pos < src.len() {
        if src[pos..].starts_with("/*") {
            depth += 1;
            pos += 2;
        } else if src[pos..].starts_with("*/") {
            depth -= 1;
            pos += 2;
            if depth == 0 {
                return Ok(pos);
            }
        } else {
            pos += 1;
        }
    }
    err(start, "unterminated block comment")
}

/// Skip a Rhai string or character literal starting at `pos`; returns the position after it.
pub fn skip_string(src: &str, start: usize) -> ScanResult<usize> {
    let quote = byte(src, start).unwrap_or(0);
    let mut pos = start + 1;
    while let Some(c) = byte(src, pos) {
        match c {
            b'\\' if quote != b'`' => pos += 2,
            b'$' if quote == b'`' && byte(src, pos + 1) == Some(b'{') => {
                pos = match_close(src, pos + 1)? + 1;
            }
            c if c == quote => return Ok(pos + 1),
            _ => pos += 1,
        }
    }
    let what = if quote == b'\'' {
        "character literal"
    } else {
        "string"
    };
    err(start, format!("unterminated {what}"))
}

fn closer(open: u8) -> u8 {
    match open {
        b'(' => b')',
        b'[' => b']',
        _ => b'}',
    }
}

/// Find the bracket matching the one at `open` in Rhai code. Returns its position.
pub fn match_close(src: &str, open: usize) -> ScanResult<usize> {
    let mut stack = vec![(byte(src, open).unwrap_or(b'{'), open)];
    let mut pos = open + 1;
    while pos < src.len() {
        let c = src.as_bytes()[pos];
        match c {
            b'/' if matches!(byte(src, pos + 1), Some(b'/' | b'*')) => {
                pos = skip_trivia(src, pos)?;
                continue;
            }
            b'"' | b'`' | b'\'' => {
                pos = skip_string(src, pos)?;
                continue;
            }
            b'(' | b'[' | b'{' => stack.push((c, pos)),
            b')' | b']' | b'}' => {
                let (o, _) = stack.pop().unwrap();
                if closer(o) != c {
                    return err(
                        pos,
                        format!(
                            "mismatched `{}`, expected `{}`",
                            c as char,
                            closer(o) as char
                        ),
                    );
                }
                if stack.is_empty() {
                    return Ok(pos);
                }
            }
            _ => {}
        }
        pos += 1;
    }
    let (o, opos) = stack.pop().unwrap();
    err(opos, format!("unclosed `{}`", o as char))
}

/// Find the end of a Rhai statement or expression starting at `start`.
///
/// The statement ends at `;`, at an unmatched closing bracket, or at a newline unless the
/// line obviously continues (it ends with an operator, or the next line starts with one).
/// Returns the end position (exclusive, not including the `;`).
pub fn stmt_end(src: &str, start: usize) -> ScanResult<usize> {
    let bytes = src.as_bytes();
    let mut pos = start;
    let mut last_sig: Option<u8> = None;
    while pos < src.len() {
        let c = bytes[pos];
        match c {
            b'/' if matches!(byte(src, pos + 1), Some(b'/' | b'*')) => {
                let next = skip_inline_trivia(src, pos)?;
                if next == pos {
                    pos += 1;
                } else {
                    pos = next;
                }
                continue;
            }
            b'"' | b'`' | b'\'' => {
                pos = skip_string(src, pos)?;
                last_sig = Some(b'"');
                continue;
            }
            b'(' | b'[' | b'{' => {
                pos = match_close(src, pos)? + 1;
                last_sig = Some(b')');
                continue;
            }
            b';' | b')' | b']' | b'}' => return Ok(pos),
            b'\n' => {
                let continues_line = matches!(
                    last_sig,
                    Some(
                        b'+' | b'-'
                            | b'*'
                            | b'/'
                            | b'%'
                            | b'&'
                            | b'|'
                            | b'^'
                            | b'='
                            | b'<'
                            | b'>'
                            | b'!'
                            | b','
                            | b'.'
                            | b':'
                            | b'?'
                    )
                );
                if !continues_line && !next_line_continues(src, pos + 1)? {
                    return Ok(pos);
                }
            }
            c if c.is_ascii_whitespace() => {}
            c => last_sig = Some(c),
        }
        pos += 1;
    }
    Ok(src.len())
}

fn next_line_continues(src: &str, pos: usize) -> ScanResult<bool> {
    let pos = skip_trivia(src, pos)?;
    let rest = &src[pos..];
    Ok(rest.starts_with('.') && !rest.starts_with("..")
        || rest.starts_with("&&")
        || rest.starts_with("||")
        || rest.starts_with("?.")
        || rest.starts_with("|>"))
}

/// Trim whitespace (and trailing comments) from a range, returning the trimmed range.
pub fn trim_range(src: &str, start: usize, end: usize) -> (usize, usize) {
    let s = &src[start..end];
    let lead = s.len() - s.trim_start().len();
    let trail = s.len() - s.trim_end().len();
    if lead + trail >= s.len() {
        return (start, start);
    }
    (start + lead, end - trail)
}

/// Inside `{{ … }}`: given the position right after `{{`, find the position of the closing `}}`.
pub fn scan_interp(src: &str, start: usize) -> ScanResult<usize> {
    let mut pos = start;
    while pos < src.len() {
        let c = src.as_bytes()[pos];
        match c {
            b'"' | b'`' | b'\'' => {
                pos = skip_string(src, pos)?;
                continue;
            }
            b'(' | b'[' | b'{' => {
                pos = match_close(src, pos)? + 1;
                continue;
            }
            b'}' if byte(src, pos + 1) == Some(b'}') => return Ok(pos),
            b')' | b']' | b'}' => {
                return err(pos, format!("unbalanced `{}` in interpolation", c as char));
            }
            _ => {}
        }
        pos += 1;
    }
    err(start.saturating_sub(2), "unterminated `{{`")
}

/// True if `//` at `pos` starts a comment in request text: at line start or after whitespace.
pub fn is_text_comment(src: &str, pos: usize) -> bool {
    src[pos..].starts_with("//") && (pos == 0 || src.as_bytes()[pos - 1].is_ascii_whitespace())
}

/// Match the `}` closing a request-text block (`connection { … }`).
pub fn match_close_text(src: &str, open: usize) -> ScanResult<usize> {
    let mut depth = 0usize;
    let mut pos = open;
    while pos < src.len() {
        let c = src.as_bytes()[pos];
        if c == b'{' && byte(src, pos + 1) == Some(b'{') {
            pos = scan_interp(src, pos + 2)? + 2;
            continue;
        }
        if is_text_comment(src, pos) {
            pos = src[pos..].find('\n').map_or(src.len(), |i| pos + i);
            continue;
        }
        match c {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(pos);
                }
            }
            _ => {}
        }
        pos += 1;
    }
    err(open, "unclosed `{`")
}

/// Match the bracket closing a JSON template (`send { … }` / `send [ … ]`).
pub fn match_close_json(src: &str, open: usize) -> ScanResult<usize> {
    let mut stack: Vec<(u8, usize)> = Vec::new();
    let mut in_string = false;
    let mut pos = open;
    while pos < src.len() {
        let c = src.as_bytes()[pos];
        if c == b'{' && byte(src, pos + 1) == Some(b'{') {
            pos = scan_interp(src, pos + 2)? + 2;
            continue;
        }
        if in_string {
            match c {
                b'\\' => pos += 1,
                b'"' => in_string = false,
                _ => {}
            }
        } else {
            match c {
                b'"' => in_string = true,
                b'{' | b'[' => stack.push((c, pos)),
                b'}' | b']' => {
                    let Some((o, _)) = stack.pop() else {
                        return err(pos, format!("unbalanced `{}`", c as char));
                    };
                    if closer(o) != c {
                        return err(
                            pos,
                            format!(
                                "mismatched `{}`, expected `{}`",
                                c as char,
                                closer(o) as char
                            ),
                        );
                    }
                    if stack.is_empty() {
                        return Ok(pos);
                    }
                }
                _ => {}
            }
        }
        pos += 1;
    }
    err(open, format!("unclosed `{}`", src.as_bytes()[open] as char))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_code_brackets_across_strings_and_comments() {
        let src = r#"{ let a = "}"; // }
            let b = `x ${ #{a: 1}.a } }`; /* } */ }"#;
        assert_eq!(match_close(src, 0).unwrap(), src.len() - 1);
    }

    #[test]
    fn statement_ends_at_newline_unless_continued() {
        let src = "a + \n b\nc";
        assert_eq!(&src[..stmt_end(src, 0).unwrap()], "a + \n b");
        let src = "foo()\n  .bar()\nnext";
        assert_eq!(&src[..stmt_end(src, 0).unwrap()], "foo()\n  .bar()");
        let src = "#{\n a: 1\n}\nnext";
        assert_eq!(&src[..stmt_end(src, 0).unwrap()], "#{\n a: 1\n}");
    }

    #[test]
    fn text_blocks_keep_urls_and_skip_comments() {
        let src = "{ GET https://x/{{ #{a: 1}.a }} // }\n }";
        assert_eq!(match_close_text(src, 0).unwrap(), src.len() - 1);
    }

    #[test]
    fn json_blocks_handle_interpolation_and_strings() {
        let src = r#"{ "a": {{x}}, "b": "}{{y}}", "c": [1, {}] }"#;
        assert_eq!(match_close_json(src, 0).unwrap(), src.len() - 1);
    }
}
