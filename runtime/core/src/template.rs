//! Prompt templates for `model` nodes.
//!
//! A model node's `prompt_ref` is the prompt the YAML author wrote under
//! `prompt:`, compiled into the IR as is. The runtime renders it against the
//! run's state when it builds the node's work item, so the model receives the
//! text with the state filled in.
//!
//! # Grammar
//!
//! ```text
//! template := (text | "{{" ws expr ws "}}" | raw)*
//! expr     := path | literal
//! path     := "state" ("." segment)+
//! segment  := identifier | digits            # digits index an array
//! literal  := "'" chars "'" | '"' chars '"'   # inserted as is
//! raw      := "{% raw %}" chars "{% endraw %}"  # inserted as is
//! ```
//!
//! Substitution, plus the two Jinja2 ways to write a literal `{{`, `{%` or
//! `{#` (a JSON example in a prompt needs them): a quoted string,
//! `{{ '{{' }}`, and a raw block, `{% raw %}{"a": {{1}}}{% endraw %}`.
//! Filters (`{{ state.x | join(", ") }}`), operators, calls, other statements
//! (`{% if %}`) and comments (`{# #}`) are refused with a reason, never passed
//! through as text and never guessed at. A workflow is checked with
//! [`validate_template`] when it is registered, so an author hears about
//! unsupported syntax once, before any run.
//!
//! # Rendering
//!
//! A string value is inserted as is. Any other value (number, boolean, null,
//! array, object) is inserted as compact JSON. A path the state does not have
//! is an error, not an empty string: a prompt with a hole in it would still
//! reach the model and be answered. A rendered prompt is at most
//! [`MAX_RENDERED_PROMPT_BYTES`]; a larger one is an error, so a template that
//! repeats a large state value cannot put an unbounded payload on the queue.

use serde_json::{Map, Value};

/// The largest prompt a template may render to, in bytes.
pub const MAX_RENDERED_PROMPT_BYTES: usize = 256 * 1024;

/// File extensions that mark a `prompt_ref` as a prompt FILE rather than
/// inline prompt text.
pub const PROMPT_FILE_EXTENSIONS: &[&str] = &[
    ".md", ".txt", ".j2", ".jinja", ".jinja2", ".prompt", ".tmpl", ".tpl",
];

/// Whether `prompt_ref` names a prompt file (`prompts/summarize.md`) instead
/// of holding the prompt text.
///
/// The SDK compilers write a file path when the author gave no inline prompt
/// (`prompts/<node>.md`), and an author may write one under `prompt:`. The
/// file stays on the machine that compiled the workflow, so the runtime
/// cannot read it. A reference is one line with no `{{`, ending in one of
/// [`PROMPT_FILE_EXTENSIONS`], and either a single token or a path (it has a
/// `/`, so `prompts/customer support.md` is a file while `Please summarize
/// notes.md` is a prompt).
pub fn is_prompt_file_ref(prompt_ref: &str) -> bool {
    let s = prompt_ref.trim();
    if s.is_empty() || s.contains('\n') || s.contains("{{") {
        return false;
    }
    if s.chars().any(char::is_whitespace) && !s.contains('/') {
        return false;
    }
    let lower = s.to_ascii_lowercase();
    PROMPT_FILE_EXTENSIONS
        .iter()
        .any(|ext| lower.len() > ext.len() && lower.ends_with(ext))
}

