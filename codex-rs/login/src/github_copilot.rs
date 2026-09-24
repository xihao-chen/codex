//! GitHub device authorization and separate Copilot credentials.

use std::io;
use std::path::Path;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClient;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_http_client::RouteAwareClientPool;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use url::Url;

const GITHUB_URL: &str = "https://github.com";
const TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

#[path = "github_copilot_storage.rs"]
mod storage;

#[derive(Clone, Deserialize, Serialize)]
pub struct CopilotCredentials {
    github_token: String,
    copilot_token: String,
    expires_at: u64,
    base_url: String,
}

impl CopilotCredentials {
    pub fn api_token(&self) -> &str {
        &self.github_token
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn account_identity(&self) -> String {
        format!(
            "github-copilot:{:x}",
            Sha256::digest(self.github_token.as_bytes())
        )
    }

    pub fn is_expired(&self) -> io::Result<bool> {
        Ok(unix_time()?.saturating_add(300) >= self.expires_at)
    }
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: Option<u64>,
}

#[derive(Deserialize)]
struct OAuthTokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    interval: Option<u64>,
}

#[derive(Deserialize)]
struct CopilotTokenResponse {
    token: String,
    expires_at: u64,
}

fn unix_time() -> io::Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_secs())
}

fn http_client() -> HttpClient {
    RouteAwareClientPool::new_without_redirects_or_request_logging(
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        ClientRouteClass::Auth,
    )
    .into_client()
}

async fn post_form<T: for<'de> Deserialize<'de>>(
    client: &HttpClient,
    url: &str,
    body: String,
) -> io::Result<T> {
    let response = tokio::time::timeout(
        HTTP_TIMEOUT,
        client
            .post(url)
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("User-Agent", "codex")
            .body(body)
            .send(),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "GitHub authorization timed out"))?
    .map_err(io::Error::other)?;
    if !response.status().is_success() {
        return Err(io::Error::other(format!(
            "GitHub authorization failed with HTTP {}",
            response.status()
        )));
    }
    response.json().await.map_err(io::Error::other)
}

fn copilot_base_url(token: &str) -> io::Result<String> {
    let proxy_host = token
        .split(';')
        .find_map(|part| part.strip_prefix("proxy-ep="))
        .ok_or_else(|| io::Error::other("Copilot token did not include an API endpoint"))?;
    let domain = proxy_host
        .strip_prefix("proxy.")
        .ok_or_else(|| io::Error::other("invalid Copilot API endpoint"))?;
    let url = Url::parse(&format!("https://api.{domain}"))
        .map_err(|_| io::Error::other("invalid Copilot API endpoint"))?;
    let host = url
        .host_str()
        .ok_or_else(|| io::Error::other("invalid Copilot API endpoint"))?;
    if !host.ends_with(".githubcopilot.com")
        || !host.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'.' || byte == b'-'
        })
        || url.port().is_some()
        || url.path() != "/"
    {
        return Err(io::Error::other("untrusted Copilot API endpoint"));
    }
    Ok(url.origin().ascii_serialization())
}

pub fn load(home: &Path) -> io::Result<Option<CopilotCredentials>> {
    storage::load_credentials(home)?
        .map(|credential| {
            if credential.base_url != copilot_base_url(&credential.copilot_token)? {
                return Err(io::Error::other(
                    "stored Copilot API endpoint does not match its token",
                ));
            }
            Ok(credential)
        })
        .transpose()
}

pub fn logout(home: &Path) -> io::Result<bool> {
    storage::remove_files(home)
}

pub fn load_models_cache(home: &Path) -> io::Result<Option<String>> {
    storage::load_models_cache(home)
}

pub fn save_models_cache(home: &Path, value: &str) -> io::Result<()> {
    storage::save_models_cache(home, value)
}

pub async fn current(home: &Path) -> io::Result<CopilotCredentials> {
    let credential = load(home)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "GitHub Copilot is not logged in; run `codex login --provider github-copilot`",
        )
    })?;
    if !credential.is_expired()? {
        return Ok(credential);
    }
    refresh_with_token(home, credential.github_token).await
}

