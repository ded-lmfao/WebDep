use axum::{
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::Response,
    Json,
};
pub(crate) use axum::http::header;
pub(crate) use axum::response::IntoResponse;
use serde::Serialize;
use std::sync::Arc;

pub(crate) use crate::auth::{constant_time_eq, verify_password, Auth, User};
pub(crate) use crate::cache::AppCache;
pub(crate) use crate::caddy::Caddy;
pub(crate) use crate::db::{now, DbPool};
pub(crate) use crate::deployer::Deployer;
pub(crate) use crate::logs::Logs;
pub(crate) use crate::monitor::Monitor;
pub(crate) use crate::projects::{Project, Projects};
pub(crate) use crate::runtime::Runtime;
pub(crate) use crate::static_assets::static_handler;

#[derive(Clone)]
pub struct AppState {
    pub db: DbPool,
    pub auth: Arc<Auth>,
    pub projects: Arc<Projects>,
    pub logs: Arc<Logs>,
    pub monitor: Arc<Monitor>,
    pub caddy: Arc<Caddy>,
    pub runtime: Arc<Runtime>,
    pub deployer: Arc<Deployer>,
    pub cache: Arc<AppCache>,
    pub databases: Arc<crate::databases::Databases>,
    pub updater: Arc<crate::updater::Updater>,
    pub cluster: Arc<crate::cluster::ClusterManager>,
}


pub(crate) fn is_https_request(headers: &HeaderMap, force_secure: bool) -> bool {
    if force_secure {
        return true;
    }
    if let Some(proto) = headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()) {
        if proto
            .split(',')
            .next()
            .map(|s| s.trim().eq_ignore_ascii_case("https"))
            .unwrap_or(false)
        {
            return true;
        }
    }
    if let Some(ssl) = headers.get("x-forwarded-ssl").and_then(|v| v.to_str().ok()) {
        if ssl.trim().eq_ignore_ascii_case("on") {
            return true;
        }
    }
    if let Some(fwd) = headers.get("forwarded").and_then(|v| v.to_str().ok()) {
        let lower = fwd.to_ascii_lowercase();
        if lower.contains("proto=https") {
            return true;
        }
    }
    false
}

pub(crate) fn clear_session_cookie_value(is_https: bool) -> String {
    if is_https {
        "nd_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0; Secure".to_string()
    } else {
        "nd_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0".to_string()
    }
}

fn should_touch_session_activity(path: &str, method: &Method) -> bool {
    if matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    ) {
        return path != "/api/auth/logout" && path != "/api/auth/presence";
    }
    !matches!(
        path,
        "/api/overview"
            | "/api/overview/network"
            | "/api/monitor"
            | "/api/logs/stream"
            | "/api/users"
            | "/api/system/update/status"
            | "/api/system/version"
            | "/healthz"
    )
}

