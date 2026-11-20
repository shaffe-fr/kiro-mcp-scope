//! Surgical edits of JSON files.
//!
//! The copies kms synchronizes are often tracked by git and hand-formatted, so
//! a parse-and-reserialize round trip would rewrite far more than the change.
//! Instead the byte span of the one value to replace is located in the original
//! text and only that span is substituted. Everything around it — whitespace,
//! key order, other servers, line endings, BOM, trailing newline —
//! stays byte for byte identical.
//!
//! The replacement itself is rendered with the file's own indentation unit and
//! line ending, detected from the text.

use std::path::Path;

use serde_json::Value;

/// How a file lays out its JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub bom: bool,
    /// `"\n"` or `"\r\n"`.
    pub eol: &'static str,
    /// One indentation unit: spaces or a tab.
    pub indent: String,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            bom: false,
            eol: "\n",
            indent: "  ".to_string(),
        }
    }
}

const BOM: char = '\u{feff}';

/// Detect BOM, line ending and indentation unit. Falls back to LF and two
/// spaces when the text gives no hint.
pub fn detect_layout(text: &str) -> Layout {
    let bom = text.starts_with(BOM);
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut indent = None;
    for line in text.lines().skip(1) {
        let trimmed = line.trim_start_matches([' ', '\t']);
        if trimmed.is_empty() || trimmed.len() == line.len() {
            continue;
        }
        let lead = &line[..line.len() - trimmed.len()];
        indent = Some(if lead.contains('\t') {
            "\t".to_string()
        } else {
            lead.to_string()
        });
        break;
    }
    Layout {
        bom,
        eol,
        indent: indent.unwrap_or_else(|| "  ".to_string()),
    }
}

/// Parse a JSON document, tolerating a BOM. The error never quotes content.
pub fn parse(text: &str) -> Result<Value, String> {
    let body = text.strip_prefix(BOM).unwrap_or(text);
    serde_json::from_str(body).map_err(|err| format!("invalid JSON ({err})"))
}

/// Byte range of a value inside the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\r' | b'\n') {
        i += 1;
    }
    i
}

/// Index just past the closing quote of the string starting at `i`.
fn skip_string(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return Some(j + 1),
            _ => j += 1,
        }
    }
    None
}

