//! `POST /agents/discover` fetches a URL the caller chose. These pin the
//! egress guard against a real local listener: a private address is refused
//! before any connection, a redirect is not followed, the card is size
//! capped, and every fetch failure reads the same.
//!
//! Until 2026-11 the registries used a default client: it followed
//! redirects, reached loopback and metadata addresses, and returned the
//! connection error, so the route could read internal services and map ports.

use jamjet_agents::outbound::{discover_card, DiscoveryError, FETCH_FAILED, MAX_CARD_BYTES};
use jamjet_agents::{AgentRegistry, InMemoryAgentRegistry};
use reqwest::Url;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn card_json() -> String {
    card_with_description("a card")
}

fn card_with_description(description: &str) -> String {
    use jamjet_agents::card::{AgentCapabilities, AgentCard, AuthSpec, AutonomyLevel};
    serde_json::to_string(&AgentCard {
        id: "egress".into(),
        uri: "jamjet://test/egress".into(),
        name: "egress".into(),
        description: description.into(),
        version: "1.0.0".into(),
        capabilities: AgentCapabilities {
            skills: vec![],
            protocols: vec![],
            tools_provided: vec![],
            tools_consumed: vec![],
        },
        autonomy: AutonomyLevel::Guided,
        constraints: None,
        auth: AuthSpec::None,
        latency_class: None,
        cost_class: None,
        reasoning_modes: vec![],
        labels: Default::default(),
    })
    .unwrap()
}

/// A one-route HTTP server on 127.0.0.1 that answers every request with
/// `response` and counts connections.
async fn serve(response: String) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            let response = response.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    (format!("http://{addr}"), hits)
}

fn ok_json(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// The default registry path refuses a loopback URL without connecting.
#[tokio::test]
async fn discovery_refuses_a_loopback_url_without_connecting() {
    let (url, hits) = serve(ok_json(&card_json())).await;
    let registry = InMemoryAgentRegistry::new();
    let err = registry.discover_remote(&url).await.unwrap_err();
    assert!(err.contains("refused"), "{err}");
    assert_eq!(hits.load(Ordering::SeqCst), 0, "no connection is made");

    // https to the same listener is refused for its address, too.
    let https = url.replace("http://", "https://");
    let err = registry.discover_remote(&https).await.unwrap_err();
    assert!(err.contains("refused"), "{err}");
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

/// A name (not an IP literal) that resolves to loopback is refused after the
/// lookup, before any connection: the check that stops `*.nip.io`-style names
/// and A records pointing inward. `fetch_card` skips the name heuristics, so
/// only the resolved-address check stands between this and the listener.
#[tokio::test]
async fn a_name_that_resolves_inward_is_refused_without_connecting() {
    let (url, hits) = serve(ok_json(&card_json())).await;
    let port = url.rsplit(':').next().unwrap();
    let named = Url::parse(&format!("https://localhost:{port}/.well-known/agent.json")).unwrap();
    let err = jamjet_agents::outbound::fetch_card(&named, false)
        .await
        .unwrap_err();
    assert_eq!(
        err,
        DiscoveryError::FetchFailed,
        "the same answer as any failure"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0, "no connection is made");
}

/// With the local-development opt-in the same card is fetched and parsed:
/// the control for the test above.
#[tokio::test]
async fn the_opt_in_fetches_a_local_card() {
    let (url, hits) = serve(ok_json(&card_json())).await;
    let card = discover_card(&url, true).await.expect("fetched");
    assert_eq!(card.id, "egress");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

/// A redirect is not followed, even to an address the opt-in allows.
#[tokio::test]
async fn a_redirect_is_not_followed() {
    let (inner, inner_hits) = serve(ok_json(&card_json())).await;
    let redirect = format!(
        "HTTP/1.1 302 Found\r\nlocation: {inner}/.well-known/agent.json\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    );
    let (outer, _) = serve(redirect).await;
    let err = discover_card(&outer, true).await.unwrap_err();
    assert_eq!(err, DiscoveryError::FetchFailed);
    assert_eq!(
        inner_hits.load(Ordering::SeqCst),
        0,
        "the redirect target is never reached"
    );
}

/// A card past the size cap is refused rather than read into memory.
#[tokio::test]
async fn an_oversized_card_is_refused() {
    // A valid card, so only the cap can refuse it.
    let big = card_with_description(&"x".repeat(MAX_CARD_BYTES + 1));
    let (url, _) = serve(ok_json(&big)).await;
    assert_eq!(
        discover_card(&url, true).await.unwrap_err(),
        DiscoveryError::FetchFailed
    );
}

/// The same, sent with no `content-length`, so the cap on what is read (not
/// the declared length) is what refuses it.
#[tokio::test]
async fn an_oversized_card_without_a_length_is_refused() {
    let big = card_with_description(&"x".repeat(MAX_CARD_BYTES + 1));
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{big}"
    );
    let (url, _) = serve(response).await;
    assert_eq!(
        discover_card(&url, true).await.unwrap_err(),
        DiscoveryError::FetchFailed
    );
}

/// A closed port, an HTTP error and a body that is not a card all read the
/// same to the caller, so the route cannot tell open ports from closed ones.
#[tokio::test]
async fn every_fetch_failure_reads_the_same() {
    // A port with nothing listening.
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        format!("http://{addr}")
    };
    let (not_found, _) = serve(
        "HTTP/1.1 404 Not Found\r\ncontent-length: 9\r\nconnection: close\r\n\r\nnot here!".into(),
    )
    .await;
    let (not_a_card, _) = serve(ok_json("{\"hello\":\"world\"}")).await;
    for url in [closed, not_found, not_a_card] {
        let err = discover_card(&url, true).await.unwrap_err();
        assert_eq!(err, DiscoveryError::FetchFailed, "{url}");
        assert_eq!(err.to_string(), FETCH_FAILED);
    }
}
