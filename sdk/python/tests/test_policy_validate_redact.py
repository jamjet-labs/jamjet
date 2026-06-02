import pytest

from jamjet.cloud.policy import validate


def test_validate_accepts_redact_with_all_required_keys():
    parsed = {
        "version": 1,
        "rules": [
            {
                "match": "openai:*",
                "action": "redact",
                "types": ["EMAIL", "US_SSN"],
                "on_detect": "block",
                "scope": ["messages", "tools"],
            },
        ],
    }
    validated = validate(parsed)
    assert validated["rules"][0]["action"] == "redact"
    assert validated["rules"][0]["types"] == ["EMAIL", "US_SSN"]


def test_validate_accepts_redact_with_replace_action():
    parsed = {
        "version": 1,
        "rules": [
            {
                "match": "anthropic:*",
                "action": "redact",
                "types": ["EMAIL"],
                "on_detect": "replace",
                "scope": ["messages"],
            },
        ],
    }
    validated = validate(parsed)
    assert validated["rules"][0]["on_detect"] == "replace"


def test_validate_rejects_redact_missing_types():
    parsed = {
        "version": 1,
        "rules": [
            {"match": "openai:*", "action": "redact", "on_detect": "block", "scope": ["messages"]},
        ],
    }
    with pytest.raises(ValueError, match=r"redact.*types"):
        validate(parsed)


def test_validate_rejects_unknown_pii_type():
    parsed = {
        "version": 1,
        "rules": [
            {
                "match": "openai:*",
                "action": "redact",
                "types": ["MARTIAN_BANK_ACCOUNT"],
                "on_detect": "block",
                "scope": ["messages"],
            },
        ],
    }
    with pytest.raises(ValueError, match=r"MARTIAN_BANK_ACCOUNT"):
        validate(parsed)


def test_validate_rejects_invalid_on_detect():
    parsed = {
        "version": 1,
        "rules": [
            {
                "match": "openai:*",
                "action": "redact",
                "types": ["EMAIL"],
                "on_detect": "ask_nicely",
                "scope": ["messages"],
            },
        ],
    }
    with pytest.raises(ValueError, match=r"on_detect"):
        validate(parsed)


def test_validate_rejects_invalid_scope():
    parsed = {
        "version": 1,
        "rules": [
            {
                "match": "openai:*",
                "action": "redact",
                "types": ["EMAIL"],
                "on_detect": "block",
                "scope": ["system"],
            },  # system not allowed
        ],
    }
    with pytest.raises(ValueError, match=r"scope"):
        validate(parsed)


def test_validate_existing_tool_actions_unchanged():
    """Make sure adding `redact` validation didn't regress existing actions."""
    parsed = {
        "version": 1,
        "rules": [
            {"match": "*delete*", "action": "block"},
            {"match": "payments.*", "action": "require_approval"},
            {"match": "slack.*", "action": "audit"},
        ],
    }
    validate(parsed)  # should not raise