/// Index just past the value starting at `i`.
fn skip_value(b: &[u8], i: usize) -> Option<usize> {
    match *b.get(i)? {
        b'"' => skip_string(b, i),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut j = i;
            while j < b.len() {
                match b[j] {
                    b'"' => {
                        j = skip_string(b, j)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(j + 1);
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            None
        }
        _ => {
            let mut j = i;
            while j < b.len() && !matches!(b[j], b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n')
            {
                j += 1;
            }
            Some(j)
        }
    }
}

/// Span of the value of `key` in the object that starts at `obj_start`.
fn find_in_object(b: &[u8], obj_start: usize, key: &str) -> Option<Span> {
    if *b.get(obj_start)? != b'{' {
        return None;
    }
    let mut i = skip_ws(b, obj_start + 1);
    loop {
        match *b.get(i)? {
            b'}' => return None,
            b'"' => {}
            _ => return None,
        }
        let key_end = skip_string(b, i)?;
        let name: String = serde_json::from_slice(&b[i..key_end]).ok()?;
        i = skip_ws(b, key_end);
        if *b.get(i)? != b':' {
            return None;
        }
        i = skip_ws(b, i + 1);
        let end = skip_value(b, i)?;
        if name == key {
            return Some(Span { start: i, end });
        }
        i = skip_ws(b, end);
        match *b.get(i)? {
            b',' => i = skip_ws(b, i + 1),
            _ => return None,
        }
    }
}

/// Span of the value at `path` (object keys only) in `text`.
pub fn find_member(text: &str, path: &[&str]) -> Option<Span> {
    let b = text.as_bytes();
    let root = skip_ws(
        b,
        if text.starts_with(BOM) {
            BOM.len_utf8()
        } else {
            0
        },
    );
    let mut span = Span {
        start: root,
        end: b.len(),
    };
    for key in path {
        span = find_in_object(b, span.start, key)?;
    }
    Some(span)
}

/// Leading whitespace of the line holding byte `at`.
fn line_indent(text: &str, at: usize) -> &str {
    let line_start = text[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line = &text[line_start..at];
    let trimmed = line.trim_start_matches([' ', '\t']);
    &line[..line.len() - trimmed.len()]
}

/// Pretty JSON using the layout's indentation and line ending. Continuation
/// lines are prefixed with `base_indent` so the value sits where it is placed.
pub fn render_value(value: &Value, layout: &Layout, base_indent: &str) -> String {
    let mut buf = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(layout.indent.as_bytes());
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
    serde::Serialize::serialize(value, &mut ser)
        .expect("serializing a serde_json::Value cannot fail");
    let text = String::from_utf8(buf).expect("serde_json emits valid UTF-8");
    text.replace('\n', &format!("{}{}", layout.eol, base_indent))
}

/// A whole new document: BOM if the layout has one, final line ending.
pub fn render_document(value: &Value, layout: &Layout) -> String {
    let mut out = String::new();
    if layout.bom {
        out.push(BOM);
    }
    out.push_str(&render_value(value, layout, ""));
    out.push_str(layout.eol);
    out
}

/// Replace the value at `path` with `new`, leaving every other byte alone.
/// `None` when the path does not exist.
pub fn replace_member(text: &str, path: &[&str], new: &Value, layout: &Layout) -> Option<String> {
    let span = find_member(text, path)?;
    let base = line_indent(text, span.start).to_string();
    let rendered = render_value(new, layout, &base);
    let mut out = String::with_capacity(text.len() + rendered.len());
    out.push_str(&text[..span.start]);
    out.push_str(&rendered);
    out.push_str(&text[span.end..]);
    Some(out)
}

/// Write `contents` next to `path` and move it into place, so a crash never
/// leaves a half-written config behind. Parent directories are created.
pub fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let tmp = path.with_file_name(format!(".{name}.kms-tmp"));
    std::fs::write(&tmp, contents)?;
    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SAMPLE: &str = "{\n    \"mcpServers\": {\n        \"a\": { \"command\": \"x\", \"note\": \"}{ \\\" tricky\" },\n        \"b\": {\n            \"url\": \"https://old\"\n        }\n    },\n    \"powers\": {}\n}\n";

    #[test]
    fn detects_indent_eol_and_bom() {
        let layout = detect_layout(SAMPLE);
        assert_eq!(layout.indent, "    ");
        assert_eq!(layout.eol, "\n");
        assert!(!layout.bom);

        let crlf = "\u{feff}{\r\n\t\"a\": 1\r\n}\r\n";
        let layout = detect_layout(crlf);
        assert_eq!(layout.indent, "\t");
        assert_eq!(layout.eol, "\r\n");
        assert!(layout.bom);
    }

    #[test]
    fn finds_nested_members_past_tricky_strings() {
        let span = find_member(SAMPLE, &["mcpServers", "b"]).unwrap();
        assert!(SAMPLE[span.start..span.end].contains("https://old"));
        assert!(find_member(SAMPLE, &["mcpServers", "missing"]).is_none());
        assert!(find_member(SAMPLE, &["nope", "b"]).is_none());
    }

    #[test]
    fn replacement_touches_only_the_target_span() {
        let layout = detect_layout(SAMPLE);
        let out = replace_member(
            SAMPLE,
            &["mcpServers", "b"],
            &json!({ "url": "https://new", "disabled": true }),
            &layout,
        )
        .unwrap();

        let before = find_member(SAMPLE, &["mcpServers", "b"]).unwrap();
        assert_eq!(&out[..before.start], &SAMPLE[..before.start]);
        assert!(out.ends_with("    },\n    \"powers\": {}\n}\n"));
        assert!(out.contains("            \"url\": \"https://new\""));
        assert!(out.contains("            \"disabled\": true"));
        assert!(parse(&out).is_ok());
    }

    #[test]
    fn replacement_keeps_crlf_and_bom() {
        let text = "\u{feff}{\r\n  \"mcpServers\": {\r\n    \"a\": {\r\n      \"url\": \"u\"\r\n    }\r\n  }\r\n}\r\n";
        let layout = detect_layout(text);
        let out = replace_member(
            text,
            &["mcpServers", "a"],
            &json!({ "url": "v", "args": ["1"] }),
            &layout,
        )
        .unwrap();
        assert!(out.starts_with('\u{feff}'));
        assert!(!out.replace("\r\n", "").contains('\n'), "no bare LF added");
        assert!(parse(&out).is_ok());
    }

    #[test]
    fn document_rendering_ends_with_one_line_ending() {
        let layout = Layout::default();
        let out = render_document(&json!({ "a": 1 }), &layout);
        assert_eq!(out, "{\n  \"a\": 1\n}\n");
    }

    #[test]
    fn atomic_write_creates_parents_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b.json");
        write_atomic(&path, "{}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
        write_atomic(&path, "{\"x\":1}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"x\":1}");
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(leftovers.len(), 1);
    }
}
