//! Account security policy set in Platform Settings, cached so every request can check it
//! without a database read.
//!
//! - `require_2fa`: `off` (default), `admins`, or `everyone`. Users it applies to who have no
//!   authenticator set up can only reach the endpoints needed to set one up.

use std::sync::atomic::{AtomicU8, Ordering};

use crate::db::DbPool;

pub const REQUIRE_2FA_SETTING: &str = "require_2fa";

const OFF: u8 = 0;
const ADMINS: u8 = 1;
const EVERYONE: u8 = 2;

static REQUIRE_2FA: AtomicU8 = AtomicU8::new(OFF);

/// Parses a stored setting value; unknown values mean off.
pub fn parse_require_2fa(raw: &str) -> u8 {
    match raw.trim().to_ascii_lowercase().as_str() {
        "admins" => ADMINS,
        "everyone" => EVERYONE,
        _ => OFF,
    }
}

pub fn is_valid_require_2fa(raw: &str) -> bool {
    matches!(raw.trim().to_ascii_lowercase().as_str(), "off" | "admins" | "everyone")
}

/// Reloads the cached policy from the settings table.
pub async fn reload(db: &DbPool) {
    let raw: Option<String> = sqlx::query_scalar("SELECT value FROM settings WHERE key = $1")
        .bind(REQUIRE_2FA_SETTING)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    REQUIRE_2FA.store(parse_require_2fa(raw.as_deref().unwrap_or("off")), Ordering::Relaxed);
}

/// Whether the policy requires two-factor authentication for this user.
pub fn two_factor_required_for(is_admin: bool) -> bool {
    match REQUIRE_2FA.load(Ordering::Relaxed) {
        EVERYONE => true,
        ADMINS => is_admin,
        _ => false,
    }
}

/// Whether this user must set up two-factor authentication before using the panel.
pub fn needs_two_factor_setup(user: &super::User) -> bool {
    two_factor_required_for(user.is_admin) && !user.totp_enabled
}

#[cfg(test)]
pub fn set_for_test(raw: &str) {
    REQUIRE_2FA.store(parse_require_2fa(raw), Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_policy_values() {
        assert_eq!(parse_require_2fa("Admins"), ADMINS);
        assert_eq!(parse_require_2fa("everyone"), EVERYONE);
        assert_eq!(parse_require_2fa("junk"), OFF);
        assert!(is_valid_require_2fa("off") && !is_valid_require_2fa("sometimes"));
    }
}
