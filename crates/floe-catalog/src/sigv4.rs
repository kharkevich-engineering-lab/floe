//! AWS Signature V4 for an Iceberg REST catalog (D63, `[catalog] auth =
//! "sigv4"`): `RustFS` S3 Tables and AWS S3 Tables refuse unsigned catalog
//! requests, and `iceberg-catalog-rest` 0.10.1 has no request hook (its
//! `RestCatalogBuilder::with_client` takes a plain `reqwest::Client`, and
//! reqwest 0.12 has no per-request middleware).
//!
//! The hook is a [`SigningProxy`]: an in-process HTTP server on a Unix socket
//! in a private (0700) temporary directory. The catalog gets a
//! `reqwest::Client` whose every connection goes to that socket
//! ([`SigningProxy::client`]) and the catalog URI with an `http` scheme
//! ([`SigningProxy::catalog_uri`]). The proxy checks the `Host`, signs the
//! request with [`Signer`] (the official `aws-sigv4` crate: canonical request,
//! `x-amz-content-sha256` of the body, `X-Amz-Date`, `X-Amz-Security-Token`),
//! and forwards it over a normal (TLS-capable) client to the real endpoint,
//! streaming the response back. Nothing listens on a TCP port, and only this
//! user can open the socket.
//!
//! Credentials come from [`CredentialSource`] (D43, the same rules as the S3
//! store): both configured env vars set = static keys (plus `AWS_SESSION_TOKEN`),
//! neither = the AWS SDK default chain (ECS/EC2 roles, SSO, profiles), one alone
//! = an error. Temporary credentials are refreshed five minutes before they
//! expire. The same source feeds the data-file `FileIO` (`crate::iceberg`).
//!
//! This module depends on nothing else in floe-catalog, so the planned shared
//! Iceberg crate (`floe-ice`, code-intelligence design §2.2, R1) can lift it
//! as is. Secrets never reach a log or an error message.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use aws_credential_types::Credentials;
use aws_credential_types::provider::{ProvideCredentials, SharedCredentialsProvider};
use aws_sigv4::http_request::{
    PayloadChecksumKind, PercentEncodingMode, SignableBody, SignableRequest, SigningSettings,
    UriPathNormalizationMode, sign,
};
use aws_sigv4::sign::v4;
use axum::body::Body;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::{Method, StatusCode};

/// Refresh temporary credentials this long before they expire.
const REFRESH_BEFORE: Duration = Duration::from_mins(5);
/// Largest catalog request body the proxy buffers (it must hash the body).
/// Iceberg REST bodies are small JSON documents; a commit is kilobytes.
const MAX_REQUEST_BODY: usize = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum SigV4Error {
    #[error("sigv4: {0}")]
    Config(String),
    #[error("sigv4: credentials: {0}")]
    Credentials(String),
    #[error("sigv4: signing failed: {0}")]
    Sign(String),
    #[error("sigv4: {0}")]
    Io(#[from] std::io::Error),
}

/// Where request-signing credentials come from (D43). Cheap to share.
pub struct CredentialSource {
    provider: SharedCredentialsProvider,
    cached: tokio::sync::Mutex<Option<Credentials>>,
}

impl fmt::Debug for CredentialSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialSource").finish_non_exhaustive()
    }
}

