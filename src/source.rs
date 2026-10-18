//! Source files, spans, and mapping generated Rhai code back to the original text.

use std::path::{Path, PathBuf};

pub type FileId = usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub file: FileId,
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(file: FileId, start: usize, end: usize) -> Self {
        Span { file, start, end }
    }

    pub fn range(self) -> std::ops::Range<usize> {
        self.start..self.end
    }
}

pub struct SourceFile {
    pub path: PathBuf,
    /// Path as shown to the user (relative to the working directory when possible).
    pub name: String,
    pub text: String,
}

#[derive(Default)]
pub struct Sources {
    pub files: Vec<SourceFile>,
}

impl Sources {
    pub fn add(&mut self, path: &Path, text: String) -> FileId {
        let name = display_path(path);
        self.files.push(SourceFile {
            path: path.to_path_buf(),
            name,
            text,
        });
        self.files.len() - 1
    }

    pub fn get(&self, id: FileId) -> &SourceFile {
        &self.files[id]
    }

    /// 1-based line and column of a byte offset.
    pub fn line_col(&self, file: FileId, offset: usize) -> (usize, usize) {
        let text = &self.files[file].text;
        let offset = offset.min(text.len());
        let before = &text[..offset];
        let line = before.matches('\n').count() + 1;
        let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        (line, col)
    }
}

pub fn display_path(path: &Path) -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| path.strip_prefix(cwd).ok().map(Path::to_path_buf))
        .unwrap_or_else(|| path.to_path_buf())
        .display()
        .to_string()
}

/// A piece of generated code that was copied verbatim from a source file.
#[derive(Debug, Clone, Copy)]
struct Segment {
    gen_start: usize,
    file: FileId,
    orig_start: usize,
    len: usize,
}

/// Generated Rhai code plus a map from generated offsets back to source spans.
#[derive(Debug, Clone, Default)]
pub struct CodeBuilder {
    pub text: String,
    segments: Vec<Segment>,
    /// Where generated (unmapped) code is attributed to.
    anchor: Option<Span>,
}

impl CodeBuilder {
    pub fn new(anchor: Span) -> Self {
        CodeBuilder {
            text: String::new(),
            segments: Vec::new(),
            anchor: Some(anchor),
        }
    }

    /// Append original source text, remembering where it came from.
    pub fn push_orig(&mut self, file: FileId, start: usize, text: &str) {
        if text.is_empty() {
            return;
        }
        self.segments.push(Segment {
            gen_start: self.text.len(),
            file,
            orig_start: start,
            len: text.len(),
        });
        self.text.push_str(text);
    }

    /// Append generated text that has no counterpart in the source.
    pub fn push_gen(&mut self, text: &str) {
        self.text.push_str(text);
    }

    pub fn set_anchor(&mut self, span: Span) {
        self.anchor = Some(span);
    }

    /// Map a byte offset in the generated text back to the source.
    pub fn map_offset(&self, offset: usize) -> Option<Span> {
        let mut best: Option<&Segment> = None;
        for seg in &self.segments {
            if seg.gen_start <= offset {
                best = Some(seg);
            } else {
                break;
            }
        }
        match best {
            Some(seg) if offset < seg.gen_start + seg.len => {
                let o = seg.orig_start + (offset - seg.gen_start);
                Some(Span::new(seg.file, o, o + 1))
            }
            Some(seg) => {
                let o = seg.orig_start + seg.len.saturating_sub(1);
                Some(Span::new(seg.file, o, o + 1))
            }
            None => self.anchor.or_else(|| {
                self.segments
                    .first()
                    .map(|s| Span::new(s.file, s.orig_start, s.orig_start + 1))
            }),
        }
    }

    /// Like [`Self::map_position`], but `None` for generated code instead of the nearest source.
    pub fn map_position_exact(&self, pos: rhai::Position) -> Option<Span> {
        let span = self.map_position(pos)?;
        let inside = self.segments.iter().any(|s| {
            s.file == span.file && s.orig_start <= span.start && span.start < s.orig_start + s.len
        });
        let offset = self.gen_offset(pos)?;
        let in_segment = self
            .segments
            .iter()
            .any(|s| s.gen_start <= offset && offset < s.gen_start + s.len);
        (inside && in_segment).then_some(span)
    }

    fn gen_offset(&self, pos: rhai::Position) -> Option<usize> {
        let (line, col) = (pos.line()?, pos.position()?);
        let mut offset = 0;
        for (i, l) in self.text.split_inclusive('\n').enumerate() {
            if i + 1 == line {
                return Some(
                    offset
                        + l.char_indices()
                            .nth(col.saturating_sub(1))
                            .map_or(l.len(), |(b, _)| b),
                );
            }
            offset += l.len();
        }
        None
    }

    /// Map a Rhai position (1-based line and column, in characters) back to the source.
    pub fn map_position(&self, pos: rhai::Position) -> Option<Span> {
        let (Some(line), Some(col)) = (pos.line(), pos.position()) else {
            return self.anchor;
        };
        let mut offset = 0;
        for (i, l) in self.text.split_inclusive('\n').enumerate() {
            if i + 1 == line {
                let byte_col = l
                    .char_indices()
                    .nth(col.saturating_sub(1))
                    .map_or(l.len(), |(b, _)| b);
                return self.map_offset(offset + byte_col);
            }
            offset += l.len();
        }
        self.map_offset(self.text.len().saturating_sub(1))
    }
}
