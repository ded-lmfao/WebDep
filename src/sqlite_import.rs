use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};
use std::collections::HashSet;
use std::path::Path;
use tracing::info;

#[derive(Debug, Default, Clone, Serialize)]
pub struct ImportReport {
    pub imported: bool,
    pub users: usize,
    pub sessions: usize,
    pub passkeys: usize,
    pub projects: usize,
    pub env_vars: usize,
    pub deployments: usize,
    pub releases: usize,
    pub backups: usize,
    pub databases: usize,
    pub audit: usize,
    pub logs: usize,
    pub settings: usize,
}

fn sqlite_table_columns(conn: &Connection, table: &str) -> HashSet<String> {
    let mut cols = HashSet::new();
    let sql = format!("PRAGMA table_info({})", table);
    if let Ok(mut stmt) = conn.prepare(&sql) {
        if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(1)) {
            for c in rows.flatten() {
                cols.insert(c);
            }
        }
    }
    cols
}

fn sqlite_has_table(conn: &Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
        [table],
        |_| Ok(true),
    )
    .unwrap_or(false)
}

async fn reset_serial_sequence(
    tx: &mut Transaction<'_, Postgres>,
    table: &str,
) -> Result<(), sqlx::Error> {
    let sql = format!(
        "SELECT setval(pg_get_serial_sequence('{t}', 'id'), COALESCE((SELECT MAX(id) FROM {t}), 1), (SELECT COUNT(*) > 0 FROM {t}))",
        t = table
    );
    sqlx::query(&sql).execute(&mut **tx).await?;
    Ok(())
}

/// Automatically imports `data/webdep.sqlite` on first PostgreSQL boot when Postgres is still empty
/// (no users, projects, or databases yet and `_meta['sqlite_imported_at']` is unset).
pub async fn import_sqlite_if_needed(
    pool: &PgPool,
    sqlite_path: &Path,
) -> Result<ImportReport, Box<dyn std::error::Error + Send + Sync>> {
    if !sqlite_path.is_file() {
        return Ok(ImportReport::default());
    }

    let already_imported: Option<String> =
        sqlx::query_scalar("SELECT value FROM _meta WHERE key = 'sqlite_imported_at'")
            .fetch_optional(pool)
            .await?;
    if already_imported.is_some() {
        return Ok(ImportReport::default());
    }

    let user_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(pool)
        .await?;
    let project_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM projects")
        .fetch_one(pool)
        .await?;
    let db_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM databases")
        .fetch_one(pool)
        .await?;

    if user_count > 0 || project_count > 0 || db_count > 0 {
        return Ok(ImportReport::default());
    }

    import_sqlite_into_postgres(pool, sqlite_path, false).await
}

