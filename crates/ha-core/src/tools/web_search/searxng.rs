use anyhow::Result;
use serde_json::Value;

use super::helpers::{
    build_search_client_for_url, read_text_capped, request_error, status_error,
    JSON_RESPONSE_BYTE_CAP,
};
use super::{SearchParams, SearchResult};

pub(super) async fn search_searxng(
    instance_url: &str,
    query: &str,
    count: usize,
    params: &SearchParams,
    timeout_secs: u64,
) -> Result<Vec<SearchResult>> {
    let client = build_search_client_for_url(instance_url, timeout_secs)?;
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
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|error| request_error("SearXNG", error))?;
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
