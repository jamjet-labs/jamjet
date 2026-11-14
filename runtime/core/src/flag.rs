//! Boolean switches in the environment (`JAMJET_DEV_MODE`, the audit
//! insecure-key opt-in, ...): one rule for all of them, here and in the
//! Python SDK (`jamjet.flags.parse_flag`).
//!
//! Each switch used to have its own reading. `JAMJET_DEV_MODE` took only `1`
//! or `true`, the insecure-key opt-in read an unknown value as no, and
//! `JAMJET_EMBED_WORKERS` was on whenever it was set, `=0` included. Now:
//!
//! * unset is off;
//! * `1`, `true`, `yes`, `on` are on and `0`, `false`, `no`, `off` and the
//!   empty string are off, in any ASCII case;
//! * anything else is an error naming the variable. `jamjet-server` checks
//!   every switch before it starts, so a typo stops it instead of being read
//!   as one or the other.
//!
//! Only the five ASCII spaces Rust's `char::is_ascii_whitespace` names (space,
//! tab, LF, FF, CR) are trimmed. Python's bare `str.strip()` would also trim
//! U+001C-U+001F and the Unicode spaces, so the SDK strips exactly these five
//! too. `sdk/python/tests/fixtures/bool_flags.json` holds the cases both sides
//! test against.

/// Read a switch's value. `None` (unset) is `false`.
pub fn parse_flag(name: &str, raw: Option<&str>) -> Result<bool, String> {
    let Some(raw) = raw else { return Ok(false) };
    let value = raw.trim_matches(|c: char| c.is_ascii_whitespace());
    if value.is_ascii() {
        match value.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => return Ok(true),
            "0" | "false" | "no" | "off" | "" => return Ok(false),
            _ => {}
        }
    }
    Err(format!(
        "{name}={raw:?} is not a boolean; use 1/true/yes/on or 0/false/no/off"
    ))
}

/// [`parse_flag`] on the process environment. A value that is not valid
/// Unicode is an error too.
pub fn env_flag(name: &str) -> Result<bool, String> {
    match std::env::var(name) {
        Ok(v) => parse_flag(name, Some(&v)),
        Err(std::env::VarError::NotPresent) => Ok(false),
        Err(e) => Err(format!("failed to read {name}: {e}")),
    }
}
