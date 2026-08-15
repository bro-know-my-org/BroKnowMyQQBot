//! Capability-checked HTTP execution for BPP plugins.

use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};

use async_trait::async_trait;
use plugin_api::{
    HttpPermission, HttpRequest, HttpResponse, is_valid_http_credential_name,
    url_path_matches_prefix,
};
use reqwest::{
    Method,
    header::{AUTHORIZATION, HeaderName, HeaderValue, LOCATION},
    redirect::Policy,
};
use secrecy::{ExposeSecret as _, SecretString};
use thiserror::Error;
use tokio::net::lookup_host;
use url::{Host, Url};
use zeroize::Zeroizing;

const MAX_REQUEST_BODY_BYTES: usize = 256 * 1024;
const MAX_REQUEST_HEADERS: usize = 64;
const MAX_REQUEST_HEADER_BYTES: usize = 32 * 1024;
const MAX_BEARER_CREDENTIAL_BYTES: usize = 8 * 1024;
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
const MAX_TIMEOUT_MS: u64 = 15_000;
const MAX_REDIRECTS: usize = 3;
const MAX_RESPONSE_HEADERS: usize = 64;
const MAX_RESPONSE_HEADER_BYTES: usize = 32 * 1024;
const HTTP_BEARER_ENV_PREFIX: &str = "BKMQB_PLUGIN_HTTP_BEARER_";

#[derive(Debug, Error)]
pub enum HttpExecutionError {
    #[error("invalid plugin HTTP request: {0}")]
    InvalidRequest(String),
    #[error("plugin HTTP request is not authorized: {0}")]
    Denied(String),
    #[error("plugin HTTP DNS resolution failed: {0}")]
    Dns(String),
    #[error("plugin HTTP target resolved to a non-public address")]
    NonPublicAddress,
    #[error("plugin HTTP transport failed: {0}")]
    Transport(String),
    #[error("plugin HTTP response exceeds its configured limit")]
    ResponseTooLarge,
}

#[async_trait]
pub trait HttpExecutor: Send + Sync + 'static {
    async fn execute(
        &self,
        permissions: &[HttpPermission],
        granted_capabilities: &BTreeSet<String>,
        request: &HttpRequest,
    ) -> Result<HttpResponse, HttpExecutionError>;
}

#[derive(Default)]
pub struct SecureHttpExecutor {
    bearer_credentials: BTreeMap<String, SecretString>,
}

impl std::fmt::Debug for SecureHttpExecutor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SecureHttpExecutor")
            .field("bearer_credential_count", &self.bearer_credentials.len())
            .finish()
    }
}

impl SecureHttpExecutor {
    pub fn from_environment() -> Result<Self, HttpExecutionError> {
        let mut bearer_credentials = BTreeMap::new();
        for (raw_name, raw_value) in env::vars_os() {
            let Some(name) = raw_name.to_str() else {
                continue;
            };
            let Some(suffix) = name.strip_prefix(HTTP_BEARER_ENV_PREFIX) else {
                continue;
            };
            let credential = suffix.to_ascii_lowercase();
            if suffix.is_empty()
                || suffix.bytes().any(|byte| {
                    !byte.is_ascii_uppercase() && !byte.is_ascii_digit() && byte != b'_'
                })
                || !is_valid_http_credential_name(&credential)
            {
                return Err(HttpExecutionError::InvalidRequest(format!(
                    "environment variable `{name}` has an invalid named HTTP credential suffix"
                )));
            }
            let value_bytes = Zeroizing::new(raw_value.into_encoded_bytes());
            let value = std::str::from_utf8(&value_bytes).map_err(|_| {
                HttpExecutionError::InvalidRequest(format!(
                    "environment variable `{name}` is not valid Unicode"
                ))
            })?;
            validate_bearer_credential_value(&credential, value)?;
            bearer_credentials.insert(
                credential,
                SecretString::from(value.to_owned().into_boxed_str()),
            );
        }
        Ok(Self { bearer_credentials })
    }

    pub fn with_bearer_credential(
        mut self,
        name: impl Into<String>,
        value: SecretString,
    ) -> Result<Self, HttpExecutionError> {
        let name = name.into();
        if !is_valid_http_credential_name(&name) {
            return Err(HttpExecutionError::InvalidRequest(
                "named HTTP credential has an invalid name".to_owned(),
            ));
        }
        validate_bearer_credential_value(&name, value.expose_secret())?;
        self.bearer_credentials.insert(name, value);
        Ok(self)
    }
}

