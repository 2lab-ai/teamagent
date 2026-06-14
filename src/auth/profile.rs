//! `GET /api/oauth/profile` client — yields the `accountUuid` used as the
//! dedup key across logins and imports (FR2), plus display metadata.

use serde::Deserialize;

use super::AuthError;

/// Subset of the profile response llmux cares about. Parsing is
/// tolerant: only `account.uuid` (the dedup key) and `account.email` (the
/// account display name) are required; everything else is `None` when absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    /// `accountUuid` — THE dedup/upsert key for oauth accounts.
    pub account_uuid: String,
    pub email: String,
    /// Subscription tier when reported (e.g. `max`); display only. Derived
    /// from `has_claude_max` / `has_claude_pro`.
    pub tier: Option<String>,
    pub display_name: Option<String>,
    pub org_name: Option<String>,
    pub has_claude_max: Option<bool>,
    pub has_claude_pro: Option<bool>,
}

/// Fetch the profile for one oauth account (Bearer auth against the
/// upstream base URL).
pub async fn fetch_profile(
    client: &reqwest::Client,
    base_url: &str,
    access_token: &str,
) -> Result<Profile, AuthError> {
    let url = format!("{}/api/oauth/profile", base_url.trim_end_matches('/'));
    let response = client.get(&url).bearer_auth(access_token).send().await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(AuthError::ProfileEndpoint { status, body });
    }
    let text = response.text().await?;
    parse_profile(&text)
}

/// Wire shape (the fields teamclaude reads in `fetchProfile`).
#[derive(Debug, Default, Deserialize)]
struct ProfileResponse {
    #[serde(default)]
    account: ProfileAccount,
    #[serde(default)]
    organization: ProfileOrganization,
}

#[derive(Debug, Default, Deserialize)]
struct ProfileAccount {
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    has_claude_max: Option<bool>,
    #[serde(default)]
    has_claude_pro: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct ProfileOrganization {
    #[serde(default)]
    name: Option<String>,
}

/// Pure parse of the profile JSON body (test seam).
fn parse_profile(body: &str) -> Result<Profile, AuthError> {
    let parsed: ProfileResponse = serde_json::from_str(body)?;
    let account = parsed.account;
    let account_uuid = account
        .uuid
        .filter(|uuid| !uuid.is_empty())
        .ok_or(AuthError::ProfileIncomplete("account.uuid"))?;
    let email = account
        .email
        .filter(|email| !email.is_empty())
        .ok_or(AuthError::ProfileIncomplete("account.email"))?;
    let tier = match (account.has_claude_max, account.has_claude_pro) {
        (Some(true), _) => Some("max".to_string()),
        (_, Some(true)) => Some("pro".to_string()),
        _ => None,
    };
    Ok(Profile {
        account_uuid,
        email,
        tier,
        display_name: account.display_name,
        org_name: parsed.organization.name,
        has_claude_max: account.has_claude_max,
        has_claude_pro: account.has_claude_pro,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_profile() {
        let body = r#"{
            "account": {
                "uuid": "uuid-1", "email": "a@x.com", "display_name": "A",
                "has_claude_max": true, "has_claude_pro": false
            },
            "organization": { "name": "Org A", "organization_type": "claude_max" }
        }"#;
        let profile = parse_profile(body).unwrap();
        assert_eq!(profile.account_uuid, "uuid-1");
        assert_eq!(profile.email, "a@x.com");
        assert_eq!(profile.tier.as_deref(), Some("max"));
        assert_eq!(profile.display_name.as_deref(), Some("A"));
        assert_eq!(profile.org_name.as_deref(), Some("Org A"));
        assert_eq!(profile.has_claude_max, Some(true));
        assert_eq!(profile.has_claude_pro, Some(false));
    }

    #[test]
    fn parse_minimal_profile_tolerates_missing_optionals() {
        let body = r#"{"account": {"uuid": "uuid-2", "email": "b@x.com"}}"#;
        let profile = parse_profile(body).unwrap();
        assert_eq!(profile.account_uuid, "uuid-2");
        assert_eq!(profile.tier, None);
        assert_eq!(profile.display_name, None);
        assert_eq!(profile.org_name, None);
        assert_eq!(profile.has_claude_max, None);
        assert_eq!(profile.has_claude_pro, None);
    }

    #[test]
    fn parse_pro_tier() {
        let body = r#"{"account": {"uuid": "u", "email": "e@x.com", "has_claude_pro": true}}"#;
        assert_eq!(parse_profile(body).unwrap().tier.as_deref(), Some("pro"));
    }

    #[test]
    fn parse_missing_uuid_is_an_error() {
        let body = r#"{"account": {"email": "b@x.com"}}"#;
        assert!(matches!(
            parse_profile(body).unwrap_err(),
            AuthError::ProfileIncomplete("account.uuid")
        ));
    }

    #[test]
    fn parse_missing_email_is_an_error() {
        let body = r#"{"account": {"uuid": "uuid-3"}}"#;
        assert!(matches!(
            parse_profile(body).unwrap_err(),
            AuthError::ProfileIncomplete("account.email")
        ));
    }
}