/// Explicitly imports all data from a SQLite file into PostgreSQL inside a single transaction.
pub async fn import_sqlite_into_postgres(
    pool: &PgPool,
    sqlite_path: &Path,
    overwrite_existing: bool,
) -> Result<ImportReport, Box<dyn std::error::Error + Send + Sync>> {
    if !sqlite_path.is_file() {
        return Err(format!("SQLite file not found: {}", sqlite_path.display()).into());
    }

    let conn = Connection::open_with_flags(
        sqlite_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;

    let mut report = ImportReport {
        imported: true,
        ..Default::default()
    };

    let mut tx = pool.begin().await?;

    if overwrite_existing {
        sqlx::query(
            "TRUNCATE TABLE logs, audit, backups, project_keys, project_env, releases, deployments, databases, projects, user_passkeys, sessions, users RESTART IDENTITY CASCADE",
        )
        .execute(&mut *tx)
        .await?;
    }

    // 1. Settings
    if sqlite_has_table(&conn, "settings") {
        let mut stmt = conn.prepare("SELECT key, value FROM settings")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows.flatten() {
            sqlx::query(
                "INSERT INTO settings(key, value) VALUES($1, $2) ON CONFLICT(key) DO UPDATE SET value = EXCLUDED.value",
            )
            .bind(row.0)
            .bind(row.1)
            .execute(&mut *tx)
            .await?;
            report.settings += 1;
        }
    }

    // 2. Users
    if sqlite_has_table(&conn, "users") {
        let mut stmt = conn.prepare(
            "SELECT id, username, password_hash, disabled, permissions, allowed_projects, totp_secret, totp_pending_secret, totp_enabled, totp_backup_codes, created_at FROM users ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3).unwrap_or(0) != 0,
                r.get::<_, String>(4).unwrap_or_else(|_| "[\"*\"]".to_string()),
                r.get::<_, String>(5).unwrap_or_else(|_| "[\"*\"]".to_string()),
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, i64>(8).unwrap_or(0) != 0,
                r.get::<_, String>(9).unwrap_or_else(|_| "[]".to_string()),
                r.get::<_, i64>(10)?,
            ))
        })?;
        for u in rows.flatten() {
            sqlx::query(
                "INSERT INTO users(id, username, password_hash, disabled, permissions, allowed_projects, totp_secret, totp_pending_secret, totp_enabled, totp_backup_codes, created_at)
                 VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
                 ON CONFLICT(id) DO NOTHING",
            )
            .bind(u.0)
            .bind(u.1)
            .bind(u.2)
            .bind(u.3)
            .bind(u.4)
            .bind(u.5)
            .bind(u.6)
            .bind(u.7)
            .bind(u.8)
            .bind(u.9)
            .bind(u.10)
            .execute(&mut *tx)
            .await?;
            report.users += 1;
        }
    }

    // 3. Sessions
    if sqlite_has_table(&conn, "sessions") {
        let cols = sqlite_table_columns(&conn, "sessions");
        let last_active_expr = if cols.contains("last_active_at") {
            "COALESCE(last_active_at, 0)"
        } else {
            "0"
        };
        let sql = format!(
            "SELECT id, user_id, expires_at, {}, csrf_token FROM sessions",
            last_active_expr
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        for s in rows.flatten() {
            sqlx::query(
                "INSERT INTO sessions(id, user_id, expires_at, last_active_at, csrf_token)
                 VALUES($1,$2,$3,$4,$5)
                 ON CONFLICT(id) DO NOTHING",
            )
            .bind(s.0)
            .bind(s.1)
            .bind(s.2)
            .bind(s.3)
            .bind(s.4)
            .execute(&mut *tx)
            .await?;
            report.sessions += 1;
        }
    }

    // 4. Passkeys
    if sqlite_has_table(&conn, "user_passkeys") {
        let mut stmt = conn.prepare(
            "SELECT id, user_id, credential_id, name, public_key, alg, sign_count, created_at, last_used_at FROM user_passkeys ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i32>(5).unwrap_or(-7),
                r.get::<_, i32>(6).unwrap_or(0),
                r.get::<_, i64>(7)?,
                r.get::<_, Option<i64>>(8)?,
            ))
        })?;
        for pk in rows.flatten() {
            sqlx::query(
                "INSERT INTO user_passkeys(id, user_id, credential_id, name, public_key, alg, sign_count, created_at, last_used_at)
                 VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)
                 ON CONFLICT(id) DO NOTHING",
            )
            .bind(pk.0)
            .bind(pk.1)
            .bind(pk.2)
            .bind(pk.3)
            .bind(pk.4)
            .bind(pk.5)
            .bind(pk.6)
            .bind(pk.7)
            .bind(pk.8)
            .execute(&mut *tx)
            .await?;
            report.passkeys += 1;
        }
    }

    // 5. Projects
    if sqlite_has_table(&conn, "projects") {
        let mut stmt = conn.prepare(
            "SELECT id, name, slug, repo_url, branch, project_type, deploy_method, deploy_dir, port,
                    COALESCE(install_cmd,''), COALESCE(build_cmd,''), COALESCE(start_cmd,''), COALESCE(health_url,''),
                    health_timeout_s, stability_s, auto_deploy, webhook_secret, current_release_id, deployed_sha,
                    last_poll_at, app_status, app_pid, app_started_at, domains, COALESCE(ssl_email,''),
                    project_kind, access_mode, allowed_ips, created_at, updated_at
             FROM projects ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                (
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, Option<i32>>(8)?,
                    r.get::<_, String>(9)?,
                ),
                (
                    r.get::<_, String>(10)?,
                    r.get::<_, String>(11)?,
                    r.get::<_, String>(12)?,
                    r.get::<_, i32>(13)?,
                    r.get::<_, i32>(14)?,
                    r.get::<_, i64>(15).unwrap_or(0) != 0,
                    r.get::<_, Option<String>>(16)?,
                    r.get::<_, Option<i64>>(17)?,
                    r.get::<_, Option<String>>(18)?,
                    r.get::<_, Option<i64>>(19)?,
                ),
                (
                    r.get::<_, String>(20)?,
                    r.get::<_, Option<i64>>(21)?,
                    r.get::<_, Option<i64>>(22)?,
                    r.get::<_, String>(23)?,
                    r.get::<_, String>(24)?,
                    r.get::<_, String>(25)?,
                    r.get::<_, String>(26)?,
                    r.get::<_, String>(27)?,
                    r.get::<_, i64>(28)?,
                    r.get::<_, i64>(29)?,
                ),
            ))
        })?;
        for (a, b, c) in rows.flatten() {
            sqlx::query(
                "INSERT INTO projects(
                    id, name, slug, repo_url, branch, project_type, deploy_method, deploy_dir, port, install_cmd,
                    build_cmd, start_cmd, health_url, health_timeout_s, stability_s, auto_deploy, webhook_secret, current_release_id, deployed_sha, last_poll_at,
                    app_status, app_pid, app_started_at, domains, ssl_email, project_kind, access_mode, allowed_ips, created_at, updated_at
                 ) VALUES(
                    $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,
                    $11,$12,$13,$14,$15,$16,$17,$18,$19,$20,
                    $21,$22,$23,$24,$25,$26,$27,$28,$29,$30
                 ) ON CONFLICT(id) DO NOTHING",
            )
            .bind(a.0).bind(a.1).bind(a.2).bind(a.3).bind(a.4).bind(a.5).bind(a.6).bind(a.7).bind(a.8).bind(a.9)
            .bind(b.0).bind(b.1).bind(b.2).bind(b.3).bind(b.4).bind(b.5).bind(b.6).bind(b.7).bind(b.8).bind(b.9)
            .bind(c.0).bind(c.1).bind(c.2).bind(c.3).bind(c.4).bind(c.5).bind(c.6).bind(c.7).bind(c.8).bind(c.9)
            .execute(&mut *tx)
            .await?;
            report.projects += 1;
        }
    }

    // 6. Project Env
    if sqlite_has_table(&conn, "project_env") {
        let mut stmt = conn.prepare("SELECT project_id, key, value_enc, is_secret FROM project_env")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3).unwrap_or(0) != 0,
            ))
        })?;
        for e in rows.flatten() {
            sqlx::query(
                "INSERT INTO project_env(project_id, key, value_enc, is_secret) VALUES($1,$2,$3,$4) ON CONFLICT(project_id, key) DO NOTHING",
            )
            .bind(e.0)
            .bind(e.1)
            .bind(e.2)
            .bind(e.3)
            .execute(&mut *tx)
            .await?;
            report.env_vars += 1;
        }
    }

    // 7. Project Keys
    if sqlite_has_table(&conn, "project_keys") {
        let mut stmt = conn.prepare("SELECT project_id, public_key, private_key, created_at FROM project_keys")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        for k in rows.flatten() {
            sqlx::query(
                "INSERT INTO project_keys(project_id, public_key, private_key, created_at) VALUES($1,$2,$3,$4) ON CONFLICT(project_id) DO NOTHING",
            )
            .bind(k.0)
            .bind(k.1)
            .bind(k.2)
            .bind(k.3)
            .execute(&mut *tx)
            .await?;
        }
    }

    // 8. Deployments (pass 1 without release FKs so releases can reference deployments cleanly)
    let mut dep_release_links: Vec<(i64, Option<i64>, Option<i64>)> = Vec::new();
    if sqlite_has_table(&conn, "deployments") {
        let mut stmt = conn.prepare(
            "SELECT id, project_id, release_id, from_release_id, kind, status, stage, sha, trigger, actor, error, started_at, finished_at
             FROM deployments ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, String>(8)?,
                r.get::<_, String>(9)?,
                r.get::<_, Option<String>>(10)?,
                r.get::<_, i64>(11)?,
                r.get::<_, Option<i64>>(12)?,
            ))
        })?;
        for d in rows.flatten() {
            if d.2.is_some() || d.3.is_some() {
                dep_release_links.push((d.0, d.2, d.3));
            }
            sqlx::query(
                "INSERT INTO deployments(id, project_id, release_id, from_release_id, kind, status, stage, sha, trigger, actor, error, started_at, finished_at)
                 VALUES($1,$2,NULL,NULL,$3,$4,$5,$6,$7,$8,$9,$10,$11)
                 ON CONFLICT(id) DO NOTHING",
            )
            .bind(d.0)
            .bind(d.1)
            .bind(d.4)
            .bind(d.5)
            .bind(d.6)
            .bind(d.7)
            .bind(d.8)
            .bind(d.9)
            .bind(d.10)
            .bind(d.11)
            .bind(d.12)
            .execute(&mut *tx)
            .await?;
            report.deployments += 1;
        }
    }

    // 9. Releases
    if sqlite_has_table(&conn, "releases") {
        let mut stmt = conn.prepare(
            "SELECT id, project_id, sha, short_sha, message, author, committed_at, dir, status, created_at, deployed_at, deployment_id, build_status, health_status
             FROM releases ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                (
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                ),
                (
                    r.get::<_, String>(7)?,
                    r.get::<_, String>(8)?,
                    r.get::<_, i64>(9)?,
                    r.get::<_, Option<i64>>(10)?,
                    r.get::<_, Option<i64>>(11)?,
                    r.get::<_, String>(12)?,
                    r.get::<_, String>(13)?,
                ),
            ))
        })?;
        for (a, b) in rows.flatten() {
            sqlx::query(
                "INSERT INTO releases(id, project_id, sha, short_sha, message, author, committed_at, dir, status, created_at, deployed_at, deployment_id, build_status, health_status)
                 VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)
                 ON CONFLICT(id) DO NOTHING",
            )
            .bind(a.0).bind(a.1).bind(a.2).bind(a.3).bind(a.4).bind(a.5).bind(a.6)
            .bind(b.0).bind(b.1).bind(b.2).bind(b.3).bind(b.4).bind(b.5).bind(b.6)
            .execute(&mut *tx)
            .await?;
            report.releases += 1;
        }
    }

    // 10. Link deployments -> releases now that both exist
    for (dep_id, rel_id, from_rel_id) in dep_release_links {
        sqlx::query("UPDATE deployments SET release_id = $1, from_release_id = $2 WHERE id = $3")
            .bind(rel_id)
            .bind(from_rel_id)
            .bind(dep_id)
            .execute(&mut *tx)
            .await?;
    }

    // 11. Backups
    if sqlite_has_table(&conn, "backups") {
        let mut stmt = conn.prepare(
            "SELECT id, project_id, created_at, path, size_bytes, kind, COALESCE(note,''), release_id FROM backups ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, Option<i64>>(7)?,
            ))
        })?;
        for b in rows.flatten() {
            sqlx::query(
                "INSERT INTO backups(id, project_id, created_at, path, size_bytes, kind, note, release_id)
                 VALUES($1,$2,$3,$4,$5,$6,$7,$8)
                 ON CONFLICT(id) DO NOTHING",
            )
            .bind(b.0).bind(b.1).bind(b.2).bind(b.3).bind(b.4).bind(b.5).bind(b.6).bind(b.7)
            .execute(&mut *tx)
            .await?;
            report.backups += 1;
        }
    }

    // 12. Databases
    if sqlite_has_table(&conn, "databases") {
        let cols = sqlite_table_columns(&conn, "databases");
        let started_expr = if cols.contains("started_at") {
            "started_at"
        } else {
            "NULL"
        };
        let sql = format!(
            "SELECT id, name, engine, db_name, db_user, db_password, port, is_superuser, domain, access_mode, allowed_ips, status, container_id, {}, data_dir, linked_project_id, created_at, updated_at
             FROM databases ORDER BY id ASC",
            started_expr
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| {
            Ok((
                (
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, i32>(6)?,
                    r.get::<_, i64>(7).unwrap_or(0) != 0,
                    r.get::<_, String>(8)?,
                ),
                (
                    r.get::<_, String>(9)?,
                    r.get::<_, String>(10)?,
                    r.get::<_, String>(11)?,
                    r.get::<_, Option<String>>(12)?,
                    r.get::<_, Option<i64>>(13)?,
                    r.get::<_, String>(14)?,
                    r.get::<_, Option<i64>>(15)?,
                    r.get::<_, i64>(16)?,
                    r.get::<_, i64>(17)?,
                ),
            ))
        })?;
        for (a, b) in rows.flatten() {
            sqlx::query(
                "INSERT INTO databases(id, name, engine, db_name, db_user, db_password, port, is_superuser, domain, access_mode, allowed_ips, status, container_id, started_at, data_dir, linked_project_id, created_at, updated_at)
                 VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18)
                 ON CONFLICT(id) DO NOTHING",
            )
            .bind(a.0).bind(a.1).bind(a.2).bind(a.3).bind(a.4).bind(a.5).bind(a.6).bind(a.7).bind(a.8)
            .bind(b.0).bind(b.1).bind(b.2).bind(b.3).bind(b.4).bind(b.5).bind(b.6).bind(b.7).bind(b.8)
            .execute(&mut *tx)
            .await?;
            report.databases += 1;
        }
    }

    // 13. Audit
    if sqlite_has_table(&conn, "audit") {
        let mut stmt = conn.prepare(
            "SELECT id, ts, actor, action, target, detail, ip, project_id FROM audit ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<i64>>(7)?,
            ))
        })?;
        for a in rows.flatten() {
            sqlx::query(
                "INSERT INTO audit(id, ts, actor, action, target, detail, ip, project_id)
                 VALUES($1,$2,$3,$4,$5,$6,$7,$8)
                 ON CONFLICT(id) DO NOTHING",
            )
            .bind(a.0).bind(a.1).bind(a.2).bind(a.3).bind(a.4).bind(a.5).bind(a.6).bind(a.7)
            .execute(&mut *tx)
            .await?;
            report.audit += 1;
        }
    }

    // 14. Logs
    if sqlite_has_table(&conn, "logs") {
        let mut stmt = conn.prepare(
            "SELECT id, project_id, deployment_id, channel, level, message, ts FROM logs ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, i64>(6)?,
            ))
        })?;
        for l in rows.flatten() {
            sqlx::query(
                "INSERT INTO logs(id, project_id, deployment_id, channel, level, message, ts)
                 VALUES($1,$2,$3,$4,$5,$6,$7)
                 ON CONFLICT(id) DO NOTHING",
            )
            .bind(l.0).bind(l.1).bind(l.2).bind(l.3).bind(l.4).bind(l.5).bind(l.6)
            .execute(&mut *tx)
            .await?;
            report.logs += 1;
        }
    }

    for table in [
        "projects",
        "deployments",
        "releases",
        "logs",
        "users",
        "backups",
        "audit",
        "user_passkeys",
        "databases",
    ] {
        reset_serial_sequence(&mut tx, table).await?;
    }

    let now_ms = crate::db::now().to_string();
    sqlx::query(
        "INSERT INTO _meta(key, value) VALUES('sqlite_imported_at', $1)
         ON CONFLICT(key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(now_ms)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    info!(
        "Imported SQLite data from {} into PostgreSQL (users={}, projects={}, databases={}, deployments={}, releases={}, audit={})",
        sqlite_path.display(),
        report.users,
        report.projects,
        report.databases,
        report.deployments,
        report.releases,
        report.audit
    );

    Ok(report)
}