#[async_trait]
impl HttpExecutor for SecureHttpExecutor {
    async fn execute(
        &self,
        permissions: &[HttpPermission],
        granted_capabilities: &BTreeSet<String>,
        request: &HttpRequest,
    ) -> Result<HttpResponse, HttpExecutionError> {
        validate_request_limits(request)?;
        let method = Method::from_bytes(request.method.as_bytes())
            .map_err(|error| HttpExecutionError::InvalidRequest(error.to_string()))?;
        let mut url = Url::parse(&request.url)
            .map_err(|error| HttpExecutionError::InvalidRequest(error.to_string()))?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(request.timeout_ms);
        let mut redirects = 0_usize;

        loop {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .ok_or_else(|| HttpExecutionError::Transport("request timed out".to_owned()))?;
            let (host, port, permission) =
                authorized_target(&url, method.as_str(), permissions, granted_capabilities)?;
            let addresses = tokio::time::timeout(remaining, resolve_public_addresses(&host, port))
                .await
                .map_err(|_| {
                    HttpExecutionError::Transport("DNS resolution timed out".to_owned())
                })??;
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .ok_or_else(|| HttpExecutionError::Transport("request timed out".to_owned()))?;
            let client = reqwest::Client::builder()
                .https_only(true)
                .no_proxy()
                .redirect(Policy::none())
                .timeout(remaining)
                .resolve_to_addrs(&host, &addresses)
                .build()
                .map_err(|error| HttpExecutionError::Transport(error.to_string()))?;
            let mut builder = client.request(method.clone(), url.clone());
            for (name, value) in &request.headers {
                validate_request_header(name)?;
                let name = HeaderName::from_bytes(name.as_bytes())
                    .map_err(|error| HttpExecutionError::InvalidRequest(error.to_string()))?;
                let value = HeaderValue::from_str(value)
                    .map_err(|error| HttpExecutionError::InvalidRequest(error.to_string()))?;
                builder = builder.header(name, value);
            }
            if let Some(credential) = &permission.credential {
                let value = bearer_authorization_value(&self.bearer_credentials, credential)?;
                validate_credential_injection_limits(request, &value)?;
                builder = builder.header(AUTHORIZATION, value);
            }
            if let Some(body) = &request.body {
                builder = builder.body(body.clone());
            }
            let mut response = builder
                .send()
                .await
                .map_err(|error| HttpExecutionError::Transport(error.to_string()))?;

            if matches!(method, Method::GET | Method::HEAD) && response.status().is_redirection() {
                if let Some(location) = response.headers().get(LOCATION) {
                    if redirects >= MAX_REDIRECTS {
                        return Err(HttpExecutionError::Denied(
                            "redirect limit exceeded".to_owned(),
                        ));
                    }
                    let location = location.to_str().map_err(|_| {
                        HttpExecutionError::InvalidRequest(
                            "redirect location is not valid UTF-8".to_owned(),
                        )
                    })?;
                    url = url
                        .join(location)
                        .map_err(|error| HttpExecutionError::InvalidRequest(error.to_string()))?;
                    redirects += 1;
                    continue;
                }
            }

            let response_limit = request.max_response_bytes.min(MAX_RESPONSE_BYTES);
            if response
                .content_length()
                .is_some_and(|length| length > response_limit)
            {
                return Err(HttpExecutionError::ResponseTooLarge);
            }
            let headers = response_headers(response.headers())?;
            let status = response.status().as_u16();
            let final_url = response.url().to_string();
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|error| HttpExecutionError::Transport(error.to_string()))?
            {
                if body.len().saturating_add(chunk.len())
                    > usize::try_from(response_limit).unwrap_or(usize::MAX)
                {
                    return Err(HttpExecutionError::ResponseTooLarge);
                }
                body.extend_from_slice(&chunk);
            }
            return Ok(HttpResponse {
                status,
                final_url,
                headers,
                body,
            });
        }
    }
}

fn bearer_authorization_value(
    credentials: &BTreeMap<String, SecretString>,
    credential: &str,
) -> Result<HeaderValue, HttpExecutionError> {
    let secret = credentials.get(credential).ok_or_else(|| {
        HttpExecutionError::Denied(format!(
            "named bearer credential `{credential}` is unavailable"
        ))
    })?;
    let authorization =
        SecretString::from(format!("Bearer {}", secret.expose_secret()).into_boxed_str());
    let mut value = HeaderValue::from_str(authorization.expose_secret()).map_err(|_| {
        HttpExecutionError::InvalidRequest(format!(
            "named bearer credential `{credential}` cannot be represented as an HTTP header"
        ))
    })?;
    value.set_sensitive(true);
    Ok(value)
}

