//! Self-service accounts: public registration requests, admin review of those requests,
//! and the signed-in user's own profile.
//!
//! Registration never creates a user directly: it stores a request (with the password
//! already hashed) that an admin, or a user holding `requests_review`, approves or
//! rejects. Self-registration can be switched off with the `allow_registration` setting.

use axum::{
    extract::{Path as AxPath, Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use super::common::*;
use super::users::{insert_account, validate_display_name, validate_email, validate_username, NewAccount, MIN_PASSWORD_LEN};
use crate::auth::hash_password;

type ApiError = (StatusCode, String);

const REGISTRATION_SETTING: &str = "allow_registration";
/// Comma-separated email domains allowed to request an account (empty: any domain).
const REGISTRATION_DOMAINS_SETTING: &str = "registration_email_domains";
const MAX_PENDING_PER_IP_PER_HOUR: i64 = 3;
/// Requests accepted per hour from everyone together, so a flood can't bury real ones.
const MAX_REQUESTS_PER_HOUR: i64 = 30;
const MAX_PENDING_TOTAL: i64 = 500;
/// How long a password reset link works.
const RESET_LINK_TTL_MS: i64 = 24 * 3_600_000;
const REVIEW_POWER: &str = "requests_review";

fn db_error(e: sqlx::Error) -> ApiError {
    if let sqlx::Error::Database(ref d) = e {
        if d.is_unique_violation() {
            return (StatusCode::CONFLICT, "That username or email is already in use.".into());
        }
    }
    (StatusCode::INTERNAL_SERVER_ERROR, format!("Database error: {e}"))
}

/// Self-registration is on unless an admin turned it off.
pub(crate) async fn registration_enabled(state: &AppState) -> bool {
    sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key = $1")
        .bind(REGISTRATION_SETTING)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "false" | "0" | "off" | "no"))
        .unwrap_or(true)
}

// ---------------------------------------------------------------------------
// Bot check for self-registration: a small proof of work (no third-party service).
// The browser finds a nonce so that SHA-256("<challenge>:<nonce>") starts with
// `difficulty` zero bits; each challenge works once and expires after 10 minutes.
// ---------------------------------------------------------------------------
const CHALLENGE_TTL_MS: u64 = 600_000;
const MAX_OPEN_CHALLENGES: usize = 5_000;

fn challenge_difficulty() -> u32 {
    if cfg!(test) { 4 } else { 16 }
}

fn challenges() -> &'static std::sync::Mutex<std::collections::HashMap<String, u64>> {
    static MAP: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, u64>>> = std::sync::OnceLock::new();
    MAP.get_or_init(Default::default)
}

fn now_ms_u64() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

pub(crate) fn leading_zero_bits(bytes: &[u8]) -> u32 {
    let mut bits = 0;
    for b in bytes {
        if *b == 0 {
            bits += 8;
        } else {
            bits += b.leading_zeros();
            break;
        }
    }
    bits
}

pub(crate) fn pow_ok(challenge: &str, nonce: &str, difficulty: u32) -> bool {
    use sha2::{Digest, Sha256};
    nonce.len() <= 32 && leading_zero_bits(&Sha256::digest(format!("{challenge}:{nonce}").as_bytes())) >= difficulty
}

pub(crate) async fn registration_challenge_handler() -> Result<Json<serde_json::Value>, ApiError> {
    let now = now_ms_u64();
    let token = crate::auth::password::random_token(24);
    let mut map = challenges().lock().map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Busy".to_string()))?;
    map.retain(|_, exp| *exp > now);
    if map.len() >= MAX_OPEN_CHALLENGES {
        return Err((StatusCode::TOO_MANY_REQUESTS, "Too many account requests. Try again later.".into()));
    }
    map.insert(token.clone(), now + CHALLENGE_TTL_MS);
    Ok(Json(serde_json::json!({ "challenge": token, "difficulty": challenge_difficulty() })))
}