impl CredentialSource {
    /// D43: the env vars named `access_key_env`/`secret_key_env` both set =
    /// static keys (with `AWS_SESSION_TOKEN` when set); neither = the AWS SDK
    /// default credential chain for `region`; one alone fails closed.
    pub async fn from_env(
        access_key_env: &str,
        secret_key_env: &str,
        region: &str,
    ) -> Result<CredentialSource, SigV4Error> {
        let fixed = static_credentials(
            std::env::var(access_key_env).ok(),
            std::env::var(secret_key_env).ok(),
            std::env::var("AWS_SESSION_TOKEN").ok(),
        )
        .map_err(|()| {
            SigV4Error::Config(format!(
                "set both {access_key_env} and {secret_key_env}, or neither to use the AWS credential chain"
            ))
        })?;
        if let Some(creds) = fixed {
            return Ok(CredentialSource::fixed(creds));
        }
        let sdk = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(region.to_string()))
            .load()
            .await;
        let provider = sdk.credentials_provider().ok_or_else(|| {
            SigV4Error::Credentials("the AWS SDK has no credential provider".into())
        })?;
        Ok(CredentialSource::from_provider(provider))
    }

    /// Fixed credentials (tests, and the static-key case).
    pub fn fixed(creds: Credentials) -> CredentialSource {
        CredentialSource::from_provider(SharedCredentialsProvider::new(creds))
    }

    pub fn from_provider(provider: SharedCredentialsProvider) -> CredentialSource {
        CredentialSource {
            provider,
            cached: tokio::sync::Mutex::new(None),
        }
    }

    /// Current credentials: the cached ones while they have more than
    /// [`REFRESH_BEFORE`] left, else a fresh load (one at a time).
    pub async fn get(&self) -> Result<Credentials, SigV4Error> {
        let mut cached = self.cached.lock().await;
        if let Some(c) = cached.as_ref()
            && fresh(c, SystemTime::now())
        {
            return Ok(c.clone());
        }
        let loaded = self
            .provider
            .provide_credentials()
            .await
            .map_err(|e| SigV4Error::Credentials(error_chain(&e)))?;
        *cached = Some(loaded.clone());
        Ok(loaded)
    }
}

fn fresh(c: &Credentials, now: SystemTime) -> bool {
    c.expiry().is_none_or(|exp| {
        exp.duration_since(now)
            .is_ok_and(|left| left > REFRESH_BEFORE)
    })
}

/// D43's pairing rule (the S3 store's `explicit_credentials`): `Err` = one of
/// the two is missing or empty.
fn static_credentials(
    access_key: Option<String>,
    secret_key: Option<String>,
    session_token: Option<String>,
) -> Result<Option<Credentials>, ()> {
    match (access_key, secret_key) {
        (None, None) => Ok(None),
        (Some(access), Some(secret)) if !access.is_empty() && !secret.is_empty() => {
            let token = session_token.filter(|t| !t.is_empty());
            Ok(Some(Credentials::new(
                access,
                secret,
                token,
                None,
                "floe-catalog-static",
            )))
        }
        _ => Err(()),
    }
}

/// An error and its sources on one line (credential provider errors keep the
/// useful part in `source()`). They never carry key material.
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        out.push_str(": ");
        out.push_str(&s.to_string());
        src = s.source();
    }
    out
}

/// Signs requests for one service and region.
#[derive(Debug)]
pub struct Signer {
    creds: Arc<CredentialSource>,
    service: String,
    region: String,
}

impl Signer {
    pub fn new(creds: Arc<CredentialSource>, service: &str, region: &str) -> Signer {
        Signer {
            creds,
            service: service.to_string(),
            region: region.to_string(),
        }
    }

    pub fn credentials(&self) -> &Arc<CredentialSource> {
        &self.creds
    }

    /// Adds `Authorization`, `X-Amz-Date`, `x-amz-content-sha256` and, with
    /// temporary credentials, `X-Amz-Security-Token` to `headers`, signing
    /// every header already there (`Host` included) except `User-Agent`.
    pub async fn sign(
        &self,
        method: &str,
        url: &str,
        headers: &mut HeaderMap,
        body: &[u8],
    ) -> Result<(), SigV4Error> {
        let creds = self.creds.get().await?;
        sign_headers(
            &SigningInput {
                method,
                url,
                body,
                service: &self.service,
                region: &self.region,
                time: SystemTime::now(),
                payload_header: true,
            },
            headers,
            &creds,
        )
    }
}

/// One request to sign (all borrowed).
pub struct SigningInput<'a> {
    pub method: &'a str,
    /// Absolute, already percent-encoded URL as it goes on the wire.
    pub url: &'a str,
    pub body: &'a [u8],
    pub service: &'a str,
    pub region: &'a str,
    pub time: SystemTime,
    /// Add and sign `x-amz-content-sha256` (what S3-style verifiers, `RustFS`'s
    /// catalog included, require; harmless elsewhere).
    pub payload_header: bool,
}

