use reqwest::Client;
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, HeaderMap, HeaderValue, USER_AGENT};
use scraper::{Html, Selector};
use serde::Deserialize;
use std::io::Read;
use tokio::time::{Duration, timeout};

const DEFAULT_WEB_FETCH_TIMEOUT_SECONDS: u64 = 20;
const DEFAULT_WEB_FETCH_MAX_CHARS: usize = 8_000;

#[derive(Debug, Deserialize)]
struct WebFetchArgs {
    url: String,
    timeout_seconds: Option<u64>,
    max_chars: Option<usize>,
}

fn main() {
    if let Err(msg) = run() {
        eprintln!("{msg}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| format!("failed reading stdin: {e}"))?;
    let args: WebFetchArgs = serde_json::from_str(&input)
        .map_err(|e| format!("web_fetch requires JSON object args: {e}"))?;

    let timeout_seconds = args
        .timeout_seconds
        .unwrap_or(DEFAULT_WEB_FETCH_TIMEOUT_SECONDS);
    let max_chars = args.max_chars.unwrap_or(DEFAULT_WEB_FETCH_MAX_CHARS);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to initialize runtime: {e}"))?;
    let output = runtime.block_on(fetch_url_markdown(&args.url, timeout_seconds, max_chars))?;
    print!("{output}");
    Ok(())
}

async fn fetch_url_markdown(
    url: &str,
    timeout_seconds: u64,
    max_chars: usize,
) -> Result<String, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("fetch requires a non-empty url".to_owned());
    }

    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/134.0.0.0 Safari/537.36",
        ),
    );
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"),
    );
    headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.9"));

    let client = Client::builder()
        .default_headers(headers)
        .build()
        .map_err(|e| format!("fetch client build failed: {e}"))?;

    let response = timeout(Duration::from_secs(timeout_seconds), client.get(url).send())
        .await
        .map_err(|_| format!("fetch timed out after {timeout_seconds}s"))?
        .map_err(|e| format!("fetch request failed: {e}"))?;

    let status = response.status();
    let body = timeout(Duration::from_secs(timeout_seconds), response.text())
        .await
        .map_err(|_| format!("fetch timed out after {timeout_seconds}s"))?
        .map_err(|e| format!("fetch body read failed: {e}"))?;

    render_fetch_response(url, status.as_u16(), &body, max_chars)
}

fn render_fetch_response(
    url: &str,
    status_code: u16,
    body: &str,
    max_chars: usize,
) -> Result<String, String> {
    if !(200..300).contains(&status_code) {
        return Err(format!("fetch response error: status={status_code} url={url}"));
    }

    if body.contains("<html") || body.contains("<body") {
        let doc = Html::parse_document(body);
        let selector = Selector::parse("body")
            .map_err(|e| format!("html selector parse failed: {e}"))?;
        let text = doc
            .select(&selector)
            .flat_map(|node| node.text())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");

        return Ok(text.chars().take(max_chars).collect::<String>());
    }

    Ok(body.chars().take(max_chars).collect())
}