pub(crate) async fn security_middleware(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let peer = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.ip());
    let ip = resolve_client_ip(peer, req.headers());
    req.headers_mut().remove(CLIENT_IP_HEADER);
    if let Ok(v) = HeaderValue::from_str(&ip) {
        req.headers_mut().insert(CLIENT_IP_HEADER, v);
    }
    let is_https = is_https_request(req.headers(), state.auth.force_secure_cookies);
    let is_mutating = matches!(
        *req.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );
    let (is_api, invalidate_cache, invalidate_sessions) = {
        let path = req.uri().path();
        let is_api = path.starts_with("/api/");
        let invalidate_cache = is_api && is_mutating && path != "/api/auth/presence";
        let invalidate_sessions = invalidate_cache
            && (path.starts_with("/api/users")
                || (path.starts_with("/api/auth/") && path != "/api/auth/presence"));

        if is_api {
            let is_public_auth = path == "/api/auth/login"
                || path == "/api/auth/register"
                || path == "/api/auth/reset"
                || (path == "/api/auth/register/challenge" && *req.method() == Method::GET)
                || (path == "/api/auth/registration" && *req.method() == Method::GET)
                || path == "/api/auth/2fa/verify-login"
                || path == "/api/auth/passkeys/login/start"
                || path == "/api/auth/passkeys/login/finish"
                || (path == "/api/theme" && *req.method() == Method::GET)
                || path.starts_with("/api/webhooks/github/")
                || (path == "/api/cluster/enroll" && *req.method() == Method::POST)
                || (path == "/api/cluster/node-ws" && *req.method() == Method::GET);

            if !is_public_auth {
                let sid_opt = extract_session_id(req.headers());
                let touch = should_touch_session_activity(path, req.method());
                let session_pair = match sid_opt.as_deref() {
                    Some(sid) => state.auth.get_session_with_touch(sid, touch).await,
                    None => None,
                };

                let Some((session, user)) = session_pair else {
                    let mut res = (
                        StatusCode::UNAUTHORIZED,
                        Json(serde_json::json!({ "error": "Unauthorized. Please sign in." })),
                    )
                        .into_response();
                    attach_security_headers(&mut res, true, is_https);
                    return res;
                };

                // Accounts created with a temporary password may only change it (or sign out).
                if user.must_change_password
                    && !matches!(
                        path,
                        "/api/auth/me" | "/api/auth/password" | "/api/auth/logout" | "/api/auth/presence"
                    )
                {
                    let mut res = (
                        StatusCode::FORBIDDEN,
                        Json(serde_json::json!({
                            "error": "Change your temporary password to continue.",
                            "code": "password_change_required"
                        })),
                    )
                        .into_response();
                    attach_security_headers(&mut res, true, is_https);
                    return res;
                }

                // When the admin requires two-factor authentication, users without it can only
                // set it up (or sign out).
                if crate::auth::policy::needs_two_factor_setup(&user)
                    && !matches!(
                        path,
                        "/api/auth/me"
                            | "/api/auth/logout"
                            | "/api/auth/presence"
                            | "/api/auth/2fa/setup"
                            | "/api/auth/2fa/enable"
                    )
                {
                    let mut res = (
                        StatusCode::FORBIDDEN,
                        Json(serde_json::json!({
                            "error": "Set up two-factor authentication to continue.",
                            "code": "two_factor_required"
                        })),
                    )
                        .into_response();
                    attach_security_headers(&mut res, true, is_https);
                    return res;
                }

                if is_mutating && path != "/api/auth/logout" {
                    let csrf_hdr = req
                        .headers()
                        .get("x-csrf-token")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    if !constant_time_eq(csrf_hdr, &session.csrf_token) {
                        let mut res = (
                            StatusCode::FORBIDDEN,
                            Json(serde_json::json!({ "error": "Invalid or missing CSRF security token. Please refresh the page." })),
                        )
                            .into_response();
                        attach_security_headers(&mut res, true, is_https);
                        return res;
                    }
                }
            }
        }

        (is_api, invalidate_cache, invalidate_sessions)
    };

    let mut res = next.run(req).await;
    if invalidate_cache {
        state.cache.invalidate_all();
        if invalidate_sessions {
            state.auth.invalidate_sessions();
        }
    }
    attach_security_headers(&mut res, is_api, is_https);
    res
}

pub(crate) fn attach_security_headers(res: &mut Response, is_api: bool, is_https: bool) {
    let headers = res.headers_mut();
    headers.insert(
        header::HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::HeaderName::from_static("x-frame-options"),
        HeaderValue::from_static("DENY"),
    );
    headers.insert(
        header::HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    headers.insert(
        header::HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
    );
    headers.insert(
        header::HeaderName::from_static("cross-origin-opener-policy"),
        HeaderValue::from_static("same-origin"),
    );
    if is_https {
        headers.insert(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000; includeSubDomains"),
        );
    }
    if is_api {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("default-src 'none'; frame-ancestors 'none'"),
        );
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store, no-cache, must-revalidate"),
        );
    } else {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(
                "default-src 'self'; script-src 'self' 'unsafe-hashes' 'sha256-97l24HYIWEdSIQ8PoMHzpxiGCZuyBDXtN19RPKFsOgk='; script-src-attr 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; connect-src 'self' ws: wss:; object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'",
            ),
        );
    }
}