fn validate_bearer_credential_value(
    credential: &str,
    value: &str,
) -> Result<(), HttpExecutionError> {
    if value.trim().is_empty() {
        return Err(HttpExecutionError::InvalidRequest(format!(
            "named HTTP credential `{credential}` is empty"
        )));
    }
    if value.len() > MAX_BEARER_CREDENTIAL_BYTES {
        return Err(HttpExecutionError::InvalidRequest(format!(
            "named HTTP credential `{credential}` exceeds {MAX_BEARER_CREDENTIAL_BYTES} bytes"
        )));
    }
    let authorization = SecretString::from(format!("Bearer {value}").into_boxed_str());
    HeaderValue::from_str(authorization.expose_secret()).map_err(|_| {
        HttpExecutionError::InvalidRequest(format!(
            "named HTTP credential `{credential}` cannot be represented as an HTTP header"
        ))
    })?;
    Ok(())
}

fn validate_credential_injection_limits(
    request: &HttpRequest,
    authorization: &HeaderValue,
) -> Result<(), HttpExecutionError> {
    let request_header_bytes = request.headers.iter().fold(
        AUTHORIZATION
            .as_str()
            .len()
            .saturating_add(authorization.as_bytes().len()),
        |total, (name, value)| total.saturating_add(name.len()).saturating_add(value.len()),
    );
    if request.headers.len() >= MAX_REQUEST_HEADERS
        || request_header_bytes > MAX_REQUEST_HEADER_BYTES
    {
        return Err(HttpExecutionError::InvalidRequest(
            "request headers exceed the configured limit after credential injection".to_owned(),
        ));
    }
    Ok(())
}

fn validate_request_limits(request: &HttpRequest) -> Result<(), HttpExecutionError> {
    if request.timeout_ms == 0 || request.timeout_ms > MAX_TIMEOUT_MS {
        return Err(HttpExecutionError::InvalidRequest(format!(
            "timeout_ms must be between 1 and {MAX_TIMEOUT_MS}"
        )));
    }
    if request.max_response_bytes == 0 || request.max_response_bytes > MAX_RESPONSE_BYTES {
        return Err(HttpExecutionError::InvalidRequest(format!(
            "max_response_bytes must be between 1 and {MAX_RESPONSE_BYTES}"
        )));
    }
    if request
        .body
        .as_ref()
        .is_some_and(|body| body.len() > MAX_REQUEST_BODY_BYTES)
    {
        return Err(HttpExecutionError::InvalidRequest(
            "request body exceeds 256 KiB".to_owned(),
        ));
    }
    let request_header_bytes = request
        .headers
        .iter()
        .fold(0_usize, |total, (name, value)| {
            total.saturating_add(name.len()).saturating_add(value.len())
        });
    if request.headers.len() > MAX_REQUEST_HEADERS
        || request_header_bytes > MAX_REQUEST_HEADER_BYTES
    {
        return Err(HttpExecutionError::InvalidRequest(
            "request headers exceed the configured limit".to_owned(),
        ));
    }
    Ok(())
}

fn authorized_target<'a>(
    url: &Url,
    method: &str,
    permissions: &'a [HttpPermission],
    granted_capabilities: &BTreeSet<String>,
) -> Result<(String, u16, &'a HttpPermission), HttpExecutionError> {
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(HttpExecutionError::Denied(
            "only credential-free HTTPS URLs without fragments are allowed".to_owned(),
        ));
    }
    let Some(Host::Domain(host)) = url.host() else {
        return Err(HttpExecutionError::Denied(
            "HTTP targets must use DNS names, not IP literals".to_owned(),
        ));
    };
    let port = url.port_or_known_default().ok_or_else(|| {
        HttpExecutionError::InvalidRequest("URL does not provide a port".to_owned())
    })?;
    let mut matching_permissions = permissions.iter().filter(|permission| {
        permission.host == host
            && permission.port == port
            && permission.methods.contains(method)
            && permission
                .path_prefixes
                .iter()
                .any(|prefix| path_matches(url.path(), prefix))
    });
    let Some(permission) = matching_permissions.next() else {
        return Err(HttpExecutionError::Denied(format!(
            "{method} {host}:{port}{} is outside the manifest allowlist",
            url.path()
        )));
    };
    if matching_permissions.any(|other| other.credential != permission.credential) {
        return Err(HttpExecutionError::Denied(
            "HTTP target matches permissions with conflicting named credentials".to_owned(),
        ));
    }
    if !granted_capabilities.contains("http.request")
        || !granted_capabilities.contains(&permission.capability())
    {
        return Err(HttpExecutionError::Denied(
            "administrator did not grant the HTTP target".to_owned(),
        ));
    }
    if permission
        .credential_capability()
        .is_some_and(|capability| !granted_capabilities.contains(&capability))
    {
        return Err(HttpExecutionError::Denied(
            "administrator did not grant the named HTTP credential".to_owned(),
        ));
    }
    Ok((host.to_owned(), port, permission))
}