/// The canonical-request rules for `service`: S3 signs the path as sent
/// (single encoding, no normalization), every other service (`s3tables`
/// included) double-encodes and normalizes it. Same split as botocore's
/// `S3SigV4Auth`/`SigV4Auth`, which `RustFS`'s own conformance scripts use.
pub fn settings_for(service: &str, payload_header: bool) -> SigningSettings {
    let mut s = SigningSettings::default();
    if service == "s3" {
        s.percent_encoding_mode = PercentEncodingMode::Single;
        s.uri_path_normalization_mode = UriPathNormalizationMode::Disabled;
    }
    s.payload_checksum_kind = if payload_header {
        PayloadChecksumKind::XAmzSha256
    } else {
        PayloadChecksumKind::NoHeader
    };
    s
}

/// Signs `input` with `creds` and adds the resulting headers to `headers`
/// (which are the headers signed). Pure: the clock is `input.time`.
pub fn sign_headers(
    input: &SigningInput<'_>,
    headers: &mut HeaderMap,
    creds: &Credentials,
) -> Result<(), SigV4Error> {
    let identity = creds.clone().into();
    let params: aws_sigv4::http_request::SigningParams<'_> = v4::SigningParams::builder()
        .identity(&identity)
        .region(input.region)
        .name(input.service)
        .time(input.time)
        .settings(settings_for(input.service, input.payload_header))
        .build()
        .map_err(|e| SigV4Error::Sign(e.to_string()))?
        .into();
    let mut pairs = Vec::with_capacity(headers.len());
    for (name, value) in headers.iter() {
        let value = value
            .to_str()
            .map_err(|_| SigV4Error::Sign(format!("header {name} is not visible ASCII")))?;
        pairs.push((name.as_str(), value));
    }
    let signable = SignableRequest::new(
        input.method,
        input.url,
        pairs.into_iter(),
        SignableBody::Bytes(input.body),
    )
    .map_err(|e| SigV4Error::Sign(e.to_string()))?;
    let (instructions, _signature) = sign(signable, &params)
        .map_err(|e| SigV4Error::Sign(e.to_string()))?
        .into_parts();
    let (signed, _query) = instructions.into_parts();
    for h in signed {
        let name = HeaderName::from_bytes(h.name().as_bytes())
            .map_err(|_| SigV4Error::Sign(format!("bad header name {}", h.name())))?;
        let mut value = HeaderValue::from_str(h.value())
            .map_err(|_| SigV4Error::Sign(format!("bad {name} value")))?;
        value.set_sensitive(h.sensitive() || name == header::AUTHORIZATION);
        headers.insert(name, value);
    }
    Ok(())
}

/// Hop-by-hop headers (RFC 9110 §7.6.1) and the ones the proxy recomputes.
fn dropped_on_forward(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "host"
            | "content-length"
            | "authorization"
            | "x-amz-date"
            | "x-amz-security-token"
            | "x-amz-content-sha256"
    )
}

struct ProxyState {
    signer: Signer,
    /// `scheme://host[:port]` of the real endpoint.
    origin: String,
    /// The `Host` the catalog's requests carry, and the one signed.
    host: String,
    upstream: reqwest::Client,
}

/// The in-process signing hop between `RestCatalog` and the catalog endpoint.
/// Dropping it stops the server and removes the socket directory.
pub struct SigningProxy {
    catalog_uri: String,
    client: reqwest::Client,
    task: tokio::task::JoinHandle<()>,
    // Removed on drop, after the task is aborted.
    _dir: tempfile::TempDir,
}

impl fmt::Debug for SigningProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SigningProxy")
            .field("catalog_uri", &self.catalog_uri)
            .finish_non_exhaustive()
    }
}

