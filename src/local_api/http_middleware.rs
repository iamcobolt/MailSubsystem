use anyhow::Context;
use std::sync::Arc;

use axum::{
    extract::Request,
    extract::State,
    http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    middleware::{from_fn_with_state, Next},
    response::Response,
    Router,
};
use tower_http::compression::CompressionLayer;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::config;

use super::state::ApiState;

#[derive(Clone)]
struct ApiAuth {
    token: Arc<str>,
}

pub fn apply(router: Router<Arc<ApiState>>) -> anyhow::Result<Router<Arc<ApiState>>> {
    let mut router = router
        .layer(TraceLayer::new_for_http())
        .layer(CompressionLayer::new());

    if let Some(token) = config::api_auth_token() {
        router = router.layer(from_fn_with_state(
            ApiAuth {
                token: Arc::from(token),
            },
            require_api_auth,
        ));
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

async fn require_api_auth(
    State(auth): State<ApiAuth>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if request.method() == Method::OPTIONS || has_valid_api_token(request.headers(), &auth.token) {
        return Ok(next.run(request).await);
    }
    Err(StatusCode::UNAUTHORIZED)
}

fn has_valid_api_token(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(bearer_token)
        .is_some_and(|token| constant_time_eq(token, expected))
        || headers
            .get("x-api-token")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|token| constant_time_eq(token.trim(), expected))
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
}