fn consume_challenge(challenge: &str, nonce: &str) -> bool {
    let Ok(mut map) = challenges().lock() else { return false };
    match map.remove(challenge) {
        Some(exp) if exp > now_ms_u64() => pow_ok(challenge, nonce, challenge_difficulty()),
        _ => false,
    }
}

async fn allowed_email_domains(state: &AppState) -> Vec<String> {
    sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key = $1")
        .bind(REGISTRATION_DOMAINS_SETTING)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default()
        .split(|c: char| c == ',' || c.is_whitespace())
        .map(|d| d.trim().trim_start_matches('@').to_ascii_lowercase())
        .filter(|d| !d.is_empty())
        .collect()
}

pub(crate) async fn registration_status_handler(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "enabled": registration_enabled(&state).await }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RegisterPayload {
    username: String,
    email: String,
    password: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    challenge: String,
    #[serde(default)]
    nonce: String,
}

const REGISTER_ACK: &str = "Request sent. You can sign in once an admin approves it.";

pub(crate) async fn register_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<RegisterPayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !registration_enabled(&state).await {
        return Err((StatusCode::FORBIDDEN, "Self-registration is turned off. Ask an admin for an account.".into()));
    }
    let username = validate_username(&payload.username)?;
    let email = validate_email(&payload.email)?
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "Enter your email address.".to_string()))?;
    let display_name = validate_display_name(&payload.display_name)?;
    if payload.password.len() < MIN_PASSWORD_LEN {
        return Err((StatusCode::BAD_REQUEST, format!("Password must be at least {MIN_PASSWORD_LEN} characters.")));
    }
    if !consume_challenge(&payload.challenge, &payload.nonce) {
        return Err((StatusCode::BAD_REQUEST, "The bot check expired. Send the request again.".into()));
    }
    let domains = allowed_email_domains(&state).await;
    let email_domain = email.rsplit('@').next().unwrap_or_default();
    if !domains.is_empty() && !domains.iter().any(|d| d == email_domain) {
        return Err((StatusCode::FORBIDDEN, "Account requests are limited to certain email domains. Ask an admin for an account.".into()));
    }

    let ip = client_ip(&headers);
    let hour_ago = now() - 3_600_000;
    let (from_ip, total, last_hour): (i64, i64, i64) = sqlx::query_as(
        "SELECT COUNT(*) FILTER (WHERE ip = $1 AND created_at > $2 AND status = 'pending'),
                COUNT(*) FILTER (WHERE status = 'pending'),
                COUNT(*) FILTER (WHERE created_at > $2)
         FROM registration_requests",
    )
    .bind(&ip)
    .bind(hour_ago)
    .fetch_one(&state.db)
    .await
    .map_err(db_error)?;
    if from_ip >= MAX_PENDING_PER_IP_PER_HOUR || total >= MAX_PENDING_TOTAL || last_hour >= MAX_REQUESTS_PER_HOUR {
        return Err((StatusCode::TOO_MANY_REQUESTS, "Too many account requests. Try again later.".into()));
    }

    // Same answer whether or not the name or email is taken, so the form can't be used
    // to discover accounts.
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users WHERE LOWER(username) = LOWER($1) OR LOWER(email) = $2 OR LOWER(username) = $2)
             OR EXISTS(SELECT 1 FROM registration_requests WHERE status = 'pending' AND (LOWER(username) = LOWER($1) OR LOWER(email) = $2))",
    )
    .bind(&username)
    .bind(&email)
    .fetch_one(&state.db)
    .await
    .map_err(db_error)?;
    if taken {
        return Ok(Json(serde_json::json!({ "ok": true, "message": REGISTER_ACK })));
    }

    let hash = hash_password(&payload.password).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let inserted = sqlx::query(
        "INSERT INTO registration_requests(email, username, display_name, password_hash, ip, created_at)
         VALUES($1, $2, $3, $4, $5, $6)",
    )
    .bind(&email)
    .bind(&username)
    .bind(&display_name)
    .bind(&hash)
    .bind(&ip)
    .bind(now())
    .execute(&state.db)
    .await;
    match inserted {
        Ok(_) => {
            log_audit(&state, &username, "account.request", Some(&username), Some(&format!("Requested an account ({email})"))).await;
        }
        // A concurrent identical request: same neutral answer.
        Err(sqlx::Error::Database(d)) if d.is_unique_violation() => {}
        Err(e) => return Err(db_error(e)),
    }
    Ok(Json(serde_json::json!({ "ok": true, "message": REGISTER_ACK })))
}

