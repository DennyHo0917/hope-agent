use anyhow::Result;
use serde_json::Value;

use super::helpers::{
    build_search_client_for_url, read_text_capped, status_error, JSON_RESPONSE_BYTE_CAP,
};
use super::{SearchParams, SearchResult};

pub(super) async fn search_searxng(
    instance_url: &str,
    query: &str,
    count: usize,
    params: &SearchParams,
    timeout_secs: u64,
) -> Result<Vec<SearchResult>> {
    let mut url = format!(
        "{}/search?q={}&format=json&categories=general&pageno=1",
        instance_url.trim_end_matches('/'),
        urlencoding::encode(query)
    );
    if let Some(ref lang) = params.language {
        url.push_str(&format!("&language={}", urlencoding::encode(lang)));
    }
    if let Some(ref freshness) = params.freshness {
        url.push_str(&format!("&time_range={}", urlencoding::encode(freshness)));
    }
    let cfg = crate::config::cached_config();
    let resp = fetch_response(
        &url,
        timeout_secs,
        cfg.ssrf.default_policy,
        &cfg.ssrf.trusted_hosts,
    )
    .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        app_warn!(
            "tool",
            "web_search",
            "SearXNG request failed with HTTP {}",
            status.as_u16()
        );
        return Err(status_error("SearXNG", status));
    }
    let body_text = read_text_capped(resp, JSON_RESPONSE_BYTE_CAP)
        .await
        .map_err(|e| anyhow::anyhow!("SearXNG response read failed: {}", e))?;
    let body: Value = serde_json::from_str(&body_text).map_err(|e| {
        app_warn!(
            "tool",
            "web_search",
            "SearXNG JSON parse failed: {} ({}B response)",
            e,
            body_text.len()
        );
        anyhow::anyhow!("SearXNG JSON parse failed: {}", e)
    })?;
    let results = body.get("results").and_then(|v| v.as_array());
    let parsed: Vec<SearchResult> = results.map_or_else(Vec::new, |arr| {
        arr.iter()
            .take(count)
            .filter_map(|item| {
                let title = item.get("title")?.as_str()?.to_string();
                let url = item.get("url")?.as_str()?.to_string();
                let snippet = item
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let source = item
                    .get("engines")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|e| e.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "SearXNG".into());
                Some(SearchResult {
                    title,
                    url,
                    snippet,
                    source,
                })
            })
            .collect()
    });
    if parsed.is_empty() {
        app_warn!(
            "tool",
            "web_search",
            "SearXNG returned 0 results ({}B response)",
            body_text.len()
        );
    }
    Ok(parsed)
}

async fn fetch_response(
    url: &str,
    timeout_secs: u64,
    policy: crate::security::ssrf::SsrfPolicy,
    trusted_hosts: &[String],
) -> Result<reqwest::Response> {
    Ok(
        crate::security::http_redirect::checked_get_with_client_factory(
            url,
            policy,
            trusted_hosts,
            5,
            |target| build_search_client_for_url(target.as_str(), timeout_secs),
        )
        .await?
        .response,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::ssrf::SsrfPolicy;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn read_request(socket: &mut tokio::net::TcpStream) {
        let mut request = Vec::new();
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let mut chunk = [0; 1024];
            let count = socket.read(&mut chunk).await.unwrap();
            assert!(count > 0, "request ended before HTTP headers");
            request.extend_from_slice(&chunk[..count]);
            assert!(request.len() <= 8192, "fixture request too large");
        }
    }

    #[tokio::test]
    async fn private_redirect_is_blocked_without_leaking_search_query() {
        let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin_addr = origin.local_addr().unwrap();
        let target_addr = target.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = origin.accept().await.unwrap();
            read_request(&mut socket).await;
            socket.write_all(format!("HTTP/1.1 302 Found\r\nLocation: http://{target_addr}/?q=PRIVATE_CANARY\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        });
        let err = fetch_response(
            &format!("http://{origin_addr}/search?q=PRIVATE_CANARY"),
            2,
            SsrfPolicy::Strict,
            &[origin_addr.to_string()],
        )
        .await
        .unwrap_err();
        assert!(!err.to_string().contains("PRIVATE_CANARY"));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), target.accept())
                .await
                .is_err()
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn relative_redirect_works_and_a_loop_fails() {
        for location in ["/final", "/search"] {
            let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = origin.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = origin.accept().await.unwrap();
                read_request(&mut socket).await;
                socket.write_all(format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
                drop(socket);
                if location == "/final" {
                    let (mut socket, _) = origin.accept().await.unwrap();
                    read_request(&mut socket).await;
                    socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                        )
                        .await
                        .unwrap();
                }
            });
            let result = fetch_response(
                &format!("http://{addr}/search"),
                2,
                SsrfPolicy::Strict,
                &[addr.to_string()],
            )
            .await;
            if location == "/final" {
                assert_eq!(result.unwrap().text().await.unwrap(), "{}");
            } else {
                assert!(result.unwrap_err().to_string().contains("loop"));
            }
            server.await.unwrap();
        }
    }
}