impl Drop for SigningProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl SigningProxy {
    /// Starts the proxy for the catalog at `uri` (`http` or `https`, no
    /// userinfo, query or fragment).
    pub fn start(uri: &str, signer: Signer) -> Result<SigningProxy, SigV4Error> {
        let parsed = reqwest::Url::parse(uri)
            .map_err(|e| SigV4Error::Config(format!("catalog uri {uri:?}: {e}")))?;
        let scheme = parsed.scheme();
        let ok = matches!(scheme, "http" | "https")
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.query().is_none()
            && parsed.fragment().is_none();
        let Some(host) = parsed.host_str().filter(|_| ok) else {
            return Err(SigV4Error::Config(format!(
                "catalog uri {uri:?} must be http(s)://host[:port]/path"
            )));
        };
        // `port()` is None for the scheme's default port, as on the wire.
        let host = match parsed.port() {
            Some(p) => format!("{host}:{p}"),
            None => host.to_string(),
        };
        let origin = format!("{scheme}://{host}");
        let path = parsed.path().trim_end_matches('/');
        let catalog_uri = format!("http://{host}{path}");

        let dir = private_dir()?;
        let socket: PathBuf = dir.path().join("signer.sock");
        let listener = tokio::net::UnixListener::bind(&socket)?;
        let upstream = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| SigV4Error::Config(format!("upstream client: {e}")))?;
        let state = Arc::new(ProxyState {
            signer,
            origin,
            host,
            upstream,
        });
        let app = axum::Router::new().fallback(forward).with_state(state);
        let task = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::warn!(error = %e, "catalog: sigv4 proxy stopped");
            }
        });
        let client = reqwest::Client::builder()
            .unix_socket(socket)
            .build()
            .map_err(|e| SigV4Error::Config(format!("catalog client: {e}")))?;
        Ok(SigningProxy {
            catalog_uri,
            client,
            task,
            _dir: dir,
        })
    }

    /// The URI to give `RestCatalog` (`http`, same host and path).
    pub fn catalog_uri(&self) -> &str {
        &self.catalog_uri
    }

    /// The client to give `RestCatalog`: every request goes through the proxy.
    pub fn client(&self) -> reqwest::Client {
        self.client.clone()
    }
}

/// A fresh 0700 directory for the socket: only this user can connect.
fn private_dir() -> std::io::Result<tempfile::TempDir> {
    use std::os::unix::fs::PermissionsExt;
    tempfile::Builder::new()
        .prefix("floe-sigv4-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
}

/// One catalog request: check, sign, forward, stream the answer back.
async fn forward(State(st): State<Arc<ProxyState>>, req: axum::extract::Request) -> Response {
    match forward_inner(&st, req).await {
        Ok(resp) => resp,
        Err((status, msg)) => {
            tracing::warn!(%status, error = %msg, "catalog: sigv4 proxy refused a request");
            proxy_error(status, &msg)
        }
    }
}

async fn forward_inner(
    st: &ProxyState,
    req: axum::extract::Request,
) -> Result<Response, (StatusCode, String)> {
    let (parts, body) = req.into_parts();
    let host = parts
        .headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok());
    if host != Some(st.host.as_str()) {
        // A catalog `/v1/config` that overrides `uri` to another host lands
        // here: refuse instead of signing for a host nobody configured.
        return Err((
            StatusCode::MISDIRECTED_REQUEST,
            format!(
                "request for host {:?}, but the configured catalog is {:?}",
                host.unwrap_or_default(),
                st.host
            ),
        ));
    }
    let body = axum::body::to_bytes(body, MAX_REQUEST_BODY)
        .await
        .map_err(|e| (StatusCode::PAYLOAD_TOO_LARGE, format!("request body: {e}")))?;
    let path = parts.uri.path_and_query().map_or("/", |p| p.as_str());
    let url = format!("{}{path}", st.origin);

    let mut headers = HeaderMap::with_capacity(parts.headers.len() + 4);
    for (name, value) in &parts.headers {
        if !dropped_on_forward(name) {
            headers.append(name.clone(), value.clone());
        }
    }
    let host_value = HeaderValue::from_str(&st.host)
        .map_err(|_| (StatusCode::BAD_GATEWAY, "bad host".to_string()))?;
    headers.insert(header::HOST, host_value);
    st.signer
        .sign(parts.method.as_str(), &url, &mut headers, &body)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    let resp = st
        .upstream
        .request(parts.method.clone(), &url)
        .headers(headers)
        .body(body)
        .send()
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                format!("{}: {}", st.origin, error_chain(&e)),
            )
        })?;

    let mut out = Response::builder().status(resp.status());
    if let Some(h) = out.headers_mut() {
        for (name, value) in resp.headers() {
            if !matches!(
                name.as_str(),
                "connection" | "keep-alive" | "transfer-encoding" | "trailer" | "upgrade"
            ) {
                h.append(name.clone(), value.clone());
            }
        }
    }
    let body = if parts.method == Method::HEAD {
        Body::empty()
    } else {
        Body::from_stream(resp.bytes_stream())
    };
    out.body(body)
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("response: {e}")))
}

