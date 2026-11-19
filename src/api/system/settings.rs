use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use std::collections::HashMap;

use crate::api::common::*;

pub(crate) async fn restart_panel_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let actor = verify_power(&state, &headers, "system_manage", None).await?;
    log_audit(
        &state,
        &actor.username,
        "system.restart",
        Some("platform"),
        Some("Restarted the WebDep panel"),
    )
    .await;
    state.logs.add(None, None, "system", "warn", &format!("Panel restart requested by {}", actor.username));

    crate::updater::restart_service_delayed(600);

    Ok(Json(serde_json::json!({ "ok": true, "message": "Panel is restarting" })))
}

pub(crate) async fn system_version_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<crate::updater::GitVersionInfo>, (StatusCode, String)> {
    let _actor = verify_power(&state, &headers, "system_manage", None).await?;
    let force_refresh = q
        .get("refresh")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);
    let info = state.updater.get_version_info(force_refresh).await;
    Ok(Json(info))
}

pub(crate) async fn system_update_status_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<crate::updater::UpdateStatus>, (StatusCode, String)> {
    let _actor = verify_power(&state, &headers, "system_manage", None).await?;
    Ok(Json(state.updater.get_status()))
}

pub(crate) async fn trigger_fleet_and_core_update(
    state: &AppState,
    actor_username: &str,
    tag: Option<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if let Some(branch) = crate::updater::feature_branch_build() {
        return Err((StatusCode::CONFLICT, crate::updater::feature_branch_update_message(&branch)));
    }
    log_audit(
        state,
        actor_username,
        "system.update",
        Some("platform"),
        Some("Triggered coordinated fleet & Core GitHub Release update"),
    )
    .await;

    let nodes = state.cluster.list_nodes().await.unwrap_or_default();
    let mut connected_remotes = Vec::new();
    for n in nodes {
        if !n.is_local && state.cluster.is_node_connected(n.id).await {
            connected_remotes.push(n);
        }
    }

    if connected_remotes.is_empty() {
        state.logs.add(
            None,
            None,
            "system",
            "warn",
            &format!("1-Click platform update from GitHub Release triggered by {}", actor_username),
        );
        state
            .updater
            .start_update(tag)
            .await
            .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
        return Ok(Json(serde_json::json!({
            "ok": true,
            "remoteNodesTargeted": 0,
            "message": "Release update sequence started",
        })));
    }

    let remote_count = connected_remotes.len();
    state
        .updater
        .begin_fleet_update_status(remote_count)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    state.logs.add(
        None,
        None,
        "system",
        "warn",
        &format!(
            "Coordinated fleet update ({remote_count} remote Web-Node(s) first, then Web-Core) triggered by {}",
            actor_username
        ),
    );

    let (repo_slug, token) = state.updater.get_repo_and_token().await;
    let state_clone = state.clone();
    tokio::spawn(async move {
        let total = connected_remotes.len();
        for (idx, n) in connected_remotes.into_iter().enumerate() {
            let pct = 10u8 + (((idx + 1) * 20 / total.max(1)) as u8);
            state_clone.updater.log_fleet_step(
                &format!("Upgrading Web-Node '{}' (#{} / {})...", n.name, idx + 1, total),
                pct,
                &format!("[Fleet] Upgrading remote Web-Node '{}' (#{}) first...", n.name, n.id),
            );
            match state_clone
                .cluster
                .call_node(
                    n.id,
                    "node.self_update",
                    serde_json::json!({
                        "repoSlug": &repo_slug,
                        "tag": &tag,
                        "githubToken": &token,
                        "restart": true,
                    }),
                    std::time::Duration::from_secs(240),
                )
                .await
            {
                Ok(res) => {
                    let upgraded_tag = res["tag"].as_str().unwrap_or("latest");
                    state_clone.updater.log_fleet_step(
                        &format!("Web-Node '{}' upgraded to {}", n.name, upgraded_tag),
                        pct,
                        &format!("[Fleet] Web-Node '{}' upgraded to {} (restarting)", n.name, upgraded_tag),
                    );
                }
                Err((_, err)) => {
                    let msg = format!("Remote Web-Node '{}' update failed: {}", n.name, err);
                    state_clone.logs.add(None, None, "system", "error", &msg);
                    state_clone.updater.fail_fleet_step(msg);
                    return;
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        if let Err(e) = state_clone.updater.start_update_after_fleet(tag).await {
            state_clone.updater.fail_fleet_step(e);
        }
    });

    Ok(Json(serde_json::json!({
        "ok": true,
        "remoteNodesTargeted": remote_count,
        "message": format!("Coordinated update started: upgrading {remote_count} Web-Node(s) before Web-Core"),
    })))
}

pub(crate) async fn system_update_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: Option<Json<crate::updater::TriggerUpdatePayload>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let actor = verify_power(&state, &headers, "system_manage", None).await?;
    let tag = payload.and_then(|p| p.tag.clone());
    trigger_fleet_and_core_update(&state, &actor.username, tag).await
}

pub(crate) async fn system_update_repo_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<crate::updater::SetUpdateRepoPayload>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let actor = verify_power(&state, &headers, "system_manage", None).await?;
    let remote_url = state.updater.set_remote_repo(&payload.repo)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    log_audit(
        &state,
        &actor.username,
        "system.update_repo",
        Some("platform"),
        Some(&format!("Changed platform update origin repository to {}", remote_url)),
    )
    .await;
    state.logs.add(
        None,
        None,
        "system",
        "info",
        &format!("Platform update repository updated to '{}' by {}", remote_url, actor.username),
    );

    Ok(Json(serde_json::json!({ "ok": true, "remote_url": remote_url })))
}

pub(crate) async fn cache_stats_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<crate::cache::CacheStats>, (StatusCode, String)> {
    let _user = verify_power(&state, &headers, "settings_manage", None).await?;
    Ok(Json(state.cache.stats()))
}

pub(crate) async fn cache_clear_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let actor = verify_power(&state, &headers, "settings_manage", None).await?;
    crate::db::purge_orphaned_project_data(&state.db).await;
    state.cache.invalidate_all();
    log_audit(
        &state,
        &actor.username,
        "cache.clear",
        Some("platform"),
        Some("Flushed server in-memory response cache and purged orphaned website data"),
    )
    .await;
    Ok(Json(serde_json::json!({
        "ok": true,
        "stats": state.cache.stats()
    })))
}

