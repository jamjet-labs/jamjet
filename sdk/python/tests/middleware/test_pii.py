from jamjet.cloud.middleware.pii import RegexDetector, PIIDetection


def test_regex_detector_finds_email():
    d = RegexDetector(types=["EMAIL"])
    detections = d.scan("contact alice@example.com for details")
    assert len(detections) == 1
    assert detections[0].type == "EMAIL"
    assert detections[0].value == "alice@example.com"


def test_regex_detector_finds_us_ssn():
    d = RegexDetector(types=["US_SSN"])
    detections = d.scan("ssn 123-45-6789 on file")
    assert any(x.type == "US_SSN" and x.value == "123-45-6789" for x in detections)


def test_regex_detector_finds_credit_card():
    d = RegexDetector(types=["CREDIT_CARD"])
    detections = d.scan("card 4111 1111 1111 1111 expires 12/26")
    assert any(x.type == "CREDIT_CARD" for x in detections)


def test_regex_detector_ignores_unrequested_types():
    d = RegexDetector(types=["EMAIL"])
    detections = d.scan("ssn 123-45-6789 email alice@example.com")
    assert len(detections) == 1
    assert detections[0].type == "EMAIL"


def test_regex_detector_empty_input_returns_empty():
    d = RegexDetector(types=["EMAIL"])
    assert d.scan("") == []
    assert d.scan("no pii here") == []


def test_redact_in_place_substitutes_tokens():
    d = RegexDetector(types=["EMAIL", "US_SSN"])
    out = d.redact("contact alice@example.com or ssn 123-45-6789")
    assert "alice@example.com" not in out
    assert "123-45-6789" not in out
    assert "[REDACTED:EMAIL]" in out
    assert "[REDACTED:US_SSN]" in out
