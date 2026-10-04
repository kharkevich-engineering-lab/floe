//! GitHub's REST client (§B.11): headers, conditional GETs through the
//! [`HttpCache`], `Link` pagination, rate limits (primary, secondary,
//! `retry-after`) and bounded retries. Requests are strictly sequential: no
//! concurrency against the API, which avoids most secondary limits.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

use reqwest::StatusCode;
use reqwest::header::{HeaderMap, HeaderValue};

use crate::http_cache::{CacheEntry, HttpCache};
use crate::source::{ApiStats, SourceError};

const TIMEOUT: Duration = Duration::from_secs(30);
/// 5xx / transport errors are retried this many times.
const RETRIES: u32 = 2;
/// GitHub's guidance for a secondary limit without headers: wait at least a minute.
const SECONDARY_MIN: Duration = Duration::from_mins(1);
const SECONDARY_MAX: Duration = Duration::from_mins(15);

/// One GET's result.
#[derive(Debug, Clone, PartialEq)]
pub enum Fetched {
    /// 200, or a 304 served from the cache. `next` = `Link rel="next"`, from
    /// the response, else (a 304 need not repeat it) the cached page's.
    Ok {
        body: serde_json::Value,
        next: Option<String>,
    },
    NotFound,
    /// 401/403 that is not a rate limit (`message` for the log).
    Forbidden(String),
}

pub struct GithubHttp {
    client: reqwest::Client,
    min_rate_remaining: u32,
    /// Consecutive secondary-limit hits (doubling back-off), across passes.
    secondary_hits: AtomicU32,
}

