use axum::{
    extract::{Path as AxPath, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::Deserialize;

use super::common::*;
use crate::auth::hash_password;

pub(crate) async fn admin_reset_user_2fa_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let actor = verify_main_admin(&state, &headers).await?;
    if !actor.is_main_admin {
        return Err((
            StatusCode::FORBIDDEN,
            "Only the Main Admin can reset another user's 2FA.".to_string(),
        ));
    }

    let target = state
        .auth
        .get_user_by_id(id)
        .await
        .ok_or_else(|| (StatusCode::NOT_FOUND, "User not found".to_string()))?;

    sqlx::query(
        "UPDATE users SET totp_secret = NULL, totp_pending_secret = NULL, totp_enabled = FALSE, totp_backup_codes = '[]' WHERE id = $1",
    )
    .bind(id)
    .execute(&state.db)
    .await
    .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Database error".to_string()))?;

    state.auth.invalidate_sessions();
    state.cache.invalidate_all();

    log_audit(
        &state,
        &actor.username,
        "user.update",
        Some(&target.username),
        Some("Main Admin reset Two-Factor Authentication for user"),
    )
    .await;

    Ok(Json(serde_json::json!({ "ok": true })))
}

pub(crate) async fn list_users_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<User>>, (StatusCode, String)> {
    let _actor = verify_main_admin(&state, &headers).await?;
    let list = state
        .auth
        .list_users()
        .await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Database error".to_string()))?;
    Ok(Json(list))
}

#[derive(Deserialize)]
pub(crate) struct CreateUserPayload {
    username: String,
    password: String,
    permissions: Option<Vec<String>>,
    #[serde(rename = "allProjects")]
    all_projects: Option<bool>,
    #[serde(rename = "allowedProjects")]
    allowed_projects: Option<Vec<i64>>,
}

pub(crate) async fn create_user_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CreateUserPayload>,
) -> Result<Json<User>, (StatusCode, String)> {
    let actor = verify_main_admin(&state, &headers).await?;

    let uname = payload.username.trim().to_string();
    if uname.is_empty() || payload.password.len() < 6 {
        return Err((StatusCode::BAD_REQUEST, "Username and password (min 6 chars) required".into()));
    }

    let hashed = hash_password(&payload.password)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    let perms = payload.permissions.unwrap_or_default();
    let perms_json = serde_json::to_string(&perms).unwrap_or_else(|_| "[]".into());

    let all_projs = payload.all_projects.unwrap_or(true);
    let allowed_json = if all_projs {
        "[\"*\"]".to_string()
    } else {
        serde_json::to_string(&payload.allowed_projects.unwrap_or_default()).unwrap_or_else(|_| "[]".into())
    };

    let ts = now();

    let id: i64 = sqlx::query_scalar(
        "INSERT INTO users(username, password_hash, disabled, permissions, allowed_projects, created_at) \
         VALUES($1, $2, FALSE, $3, $4, $5) RETURNING id",
    )
    .bind(&uname)
    .bind(&hashed)
    .bind(&perms_json)
    .bind(&allowed_json)
    .bind(ts)
    .fetch_one(&state.db)
    .await
    .map_err(|e| (StatusCode::BAD_REQUEST, format!("Could not create user: {}", e)))?;

    log_audit(&state, &actor.username, "user.create", Some(&uname), Some(&format!("perms={}", perms_json))).await;

    Ok(Json(User::from_db(
        id,
        uname,
        0,
        perms_json,
        allowed_json,
        0,
        ts,
    )))
}

#[derive(Deserialize)]
pub(crate) struct UpdateUserPayload {
    disabled: Option<serde_json::Value>,
    password: Option<String>,
    permissions: Option<Vec<String>>,
    #[serde(rename = "allProjects")]
    all_projects: Option<bool>,
    #[serde(rename = "allowedProjects")]
    allowed_projects: Option<Vec<i64>>,
}

