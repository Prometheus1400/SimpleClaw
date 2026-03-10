use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use rand::RngCore;
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::form_urlencoded;

use crate::error::FrameworkError;

use super::TokenSet;

pub(crate) const OPENAI_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub(crate) const OPENAI_OAUTH_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub(crate) const OPENAI_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub(crate) const OPENAI_OAUTH_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const OPENAI_OAUTH_LOOPBACK_BIND: &str = "localhost:1455";

#[derive(Debug, Clone)]
pub(crate) struct PkceState {
    pub(crate) code_verifier: String,
    pub(crate) code_challenge: String,
    pub(crate) state: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OAuthErrorResponse {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

pub(crate) fn generate_pkce_state() -> PkceState {
    let code_verifier = random_urlsafe_token(48);
    let challenge_bytes = Sha256::digest(code_verifier.as_bytes());
    let code_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(challenge_bytes);
    let state = random_urlsafe_token(24);
    PkceState {
        code_verifier,
        code_challenge,
        state,
    }
}

pub(crate) fn build_authorize_url(pkce: &PkceState) -> String {
    let query = form_urlencoded::Serializer::new(String::new())
        .append_pair("response_type", "code")
        .append_pair("client_id", OPENAI_OAUTH_CLIENT_ID)
        .append_pair("redirect_uri", OPENAI_OAUTH_REDIRECT_URI)
        .append_pair("scope", "openid profile email offline_access")
        .append_pair("code_challenge", &pkce.code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &pkce.state)
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("id_token_add_organizations", "true")
        .finish();
    format!("{OPENAI_OAUTH_AUTHORIZE_URL}?{query}")
}

pub(crate) async fn receive_loopback_code(
    expected_state: &str,
    timeout: Duration,
) -> Result<String, FrameworkError> {
    let listener = TcpListener::bind(OPENAI_OAUTH_LOOPBACK_BIND)
        .await
        .map_err(|err| oauth_error(format!("failed to bind callback listener: {err}")))?;

    let accepted = tokio::time::timeout(timeout, listener.accept())
        .await
        .map_err(|_| oauth_error("timed out waiting for browser callback".to_owned()))?
        .map_err(|err| oauth_error(format!("failed to accept callback connection: {err}")))?;

    let (mut stream, _) = accepted;
    let mut buffer = vec![0_u8; 8192];
    let bytes_read = stream
        .read(&mut buffer)
        .await
        .map_err(|err| oauth_error(format!("failed to read callback request: {err}")))?;

    let request = String::from_utf8_lossy(&buffer[..bytes_read]);
    let first_line = request
        .lines()
        .next()
        .ok_or_else(|| oauth_error("malformed callback request".to_owned()))?;
    let path = first_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| oauth_error("callback request missing path".to_owned()))?;

    let code_result = parse_code_from_redirect(path, Some(expected_state));
    let (status_line, body) = if code_result.is_ok() {
        (
            "HTTP/1.1 200 OK",
            "<html><body><h2>SimpleClaw login complete</h2><p>You can close this tab.</p></body></html>",
        )
    } else {
        (
            "HTTP/1.1 400 Bad Request",
            "<html><body><h2>SimpleClaw login failed</h2><p>Return to terminal for details.</p></body></html>",
        )
    };

    let response = format!(
        "{status_line}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes()).await;

    code_result
}

pub(crate) async fn exchange_code_for_tokens(
    client: &Client,
    code: &str,
    pkce: &PkceState,
) -> Result<TokenSet, FrameworkError> {
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", OPENAI_OAUTH_CLIENT_ID),
        ("redirect_uri", OPENAI_OAUTH_REDIRECT_URI),
        ("code_verifier", pkce.code_verifier.as_str()),
    ];

    let response = client
        .post(OPENAI_OAUTH_TOKEN_URL)
        .form(&form)
        .send()
        .await
        .map_err(|err| oauth_error(format!("failed to exchange auth code: {err}")))?;

    parse_token_response(response).await
}

pub(crate) async fn refresh_access_token(
    client: &Client,
    refresh_token: &str,
) -> Result<TokenSet, FrameworkError> {
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", OPENAI_OAUTH_CLIENT_ID),
    ];

    let response = client
        .post(OPENAI_OAUTH_TOKEN_URL)
        .form(&form)
        .send()
        .await
        .map_err(|err| oauth_error(format!("failed to refresh token: {err}")))?;