/// An Iceberg REST error document, so `iceberg-catalog-rest` shows the reason.
fn proxy_error(status: StatusCode, msg: &str) -> Response {
    let doc = serde_json::json!({
        "error": {
            "message": msg,
            "type": "FloeSigV4ProxyError",
            "code": status.as_u16(),
        }
    });
    (status, axum::Json(doc)).into_response()
}

#[cfg(test)]
// Helpers outside #[test] fns fail the test the same way.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::UNIX_EPOCH;

    /// AWS `SigV4` test suite (`aws-sigv4`'s `aws-signing-test-suite/v4`):
    /// AKIDEXAMPLE, region us-east-1, service "service", 2015-08-30T12:36:00Z.
    fn suite_creds(token: Option<&str>) -> Credentials {
        Credentials::new(
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            token.map(str::to_string),
            None,
            "test",
        )
    }

    fn suite_time() -> SystemTime {
        UNIX_EPOCH + Duration::from_mins(24_015_636) // 2015-08-30T12:36:00Z
    }

    fn hdrs(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                HeaderName::from_static(k),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    fn suite_input<'a>(method: &'a str, body: &'a [u8], payload_header: bool) -> SigningInput<'a> {
        SigningInput {
            method,
            url: "https://example.amazonaws.com/",
            body,
            service: "service",
            region: "us-east-1",
            time: suite_time(),
            payload_header,
        }
    }

    fn auth(h: &HeaderMap) -> &str {
        h.get(header::AUTHORIZATION).unwrap().to_str().unwrap()
    }

    #[test]
    fn get_vanilla_matches_the_aws_test_suite() {
        let mut h = hdrs(&[("host", "example.amazonaws.com")]);
        sign_headers(&suite_input("GET", b"", false), &mut h, &suite_creds(None)).unwrap();
        assert_eq!(
            auth(&h),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
        assert_eq!(h.get("x-amz-date").unwrap(), "20150830T123600Z");
        assert!(h.get("x-amz-security-token").is_none());
        assert!(h.get(header::AUTHORIZATION).unwrap().is_sensitive());
    }

    #[test]
    fn session_token_is_signed_like_the_aws_test_suite() {
        let token = "6e86291e8372ff2a2260956d9b8aae1d763fbf315fa00fa31553b73ebf194267";
        let mut h = hdrs(&[("host", "example.amazonaws.com")]);
        sign_headers(
            &suite_input("GET", b"", false),
            &mut h,
            &suite_creds(Some(token)),
        )
        .unwrap();
        assert_eq!(
            auth(&h),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date;x-amz-security-token, \
             Signature=07ec1639c89043aa0e3e2de82b96708f198cceab042d4a97044c66dd9f74e7f8"
        );
        assert_eq!(h.get("x-amz-security-token").unwrap(), token);
    }

    #[test]
    fn signed_body_matches_the_aws_test_suite() {
        // post-x-www-form-urlencoded: the payload hash is a signed header.
        let mut h = hdrs(&[
            ("content-type", "application/x-www-form-urlencoded"),
            ("host", "example.amazonaws.com"),
            ("content-length", "13"),
        ]);
        sign_headers(
            &suite_input("POST", b"Param1=value1", true),
            &mut h,
            &suite_creds(None),
        )
        .unwrap();
        assert_eq!(
            h.get("x-amz-content-sha256").unwrap(),
            "9095672bbd1f56dfc5b65f3e153adc8731a4a654192329106275f4c7b24d0b6e"
        );
        assert_eq!(
            auth(&h),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=content-length;content-type;host;x-amz-content-sha256;x-amz-date, \
             Signature=d3875051da38690788ef43de4db0d8f280229d82040bfac253562e56c3f20e0b"
        );
    }

    #[test]
    fn s3_get_object_matches_the_s3_documentation() {
        // "Signature Calculations for the Authorization Header", GET Object.
        let creds = Credentials::new(
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            None,
            None,
            "test",
        );
        let mut h = hdrs(&[
            ("host", "examplebucket.s3.amazonaws.com"),
            ("range", "bytes=0-9"),
        ]);
        let input = SigningInput {
            method: "GET",
            url: "https://examplebucket.s3.amazonaws.com/test.txt",
            body: b"",
            service: "s3",
            region: "us-east-1",
            time: UNIX_EPOCH + Duration::from_hours(380_376), // 2013-05-24T00:00:00Z
            payload_header: true,
        };
        sign_headers(&input, &mut h, &creds).unwrap();
        assert_eq!(
            auth(&h),
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn s3_signs_the_path_as_sent_and_other_services_double_encode() {
        let s3 = settings_for("s3", true);
        assert_eq!(s3.percent_encoding_mode, PercentEncodingMode::Single);
        assert_eq!(
            s3.uri_path_normalization_mode,
            UriPathNormalizationMode::Disabled
        );
        assert_eq!(s3.payload_checksum_kind, PayloadChecksumKind::XAmzSha256);
        let tables = settings_for("s3tables", true);
        assert_eq!(tables.percent_encoding_mode, PercentEncodingMode::Double);
        assert_eq!(
            tables.uri_path_normalization_mode,
            UriPathNormalizationMode::Enabled
        );
        // An escaped namespace separator (%1F) signs differently in the two modes.
        let sig = |service: &str| {
            let mut h = hdrs(&[("host", "h")]);
            let input = SigningInput {
                method: "GET",
                url: "http://h/iceberg/v1/b/namespaces/a%1Fb",
                body: b"",
                service,
                region: "us-east-1",
                time: suite_time(),
                payload_header: true,
            };
            sign_headers(&input, &mut h, &suite_creds(None)).unwrap();
            auth(&h).rsplit("Signature=").next().unwrap().to_string()
        };
        assert_ne!(sig("s3"), sig("s3tables"));
    }

    #[test]
    fn static_credentials_follow_d43() {
        let some = |s: &str| Some(s.to_string());
        assert!(static_credentials(None, None, None).unwrap().is_none());
        assert!(static_credentials(some("a"), None, None).is_err());
        assert!(static_credentials(None, some("s"), None).is_err());
        assert!(static_credentials(some(""), some("s"), None).is_err());
        let c = static_credentials(some("a"), some("s"), some("t"))
            .unwrap()
            .unwrap();
        assert_eq!(c.session_token(), Some("t"));
        let c = static_credentials(some("a"), some("s"), some(""))
            .unwrap()
            .unwrap();
        assert_eq!(c.session_token(), None);
    }

    #[derive(Debug)]
    struct Counting {
        calls: Arc<Mutex<u32>>,
        ttl: Option<Duration>,
    }

    impl ProvideCredentials for Counting {
        fn provide_credentials<'a>(
            &'a self,
        ) -> aws_credential_types::provider::future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            let n = {
                let mut c = self.calls.lock().unwrap();
                *c += 1;
                *c
            };
            let expiry = self.ttl.map(|t| SystemTime::now() + t);
            aws_credential_types::provider::future::ProvideCredentials::ready(Ok(Credentials::new(
                format!("AK{n}"),
                "secret",
                Some(format!("tok{n}")),
                expiry,
                "t",
            )))
        }
    }

    #[tokio::test]
    async fn temporary_credentials_refresh_before_expiry() {
        let calls = Arc::new(Mutex::new(0));
        // Expires inside the refresh window: every get loads again.
        let src = CredentialSource::from_provider(SharedCredentialsProvider::new(Counting {
            calls: calls.clone(),
            ttl: Some(Duration::from_mins(1)),
        }));
        assert_eq!(src.get().await.unwrap().access_key_id(), "AK1");
        assert_eq!(src.get().await.unwrap().access_key_id(), "AK2");
        // Long-lived: cached.
        let calls = Arc::new(Mutex::new(0));
        let src = CredentialSource::from_provider(SharedCredentialsProvider::new(Counting {
            calls: calls.clone(),
            ttl: Some(Duration::from_hours(1)),
        }));
        src.get().await.unwrap();
        src.get().await.unwrap();
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[test]
    fn credential_source_debug_shows_no_secrets() {
        let src = CredentialSource::fixed(suite_creds(Some("tok")));
        let shown = format!("{src:?}");
        assert!(
            !shown.contains("wJalr") && !shown.contains("tok"),
            "{shown}"
        );
    }
}