impl GithubHttp {
    /// Built once at startup: a client builder error is a startup `Err`.
    pub fn new(min_rate_remaining: u32) -> Result<Self, SourceError> {
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(concat!("floe-mirror/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| SourceError::Http(format!("building the HTTP client: {e}")))?;
        Ok(GithubHttp {
            client,
            min_rate_remaining,
            secondary_hits: AtomicU32::new(0),
        })
    }

    /// A conditional GET of `url` (`ETag` from `cache`), projected by `project`
    /// before it is cached. Rate limits are errors; below `min_rate_remaining`
    /// is [`SourceError::RateLimited`] too (the pass stops incomplete).
    pub async fn get(
        &self,
        token: &str,
        url: &str,
        cache: &mut HttpCache,
        stats: &mut ApiStats,
        project: impl Fn(serde_json::Value) -> Result<serde_json::Value, SourceError>,
    ) -> Result<Fetched, SourceError> {
        let cached = cache.get(url).cloned();
        let mut attempt = 0u32;
        let resp = loop {
            let mut req = self
                .client
                .get(url)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .bearer_auth(token);
            if let Some(c) = cached.as_ref().filter(|c| !may_have_grown(url, c)) {
                req = req.header("If-None-Match", c.etag.as_str());
            }
            stats.requests += 1;
            match req.send().await {
                Ok(r) if r.status().is_server_error() && attempt < RETRIES => {
                    metrics::counter!("floe_mirror_api_requests_total", "source" => "github", "status" => "5xx").increment(1);
                }
                Ok(r) => break r,
                Err(e) if attempt < RETRIES => {
                    tracing::debug!(url, error = %e, "github request failed; retrying");
                }
                Err(e) => return Err(SourceError::Http(format!("GET {url}: {e}"))),
            }
            let d = floe_store::util::backoff(attempt, Duration::from_millis(500), Duration::from_secs(5));
            tokio::time::sleep(d).await;
            attempt += 1;
        };
        let status = resp.status();
        let headers = resp.headers().clone();
        metrics::counter!("floe_mirror_api_requests_total", "source" => "github", "status" => status_label(status)).increment(1);
        let (remaining, reset) = rate_headers(&headers);
        if remaining.is_some() {
            stats.rate_remaining = remaining;
            stats.rate_reset = reset;
            if let Some(r) = remaining {
                metrics::gauge!("floe_mirror_rate_remaining", "source" => "github").set(f64::from(r));
            }
        }
        let next_link = headers
            .get("link")
            .and_then(|v| v.to_str().ok())
            .and_then(next_link);
        if status == StatusCode::NOT_MODIFIED {
            stats.not_modified += 1;
            let Some(c) = cached else {
                return Err(SourceError::Http(format!("GET {url}: 304 without a cached body")));
            };
            self.secondary_hits.store(0, Ordering::Relaxed);
            let next = next_link.or(c.next.clone());
            self.floor(remaining, reset, stats)?;
            return Ok(Fetched::Ok {
                body: c.body,
                next,
            });
        }
        let body = resp
            .bytes()
            .await
            .map_err(|e| SourceError::Http(format!("GET {url}: reading the body: {e}")))?;
        if status == StatusCode::FORBIDDEN || status == StatusCode::TOO_MANY_REQUESTS {
            let consecutive = self.secondary_hits.load(Ordering::Relaxed);
            if let Some(until) = rate_limited(status, &headers, &body, SystemTime::now(), consecutive) {
                if is_secondary(&body) {
                    self.secondary_hits.fetch_add(1, Ordering::Relaxed);
                }
                stats.rate_limited_until = Some(until);
                return Err(SourceError::RateLimited { until });
            }
        }
        match status {
            s if s.is_success() => {
                self.secondary_hits.store(0, Ordering::Relaxed);
                let json: serde_json::Value = serde_json::from_slice(&body)
                    .map_err(|e| SourceError::Decode(format!("GET {url}: {e}")))?;
                let projected = project(json)?;
                match headers.get("etag").and_then(|v| v.to_str().ok()) {
                    Some(etag) => cache.put(
                        url,
                        CacheEntry {
                            etag: etag.to_string(),
                            next: next_link.clone(),
                            body: projected.clone(),
                        },
                    ),
                    None => cache.remove(url),
                }
                self.floor(remaining, reset, stats)?;
                Ok(Fetched::Ok {
                    body: projected,
                    next: next_link,
                })
            }
            StatusCode::UNAUTHORIZED => Err(SourceError::Unauthorized),
            StatusCode::NOT_FOUND => {
                cache.remove(url);
                Ok(Fetched::NotFound)
            }
            StatusCode::FORBIDDEN => Ok(Fetched::Forbidden(message(&body))),
            s => Err(SourceError::Http(format!("GET {url}: {s}: {}", message(&body)))),
        }
    }

    /// Stop below the configured floor of the primary limit; the loop then
    /// sleeps until the reset (`stats.rate_limited_until`), like a 403/429.
    fn floor(
        &self,
        remaining: Option<u32>,
        reset: Option<i64>,
        stats: &mut ApiStats,
    ) -> Result<(), SourceError> {
        match remaining {
            Some(r) if r < self.min_rate_remaining => {
                let until = reset
                    .and_then(|s| u64::try_from(s).ok())
                    .map_or_else(SystemTime::now, |s| SystemTime::UNIX_EPOCH + Duration::from_secs(s));
                stats.rate_limited_until = Some(until);
                Err(SourceError::RateLimited { until })
            }
            _ => Ok(()),
        }
    }
}

/// A cached last page (no `next`) that is full (`per_page` items) may have
/// grown a next page that leaves its own body, and so its `ETag`, unchanged; a
/// 304 without `Link` would then hide it. Such a page is fetched unconditionally
/// (§B.11: a 304 is never taken as the last page on its own).
fn may_have_grown(url: &str, c: &CacheEntry) -> bool {
    let per_page = url
        .split(['?', '&'])
        .find_map(|p| p.strip_prefix("per_page="))
        .and_then(|v| v.parse::<usize>().ok());
    match (per_page, c.body.as_array()) {
        (Some(n), Some(items)) => c.next.is_none() && items.len() >= n,
        _ => false,
    }
}

fn status_label(s: StatusCode) -> &'static str {
    match s.as_u16() {
        304 => "304",
        200..=299 => "200",
        400..=499 => "4xx",
        _ => "5xx",
    }
}

/// `x-ratelimit-remaining`, `x-ratelimit-reset`.
pub fn rate_headers(h: &HeaderMap) -> (Option<u32>, Option<i64>) {
    let num = |k: &str| h.get(k).and_then(|v: &HeaderValue| v.to_str().ok()).map(str::trim);
    (
        num("x-ratelimit-remaining").and_then(|v| v.parse().ok()),
        num("x-ratelimit-reset").and_then(|v| v.parse().ok()),
    )
}

/// The `rel="next"` URL of a `Link` header.
pub fn next_link(link: &str) -> Option<String> {
    link.split(',').find_map(|part| {
        let mut it = part.split(';');
        let url = it.next()?.trim();
        let is_next = it.any(|p| {
            let p = p.trim();
            p == "rel=\"next\"" || p == "rel=next"
        });
        (is_next && url.starts_with('<') && url.ends_with('>'))
            .then(|| url.trim_start_matches('<').trim_end_matches('>').to_string())
    })
}

fn message(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(str::to_string))
        .unwrap_or_default()
}

