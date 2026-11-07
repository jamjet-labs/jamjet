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
//! template := (text | "{{" ws path ws "}}")*
//! path     := "state" ("." segment)+
//! segment  := identifier | digits            # digits index an array
//! ```
//!
//! Only substitution. Filters (`{{ state.x | join(", ") }}`), operators,
//! calls, statements (`{% if %}`) and comments (`{# #}`) are refused with a
//! reason, never passed through as text and never guessed at. A workflow is
//! checked with [`validate_template`] when it is registered, so an author
//! hears about unsupported syntax once, before any run.
//!
//! # Rendering
//!
//! A string value is inserted as is. Any other value (number, boolean, null,
//! array, object) is inserted as compact JSON. A path the state does not have
//! is an error, not an empty string: a prompt with a hole in it would still
//! reach the model and be answered.

use serde_json::{Map, Value};

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
    let mut out = String::with_capacity(template.len());
    for piece in parse(template)? {
        match piece {
            Piece::Text(t) => out.push_str(t),
            Piece::State(path) => {
                let value = resolve(state, path).ok_or_else(|| {
                    format!("`{{{{ state.{path} }}}}`: the run's state has no `{path}`")
                })?;
                match value {
                    Value::String(s) => out.push_str(s),
                    other => out.push_str(&other.to_string()),
                }
            }
        }
    }
    Ok(out)
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
                return Err(format!(
                    "template statements (`{{% ... %}}`, at byte {at}) are not supported; \
                     the runtime substitutes `{{{{ state.<path> }}}}` only"
                ))
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