fn path_matches(path: &str, prefix: &str) -> bool {
    url_path_matches_prefix(path, prefix)
}

fn validate_request_header(name: &str) -> Result<(), HttpExecutionError> {
    let lower = name.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "authorization"
            | "connection"
            | "content-length"
            | "cookie"
            | "host"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    ) {
        return Err(HttpExecutionError::Denied(format!(
            "request header `{name}` is controlled by the Host"
        )));
    }
    Ok(())
}

async fn resolve_public_addresses(
    host: &str,
    port: u16,
) -> Result<Vec<SocketAddr>, HttpExecutionError> {
    let addresses = lookup_host((host, port))
        .await
        .map_err(|error| HttpExecutionError::Dns(error.to_string()))?
        .take(17)
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        return Err(HttpExecutionError::Dns(
            "DNS returned no addresses".to_owned(),
        ));
    }
    if addresses.len() > 16 {
        return Err(HttpExecutionError::Dns(
            "DNS returned too many addresses".to_owned(),
        ));
    }
    if addresses.iter().any(|address| !is_public_ip(address.ip())) {
        return Err(HttpExecutionError::NonPublicAddress);
    }
    Ok(addresses)
}

fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    !(a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 88 && c == 99)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 224)
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(address) = address.to_ipv4() {
        return is_public_ipv4(address);
    }
    let segments = address.segments();
    !(address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || segments[0] & 0xfe00 == 0xfc00
        || segments[0] & 0xffc0 == 0xfe80
        || segments[0] & 0xffc0 == 0xfec0
        || (segments[0] == 0x0064 && segments[1] == 0xff9b && matches!(segments[2], 0 | 1))
        || (segments[0] == 0x2001 && segments[1] <= 0x01ff)
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        || segments[0] == 0x2002
        || (segments[0] & 0xfff0 == 0x3ff0))
}

