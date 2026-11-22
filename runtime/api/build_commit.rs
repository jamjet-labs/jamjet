//! Which commit a build came from. Pure, so `build.rs` and a unit test both
//! include it (`#[path]`).

/// `(commit, source)`. A set but malformed `JAMJET_BUILD_COMMIT` is an error:
/// it fails the build. Order: the env value, then the `git.sha1` in
/// `.cargo_vcs_info.json` (present in a crate from crates.io), then `git_head`.
pub fn resolve_commit(
    env: Option<&str>,
    vcs_info_json: Option<&str>,
    git_head: impl FnOnce() -> Option<String>,
) -> Result<(Option<String>, &'static str), String> {
    if let Some(raw) = env.map(str::trim).filter(|v| !v.is_empty()) {
        return match normalize(raw) {
            Some(sha) => Ok((Some(sha), "build-env")),
            None => Err(format!(
                "JAMJET_BUILD_COMMIT must be 40 hex characters, got {} characters",
                raw.chars().count()
            )),
        };
    }
    if let Some(sha) = vcs_info_json.and_then(package_sha) {
        return Ok((Some(sha), "crate-package"));
    }
    if let Some(sha) = git_head().and_then(|h| normalize(h.trim())) {
        return Ok((Some(sha), "git"));
    }
    Ok((None, "none"))
}

fn normalize(s: &str) -> Option<String> {
    if s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(s.to_ascii_lowercase())
    } else {
        None
    }
}

fn package_sha(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    normalize(v.get("git")?.get("sha1")?.as_str()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA_A: &str = "0123456789abcdef0123456789abcdef01234567";
    const SHA_B: &str = "fedcba9876543210fedcba9876543210fedcba98";

    #[test]
    fn the_env_value_wins() {
        let vcs = format!(r#"{{"git":{{"sha1":"{SHA_B}"}}}}"#);
        let upper = SHA_A.to_ascii_uppercase();
        assert_eq!(
            resolve_commit(Some(&upper), Some(&vcs), || None),
            Ok((Some(SHA_A.to_string()), "build-env"))
        );
        assert_eq!(
            resolve_commit(Some("  "), Some(&vcs), || None),
            Ok((Some(SHA_B.to_string()), "crate-package"))
        );
    }

    #[test]
    fn a_malformed_env_value_fails() {
        for bad in ["abc", &SHA_A[..39], &format!("{SHA_A}0"), &"g".repeat(40)] {
            assert!(resolve_commit(Some(bad), None, || None).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_package_sha_is_read_and_no_source_is_none() {
        let with = format!(r#"{{"git":{{"sha1":"{SHA_B}"}},"path_in_vcs":""}}"#);
        assert_eq!(
            resolve_commit(None, Some(&with), || None),
            Ok((Some(SHA_B.to_string()), "crate-package"))
        );
        assert_eq!(
            resolve_commit(None, Some(r#"{"path_in_vcs":""}"#), || None),
            Ok((None, "none"))
        );
        assert_eq!(
            resolve_commit(None, None, || Some(format!("{SHA_A}\n"))),
            Ok((Some(SHA_A.to_string()), "git"))
        );
        assert_eq!(resolve_commit(None, None, || None), Ok((None, "none")));
    }
}
