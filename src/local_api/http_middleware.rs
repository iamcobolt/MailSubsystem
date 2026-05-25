use anyhow::Context;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    extract::Request,
    extract::State,
    http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    middleware::{from_fn_with_state, Next},
    response::{IntoResponse, Response},
    Json, Router,
};
use serde_json::json;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::config;

use super::state::ApiState;

const LOCAL_API_RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);

#[derive(Clone)]
struct ApiSecurity {
    auth: Option<ApiAuth>,
    rate_limiter: Option<ApiRateLimiter>,
}

impl ApiSecurity {
    fn from_config() -> anyhow::Result<Self> {
        let auth = match config::api_auth_token() {
            Some(token) => Some(ApiAuth {
                token: Arc::from(token),
                scopes: ApiScopes::from_scope_names(config::api_auth_token_scopes())?,
            }),
            None => None,
        };

        let rate_limiter = config::local_api_rate_limit_rpm()?
            .map(|rpm| ApiRateLimiter::new(rpm as usize, LOCAL_API_RATE_LIMIT_WINDOW));

        Ok(Self { auth, rate_limiter })
    }

    fn is_enabled(&self) -> bool {
        self.auth.is_some() || self.rate_limiter.is_some()
    }

    fn authenticate(&self, headers: &HeaderMap) -> Result<ApiPrincipal, StatusCode> {
        match &self.auth {
            Some(auth) => auth.authenticate(headers).ok_or(StatusCode::UNAUTHORIZED),
            None => Ok(ApiPrincipal::tokenless()),
        }
    }

    fn check_rate_limit(&self, principal: &ApiPrincipal) -> Option<Duration> {
        self.rate_limiter
            .as_ref()
            .and_then(|limiter| limiter.check(&principal.rate_limit_key))
    }
}

#[derive(Clone)]
struct ApiAuth {
    token: Arc<str>,
    scopes: ApiScopes,
}