pub(crate) async fn update_user_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
    Json(payload): Json<UpdateUserPayload>,
) -> Result<Json<User>, (StatusCode, String)> {
    let actor = verify_main_admin(&state, &headers).await?;

    let existing = state
        .auth
        .get_user_by_id(id)
        .await
        .ok_or_else(|| (StatusCode::NOT_FOUND, "User not found".to_string()))?;

    if existing.is_main_admin && !actor.is_main_admin {
        return Err((StatusCode::FORBIDDEN, "Only the Main Admin can modify the Main Admin account.".into()));
    }

    if let Some(pwd) = payload.password.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if pwd.len() < 6 {
            return Err((StatusCode::BAD_REQUEST, "Password must be at least 6 characters".into()));
        }
        let hashed = hash_password(pwd).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
        let _ = sqlx::query("UPDATE users SET password_hash = $1 WHERE id = $2")
            .bind(hashed)
            .bind(id)
            .execute(&state.db)
            .await;
        if existing.is_main_admin {
            crate::config::Config::sync_dotenv_admin(Some(&existing.username), Some(pwd));
        }
    }

    if !existing.is_main_admin {
        if let Some(d_val) = payload.disabled {
            let is_disabled = match d_val {
                serde_json::Value::Bool(b) => b,
                serde_json::Value::Number(n) => n.as_i64().unwrap_or(0) != 0,
                serde_json::Value::String(s) => s == "1" || s.eq_ignore_ascii_case("true"),
                _ => false,
            };
            let _ = sqlx::query("UPDATE users SET disabled = $1 WHERE id = $2")
                .bind(is_disabled)
                .bind(id)
                .execute(&state.db)
                .await;
            if is_disabled {
                let _ = state
                    .auth
                    .revoke_all_user_sessions(id, "User account disabled")
                    .await;
            }
        }
        if let Some(perms) = payload.permissions.as_ref() {
            let perms_json = serde_json::to_string(perms).unwrap_or_else(|_| "[]".into());
            let _ = sqlx::query("UPDATE users SET permissions = $1 WHERE id = $2")
                .bind(perms_json)
                .bind(id)
                .execute(&state.db)
                .await;
        }
        if payload.all_projects.is_some() || payload.allowed_projects.is_some() {
            let all_p = payload.all_projects.unwrap_or(false);
            let allowed_json = if all_p {
                "[\"*\"]".to_string()
            } else {
                serde_json::to_string(&payload.allowed_projects.unwrap_or_default()).unwrap_or_else(|_| "[]".into())
            };
            let _ = sqlx::query("UPDATE users SET allowed_projects = $1 WHERE id = $2")
                .bind(allowed_json)
                .bind(id)
                .execute(&state.db)
                .await;
        }
        state
            .auth
            .notify_revocation(crate::auth::access::AccessRevocationEvent {
                kind: "grant_updated".to_string(),
                user_id: id,
                scope_type: None,
                scope_id: None,
                reason: "User account permissions updated".to_string(),
                ts: now(),
            })
            .await;
    }

    log_audit(&state, &actor.username, "user.update", Some(&existing.username), None).await;

    let updated = state
        .auth
        .get_user_by_id(id)
        .await
        .ok_or_else(|| (StatusCode::NOT_FOUND, "User not found".to_string()))?;

    Ok(Json(updated))
}

pub(crate) async fn delete_user_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let actor = verify_main_admin(&state, &headers).await?;

    let target = state
        .auth
        .get_user_by_id(id)
        .await
        .ok_or_else(|| (StatusCode::NOT_FOUND, "User not found".to_string()))?;

    if target.is_main_admin {
        return Err((StatusCode::BAD_REQUEST, "Cannot delete the Main Admin account.".into()));
    }
    if target.id == actor.id {
        return Err((StatusCode::BAD_REQUEST, "Cannot delete your own currently logged-in account.".into()));
    }

    let _ = state
        .auth
        .revoke_all_user_sessions(id, "User account deleted")
        .await;
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    log_audit(&state, &actor.username, "user.delete", Some(&target.username), Some("Deleted user account")).await;

    Ok(Json(serde_json::json!({ "ok": true })))
}