pub(crate) async fn axum_static_fallback(headers: HeaderMap, uri: axum::http::Uri) -> Response {
    static_handler(uri.path(), &headers, uri.query()).await
}


// ---------------------------------------------------------------------------
// Helpers: Session Cookie
// ---------------------------------------------------------------------------
pub(crate) fn extract_session_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|part| {
            let (key, val) = part.trim().split_once('=')?;
            
            
            if key == "nd_session" {
                Some(val.to_string())
            } else {
                None
            }
        })
}

pub(crate) async fn current_user(state: &AppState, headers: &HeaderMap) -> Option<User> {
    let sid = extract_session_id(headers)?;
    state.auth.get_session(&sid).await.map(|(_, u)| u)
}

pub(crate) async fn require_current_user(state: &AppState, headers: &HeaderMap) -> Result<User, (StatusCode, String)> {
    current_user(state, headers)
        .await
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, "Unauthorized".to_string()))
}

pub(crate) async fn verify_project_access(
    state: &AppState,
    headers: &HeaderMap,
    project_id: i64,
) -> Result<User, (StatusCode, String)> {
    let u = require_current_user(state, headers).await?;
    if !state.auth.can_access_project_for_user(&u, project_id).await {
        return Err((
            StatusCode::FORBIDDEN,
            "Access denied: You are not assigned to this website.".to_string(),
        ));
    }
    Ok(u)
}

/// Admins (main admin or role `admin`).
pub(crate) async fn verify_admin(state: &AppState, headers: &HeaderMap) -> Result<User, (StatusCode, String)> {
    let u = require_current_user(state, headers).await?;
    if !u.is_admin {
        return Err((StatusCode::FORBIDDEN, "Only an admin can perform this action.".to_string()));
    }
    Ok(u)
}

/// Best-effort client address (proxy headers first). Spoofable: use only for audit and
/// soft rate limits.
/// Header the middleware sets to the verified client address (incoming copies are dropped).
pub(crate) const CLIENT_IP_HEADER: &str = "x-webdep-client-ip";

/// Client address worked out by `security_middleware`: the TCP peer, or the first
/// `X-Forwarded-For` / `X-Real-IP` hop only when the peer is a proxy on this machine (Caddy).
pub(crate) fn client_ip(headers: &HeaderMap) -> String {
    headers
        .get(CLIENT_IP_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

pub(crate) fn resolve_client_ip(peer: Option<std::net::IpAddr>, headers: &HeaderMap) -> String {
    let forwarded = || {
        headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .or_else(|| headers.get("x-real-ip").and_then(|v| v.to_str().ok()))
            .map(|s| s.trim().to_string())
            .filter(|s| s.parse::<std::net::IpAddr>().is_ok())
    };
    match peer {
        Some(ip) if ip.is_loopback() => forwarded().unwrap_or_else(|| ip.to_string()),
        Some(ip) => ip.to_string(),
        None => "unknown".to_string(),
    }
}

pub(crate) async fn verify_power(
    state: &AppState,
    headers: &HeaderMap,
    power: &str,
    project_id: Option<i64>,
) -> Result<User, (StatusCode, String)> {
    verify_any_power_for_scope(state, headers, &[power], "project", project_id).await
}

pub(crate) async fn verify_any_power(
    state: &AppState,
    headers: &HeaderMap,
    powers: &[&str],
    project_id: Option<i64>,
) -> Result<User, (StatusCode, String)> {
    verify_any_power_for_scope(state, headers, powers, "project", project_id).await
}

pub(crate) async fn verify_any_power_for_scope(
    state: &AppState,
    headers: &HeaderMap,
    powers: &[&str],
    scope_type: &str,
    scope_id: Option<i64>,
) -> Result<User, (StatusCode, String)> {
    let u = current_user(state, headers)
        .await
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, "Unauthorized session".to_string()))?;
    if scope_type == "project" {
        if let Some(pid) = scope_id {
            if !state.auth.can_access_project_for_user(&u, pid).await {
                return Err((
                    StatusCode::FORBIDDEN,
                    "Access denied: You are not assigned to this website.".to_string(),
                ));
            }
        }
    }
    if !state
        .auth
        .has_any_power_for_scope(&u, powers, scope_type, scope_id)
        .await
    {
        return Err((
            StatusCode::FORBIDDEN,
            if powers.len() == 1 {
                format!("Permission denied: Missing '{}' privilege.", powers[0])
            } else {
                format!("Permission denied: Requires one of [{}].", powers.join(", "))
            },
        ));
    }
    Ok(u)
}