pub(crate) async fn get_settings_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<HashMap<String, String>>, (StatusCode, String)> {
    let _actor = verify_power(&state, &headers, "settings_manage", None).await?;
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT key, value FROM settings")
        .fetch_all(&state.db)
        .await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Database error".to_string()))?;

    let mut map: HashMap<String, String> = rows.into_iter().collect();
    let current_tok = map.get("github_token").cloned().unwrap_or_default();

    // If github_token is not yet saved in DB, try auto-detecting from gh CLI once and persisting it
    if current_tok.trim().is_empty() {
        if let Ok(out) = tokio::process::Command::new("gh").args(["auth", "token"]).output().await {
            if out.status.success() {
                let detected = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !detected.is_empty() {
                    let _ = sqlx::query(
                        "INSERT INTO settings(key, value) VALUES('github_token', $1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                    )
                    .bind(&detected)
                    .execute(&state.db)
                    .await;
                    let _ = crate::git::Git::read_github_tokens(&state.db).await;
                    map.insert("github_token".to_string(), detected);
                }
            }
        }
    }

    // Build unified list of configured GitHub tokens
    let mut tok_list: Vec<crate::git::StoredGithubToken> = map
        .get("github_tokens")
        .and_then(|raw| serde_json::from_str::<Vec<crate::git::StoredGithubToken>>(raw).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|t| !t.token.trim().is_empty())
        .collect();

    if tok_list.is_empty() {
        for part in current_tok.split(|c| c == ',' || c == '\n' || c == ';') {
            let trimmed = part.trim();
            if !trimmed.is_empty() {
                tok_list.push(crate::git::StoredGithubToken {
                    label: format!("Account #{}", tok_list.len() + 1),
                    token: trimmed.to_string(),
                });
            }
        }
    }

    if !tok_list.is_empty() {
        if let Ok(serialized) = serde_json::to_string(&tok_list) {
            map.insert("github_tokens".to_string(), serialized);
        }
        let first_tok = tok_list[0].token.trim();
        let suffix = if first_tok.len() >= 4 { &first_tok[first_tok.len() - 4..] } else { "****" };
        map.insert("github_token_configured".to_string(), "true".to_string());
        map.insert("github_token_count".to_string(), tok_list.len().to_string());
        map.insert(
            "github_token_masked".to_string(),
            if tok_list.len() > 1 {
                format!("{} tokens configured (••••••••{})", tok_list.len(), suffix)
            } else {
                format!("••••••••{}", suffix)
            },
        );
    }

    Ok(Json(map))
}

const DEFAULT_THEME_ID: &str = "midnight-ink";
const ALLOWED_THEME_IDS: &[&str] = &[
    "midnight-ink",
    "obsidian-carbon",
    "nordic-frost",
    "emerald-terminal",
    "catppuccin-mocha",
    "tokyo-dusk",
    "gruvbox-warm",
    "crimson-ember",
    "daylight-crisp",
    "paper-espresso",
    "sage-mist",
    "lavender-dusk",
    "harbor-slate",
    "cocoa-ember",
    "soft-linen",
];

pub(crate) fn normalize_theme_id(raw: &str) -> &'static str {
    let trimmed = raw.trim();
    ALLOWED_THEME_IDS
        .iter()
        .copied()
        .find(|&id| id.eq_ignore_ascii_case(trimmed))
        .unwrap_or(DEFAULT_THEME_ID)
}