async fn refresh_with_token(home: &Path, github_token: String) -> io::Result<CopilotCredentials> {
    let refreshed = exchange_token(&http_client(), github_token.clone()).await?;
    storage::save_credentials(
        home,
        &refreshed,
        storage::CredentialWrite::RefreshIfGithubToken(&github_token),
    )?;
    Ok(refreshed)
}

pub async fn refresh(home: &Path) -> io::Result<()> {
    let credential = load(home)?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "GitHub Copilot is not logged in")
    })?;
    refresh_with_token(home, credential.github_token).await?;
    Ok(())
}

async fn exchange_token(
    client: &HttpClient,
    github_token: String,
) -> io::Result<CopilotCredentials> {
    let response = tokio::time::timeout(
        HTTP_TIMEOUT,
        client
            .get(TOKEN_URL)
            .header("Accept", "application/json")
            .header("User-Agent", "codex")
            .header("Authorization", format!("token {github_token}"))
            .send(),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Copilot token exchange timed out"))?
    .map_err(io::Error::other)?;
    if !response.status().is_success() {
        return Err(io::Error::other(format!(
            "Copilot token exchange failed with HTTP {}",
            response.status()
        )));
    }
    let token: CopilotTokenResponse = response.json().await.map_err(io::Error::other)?;
    if token.token.len() > 8192 || github_token.len() > 8192 {
        return Err(io::Error::other("Copilot credential is too large"));
    }
    Ok(CopilotCredentials {
        base_url: copilot_base_url(&token.token)?,
        github_token,
        copilot_token: token.token,
        expires_at: token.expires_at,
    })
}

pub async fn login(
    home: &Path,
    client_id: &str,
    on_code: impl FnOnce(&str, &str),
) -> io::Result<()> {
    if !valid_client_id(client_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "GitHub Copilot OAuth client ID must contain 1-128 ASCII letters, digits, or dots",
        ));
    }
    let client = http_client();
    let device: DeviceCodeResponse = post_form(
        &client,
        &format!("{GITHUB_URL}/login/device/code"),
        format!(
            "client_id={}&scope=read%3Auser",
            urlencoding::encode(client_id)
        ),
    )
    .await?;
    if device.device_code.is_empty()
        || device.device_code.len() > 2048
        || device.user_code.is_empty()
        || device.user_code.len() > 32
        || device.expires_in == 0
    {
        return Err(io::Error::other(
            "invalid GitHub device authorization response",
        ));
    }
    let verification = Url::parse(&device.verification_uri)
        .map_err(|_| io::Error::other("invalid GitHub device authorization URL"))?;
    if verification.scheme() != "https" || verification.host_str() != Some("github.com") {
        return Err(io::Error::other(
            "untrusted GitHub device authorization URL",
        ));
    }
    on_code(verification.as_str(), &device.user_code);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(device.expires_in);
    let mut interval = device.interval.unwrap_or(5).max(5);
    let github_token = loop {
        tokio::time::sleep(
            Duration::from_secs(interval)
                .min(deadline.saturating_duration_since(tokio::time::Instant::now())),
        )
        .await;
        if tokio::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "GitHub device authorization expired",
            ));
        }
        let result: OAuthTokenResponse = post_form(
            &client,
            &format!("{GITHUB_URL}/login/oauth/access_token"),
            format!(
                "client_id={}&device_code={}&grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code",
                urlencoding::encode(client_id),
                urlencoding::encode(&device.device_code)
            ),
        )
        .await?;
        if let Some(token) = result.access_token.filter(|token| !token.is_empty()) {
            break token;
        }
        match result.error.as_deref() {
            Some("authorization_pending") => {}
            Some("slow_down") => {
                interval = interval.saturating_add(5).max(result.interval.unwrap_or(0));
            }
            Some(error) => return Err(io::Error::other(format!("GitHub authorization: {error}"))),
            None => return Err(io::Error::other("invalid GitHub authorization response")),
        }
    };
    let credential = exchange_token(&client, github_token).await?;
    storage::save_credentials(home, &credential, storage::CredentialWrite::Login)
}

fn valid_client_id(client_id: &str) -> bool {
    !client_id.is_empty()
        && client_id.len() <= 128
        && client_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.')
}

#[cfg(test)]
#[path = "github_copilot_tests.rs"]
mod tests;