pub(crate) async fn get_authorized_project(
    state: &AppState,
    headers: &HeaderMap,
    power: &str,
    project_id: i64,
) -> Result<(User, crate::projects::Project), (StatusCode, String)> {
    get_authorized_project_any(state, headers, &[power], project_id).await
}

pub(crate) async fn get_authorized_project_any(
    state: &AppState,
    headers: &HeaderMap,
    powers: &[&str],
    project_id: i64,
) -> Result<(User, crate::projects::Project), (StatusCode, String)> {
    let actor = verify_any_power(state, headers, powers, Some(project_id)).await?;
    let p = state
        .projects
        .get(project_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Project not found".into()))?;
    Ok((actor, p))
}

pub(crate) async fn log_audit(state: &AppState, actor: &str, action: &str, target: Option<&str>, detail: Option<&str>) {
    log_audit_project(state, None, actor, action, target, detail).await;
}

pub(crate) async fn create_authenticated_response(
    state: &AppState,
    headers: &HeaderMap,
    user: User,
) -> Result<Response, (StatusCode, String)> {
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let session = state
        .auth
        .create_session_with_meta(user.id, &client_ip(headers), &user_agent)
        .await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Failed to create session".to_string()))?;

    let _ = sqlx::query("UPDATE users SET last_login_at = $1 WHERE id = $2")
        .bind(now())
        .bind(user.id)
        .execute(&state.db)
        .await;
    // Return the full user (profile + grants) so the UI can gate itself.
    let user = state.auth.get_user_by_id(user.id).await.unwrap_or(user);
    let active_sessions = state.auth.count_active_sessions(user.id).await;
    let passkey_count = state.auth.count_user_passkeys(user.id).await;
    let secure_attr = if is_https_request(headers, state.auth.force_secure_cookies) {
        "; Secure"
    } else {
        ""
    };
    let cookie_val = format!(
        "nd_session={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{}",
        session.id, state.auth.session_ttl_s, secure_attr
    );

    let body = Json(LoginResponse {
        two_factor_required: crate::auth::policy::needs_two_factor_setup(&user),
        user,
        csrf: session.csrf_token,
        active_sessions,
        passkey_count,
    });

    let mut res = body.into_response();
    if let Ok(cookie_hdr) = HeaderValue::from_str(&cookie_val) {
        res.headers_mut().insert(header::SET_COOKIE, cookie_hdr);
    }
    Ok(res)
}

pub(crate) fn check_api_cache(cache: &crate::cache::AppCache, cache_key: &str) -> Option<Response> {
    check_api_cache_negotiated(cache, cache_key, false)
}

pub(crate) fn check_api_cache_negotiated(
    cache: &crate::cache::AppCache,
    cache_key: &str,
    want_gzip: bool,
) -> Option<Response> {
    let (cached_bytes, is_gzip) = cache.get_bytes_negotiated(cache_key, want_gzip)?;
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::VARY, HeaderValue::from_static("Accept-Encoding"))
        .header("x-cache", HeaderValue::from_static("HIT"));
    if is_gzip {
        builder = builder.header(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    }
    let res = builder
        .body(axum::body::Body::from(cached_bytes))
        .unwrap_or_else(|_| Json(serde_json::json!({})).into_response());
    Some(res)
}

pub(crate) fn cached_json_response<T: Serialize>(
    cache: &crate::cache::AppCache,
    cache_key: String,
    val: &T,
    ttl: std::time::Duration,
) -> Result<Response, StatusCode> {
    let json_str = serde_json::to_string(val).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let bytes = axum::body::Bytes::from(json_str);
    cache.set_bytes(cache_key, bytes.clone(), ttl);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-cache", HeaderValue::from_static("MISS"))
        .body(axum::body::Body::from(bytes))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

pub(crate) fn json_bytes_response(
    bytes: axum::body::Bytes,
    is_gzip: bool,
    cache_tag: &'static str,
) -> Result<Response, StatusCode> {
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::VARY, HeaderValue::from_static("Accept-Encoding"))
        .header("x-cache", HeaderValue::from_static(cache_tag));
    if is_gzip {
        builder = builder.header(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    }
    builder
        .body(axum::body::Body::from(bytes))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// Coalesces concurrent cold misses on `cache_key` (singleflight) while running blocking
/// serialization/computation via `compute`.
pub(crate) async fn singleflight_json_bytes<F, Fut>(
    cache: &crate::cache::AppCache,
    cache_key: String,
    ttl: std::time::Duration,
    req_headers: &HeaderMap,
    mut compute: F,
) -> Result<Response, StatusCode>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<axum::body::Bytes, StatusCode>>,
{
    let want_gzip = crate::static_assets::accepts_gzip(req_headers);
    for _attempt in 0..3 {
        if let Some(res) = check_api_cache_negotiated(cache, &cache_key, want_gzip) {
            return Ok(res);
        }
        match cache.acquire_inflight(&cache_key) {
            crate::cache::InflightRole::Leader(_guard) => {
                // Re-check inside leader in case a previous leader stored just before we acquired
                if let Some(res) = check_api_cache_negotiated(cache, &cache_key, want_gzip) {
                    return Ok(res);
                }
                let start_gen = cache.generation();
                let bytes = compute().await?;
                let gz = if want_gzip {
                    crate::static_assets::gzip_compress_bytes(&bytes)
                } else {
                    None
                };
                cache.set_bytes_with_gzip_if_gen(cache_key, bytes.clone(), gz.clone(), ttl, start_gen);
                if let Some(gz_bytes) = gz {
                    return json_bytes_response(gz_bytes, true, "MISS");
                }
                return json_bytes_response(bytes, false, "MISS");
            }
            crate::cache::InflightRole::Follower(mut rx) => {
                if !*rx.borrow() {
                    let _ = rx.changed().await;
                }
                if let Some(res) = check_api_cache_negotiated(cache, &cache_key, want_gzip) {
                    return Ok(res);
                }
            }
        }
    }
    let bytes = compute().await?;
    json_bytes_response(bytes, false, "MISS")
}

pub(crate) async fn fetch_project(
    projects: &crate::projects::Projects,
    id: i64,
) -> Result<crate::projects::Project, (StatusCode, String)> {
    projects
        .get(id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Project not found".to_string()))
}

pub(crate) async fn verify_current_user_password(
    state: &AppState,
    user: &User,
    password: &str,
) -> Result<(), (StatusCode, String)> {
    let (_, current_hash) = state
        .auth
        .get_user_by_username(&user.username)
        .await
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, "User not found".to_string()))?;

    if !verify_password(password, &current_hash) {
        return Err((StatusCode::BAD_REQUEST, "Incorrect password".to_string()));
    }
    Ok(())
}

pub(crate) async fn apply_2fa_state_change(
    state: &AppState,
    username: &str,
    audit_desc: &str,
) {
    state.auth.invalidate_sessions();
    state.cache.invalidate_all();
    log_audit(
        state,
        username,
        "user.update",
        Some(username),
        Some(audit_desc),
    )
    .await;
}

pub(crate) async fn insert_success_release(
    db: &crate::db::DbPool,
    project_id: i64,
    sha: &str,
    short_sha: &str,
    message: &str,
    author: &str,
    dir: &str,
    ts: i64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO releases(project_id, sha, short_sha, message, author, committed_at, dir, status, build_status, health_status, created_at, deployed_at) \
         VALUES($1, $2, $3, $4, $5, $6, $7, 'succeeded', 'success', 'ok', $8, $9) RETURNING id",
    )
    .bind(project_id)
    .bind(sha)
    .bind(short_sha)
    .bind(message)
    .bind(author)
    .bind(ts)
    .bind(dir)
    .bind(ts)
    .bind(ts)
    .fetch_one(db)
    .await
}

pub(crate) fn trigger_background_deploy(deployer: Arc<crate::deployer::Deployer>, project_id: i64, dep_id: i64) {
    tokio::spawn(async move {
        deployer.run_deploy(project_id, dep_id).await;
    });
}

pub(crate) async fn log_audit_project(
    state: &AppState,
    project_id: Option<i64>,
    actor: &str,
    action: &str,
    target: Option<&str>,
    detail: Option<&str>,
) {
    let _ = sqlx::query(
        "INSERT INTO audit(project_id, ts, actor, action, target, detail, ip) VALUES($1, $2, $3, $4, $5, $6, '127.0.0.1')",
    )
    .bind(project_id)
    .bind(now())
    .bind(actor)
    .bind(action)
    .bind(target)
    .bind(detail)
    .execute(&state.db)
    .await;
    if let Some(pid) = project_id {
        trim_app_audit(&state.db, pid).await;
    }
}

/// Keeps only the newest `APP_AUDIT_CAP` audit entries for one website or service; older ones
/// are deleted as new ones arrive. Panel-wide entries are not touched.
pub(crate) const APP_AUDIT_CAP: i64 = 50;

pub(crate) async fn trim_app_audit(db: &crate::db::DbPool, project_id: i64) {
    let _ = sqlx::query(
        "DELETE FROM audit WHERE project_id = $1 AND id NOT IN (
            SELECT id FROM audit WHERE project_id = $1 ORDER BY id DESC LIMIT $2)",
    )
    .bind(project_id)
    .bind(APP_AUDIT_CAP)
    .execute(db)
    .await;
}