    parse_token_response(response).await
}

pub(crate) fn parse_code_from_redirect(
    input: &str,
    expected_state: Option<&str>,
) -> Result<String, FrameworkError> {
    let query = input
        .split_once('?')
        .map(|(_, value)| value)
        .unwrap_or(input);
    let params: HashMap<String, String> = form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect();

    if let Some(err) = params.get("error") {
        let description = params
            .get("error_description")
            .cloned()
            .unwrap_or_else(|| "OAuth authorization failed".to_owned());
        return Err(oauth_error(format!(
            "authorization failed: {err} ({description})"
        )));
    }

    if let Some(expected) = expected_state {
        match params.get("state") {
            Some(state) if state == expected => {}
            Some(_) => return Err(oauth_error("state mismatch".to_owned())),
            None => return Err(oauth_error("missing state in callback".to_owned())),
        }
    }

    match params.get("code") {
        Some(code) if !code.trim().is_empty() => Ok(code.clone()),
        _ => Err(oauth_error("missing code in callback".to_owned())),
    }
}

pub(crate) fn extract_account_id_from_jwt(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims = serde_json::from_slice::<serde_json::Value>(&decoded).ok()?;

    for key in [
        "account_id",
        "accountId",
        "acct",
        "sub",
        "https://api.openai.com/account_id",
    ] {
        if let Some(value) = claims.get(key).and_then(serde_json::Value::as_str)
            && !value.trim().is_empty()
        {
            return Some(value.to_owned());
        }
    }
    None
}

async fn parse_token_response(response: reqwest::Response) -> Result<TokenSet, FrameworkError> {
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        if let Ok(parsed) = serde_json::from_str::<OAuthErrorResponse>(&body) {
            let detail = parsed.error_description.unwrap_or(parsed.error);
            return Err(oauth_error(format!(
                "token request failed ({status}): {detail}"
            )));
        }

        return Err(oauth_error(format!(
            "token request failed ({status}): {body}"
        )));
    }

    let token = response
        .json::<TokenResponse>()
        .await
        .map_err(|err| oauth_error(format!("failed to parse token response: {err}")))?;

    let expires_at_unix = token.expires_in.and_then(|seconds| {
        if seconds <= 0 {
            None
        } else {
            Some(now_unix().saturating_add(seconds))
        }
    });

    Ok(TokenSet {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        id_token: token.id_token,
        expires_at_unix,
        token_type: token.token_type,
        scope: token.scope,
    })
}

fn random_urlsafe_token(bytes_len: usize) -> String {
    let mut bytes = vec![0_u8; bytes_len];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn now_unix() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs() as i64,
        Err(_) => 0,
    }
}

fn oauth_error(message: String) -> FrameworkError {
    FrameworkError::Provider(format!("OpenAI OAuth {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_generation_is_nonempty() {
        let pkce = generate_pkce_state();
        assert!(pkce.code_verifier.len() >= 43);
        assert!(!pkce.code_challenge.is_empty());
        assert!(!pkce.state.is_empty());
    }

    #[test]
    fn authorize_url_contains_expected_parameters() {
        let pkce = PkceState {
            code_verifier: "verifier".to_owned(),
            code_challenge: "challenge".to_owned(),
            state: "state123".to_owned(),
        };
        let url = build_authorize_url(&pkce);
        assert!(url.starts_with(OPENAI_OAUTH_AUTHORIZE_URL));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge=challenge"));
        assert!(url.contains("state=state123"));
    }

    #[test]
    fn parse_redirect_extracts_code() {
        let code = parse_code_from_redirect("/auth/callback?code=abc123&state=xyz", Some("xyz"))
            .expect("code should parse");
        assert_eq!(code, "abc123");
    }

    #[test]
    fn parse_redirect_rejects_state_mismatch() {
        let err = parse_code_from_redirect("/auth/callback?code=abc123&state=nope", Some("xyz"))
            .expect_err("state mismatch should fail");
        assert!(err.to_string().contains("state mismatch"));
    }

    #[test]
    fn extract_account_id_from_jwt_reads_claim() {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("{}");
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"account_id":"acct_123"}"#);
        let token = format!("{header}.{payload}.sig");
        let account_id = extract_account_id_from_jwt(&token);
        assert_eq!(account_id.as_deref(), Some("acct_123"));
    }
}