/// One piece of a parsed template.
enum Piece<'a> {
    /// Inserted as is: plain text, a quoted literal or a raw block's body.
    Text(&'a str),
    /// The path after `state.`, e.g. `query` or `search.results.0`.
    State(&'a str),
}

/// Check that `template` uses only the syntax [`render_template`] supports.
pub fn validate_template(template: &str) -> Result<(), String> {
    parse(template).map(|_| ())
}

/// Render `template` against the run's `state`.
pub fn render_template(template: &str, state: &Map<String, Value>) -> Result<String, String> {
    let over = || format!("the rendered prompt is over the {MAX_RENDERED_PROMPT_BYTES}-byte limit");
    let mut out = String::with_capacity(template.len().min(MAX_RENDERED_PROMPT_BYTES));
    for piece in parse(template)? {
        let room = MAX_RENDERED_PROMPT_BYTES - out.len();
        let mut push = |text: &str| {
            if text.len() > room {
                return Err(over());
            }
            out.push_str(text);
            Ok(())
        };
        match piece {
            Piece::Text(t) => push(t)?,
            Piece::State(path) => {
                let value = resolve(state, path).ok_or_else(|| {
                    format!("`{{{{ state.{path} }}}}`: the run's state has no `{path}`")
                })?;
                match value {
                    Value::String(s) => push(s)?,
                    // Serialized into a writer that stops at the room left,
                    // so a huge object is never built in full only to be
                    // refused.
                    other => {
                        let mut w = Bounded {
                            buf: Vec::new(),
                            room,
                        };
                        serde_json::to_writer(&mut w, other).map_err(|_| over())?;
                        push(std::str::from_utf8(&w.buf).map_err(|e| e.to_string())?)?;
                    }
                }
            }
        }
    }
    Ok(out)
}

/// A writer that refuses to grow past `room` bytes.
struct Bounded {
    buf: Vec<u8>,
    room: usize,
}

impl std::io::Write for Bounded {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if self.buf.len() + data.len() > self.room {
            return Err(std::io::Error::other("over the prompt limit"));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn parse(template: &str) -> Result<Vec<Piece<'_>>, String> {
    let mut pieces = Vec::new();
    let mut rest = template;
    let mut offset = 0usize;
    loop {
        let Some(open) = find_open(rest) else {
            if !rest.is_empty() {
                pieces.push(Piece::Text(rest));
            }
            return Ok(pieces);
        };
        if open > 0 {
            pieces.push(Piece::Text(&rest[..open]));
        }
        let at = offset + open;
        match &rest[open..open + 2] {
            "{%" => {
                // `{% raw %}...{% endraw %}`: the body is text, inserted as is.
                if let Some(body_start) = raw_open(&rest[open..]) {
                    let body = &rest[open + body_start..];
                    let (body_len, end_len) = raw_close(body).ok_or_else(|| {
                        format!("`{{% raw %}}` at byte {at} is never closed with `{{% endraw %}}`")
                    })?;
                    if body_len > 0 {
                        pieces.push(Piece::Text(&body[..body_len]));
                    }
                    let consumed = open + body_start + body_len + end_len;
                    offset += consumed;
                    rest = &rest[consumed..];
                    continue;
                }
                return Err(format!(
                    "template statements (`{{% ... %}}`, at byte {at}) are not supported, \
                     except `{{% raw %}}`; the runtime substitutes `{{{{ state.<path> }}}}` only"
                ));
            }
            "{#" => {
                return Err(format!(
                    "template comments (`{{# ... #}}`, at byte {at}) are not supported; \
                     the runtime substitutes `{{{{ state.<path> }}}}` only"
                ))
            }
            _ => {}
        }
        let after_open = &rest[open + 2..];
        // A quoted literal is read up to its closing quote first, so it can
        // hold `}}` itself (`{{ "}}" }}`).
        let lead = after_open.len() - after_open.trim_start().len();
        let body = &after_open[lead..];
        if let Some(q) = body.chars().next().filter(|c| *c == '\'' || *c == '"') {
            let close_q = body[1..]
                .find(q)
                .ok_or_else(|| format!("the string literal at byte {at} is never closed"))?
                + 1;
            let tail = &body[close_q + 1..];
            let ws = tail.len() - tail.trim_start().len();
            if !tail[ws..].starts_with("}}") {
                return Err(format!(
                    "unsupported template expression at byte {at}: a quoted literal must be \
                     the whole expression, as in `{{{{ '{{{{' }}}}`"
                ));
            }
            if close_q > 1 {
                pieces.push(Piece::Text(&body[1..close_q]));
            }
            let consumed = open + 2 + lead + close_q + 1 + ws + 2;
            offset += consumed;
            rest = &rest[consumed..];
            continue;
        }
        let close = after_open
            .find("}}")
            .ok_or_else(|| format!("`{{{{` at byte {at} is never closed with `}}}}`"))?;
        let inner = after_open[..close].trim();
        let path = state_path(inner).ok_or_else(|| {
            format!(
                "unsupported template expression `{{{{ {inner} }}}}` at byte {at}: the runtime \
                 substitutes `{{{{ state.<path> }}}}` only (no filters, operators or calls)"
            )
        })?;
        pieces.push(Piece::State(path));
        let consumed = open + 2 + close + 2;
        offset += consumed;
        rest = &rest[consumed..];
    }
}

/// If `s` starts with a `{% raw %}` tag (spaces optional inside it), the
/// length of the tag.
fn raw_open(s: &str) -> Option<usize> {
    let end = 2 + s.get(2..)?.find("%}")?;
    (s[2..end].trim() == "raw").then_some(end + 2)
}

/// In a raw block's `body`, the length of the text before its
/// `{% endraw %}` tag and the length of that tag.
fn raw_close(body: &str) -> Option<(usize, usize)> {
    let mut from = 0;
    while let Some(i) = body[from..].find("{%") {
        let start = from + i;
        let end = start + 2 + body[start + 2..].find("%}")?;
        if body[start + 2..end].trim() == "endraw" {
            return Some((start, end + 2 - start));
        }
        from = start + 2;
    }
    None
}

/// The byte index of the next `{{`, `{%` or `{#`.
fn find_open(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    (0..b.len().saturating_sub(1)).find(|&i| b[i] == b'{' && matches!(b[i + 1], b'{' | b'%' | b'#'))
}

/// `Some(path)` when `expr` is `state.<segment>(.<segment>)*`.
fn state_path(expr: &str) -> Option<&str> {
    let path = expr.strip_prefix("state.")?;
    let ok = !path.is_empty()
        && path.split('.').all(|seg| {
            let mut chars = seg.chars();
            match chars.next() {
                Some(c) if c.is_ascii_digit() => seg.chars().all(|c| c.is_ascii_digit()),
                Some(c) if c.is_ascii_alphabetic() || c == '_' => {
                    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
                }
                _ => false,
            }
        });
    ok.then_some(path)
}

fn resolve<'a>(state: &'a Map<String, Value>, path: &str) -> Option<&'a Value> {
    let (first, rest) = match path.split_once('.') {
        Some((f, r)) => (f, Some(r)),
        None => (path, None),
    };
    let top = state.get(first)?;
    match rest {
        None => Some(top),
        Some(rest) => crate::condition::resolve_path(top, rest),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn substitutes_a_state_key() {
        let s = state(json!({ "query": "What is JamJet?" }));
        assert_eq!(
            render_template(
                "Answer this question clearly and concisely:\n\n{{ state.query }}\n",
                &s
            )
            .unwrap(),
            "Answer this question clearly and concisely:\n\nWhat is JamJet?\n"
        );
    }

    #[test]
    fn substitutes_nested_paths_array_indexes_and_tight_braces() {
        let s = state(json!({ "pr": { "title": "Fix", "files": ["a.rs", "b.rs"] } }));
        assert_eq!(
            render_template("{{state.pr.title}}: {{ state.pr.files.1 }}", &s).unwrap(),
            "Fix: b.rs"
        );
    }

    #[test]
    fn non_string_values_render_as_json() {
        let s =
            state(json!({ "n": 3, "ok": true, "none": null, "list": [1, "x"], "obj": { "a": 1 } }));
        assert_eq!(
            render_template(
                "{{ state.n }} {{ state.ok }} {{ state.none }} {{ state.list }} {{ state.obj }}",
                &s
            )
            .unwrap(),
            r#"3 true null [1,"x"] {"a":1}"#
        );
    }

    #[test]
    fn text_without_placeholders_is_unchanged() {
        let s = state(json!({}));
        let t = "Output JSON: {\"steps\": [\"step 1\"]} and a lone } brace";
        assert_eq!(render_template(t, &s).unwrap(), t);
    }

    #[test]
    fn a_missing_key_is_an_error_not_an_empty_string() {
        let s = state(json!({ "question": "x" }));
        let err = render_template("Q: {{ state.query }}", &s).unwrap_err();
        assert!(err.contains("no `query`"), "{err}");
        let err = render_template("{{ state.a.b }}", &state(json!({ "a": "flat" }))).unwrap_err();
        assert!(err.contains("no `a.b`"), "{err}");
    }

    #[test]
    fn a_quoted_literal_and_a_raw_block_write_template_markers_as_text() {
        let s = state(json!({ "q": "hi" }));
        assert_eq!(
            render_template("Use {{ '{{' }} name {{ \"}}\" }} for {{ state.q }}", &s).unwrap(),
            "Use {{ name }} for hi"
        );
        assert_eq!(
            render_template(
                "Reply like {% raw %}{\"a\": {{x}}, \"b\": \"{% if %}\"}{% endraw %} to {{ state.q }}",
                &s
            )
            .unwrap(),
            "Reply like {\"a\": {{x}}, \"b\": \"{% if %}\"} to hi"
        );
        assert_eq!(render_template("{%raw%}{#{%endraw%}", &s).unwrap(), "{#");
        for bad in [
            "{% raw %}never closed",
            "{{ 'it's' }}",
            "{{ 'a\" }}",
            "{%}",
            "{{ 'x' | upper }}",
        ] {
            assert!(validate_template(bad).is_err(), "accepted: {bad}");
        }
    }

    /// An `{% endraw %}` with no `{% raw %}` before it is a statement like
    /// any other: refused, never read as the end of a raw block.
    #[test]
    fn an_endraw_without_a_raw_is_refused() {
        for t in ["{% if %}x{% endraw %}", "x{% endraw %}"] {
            assert!(validate_template(t).is_err(), "accepted: {t}");
        }
    }

    /// The writer a non-string value is serialized into stops at the room
    /// left, so a huge value is never built in full.
    #[test]
    fn the_bounded_writer_stops_at_its_room() {
        use std::io::Write;
        let mut w = Bounded {
            buf: Vec::new(),
            room: 4,
        };
        assert!(w.write_all(b"abcd").is_ok());
        assert!(w.write_all(b"e").is_err());
        assert_eq!(w.buf, b"abcd");
    }

    /// A non-string value over the room left is refused without being
    /// serialized in full.
    #[test]
    fn a_large_object_value_is_refused_at_the_limit() {
        let big: Vec<u32> = (0..200_000).collect();
        let s = state(json!({ "big": big }));
        let err = render_template("{{ state.big }}", &s).unwrap_err();
        assert!(err.contains("limit"), "{err}");
        let small = state(json!({ "o": { "a": [1, 2] } }));
        assert_eq!(
            render_template("{{ state.o }}", &small).unwrap(),
            r#"{"a":[1,2]}"#
        );
    }

    #[test]
    fn a_rendered_prompt_over_the_limit_is_an_error() {
        let big = "x".repeat(MAX_RENDERED_PROMPT_BYTES / 2 + 1);
        let s = state(json!({ "big": big }));
        assert!(render_template("{{ state.big }}", &s).is_ok());
        let err = render_template("{{ state.big }}{{ state.big }}", &s).unwrap_err();
        assert!(err.contains("limit"), "{err}");
    }

    #[test]
    fn unsupported_syntax_is_refused() {
        for t in [
            "{{ state.items | join(', ') }}",
            "{{ state.retries > 2 }}",
            "{{ query }}",
            "{{ state. }}",
            "{{ state.a..b }}",
            "{{ state.repo.split('/')[0] }}",
            "{%- if state.critique %}x{% endif %}",
            "{# note #}",
            "open {{ state.query",
            "{{- state.query -}}",
        ] {
            assert!(validate_template(t).is_err(), "accepted: {t}");
            assert!(render_template(t, &state(json!({ "query": "q", "items": [] }))).is_err());
        }
    }

    #[test]
    fn supported_syntax_validates() {
        for t in [
            "",
            "plain text",
            "{{ state.query }}",
            "{{state.a.b.0}}",
            "{{ state.__plan__ }} and {{ state.__critic_0_verdict__.feedback }}",
        ] {
            assert!(validate_template(t).is_ok(), "refused: {t}");
        }
    }

    #[test]
    fn prompt_file_references_are_told_apart_from_prompt_text() {
        for r in [
            "prompts/summarize.md",
            "prompts/x.txt",
            "p.j2",
            "PROMPTS/A.MD",
            "prompts/customer support.md",
        ] {
            assert!(is_prompt_file_ref(r), "{r}");
        }
        for r in [
            "",
            "Summarize this.md file for me",
            "Please summarize notes.md",
            "Read the notes.md\nthen answer prompts/x.md",
            "hello",
            "{{ state.file }}.md",
            ".md",
            "Answer:\n\n{{ state.query }}\n",
        ] {
            assert!(!is_prompt_file_ref(r), "{r}");
        }
    }
}
