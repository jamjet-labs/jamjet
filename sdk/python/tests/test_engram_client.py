"""Tests for the Engram Python client."""

from __future__ import annotations

from jamjet.engram import EngramClient
from jamjet.engram.client import ConsolidationResult, ContextBlock


def test_client_defaults():
    client = EngramClient()
    assert client.base_url == "http://localhost:9090"
    assert client._token is None


def test_client_custom_url():
    client = EngramClient("http://my-server:8080", api_token="tok_123")
    assert client.base_url == "http://my-server:8080"
    assert client._token == "tok_123"


def test_auth_headers_with_token():
    client = EngramClient(api_token="tok_abc")
    headers = client._auth_headers()
    assert headers["Authorization"] == "Bearer tok_abc"


def test_auth_headers_without_token():
    client = EngramClient()
    headers = client._auth_headers()
    assert headers == {}


def test_context_block_dataclass():
    block = ContextBlock(
        text="<memory>test</memory>",
        token_count=5,
        facts_included=1,
        facts_omitted=0,
    )
    assert block.text == "<memory>test</memory>"
    assert block.token_count == 5
    assert block.tier_breakdown == {}


def test_consolidation_result_defaults():
    result = ConsolidationResult()
    assert result.facts_decayed == 0
    assert result.llm_calls_used == 0


def test_trailing_slash_stripped():
    client = EngramClient("http://localhost:9090/")
    assert client.base_url == "http://localhost:9090"
