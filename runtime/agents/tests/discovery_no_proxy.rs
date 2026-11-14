//! Discovery ignores proxy environment variables. A proxy resolves the host
//! itself, so the address check and the pin would not apply to where the
//! request really went. Its own test binary, because it sets the process
//! environment.

use jamjet_agents::outbound::discover_card;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn counting_server(response: &'static str) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
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

#[tokio::test]
async fn a_proxy_in_the_environment_is_not_used() {
    let (proxy, proxy_hits) = counting_server(
        "HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
    )
    .await;
    let (target, target_hits) =
        counting_server("HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            .await;
    for key in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        unsafe {
            std::env::set_var(key, &proxy);
        }
    }
    for key in ["NO_PROXY", "no_proxy"] {
        unsafe {
            std::env::remove_var(key);
        }
    }
    let _ = discover_card(&target, true).await;
    assert_eq!(proxy_hits.load(Ordering::SeqCst), 0, "the proxy was used");
    assert_eq!(
        target_hits.load(Ordering::SeqCst),
        1,
        "the target was reached directly"
    );
}