pub(crate) async fn get_public_theme_handler(
    State(state): State<AppState>,
) -> Json<serde_json::Value> {
    let theme = sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key = 'panel_theme'")
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .map(|s| normalize_theme_id(&s))
        .unwrap_or(DEFAULT_THEME_ID);
    Json(serde_json::json!({ "theme": theme }))
}

pub(crate) async fn update_settings_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<HashMap<String, serde_json::Value>>,
) -> Result<StatusCode, (StatusCode, String)> {
    let actor = verify_power(&state, &headers, "settings_manage", None).await?;
    let only_theme = payload.len() == 1 && payload.contains_key("panel_theme");
    let mut saved_theme: Option<&'static str> = None;
    let mut touched_tokens = false;

    for (k, v) in payload {
        if k == "github_tokens" {
            let tokens_vec: Vec<crate::git::StoredGithubToken> = if let Some(arr) = v.as_array() {
                arr.iter()
                    .filter_map(|item| serde_json::from_value::<crate::git::StoredGithubToken>(item.clone()).ok())
                    .filter(|t| !t.token.trim().is_empty())
                    .collect()
            } else if let Some(s) = v.as_str() {
                serde_json::from_str::<Vec<crate::git::StoredGithubToken>>(s)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|t| !t.token.trim().is_empty())
                    .collect()
            } else {
                Vec::new()
            };

            let json_str = serde_json::to_string(&tokens_vec).unwrap_or_else(|_| "[]".to_string());
            let _ = sqlx::query(
                "INSERT INTO settings(key, value) VALUES('github_tokens', $1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            )
            .bind(&json_str)
            .execute(&state.db)
            .await;

            let primary = tokens_vec.first().map(|t| t.token.trim().to_string()).unwrap_or_default();
            let _ = sqlx::query(
                "INSERT INTO settings(key, value) VALUES('github_token', $1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            )
            .bind(&primary)
            .execute(&state.db)
            .await;
            touched_tokens = true;
            continue;
        }

        let val_str = match v {
            serde_json::Value::String(s) => s.trim().to_string(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            _ => v.to_string(),
        };
        if k == "github_token" && val_str.is_empty() {
            continue;
        }
        if k == "github_token" {
            touched_tokens = true;
        }
        let final_val = if k == "panel_theme" {
            let normalized = normalize_theme_id(&val_str);
            saved_theme = Some(normalized);
            normalized.to_string()
        } else {
            val_str
        };
        let _ = sqlx::query(
            "INSERT INTO settings(key, value) VALUES($1, $2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        )
        .bind(&k)
        .bind(&final_val)
        .execute(&state.db)
        .await;
    }
    if touched_tokens {
        let _ = crate::git::Git::read_github_tokens(&state.db).await;
    }
    if only_theme {
        let theme_id = saved_theme.unwrap_or(DEFAULT_THEME_ID);
        log_audit(
            &state,
            &actor.username,
            "settings.update",
            Some("platform"),
            Some(&format!("Changed platform theme preset to '{}'", theme_id)),
        )
        .await;
    } else {
        log_audit(&state, &actor.username, "settings.update", Some("platform"), Some("Updated global deployment & GitHub settings")).await;
    }
    Ok(StatusCode::OK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_theme_id_and_asset_parity() {
        assert_eq!(ALLOWED_THEME_IDS.len(), 15);
        assert_eq!(normalize_theme_id("nordic-frost"), "nordic-frost");
        assert_eq!(normalize_theme_id(" DAYLIGHT-CRISP "), "daylight-crisp");
        assert_eq!(normalize_theme_id("unknown-theme"), "midnight-ink");
        assert_eq!(normalize_theme_id(""), "midnight-ink");

        let css = std::fs::read_to_string("public/css/themes.css").expect("public/css/themes.css");
        let js = std::fs::read_to_string("public/js/themes.js").expect("public/js/themes.js");
        let html = std::fs::read_to_string("public/index.html").expect("public/index.html");
        assert!(html.contains("id=\"settings-theme-card\""));
        assert!(html.contains("id=\"settings-theme-grid\""));

        for id in ALLOWED_THEME_IDS {
            assert!(
                css.contains(&format!("data-theme=\"{}\"", id)),
                "Missing CSS selector for theme '{}'",
                id
            );
            assert!(
                js.contains(&format!("id: '{}'", id)),
                "Missing JS preset definition for theme '{}'",
                id
            );
        }
    }
}