/// Status of the newest request matching a login name/email whose stored password matches,
/// used to explain a failed sign-in ("waiting for approval" / "declined").
pub(crate) async fn request_status_for_login(state: &AppState, login: &str, password: &str) -> Option<String> {
    let rows = sqlx::query(
        "SELECT status, password_hash FROM registration_requests
         WHERE status IN ('pending', 'rejected') AND (LOWER(username) = LOWER($1) OR LOWER(email) = LOWER($1))
         ORDER BY created_at DESC LIMIT 3",
    )
    .bind(login.trim())
    .fetch_all(&state.db)
    .await
    .ok()?;
    rows.into_iter().find_map(|r| {
        let hash: String = r.try_get(1).ok()?;
        crate::auth::verify_password(password, &hash).then(|| r.try_get::<String, _>(0).ok()).flatten()
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RequestItem {
    id: i64,
    email: String,
    username: String,
    display_name: String,
    status: String,
    reason: Option<String>,
    ip: Option<String>,
    created_at: i64,
    reviewed_by: Option<String>,
    reviewed_at: Option<i64>,
}

#[derive(Deserialize)]
pub(crate) struct RequestListQuery {
    status: Option<String>,
}

async fn verify_reviewer(state: &AppState, headers: &HeaderMap) -> Result<User, ApiError> {
    let u = require_current_user(state, headers).await?;
    if !u.has_power(REVIEW_POWER) {
        return Err((StatusCode::FORBIDDEN, "You don't have permission to review account requests.".into()));
    }
    Ok(u)
}

pub(crate) async fn list_requests_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<RequestListQuery>,
) -> Result<Json<Vec<RequestItem>>, ApiError> {
    verify_reviewer(&state, &headers).await?;
    let pending_only = q.status.as_deref() != Some("all");
    let rows = sqlx::query(
        "SELECT r.id, r.email, r.username, r.display_name, r.status, r.reason, r.ip, r.created_at, u.username, r.reviewed_at
         FROM registration_requests r LEFT JOIN users u ON u.id = r.reviewed_by
         WHERE ($1 = FALSE OR r.status = 'pending')
         ORDER BY (r.status = 'pending') DESC, r.created_at DESC LIMIT 500",
    )
    .bind(pending_only)
    .fetch_all(&state.db)
    .await
    .map_err(db_error)?;
    let list = rows
        .into_iter()
        .filter_map(|r| {
            Some(RequestItem {
                id: r.try_get(0).ok()?,
                email: r.try_get(1).ok()?,
                username: r.try_get(2).ok()?,
                display_name: r.try_get(3).ok()?,
                status: r.try_get(4).ok()?,
                reason: r.try_get(5).ok()?,
                ip: r.try_get(6).ok()?,
                created_at: r.try_get(7).ok()?,
                reviewed_by: r.try_get(8).ok()?,
                reviewed_at: r.try_get(9).ok()?,
            })
        })
        .collect();
    Ok(Json(list))
}

pub(crate) async fn approve_request_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let actor = verify_reviewer(&state, &headers).await?;
    let mut tx = state.db.begin().await.map_err(db_error)?;
    let row = sqlx::query(
        "SELECT email, username, display_name, password_hash FROM registration_requests
         WHERE id = $1 AND status = 'pending' FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_error)?
    .ok_or_else(|| (StatusCode::NOT_FOUND, "Request not found or already reviewed.".to_string()))?;
    let email: String = row.try_get(0).map_err(db_error)?;
    let username: String = row.try_get(1).map_err(db_error)?;
    let display_name: String = row.try_get(2).map_err(db_error)?;
    let hash: String = row.try_get(3).map_err(db_error)?;

    // The user chose this password, so no forced change. New accounts start with no access.
    let user_id = insert_account(
        &mut tx,
        &NewAccount {
            username: &username,
            email: Some(&email),
            display_name: &display_name,
            password_hash: &hash,
            role: "user",
            must_change_password: false,
        },
    )
    .await
    .map_err(db_error)?;
    sqlx::query("UPDATE registration_requests SET status = 'approved', reviewed_by = $1, reviewed_at = $2, user_id = $3 WHERE id = $4")
        .bind(actor.id)
        .bind(now())
        .bind(user_id)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
    tx.commit().await.map_err(db_error)?;

    log_audit(&state, &actor.username, "account.approve", Some(&username), Some("Approved account request")).await;
    state.cache.invalidate_all();
    Ok(Json(serde_json::json!({ "ok": true, "userId": user_id })))
}

#[derive(Deserialize)]
pub(crate) struct RejectPayload {
    #[serde(default)]
    reason: String,
}

pub(crate) async fn reject_request_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
    Json(payload): Json<RejectPayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let actor = verify_reviewer(&state, &headers).await?;
    let reason: String = payload.reason.trim().chars().take(300).collect();
    let username: Option<String> = sqlx::query_scalar(
        "UPDATE registration_requests SET status = 'rejected', reason = NULLIF($1, ''), reviewed_by = $2, reviewed_at = $3
         WHERE id = $4 AND status = 'pending' RETURNING username",
    )
    .bind(&reason)
    .bind(actor.id)
    .bind(now())
    .bind(id)
    .fetch_optional(&state.db)
    .await
    .map_err(db_error)?;
    let username = username.ok_or_else(|| (StatusCode::NOT_FOUND, "Request not found or already reviewed.".to_string()))?;
    log_audit(&state, &actor.username, "account.reject", Some(&username), Some("Rejected account request")).await;
    state.cache.invalidate_all();
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub(crate) async fn delete_request_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let actor = verify_admin(&state, &headers).await?;
    let username: Option<String> = sqlx::query_scalar("DELETE FROM registration_requests WHERE id = $1 RETURNING username")
        .bind(id)
        .fetch_optional(&state.db)
        .await
        .map_err(db_error)?;
    if let Some(name) = username {
        log_audit(&state, &actor.username, "account.request_delete", Some(&name), Some("Deleted account request")).await;
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// Password reset links (no email): an admin creates a one-time link and gives it to the user.
// ---------------------------------------------------------------------------
fn token_hash(token: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub(crate) async fn create_reset_link_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let actor = verify_admin(&state, &headers).await?;
    let target = state
        .auth
        .get_user_by_id(id)
        .await
        .ok_or_else(|| (StatusCode::NOT_FOUND, "User not found".to_string()))?;
    if target.is_main_admin {
        return Err((StatusCode::BAD_REQUEST, "The main admin's password is set in the panel's .env file (ADMIN_PASSWORD).".into()));
    }
    if target.is_admin && !actor.is_main_admin {
        return Err((StatusCode::FORBIDDEN, "Only the main admin can reset an admin's password.".into()));
    }
    let token = crate::auth::password::random_token(32);
    let now = now();
    let mut tx = state.db.begin().await.map_err(db_error)?;
    sqlx::query("UPDATE password_resets SET used_at = $2 WHERE user_id = $1 AND used_at IS NULL")
        .bind(id)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
    sqlx::query("INSERT INTO password_resets(user_id, token_hash, created_by, created_at, expires_at) VALUES($1, $2, $3, $4, $5)")
        .bind(id)
        .bind(token_hash(&token))
        .bind(actor.id)
        .bind(now)
        .bind(now + RESET_LINK_TTL_MS)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
    tx.commit().await.map_err(db_error)?;
    log_audit(&state, &actor.username, "user.reset_link", Some(&target.username), Some("Created a password reset link (valid 24 hours)")).await;
    Ok(Json(serde_json::json!({ "token": token, "expiresAt": now + RESET_LINK_TTL_MS })))
}

#[derive(Deserialize)]
pub(crate) struct ResetPayload {
    token: String,
    password: String,
}

pub(crate) async fn reset_password_handler(
    State(state): State<AppState>,
    Json(p): Json<ResetPayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if p.password.len() < MIN_PASSWORD_LEN {
        return Err((StatusCode::BAD_REQUEST, format!("Password must be at least {MIN_PASSWORD_LEN} characters.")));
    }
    let invalid = || (StatusCode::BAD_REQUEST, "This reset link is invalid or has expired. Ask an admin for a new one.".to_string());
    let now = now();
    let mut tx = state.db.begin().await.map_err(db_error)?;
    let user_id: Option<i64> = sqlx::query_scalar(
        "UPDATE password_resets SET used_at = $2 WHERE token_hash = $1 AND used_at IS NULL AND expires_at > $2 RETURNING user_id",
    )
    .bind(token_hash(p.token.trim()))
    .bind(now)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_error)?;
    let user_id = user_id.ok_or_else(invalid)?;
    let hash = hash_password(&p.password).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let username: String = sqlx::query_scalar(
        "UPDATE users SET password_hash = $1, must_change_password = FALSE WHERE id = $2 RETURNING username",
    )
    .bind(hash)
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(db_error)?;
    tx.commit().await.map_err(db_error)?;
    state.auth.revoke_all_user_sessions(user_id, "Password reset with a reset link").await;
    log_audit(&state, &username, "user.password_reset", Some(&username), Some("Set a new password with a reset link")).await;
    Ok(Json(serde_json::json!({ "ok": true, "username": username })))
}

// ---------------------------------------------------------------------------
// Sessions: your own, and (for admins) anyone's.
// ---------------------------------------------------------------------------
pub(crate) async fn my_sessions_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user = require_current_user(&state, &headers).await?;
    let current = extract_session_id(&headers).map(|s| crate::auth::manager::session_ref(&s));
    let list = state.auth.list_user_sessions(user.id).await;
    Ok(Json(serde_json::json!({ "sessions": list, "current": current })))
}