fn is_secondary(body: &[u8]) -> bool {
    let v: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
    let field = |k: &str| {
        v.get(k)
            .and_then(|m| m.as_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default()
    };
    field("message").contains("secondary rate limit")
        || field("documentation_url").contains("secondary-rate-limits")
}

/// Whether a 403/429 is a rate limit, and until when (§B.11): `retry-after`;
/// `x-ratelimit-remaining = 0` (until `x-ratelimit-reset`); or a secondary
/// limit named in the body, which often carries neither header: at least 60 s,
/// doubled per consecutive hit up to 15 min. A 429 is always a limit.
pub fn rate_limited(
    status: StatusCode,
    h: &HeaderMap,
    body: &[u8],
    now: SystemTime,
    consecutive_secondary: u32,
) -> Option<SystemTime> {
    if status != StatusCode::FORBIDDEN && status != StatusCode::TOO_MANY_REQUESTS {
        return None;
    }
    if let Some(secs) = h
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        return Some(now + Duration::from_secs(secs));
    }
    let (remaining, reset) = rate_headers(h);
    if remaining == Some(0) {
        let at = reset
            .and_then(|s| u64::try_from(s).ok())
            .map_or(now + SECONDARY_MIN, |s| SystemTime::UNIX_EPOCH + Duration::from_secs(s));
        return Some(at.max(now));
    }
    if is_secondary(body) || status == StatusCode::TOO_MANY_REQUESTS {
        let wait = SECONDARY_MIN
            .saturating_mul(1u32 << consecutive_secondary.min(8))
            .min(SECONDARY_MAX);
        return Some(now + wait);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_static(v));
        }
        h
    }

    #[test]
    fn parses_link_next() {
        let l = r#"<https://api.github.com/user/repos?page=2>; rel="next", <https://api.github.com/user/repos?page=5>; rel="last""#;
        assert_eq!(
            next_link(l).as_deref(),
            Some("https://api.github.com/user/repos?page=2")
        );
        assert_eq!(next_link(r#"<https://x?page=1>; rel="prev""#), None);
    }

    #[test]
    fn a_full_last_page_is_refetched_unconditionally() {
        let entry = |n: usize, next: Option<&str>| CacheEntry {
            etag: "\"e\"".into(),
            next: next.map(str::to_string),
            body: serde_json::Value::Array(vec![serde_json::Value::Null; n]),
        };
        let url = "https://api.github.com/user/repos?affiliation=owner&per_page=100&sort=full_name";
        assert!(may_have_grown(url, &entry(100, None)));
        assert!(!may_have_grown(url, &entry(99, None)), "short: really the last page");
        assert!(!may_have_grown(url, &entry(100, Some("https://api.github.com/x?page=2"))));
        assert!(!may_have_grown("https://api.github.com/user", &entry(100, None)));
        let one = CacheEntry {
            etag: "\"u\"".into(),
            next: None,
            body: serde_json::json!({"login": "me"}),
        };
        assert!(!may_have_grown(url, &one), "not a listing");
    }

    #[test]
    fn classifies_rate_limits() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let f = StatusCode::FORBIDDEN;
        // retry-after
        assert_eq!(
            rate_limited(f, &headers(&[("retry-after", "30")]), b"{}", now, 0),
            Some(now + Duration::from_secs(30))
        );
        // primary exhausted
        assert_eq!(
            rate_limited(
                f,
                &headers(&[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", "1000100")]),
                b"{}",
                now,
                0
            ),
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_100))
        );
        // secondary without headers and remaining > 0: at least 60 s, doubling, capped.
        let body = br#"{"message":"You have exceeded a secondary rate limit."}"#;
        let h = headers(&[("x-ratelimit-remaining", "4000")]);
        assert_eq!(rate_limited(f, &h, body, now, 0), Some(now + Duration::from_mins(1)));
        assert_eq!(rate_limited(f, &h, body, now, 1), Some(now + Duration::from_mins(2)));
        assert_eq!(rate_limited(f, &h, body, now, 9), Some(now + Duration::from_mins(15)));
        let doc = br#"{"message":"x","documentation_url":"https://docs.github.com/rest/overview/rate-limits-for-the-rest-api#about-secondary-rate-limits"}"#;
        assert!(rate_limited(f, &h, doc, now, 0).is_some());
        // An access-denied 403 is not a limit.
        assert_eq!(
            rate_limited(f, &h, br#"{"message":"Resource not accessible"}"#, now, 0),
            None
        );
        assert_eq!(rate_limited(StatusCode::NOT_FOUND, &h, body, now, 0), None);
    }
}