impl ApiAuth {
    fn authenticate(&self, headers: &HeaderMap) -> Option<ApiPrincipal> {
        matching_api_token(headers, &self.token).map(|_| ApiPrincipal {
            rate_limit_key: format!("token:{}", self.token),
            scopes: self.scopes,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ApiPrincipal {
    rate_limit_key: String,
    scopes: ApiScopes,
}

impl ApiPrincipal {
    fn tokenless() -> Self {
        Self {
            rate_limit_key: "tokenless-localhost".to_string(),
            scopes: ApiScopes::all_dashboard(),
        }
    }

    fn allows(&self, scope: ApiScope) -> bool {
        self.scopes.allows(scope)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApiScope {
    DashboardRead,
    DashboardAdmin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ApiScopes {
    dashboard_read: bool,
    dashboard_admin: bool,
}

impl ApiScopes {
    fn none() -> Self {
        Self {
            dashboard_read: false,
            dashboard_admin: false,
        }
    }

    fn all_dashboard() -> Self {
        Self {
            dashboard_read: true,
            dashboard_admin: true,
        }
    }

    fn from_scope_names(names: Vec<String>) -> anyhow::Result<Self> {
        if names.is_empty() {
            return Ok(Self::all_dashboard());
        }

        let mut scopes = Self::none();
        for name in names {
            match name.trim().to_ascii_lowercase().as_str() {
                "dashboard:read" => scopes.dashboard_read = true,
                "dashboard:admin" => scopes.dashboard_admin = true,
                unknown => anyhow::bail!("unknown API auth token scope '{}'", unknown),
            }
        }
        if scopes.dashboard_admin {
            scopes.dashboard_read = true;
        }
        Ok(scopes)
    }

    fn allows(&self, scope: ApiScope) -> bool {
        match scope {
            ApiScope::DashboardRead => self.dashboard_read || self.dashboard_admin,
            ApiScope::DashboardAdmin => self.dashboard_admin,
        }
    }
}

#[derive(Clone)]
struct ApiRateLimiter {
    buckets: Arc<Mutex<HashMap<String, VecDeque<Instant>>>>,
    max_requests_per_window: usize,
    window: Duration,
}

impl ApiRateLimiter {
    fn new(max_requests_per_window: usize, window: Duration) -> Self {
        Self {
            buckets: Arc::new(Mutex::new(HashMap::new())),
            max_requests_per_window,
            window,
        }
    }

    fn check(&self, key: &str) -> Option<Duration> {
        if self.max_requests_per_window == 0 {
            return None;
        }

        let now = Instant::now();
        let window_start = now.checked_sub(self.window).unwrap_or(now);
        let mut buckets = self.buckets.lock().expect("API rate limiter poisoned");
        let bucket = buckets.entry(key.to_string()).or_default();

        while bucket
            .front()
            .copied()
            .is_some_and(|instant| instant <= window_start)
        {
            bucket.pop_front();
        }

        if bucket.len() >= self.max_requests_per_window {
            let oldest = bucket.front().copied().unwrap_or(now);
            return Some(
                self.window
                    .saturating_sub(now.saturating_duration_since(oldest)),
            );
        }

        bucket.push_back(now);
        None
    }
}

pub fn apply(router: Router<Arc<ApiState>>) -> anyhow::Result<Router<Arc<ApiState>>> {
    let mut router = router
        .layer(TraceLayer::new_for_http())
        .layer(CompressionLayer::new());

    let security = ApiSecurity::from_config()?;
    if security.is_enabled() {
        router = router.layer(from_fn_with_state(security, enforce_api_security));
    }

    let allowed_origins = config::api_allowed_origins();
    if allowed_origins.is_empty() {
        return Ok(router);
    }

    let allowed_origins = allowed_origins
        .into_iter()
        .map(|origin| {
            HeaderValue::from_str(&origin)
                .with_context(|| format!("invalid API_ALLOWED_ORIGINS entry '{}'", origin))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    let cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            HeaderName::from_static("x-api-token"),
        ])
        .allow_origin(AllowOrigin::list(allowed_origins));

    Ok(router.layer(cors))
}

async fn enforce_api_security(
    State(security): State<ApiSecurity>,
    request: Request,
    next: Next,
) -> Response {
    if is_security_exempt(request.method(), request.uri().path()) {
        return next.run(request).await;
    }

    let principal = match security.authenticate(request.headers()) {
        Ok(principal) => principal,
        Err(status) => return status.into_response(),
    };

    if let Some(scope) = required_scope(request.method(), request.uri().path()) {
        if !principal.allows(scope) {
            return StatusCode::FORBIDDEN.into_response();
        }
    }

    if let Some(retry_after) = security.check_rate_limit(&principal) {
        return rate_limited_response(retry_after);
    }

    next.run(request).await
}

fn is_security_exempt(method: &Method, path: &str) -> bool {
    method == Method::OPTIONS || normalized_api_path(path) == "health"
}

fn required_scope(method: &Method, path: &str) -> Option<ApiScope> {
    if !is_dashboard_path(path) {
        return None;
    }

    if method == Method::GET || method == Method::HEAD {
        Some(ApiScope::DashboardRead)
    } else {
        Some(ApiScope::DashboardAdmin)
    }
}

fn is_dashboard_path(path: &str) -> bool {
    let path = normalized_api_path(path);
    path == "status" || path == "stats" || path == "runs" || path.starts_with("runs/")
}

fn normalized_api_path(path: &str) -> &str {
    path.strip_prefix("/api/")
        .or_else(|| path.strip_prefix('/'))
        .unwrap_or(path)
}

fn rate_limited_response(retry_after: Duration) -> Response {
    let retry_after_seconds = retry_after.as_secs().max(1).to_string();
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        Json(json!({ "error": "rate limit exceeded; retry later" })),
    )
        .into_response();
    response.headers_mut().insert(
        header::RETRY_AFTER,
        HeaderValue::from_str(&retry_after_seconds)
            .unwrap_or_else(|_| HeaderValue::from_static("1")),
    );
    response
}

#[cfg(test)]
fn has_valid_api_token(headers: &HeaderMap, expected: &str) -> bool {
    matching_api_token(headers, expected).is_some()
}

fn matching_api_token<'a>(headers: &'a HeaderMap, expected: &str) -> Option<&'a str> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(bearer_token);
    if bearer.is_some_and(|token| constant_time_eq(token, expected)) {
        return bearer;
    }

    let header_token = headers
        .get("x-api-token")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|token| !token.is_empty());
    if header_token.is_some_and(|token| constant_time_eq(token, expected)) {
        return header_token;
    }

    None
}

fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") {
        let token = token.trim();
        if !token.is_empty() {
            return Some(token);
        }
    }
    None
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut diff = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        let a = left.get(index).copied().unwrap_or(0);
        let b = right.get(index).copied().unwrap_or(0);
        diff |= usize::from(a ^ b);
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_token_accepts_bearer_or_token_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer secret-token"),
        );
        assert!(has_valid_api_token(&headers, "secret-token"));