pub(crate) async fn revoke_my_session_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(reference): AxPath<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user = require_current_user(&state, &headers).await?;
    if !state.auth.revoke_session_by_ref(user.id, &reference).await {
        return Err((StatusCode::NOT_FOUND, "Session not found".into()));
    }
    log_audit(&state, &user.username, "session.revoke", Some(&user.username), Some("Signed out one of their sessions")).await;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn session_admin_target(state: &AppState, headers: &HeaderMap, id: i64) -> Result<(User, User), ApiError> {
    let actor = verify_admin(state, headers).await?;
    let target = state
        .auth
        .get_user_by_id(id)
        .await
        .ok_or_else(|| (StatusCode::NOT_FOUND, "User not found".to_string()))?;
    if target.is_admin && !actor.is_main_admin && target.id != actor.id {
        return Err((StatusCode::FORBIDDEN, "Only the main admin can see another admin's sessions.".into()));
    }
    Ok((actor, target))
}

pub(crate) async fn user_sessions_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (_, target) = session_admin_target(&state, &headers, id).await?;
    Ok(Json(serde_json::json!({ "sessions": state.auth.list_user_sessions(target.id).await })))
}

pub(crate) async fn revoke_user_sessions_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, target) = session_admin_target(&state, &headers, id).await?;
    let n = state.auth.revoke_all_user_sessions(target.id, "Signed out everywhere by an admin").await;
    log_audit(&state, &actor.username, "session.revoke_all", Some(&target.username), Some(&format!("Signed out {n} session(s)"))).await;
    Ok(Json(serde_json::json!({ "ok": true, "revoked": n })))
}