#[cfg(test)]
mod audit_cap_tests {
    use super::*;

    #[tokio::test]
    async fn app_audit_keeps_newest_fifty() {
        let db = crate::pg_control::test_db::isolated_pool().await;
        let pid: i64 = sqlx::query_scalar(
            "INSERT INTO projects(name, slug, repo_url, deploy_dir, created_at, updated_at)
             VALUES('cap', 'cap-app', 'local', '/tmp/cap', 0, 0) RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        for i in 0..55 {
            sqlx::query("INSERT INTO audit(project_id, ts, actor, action) VALUES($2, $1, 'a', 'x')")
                .bind(i as i64)
                .bind(pid)
                .execute(&db)
                .await
                .unwrap();
        }
        sqlx::query("INSERT INTO audit(ts, actor, action) VALUES(0, 'a', 'panel')")
            .execute(&db)
            .await
            .unwrap();
        trim_app_audit(&db, pid).await;
        let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit WHERE project_id = $1").bind(pid).fetch_one(&db).await.unwrap();
        assert_eq!(kept, 50);
        let panel: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit WHERE project_id IS NULL").fetch_one(&db).await.unwrap();
        assert_eq!(panel, 1, "panel-wide entries are not capped");
    }
}


#[derive(Serialize)]
pub(crate) struct LoginResponse {
    pub(crate) user: User,
    pub(crate) csrf: String,
    /// The admin requires 2FA for this user and none is set up yet.
    #[serde(rename = "twoFactorRequired")]
    pub(crate) two_factor_required: bool,
    #[serde(rename = "activeSessions")]
    pub(crate) active_sessions: usize,
    #[serde(rename = "passkeyCount")]
    pub(crate) passkey_count: usize,
}


#[derive(Serialize)]
pub(crate) struct ActionResponse {
    #[serde(rename = "deploymentId")]
    pub(crate) deployment_id: i64,
}

#[cfg(test)]
mod security_tests {
    use super::*;