        headers.clear();
        headers.insert("x-api-token", HeaderValue::from_static("secret-token"));
        assert!(has_valid_api_token(&headers, "secret-token"));
    }

    #[test]
    fn api_token_rejects_missing_or_wrong_token() {
        let mut headers = HeaderMap::new();
        assert!(!has_valid_api_token(&headers, "secret-token"));

        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer wrong-token"),
        );
        assert!(!has_valid_api_token(&headers, "secret-token"));
    }

    #[test]
    fn api_scopes_default_to_dashboard_read_and_admin() {
        let scopes = ApiScopes::from_scope_names(Vec::new()).expect("default scopes");

        assert!(scopes.allows(ApiScope::DashboardRead));
        assert!(scopes.allows(ApiScope::DashboardAdmin));
    }

    #[test]
    fn api_scopes_can_limit_dashboard_admin() {
        let scopes = ApiScopes::from_scope_names(vec!["dashboard:read".to_string()])
            .expect("read-only scope");

        assert!(scopes.allows(ApiScope::DashboardRead));
        assert!(!scopes.allows(ApiScope::DashboardAdmin));
    }

    #[test]
    fn api_scopes_reject_unknown_names() {
        let error = ApiScopes::from_scope_names(vec!["mailbox:admin".to_string()])
            .expect_err("unknown scope rejected");

        assert!(error.to_string().contains("unknown API auth token scope"));
    }

    #[test]
    fn dashboard_routes_require_dashboard_scope() {
        assert_eq!(
            required_scope(&Method::GET, "/api/status"),
            Some(ApiScope::DashboardRead)
        );
        assert_eq!(
            required_scope(&Method::GET, "/stats"),
            Some(ApiScope::DashboardRead)
        );
        assert_eq!(
            required_scope(&Method::GET, "/api/runs/run-1"),
            Some(ApiScope::DashboardRead)
        );
        assert_eq!(
            required_scope(&Method::POST, "/api/status"),
            Some(ApiScope::DashboardAdmin)
        );
        assert_eq!(required_scope(&Method::GET, "/api/health"), None);
        assert_eq!(required_scope(&Method::GET, "/api/emails"), None);
    }

    #[test]
    fn security_exempts_health_and_options() {
        assert!(is_security_exempt(&Method::GET, "/api/health"));
        assert!(is_security_exempt(&Method::OPTIONS, "/api/stats"));
        assert!(!is_security_exempt(&Method::GET, "/api/stats"));
    }

    #[test]
    fn api_rate_limiter_isolates_keys_and_returns_retry_after() {
        let limiter = ApiRateLimiter::new(2, Duration::from_secs(60));

        assert!(limiter.check("token:a").is_none());
        assert!(limiter.check("token:a").is_none());
        let retry_after = limiter
            .check("token:a")
            .expect("third request is rate limited");
        assert!(retry_after > Duration::from_secs(0));

        assert!(limiter.check("token:b").is_none());
    }
}
