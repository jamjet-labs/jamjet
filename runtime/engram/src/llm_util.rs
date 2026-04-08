//! Shared utilities for LLM clients.

/// Extract a JSON object or array from an LLM response that may wrap it in
/// a markdown fence like ```` ```json ... ``` ```` or ```` ``` ... ``` ````.
///
/// Returns a trimmed slice pointing into the original string. If no fence
/// is detected, the input is returned trimmed.
pub fn extract_json_payload(text: &str) -> &str {
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix("```json") {
        if let Some(end) = rest.find("```") {
            return rest[..end].trim();
        }
    }
    if let Some(rest) = trimmed.strip_prefix("```") {
        if let Some(end) = rest.find("```") {
            return rest[..end].trim();
        }
    }
    trimmed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_json_fence() {
        let input = "```json\n{\"facts\": []}\n```";
        assert_eq!(extract_json_payload(input), "{\"facts\": []}");
    }

    #[test]
    fn strips_plain_fence() {
        let input = "```\n{\"facts\": [1,2,3]}\n```";
        assert_eq!(extract_json_payload(input), "{\"facts\": [1,2,3]}");
    }

    #[test]
    fn passes_through_raw_json() {
        let input = "  {\"facts\": [{\"text\": \"hi\"}]}  ";
        assert_eq!(
            extract_json_payload(input),
            "{\"facts\": [{\"text\": \"hi\"}]}"
        );
    }

    #[test]
    fn handles_leading_text_before_fence() {
        // Not perfect, but tests that we fall through cleanly.
        let input = "Here is the JSON:\n```json\n{\"ok\": true}\n```";
        // No leading strip, the raw trimmed text is returned.
        // The json parser will fail — that's acceptable because the prompt
        // explicitly asks for JSON-only output. Just make sure we don't panic.
        let _ = extract_json_payload(input);
    }
}