pub(crate) async fn revoke_user_session_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath((id, reference)): AxPath<(i64, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, target) = session_admin_target(&state, &headers, id).await?;
    if !state.auth.revoke_session_by_ref(target.id, &reference).await {
        return Err((StatusCode::NOT_FOUND, "Session not found".into()));
    }
    log_audit(&state, &actor.username, "session.revoke", Some(&target.username), Some("Signed out one session")).await;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProfilePayload {
    display_name: Option<String>,
    email: Option<String>,
}

pub(crate) async fn update_profile_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ProfilePayload>,
) -> Result<Json<User>, ApiError> {
    let user = require_current_user(&state, &headers).await?;
    if let Some(raw) = payload.display_name.as_deref() {
        sqlx::query("UPDATE users SET display_name = $1 WHERE id = $2")
            .bind(validate_display_name(raw)?)
            .bind(user.id)
            .execute(&state.db)
            .await
            .map_err(db_error)?;
    }
    if let Some(raw) = payload.email.as_deref() {
        sqlx::query("UPDATE users SET email = $1 WHERE id = $2")
            .bind(validate_email(raw)?)
            .bind(user.id)
            .execute(&state.db)
            .await
            .map_err(db_error)?;
    }
    state.auth.invalidate_sessions();
    log_audit(&state, &user.username, "user.update", Some(&user.username), Some("Updated own profile")).await;
    let updated = state
        .auth
        .get_user_by_id(user.id)
        .await
        .ok_or_else(|| (StatusCode::NOT_FOUND, "User not found".to_string()))?;
    Ok(Json(updated))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{header, Method, Request, StatusCode};
    use serde_json::json;
    use tower::ServiceExt;

    async fn call(
        router: &axum::Router,
        method: Method,
        uri: &str,
        session: Option<(&str, &str)>,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder().method(method).uri(uri).header(header::CONTENT_TYPE, "application/json");
        if let Some((sid, csrf)) = session {
            req = req.header(header::COOKIE, format!("nd_session={sid}")).header("x-csrf-token", csrf);
        }
        let res = router.clone().oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    /// Fetches a bot-check challenge and solves it.
    async fn solve(router: &axum::Router) -> serde_json::Value {
        let (_, c) = call(router, Method::GET, "/api/auth/register/challenge", None, json!({})).await;
        let challenge = c["challenge"].as_str().unwrap().to_string();
        let difficulty = c["difficulty"].as_u64().unwrap() as u32;
        let nonce = (0u64..).find(|n| super::pow_ok(&challenge, &n.to_string(), difficulty)).unwrap();
        json!({ "challenge": challenge, "nonce": nonce.to_string() })
    }

    #[test]
    fn counts_leading_zero_bits() {
        assert_eq!(super::leading_zero_bits(&[0, 0x0f]), 12);
        assert_eq!(super::leading_zero_bits(&[0x80]), 0);
        assert_eq!(super::leading_zero_bits(&[0, 0]), 16);
    }

    #[tokio::test]
    async fn reset_links_sessions_and_two_factor_policy() {
        let state = crate::api::common::tests_state_helper().await;
        let router = crate::api::create_router(state.clone());
        let admin = state.auth.create_session(1).await.unwrap();
        let admin_s = Some((admin.id.as_str(), admin.csrf_token.as_str()));

        let (_, u) = call(&router, Method::POST, "/api/users", admin_s,
            json!({"username": "resetme", "password": "OldPass1234", "mustChangePassword": false})).await;
        let uid = u["id"].as_i64().unwrap();

        // Disabled accounts are only revealed to someone with the right password.
        call(&router, Method::PUT, &format!("/api/users/{uid}"), admin_s, json!({"disabled": true})).await;
        let (st, _) = call(&router, Method::POST, "/api/auth/login", None, json!({"username": "resetme", "password": "wrong-pass"})).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        let (st, _) = call(&router, Method::POST, "/api/auth/login", None, json!({"username": "resetme", "password": "OldPass1234"})).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        call(&router, Method::PUT, &format!("/api/users/{uid}"), admin_s, json!({"disabled": false})).await;

        // Reset link: works once, then never again.
        let (st, link) = call(&router, Method::POST, &format!("/api/users/{uid}/reset-link"), admin_s, json!({})).await;
        assert_eq!(st, StatusCode::OK);
        let token = link["token"].as_str().unwrap().to_string();
        let (st, _) = call(&router, Method::POST, "/api/auth/reset", None, json!({"token": token, "password": "NewPass5678"})).await;
        assert_eq!(st, StatusCode::OK);
        let (st, _) = call(&router, Method::POST, "/api/auth/reset", None, json!({"token": token, "password": "Again9999"})).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, _) = call(&router, Method::POST, "/api/auth/login", None, json!({"username": "resetme", "password": "NewPass5678"})).await;
        assert_eq!(st, StatusCode::OK);

        // Sessions: listed for the admin and revocable.
        let (_, list) = call(&router, Method::GET, &format!("/api/users/{uid}/sessions"), admin_s, json!({})).await;
        assert_eq!(list["sessions"].as_array().unwrap().len(), 1);
        let (st, _) = call(&router, Method::DELETE, &format!("/api/users/{uid}/sessions"), admin_s, json!({})).await;
        assert_eq!(st, StatusCode::OK);
        let (_, list) = call(&router, Method::GET, &format!("/api/users/{uid}/sessions"), admin_s, json!({})).await;
        assert!(list["sessions"].as_array().unwrap().is_empty());

        // Required 2FA: a user without it can only reach the setup endpoints.
        crate::auth::policy::set_for_test("everyone");
        let sess = state.auth.create_session(uid).await.unwrap();
        let s = Some((sess.id.as_str(), sess.csrf_token.as_str()));
        let (st, body) = call(&router, Method::GET, "/api/projects", s, json!({})).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], "two_factor_required");
        let (st, me) = call(&router, Method::GET, "/api/auth/me", s, json!({})).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(me["twoFactorRequired"], true);
        crate::auth::policy::set_for_test("off");
        let (st, _) = call(&router, Method::GET, "/api/projects", s, json!({})).await;
        assert_eq!(st, StatusCode::OK);

        // An admin without 2FA can't require it for admins (would lock themselves out).
        let (st, _) = call(&router, Method::PUT, "/api/settings", admin_s, json!({"require_2fa": "admins"})).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn registration_approval_and_delegated_access() {
        let state = crate::api::common::tests_state_helper().await;
        let router = crate::api::create_router(state.clone());
        let admin = state.auth.create_session(1).await.unwrap();
        let admin_s = Some((admin.id.as_str(), admin.csrf_token.as_str()));

        // Register: needs a solved bot check; neutral ack; duplicate is silent.
        let (st, _) = call(&router, Method::POST, "/api/auth/register", None,
            json!({"username": "bot", "email": "bot@x.io", "password": "BotPass1234"})).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "no bot check");
        let reg = |extra: serde_json::Value| {
            let mut v = json!({"username": "priya", "email": "Priya@Acme.io", "password": "PriyaPass123", "displayName": "Priya"});
            v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            v
        };
        let (st, _) = call(&router, Method::POST, "/api/auth/register", None, reg(solve(&router).await)).await;
        assert_eq!(st, StatusCode::OK);
        let (st, _) = call(&router, Method::POST, "/api/auth/register", None, reg(solve(&router).await)).await;
        assert_eq!(st, StatusCode::OK);
        let (st, _) = call(&router, Method::POST, "/api/auth/login", None, json!({"username": "priya", "password": "PriyaPass123"})).await;
        assert_eq!(st, StatusCode::FORBIDDEN);

        let (_, list) = call(&router, Method::GET, "/api/requests", admin_s, json!({})).await;
        let list = list.as_array().unwrap();
        assert_eq!(list.len(), 1, "duplicate request was not stored");
        let req_id = list[0]["id"].as_i64().unwrap();
        let (st, approved) = call(&router, Method::POST, &format!("/api/requests/{req_id}/approve"), admin_s, json!({})).await;
        assert_eq!(st, StatusCode::OK);
        let priya_id = approved["userId"].as_i64().unwrap();

        // Approved account signs in by email and starts with no app access.
        let (st, login) = call(&router, Method::POST, "/api/auth/login", None, json!({"username": "priya@acme.io", "password": "PriyaPass123"})).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(login["user"]["access"]["projects"], json!({}));

        // Self-registration toggle.
        let (st, _) = call(&router, Method::PUT, "/api/settings", admin_s, json!({"allow_registration": "false"})).await;
        assert!(st.is_success());
        let mut late = json!({"username": "late", "email": "late@x.io", "password": "LatePass123"});
        late.as_object_mut().unwrap().extend(solve(&router).await.as_object().unwrap().clone());
        let (st, _) = call(&router, Method::POST, "/api/auth/register", None, late).await;
        assert_eq!(st, StatusCode::FORBIDDEN);

        // Delegation: a lead with deploy + manage on one app can give deploy, not env.
        let (_, shop) = call(&router, Method::POST, "/api/projects", admin_s, json!({"name": "Shop", "slug": "acct-shop", "repo_url": ""})).await;
        let (_, blog) = call(&router, Method::POST, "/api/projects", admin_s, json!({"name": "Blog", "slug": "acct-blog", "repo_url": ""})).await;
        let (shop_id, blog_id) = (shop["id"].as_i64().unwrap(), blog["id"].as_i64().unwrap());
        let (st, lead) = call(&router, Method::POST, "/api/users", admin_s, json!({
            "username": "lead", "password": "LeadPass123", "mustChangePassword": false,
            "grants": [{"scope": "project", "id": shop_id, "powers": ["deploy", "access_manage"]}]
        })).await;
        assert_eq!(st, StatusCode::OK);
        let lead_id = lead["id"].as_i64().unwrap();
        let lead_sess = state.auth.create_session(lead_id).await.unwrap();
        let lead_s = Some((lead_sess.id.as_str(), lead_sess.csrf_token.as_str()));
        let path = format!("/api/access/project/{shop_id}/{priya_id}");
        let (st, _) = call(&router, Method::PUT, &path, lead_s, json!({"powers": ["deploy"]})).await;
        assert_eq!(st, StatusCode::OK);
        let (st, _) = call(&router, Method::PUT, &path, lead_s, json!({"powers": ["env"]})).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        let (st, _) = call(&router, Method::PUT, &format!("/api/access/project/{blog_id}/{priya_id}"), lead_s, json!({"powers": []})).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "no manage access on the other app");
        let priya = state.auth.get_user_by_id(priya_id).await.unwrap();
        assert!(priya.has_project_power(shop_id, &["deploy"]));
        assert!(!priya.has_project_power(shop_id, &["env"]));
        assert!(!priya.can_access_project(blog_id));

        // Temporary passwords block everything but the password change.
        let (_, temp) = call(&router, Method::POST, "/api/users", admin_s, json!({"username": "newbie", "password": "TempPass123"})).await;
        let temp_sess = state.auth.create_session(temp["id"].as_i64().unwrap()).await.unwrap();
        let temp_s = Some((temp_sess.id.as_str(), temp_sess.csrf_token.as_str()));
        let (st, body) = call(&router, Method::GET, "/api/projects", temp_s, json!({})).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], "password_change_required");
        let (st, _) = call(&router, Method::POST, "/api/auth/password", temp_s, json!({"current": "TempPass123", "next": "OwnPass4567", "revokeOtherSessions": false})).await;
        assert_eq!(st, StatusCode::OK);
        let (st, _) = call(&router, Method::GET, "/api/projects", temp_s, json!({})).await;
        assert_eq!(st, StatusCode::OK);
    }
}
