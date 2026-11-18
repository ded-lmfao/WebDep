use rand::RngCore;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::time::{SystemTime, UNIX_EPOCH};

use super::crypto_p256::{b64url_decode, b64url_encode, extract_p256_pubkey, verify_p256_ecdsa};
use super::manager::Auth;
use super::password::{constant_time_eq, random_token};
use super::user::User;

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct PasskeyInfo {
    pub id: i64,
    #[serde(rename = "userId")]
    pub user_id: i64,
    #[serde(rename = "credentialId")]
    pub credential_id: String,
    pub name: String,
    pub alg: i64,
    #[serde(rename = "signCount")]
    pub sign_count: i64,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    #[serde(rename = "lastUsedAt")]
    pub last_used_at: Option<i64>,
}

impl Auth {
    pub fn create_passkey_challenge(&self, user_id: Option<i64>) -> (String, String) {
        let now = Self::now_secs();
        let challenge_id = random_token(24);
        let mut raw_challenge = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut raw_challenge);
        let challenge_b64 = b64url_encode(&raw_challenge);
        if let Ok(mut map) = self.passkey_challenges.lock() {
            map.retain(|_, (_, _, exp)| *exp > now);
            map.insert(challenge_id.clone(), (user_id, challenge_b64.clone(), now + 300));
        }
        (challenge_id, challenge_b64)
    }

    pub fn consume_passkey_challenge(&self, challenge_id: &str) -> Option<(Option<i64>, String)> {
        let now = Self::now_secs();
        let mut map = self.passkey_challenges.lock().ok()?;
        map.retain(|_, (_, _, exp)| *exp > now);
        map.remove(challenge_id).map(|(uid, ch, _)| (uid, ch))
    }

    pub async fn list_passkeys(&self, user_id: i64) -> Vec<PasskeyInfo> {
        let Ok(rows) = sqlx::query(
            "SELECT id, user_id, credential_id, name, alg, sign_count, created_at, last_used_at
             FROM user_passkeys WHERE user_id = $1 ORDER BY id DESC",
        )
        .bind(user_id)
        .fetch_all(&self.db)
        .await
        else {
            return Vec::new();
        };

        rows.into_iter()
            .filter_map(|r| {
                Some(PasskeyInfo {
                    id: r.try_get(0).ok()?,
                    user_id: r.try_get(1).ok()?,
                    credential_id: r.try_get(2).ok()?,
                    name: r.try_get(3).ok()?,
                    alg: r.try_get::<i32, _>(4).ok()? as i64,
                    sign_count: r.try_get::<i32, _>(5).ok()? as i64,
                    created_at: r.try_get(6).ok()?,
                    last_used_at: r.try_get(7).ok()?,
                })
            })
            .collect()
    }

    pub async fn count_user_passkeys(&self, user_id: i64) -> usize {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM user_passkeys WHERE user_id = $1")
            .bind(user_id)
            .fetch_one(&self.db)
            .await
            .unwrap_or(0) as usize
    }

    pub async fn total_passkeys_count(&self) -> usize {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM user_passkeys p JOIN users u ON u.id = p.user_id WHERE COALESCE(u.disabled, FALSE) = FALSE",
        )
        .fetch_one(&self.db)
        .await
        .unwrap_or(0) as usize
    }

    pub async fn list_login_credential_ids(&self, username_opt: Option<&str>) -> Vec<String> {
        if let Some(uname) = username_opt.map(|s| s.trim()).filter(|s| !s.is_empty()) {
            sqlx::query_scalar(
                "SELECT p.credential_id FROM user_passkeys p
                 JOIN users u ON u.id = p.user_id
                 WHERE LOWER(u.username) = LOWER($1) AND COALESCE(u.disabled, FALSE) = FALSE
                 ORDER BY p.id DESC",
            )
            .bind(uname)
            .fetch_all(&self.db)
            .await
            .unwrap_or_default()
        } else {
            sqlx::query_scalar(
                "SELECT p.credential_id FROM user_passkeys p
                 JOIN users u ON u.id = p.user_id
                 WHERE COALESCE(u.disabled, FALSE) = FALSE
                 ORDER BY p.id DESC LIMIT 50",
            )
            .fetch_all(&self.db)
            .await
            .unwrap_or_default()
        }
    }

    pub async fn register_passkey(
        &self,
        user_id: i64,
        name: &str,
        credential_id_b64: &str,
        public_key_spki_b64: Option<&str>,
        attestation_object_b64: Option<&str>,
        auth_data_b64: Option<&str>,
        client_data_b64: &str,
        expected_challenge_b64: &str,
    ) -> Result<PasskeyInfo, String> {
        let cred_raw = b64url_decode(credential_id_b64)
            .ok_or_else(|| "Invalid credential ID encoding".to_string())?;
        if cred_raw.is_empty() || cred_raw.len() > 1024 {
            return Err("Invalid credential ID length".to_string());
        }
        let norm_cred_id = b64url_encode(&cred_raw);

        let client_data_bytes = b64url_decode(client_data_b64)
            .ok_or_else(|| "Invalid clientDataJSON encoding".to_string())?;
        let client_json: serde_json::Value = serde_json::from_slice(&client_data_bytes)
            .map_err(|_| "Invalid clientDataJSON payload".to_string())?;

        if client_json.get("type").and_then(|v| v.as_str()) != Some("webauthn.create") {
            return Err("WebAuthn clientDataJSON type mismatch (expected webauthn.create)".to_string());
        }
        let got_challenge = client_json
            .get("challenge")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !constant_time_eq(got_challenge, expected_challenge_b64) {
            return Err("WebAuthn registration challenge mismatch".to_string());
        }

        let mut pub_point: Option<[u8; 65]> = None;
        if let Some(spki_b64) = public_key_spki_b64.filter(|s| !s.trim().is_empty()) {
            if let Some(spki_bytes) = b64url_decode(spki_b64) {
                pub_point = extract_p256_pubkey(&spki_bytes);
            }
        }
        if pub_point.is_none() {
            if let Some(ad_b64) = auth_data_b64.filter(|s| !s.trim().is_empty()) {
                if let Some(ad_bytes) = b64url_decode(ad_b64) {
                    pub_point = extract_p256_pubkey(&ad_bytes);
                }
            }
        }
        if pub_point.is_none() {
            if let Some(att_b64) = attestation_object_b64.filter(|s| !s.trim().is_empty()) {
                if let Some(att_bytes) = b64url_decode(att_b64) {
                    pub_point = extract_p256_pubkey(&att_bytes);
                }
            }
        }

        let point65 = pub_point.ok_or_else(|| {
            "Unsupported passkey public key format (expected ES256 / P-256)".to_string()
        })?;
        let pub_hex = hex::encode(point65);

        let clean_name: String = {
            let trimmed = name.trim();
            if trimmed.is_empty() {
                "Passkey / Biometric Device".to_string()
            } else {
                trimmed.chars().take(64).collect()
            }
        };

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        let id: i64 = sqlx::query_scalar(
            "INSERT INTO user_passkeys (user_id, credential_id, name, public_key, alg, sign_count, created_at)
             VALUES ($1, $2, $3, $4, -7, 0, $5)
             ON CONFLICT(credential_id) DO UPDATE SET
               user_id = EXCLUDED.user_id,
               name = EXCLUDED.name,
               public_key = EXCLUDED.public_key
             RETURNING id",
        )
        .bind(user_id)
        .bind(&norm_cred_id)
        .bind(&clean_name)
        .bind(&pub_hex)
        .bind(now_ms)
        .fetch_one(&self.db)
        .await
        .map_err(|e| format!("Failed to save passkey: {}", e))?;

        Ok(PasskeyInfo {
            id,
            user_id,
            credential_id: norm_cred_id,
            name: clean_name,
            alg: -7,
            sign_count: 0,
            created_at: now_ms,
            last_used_at: None,
        })
    }

    pub async fn verify_passkey_login(
        &self,
        credential_id_b64: &str,
        auth_data_b64: &str,
        client_data_b64: &str,
        signature_b64: &str,
        expected_challenge_b64: &str,
    ) -> Result<(User, String), String> {
        let cred_raw = b64url_decode(credential_id_b64)
            .ok_or_else(|| "Invalid credential ID".to_string())?;
        let norm_cred_id = b64url_encode(&cred_raw);

        let auth_data = b64url_decode(auth_data_b64)
            .ok_or_else(|| "Invalid authenticatorData".to_string())?;
        if auth_data.len() < 37 {
            return Err("authenticatorData too short".to_string());
        }
        let flags = auth_data[32];
        if (flags & 0x01) == 0 {
            return Err("Authenticator did not assert User Presence".to_string());
        }
        let new_sign_count = u32::from_be_bytes([auth_data[33], auth_data[34], auth_data[35], auth_data[36]]) as i64;

        let client_data = b64url_decode(client_data_b64)
            .ok_or_else(|| "Invalid clientDataJSON".to_string())?;
        let client_json: serde_json::Value = serde_json::from_slice(&client_data)
            .map_err(|_| "Malformed clientDataJSON".to_string())?;
        if client_json.get("type").and_then(|v| v.as_str()) != Some("webauthn.get") {
            return Err("WebAuthn clientDataJSON type mismatch (expected webauthn.get)".to_string());
        }
        let got_challenge = client_json
            .get("challenge")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !constant_time_eq(got_challenge, expected_challenge_b64) {
            return Err("Passkey challenge expired or mismatched".to_string());
        }

        let sig_bytes = b64url_decode(signature_b64)
            .ok_or_else(|| "Invalid passkey signature encoding".to_string())?;

        let (passkey_id, user_id, passkey_name, pub_hex, prev_sign_count): (i64, i64, String, String, i32) = sqlx::query_as(
            "SELECT id, user_id, name, public_key, sign_count FROM user_passkeys WHERE credential_id = $1",
        )
        .bind(&norm_cred_id)
        .fetch_optional(&self.db)
        .await
        .ok()
        .flatten()
        .ok_or_else(|| "Passkey is not registered on this server".to_string())?;

        let pub_bytes = hex::decode(&pub_hex)
            .map_err(|_| "Stored passkey public key is corrupted".to_string())?;

        use sha2::{Digest, Sha256};
        let client_hash = Sha256::digest(&client_data);
        let mut signed_payload = Vec::with_capacity(auth_data.len() + 32);
        signed_payload.extend_from_slice(&auth_data);
        signed_payload.extend_from_slice(&client_hash);
        let msg_hash_generic = Sha256::digest(&signed_payload);
        let mut msg_hash = [0u8; 32];
        msg_hash.copy_from_slice(&msg_hash_generic);

        if !verify_p256_ecdsa(&pub_bytes, &msg_hash, &sig_bytes) {
            return Err("Passkey cryptographic signature verification failed".to_string());
        }

        let user = self
            .get_user_by_id(user_id)
            .await
            .ok_or_else(|| "Account associated with passkey no longer exists".to_string())?;
        if user.disabled {
            return Err("Account disabled".to_string());
        }

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let updated_count = new_sign_count.max(prev_sign_count as i64) as i32;
        let _ = sqlx::query("UPDATE user_passkeys SET last_used_at = $1, sign_count = $2 WHERE id = $3")
            .bind(now_ms)
            .bind(updated_count)
            .bind(passkey_id)
            .execute(&self.db)
            .await;

        Ok((user, passkey_name))
    }

    pub async fn delete_passkey(&self, user_id: i64, passkey_id: i64) -> bool {
        sqlx::query("DELETE FROM user_passkeys WHERE id = $1 AND user_id = $2")
            .bind(passkey_id)
            .bind(user_id)
            .execute(&self.db)
            .await
            .map(|r| r.rows_affected() > 0)
            .unwrap_or(false)
    }
}