    #[test]
    fn test_is_https_request_detection() {
        let mut hdrs = HeaderMap::new();
        assert!(!is_https_request(&hdrs, false));
        assert!(is_https_request(&hdrs, true));

        hdrs.insert("x-forwarded-proto", HeaderValue::from_static("https, http"));
        assert!(is_https_request(&hdrs, false));

        hdrs.clear();
        hdrs.insert("forwarded", HeaderValue::from_static("for=1.2.3.4;proto=https"));
        assert!(is_https_request(&hdrs, false));

        hdrs.clear();
        hdrs.insert("x-forwarded-ssl", HeaderValue::from_static("on"));
        assert!(is_https_request(&hdrs, false));
    }

    #[test]
    fn test_security_headers_and_csp() {
        let mut api_res = (StatusCode::OK, "ok").into_response();
        attach_security_headers(&mut api_res, true, true);
        assert_eq!(
            api_res.headers().get(header::CONTENT_SECURITY_POLICY).and_then(|v| v.to_str().ok()),
            Some("default-src 'none'; frame-ancestors 'none'")
        );
        assert!(api_res.headers().contains_key(header::STRICT_TRANSPORT_SECURITY));

        let mut ui_res = (StatusCode::OK, "<html></html>").into_response();
        attach_security_headers(&mut ui_res, false, false);
        let ui_csp = ui_res
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(ui_csp.contains("object-src 'none'"));
        assert!(ui_csp.contains("frame-ancestors 'none'"));
        assert!(!ui_res.headers().contains_key(header::STRICT_TRANSPORT_SECURITY));
    }