fn response_headers(
    source: &reqwest::header::HeaderMap,
) -> Result<BTreeMap<String, String>, HttpExecutionError> {
    if source.len() > MAX_RESPONSE_HEADERS {
        return Err(HttpExecutionError::ResponseTooLarge);
    }
    let mut total = 0_usize;
    let mut headers = BTreeMap::new();
    for (name, value) in source {
        total = total
            .saturating_add(name.as_str().len())
            .saturating_add(value.as_bytes().len());
        if total > MAX_RESPONSE_HEADER_BYTES {
            return Err(HttpExecutionError::ResponseTooLarge);
        }
        if let Ok(value) = value.to_str() {
            headers.insert(name.as_str().to_owned(), value.to_owned());
        }
    }
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        net::IpAddr,
        str::FromStr,
    };

    use plugin_api::{HttpPermission, HttpRequest};
    use reqwest::header::HeaderValue;
    use secrecy::{ExposeSecret as _, SecretString};
    use url::Url;

    use super::{
        MAX_BEARER_CREDENTIAL_BYTES, MAX_REQUEST_HEADERS, SecureHttpExecutor, authorized_target,
        bearer_authorization_value, is_public_ip, validate_credential_injection_limits,
    };

    fn permission() -> HttpPermission {
        HttpPermission {
            host: "api.example.com".to_owned(),
            port: 443,
            methods: BTreeSet::from(["GET".to_owned()]),
            path_prefixes: BTreeSet::from(["/v1/".to_owned()]),
            credential: None,
        }
    }

    #[test]
    fn target_requires_exact_granted_host_method_and_path() {
        let permission = permission();
        let grants = BTreeSet::from(["http.request".to_owned(), permission.capability()]);
        let allowed = Url::parse("https://api.example.com/v1/status").unwrap();
        assert!(
            authorized_target(&allowed, "GET", std::slice::from_ref(&permission), &grants).is_ok()
        );

        for denied in [
            "http://api.example.com/v1/status",
            "https://api.example.com.evil.test/v1/status",
            "https://api.example.com/v2/status",
            "https://127.0.0.1/v1/status",
        ] {
            let denied = Url::parse(denied).unwrap();
            assert!(
                authorized_target(&denied, "GET", std::slice::from_ref(&permission), &grants)
                    .is_err()
            );
        }
        assert!(authorized_target(&allowed, "POST", &[permission], &grants).is_err());
    }

    #[test]
    fn named_bearer_permission_requires_an_explicit_grant() {
        let mut permission = permission();
        permission.credential = Some("github_issue".to_owned());
        let url = Url::parse("https://api.example.com/v1/issues").unwrap();
        let mut grants = BTreeSet::from(["http.request".to_owned(), permission.capability()]);
        assert!(
            authorized_target(&url, "GET", std::slice::from_ref(&permission), &grants).is_err()
        );
        grants.insert(permission.credential_capability().unwrap());
        assert!(authorized_target(&url, "GET", std::slice::from_ref(&permission), &grants).is_ok());
    }

    #[test]
    fn conflicting_matching_credentials_are_rejected_defensively() {
        let mut first = permission();
        first.credential = Some("github_issue".to_owned());
        let mut second = permission();
        second.path_prefixes = BTreeSet::from(["/v1/issues/".to_owned()]);
        second.credential = Some("other_token".to_owned());
        let url = Url::parse("https://api.example.com/v1/issues/new").unwrap();
        let grants = BTreeSet::from([
            "http.request".to_owned(),
            first.capability(),
            first.credential_capability().unwrap(),
            second.credential_capability().unwrap(),
        ]);
        assert!(authorized_target(&url, "GET", &[first, second], &grants).is_err());
    }

    #[test]
    fn programmatic_credentials_enforce_name_and_value_contracts() {
        let secret = || SecretString::from("token".to_owned().into_boxed_str());
        assert!(
            SecureHttpExecutor::default()
                .with_bearer_credential("github_issue", secret())
                .is_ok()
        );
        assert!(
            SecureHttpExecutor::default()
                .with_bearer_credential("GITHUB_ISSUE", secret())
                .is_err()
        );
        assert!(
            SecureHttpExecutor::default()
                .with_bearer_credential(
                    "github_issue",
                    SecretString::from(" ".to_owned().into_boxed_str()),
                )
                .is_err()
        );
        assert!(
            SecureHttpExecutor::default()
                .with_bearer_credential(
                    "github_issue",
                    SecretString::from("token\r\nforged: value".to_owned().into_boxed_str()),
                )
                .is_err()
        );
        assert!(
            SecureHttpExecutor::default()
                .with_bearer_credential(
                    "github_issue",
                    SecretString::from(
                        "x".repeat(MAX_BEARER_CREDENTIAL_BYTES + 1).into_boxed_str(),
                    ),
                )
                .is_err()
        );
    }

    #[test]
    fn host_builds_a_sensitive_bearer_header_without_returning_the_secret() {
        let credentials = BTreeMap::from([(
            "github_issue".to_owned(),
            SecretString::from("github_pat_test".to_owned().into_boxed_str()),
        )]);
        let value = bearer_authorization_value(&credentials, "github_issue").unwrap();
        assert!(value.is_sensitive());
        assert_eq!(value.to_str().unwrap(), "Bearer github_pat_test");
        assert_eq!(
            credentials["github_issue"].expose_secret(),
            "github_pat_test"
        );
    }

    #[test]
    fn credential_injection_obeys_the_total_header_count_limit() {
        let mut request = HttpRequest {
            method: "GET".to_owned(),
            url: "https://api.example.com/v1/issues".to_owned(),
            headers: (0..MAX_REQUEST_HEADERS)
                .map(|index| (format!("x-header-{index}"), "value".to_owned()))
                .collect(),
            body: None,
            timeout_ms: 1_000,
            max_response_bytes: 1_024,
        };
        let authorization = HeaderValue::from_static("Bearer token");
        assert!(validate_credential_injection_limits(&request, &authorization).is_err());
        request.headers.pop_last();
        assert!(validate_credential_injection_limits(&request, &authorization).is_ok());
    }

    #[test]
    fn rejects_private_local_metadata_and_documentation_addresses() {
        for address in [
            "0.0.0.0",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.168.0.1",
            "198.18.0.1",
            "203.0.113.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
        ] {
            assert!(
                !is_public_ip(IpAddr::from_str(address).unwrap()),
                "{address}"
            );
        }
        for address in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            assert!(
                is_public_ip(IpAddr::from_str(address).unwrap()),
                "{address}"
            );
        }
    }
}