    #[test]
    fn test_should_touch_session_activity_excludes_polls() {
        assert!(!should_touch_session_activity("/api/overview", &Method::GET));
        assert!(!should_touch_session_activity("/api/monitor", &Method::GET));
        assert!(!should_touch_session_activity("/api/auth/presence", &Method::POST));
        assert!(should_touch_session_activity("/api/projects", &Method::GET));
        assert!(should_touch_session_activity("/api/projects/1/deploy", &Method::POST));
    }
}

#[cfg(test)]
pub(crate) async fn tests_state_helper() -> AppState {
    use std::sync::Arc;

    let base = std::env::temp_dir().join(format!(
        "webdep_test_state_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let db = crate::pg_control::test_db::isolated_pool().await;
    let vault = Arc::new(crate::vault::Vault::new(&base).unwrap());
    let auth = Arc::new(crate::auth::Auth::new(db.clone(), 3600));
    auth.ensure_admin().await;
    crate::auth::grants::migrate_legacy_once(&db).await.unwrap();
    crate::auth::grants::grant_legacy_delete_once(&db).await.unwrap();
    let projects = Arc::new(crate::projects::Projects::new(db.clone(), vault.clone()));
    let logs = Arc::new(crate::logs::Logs::new(db.clone()));
    let monitor = Arc::new(crate::monitor::Monitor::new());
    let cache = Arc::new(crate::cache::AppCache::new());
    let caddy = Arc::new(crate::caddy::Caddy::new(&base));
    let runtime = Arc::new(crate::runtime::Runtime::new(db.clone(), (*logs).clone()));
    let deployer = Arc::new(
        crate::deployer::Deployer::new(
            db.clone(),
            (*logs).clone(),
            projects.clone(),
            runtime.clone(),
            cache.clone(),
        )
        .await,
    );
    let databases = Arc::new(crate::databases::Databases::new(
        db.clone(),
        vault,
        (*logs).clone(),
        &base,
    ));
    let updater = Arc::new(crate::updater::Updater::with_db(db.clone()));
    let cluster = Arc::new(crate::cluster::ClusterManager::new(db.clone(), base.clone()));

    AppState {
        db,
        auth,
        projects,
        logs,
        monitor,
        cache,
        caddy,
        runtime,
        deployer,
        databases,
        updater,
        cluster,
    }
}
