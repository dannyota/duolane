use crate::secure_fs::{bounded, checked};
use rustix::fs::{self, FileType, Mode, OFlags};
use serde::Deserialize;
use std::fmt;
use std::fs::File;

const CONFIG: &str = ".claude.json";
const CONFIG_LIMIT: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub email: String,
    pub plan: Option<&'static str>,
}

impl Identity {
    pub(super) fn same_login(&self, other: &Identity) -> bool {
        self.email.eq_ignore_ascii_case(&other.email)
    }
}

impl fmt::Display for Identity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.plan {
            Some(plan) => write!(formatter, "{} ({plan})", self.email),
            None => formatter.write_str(&self.email),
        }
    }
}

#[derive(Deserialize)]
struct Config {
    #[serde(rename = "oauthAccount")]
    account: Option<Account>,
}

#[derive(Deserialize)]
struct Account {
    #[serde(rename = "emailAddress")]
    email: Option<String>,
    #[serde(rename = "organizationType")]
    organization: Option<String>,
    #[serde(rename = "userRateLimitTier")]
    user_tier: Option<String>,
    #[serde(rename = "organizationRateLimitTier")]
    organization_tier: Option<String>,
}

pub(super) fn read(dir: &File) -> Option<Identity> {
    let fd = fs::openat(
        dir,
        CONFIG,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NOCTTY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    let file = File::from(fd);
    checked(&file, FileType::RegularFile, Mode::WGRP | Mode::WOTH).ok()?;
    parse(&bounded(file, CONFIG_LIMIT).ok()?)
}

fn parse(bytes: &[u8]) -> Option<Identity> {
    let account = serde_json::from_slice::<Config>(bytes).ok()?.account?;
    let email = account.email.filter(|email| valid_email(email))?;
    let tier = [&account.user_tier, &account.organization_tier]
        .into_iter()
        .flatten()
        .find_map(|tier| max_tier(tier));
    let organization = account.organization.as_deref().and_then(organization_plan);
    let plan = match (organization, tier) {
        (Some("Max") | None, Some(tier)) => Some(tier),
        (Some("Team"), Some("Max 5x")) => Some("Team, Max 5x"),
        (Some("Team"), Some("Max 20x")) => Some("Team, Max 20x"),
        (organization, _) => organization,
    };
    Some(Identity { email, plan })
}

fn valid_email(email: &str) -> bool {
    (3..=254).contains(&email.len())
        && email.contains('@')
        && email.bytes().all(|byte| byte.is_ascii_graphic())
}

fn max_tier(tier: &str) -> Option<&'static str> {
    if tier.ends_with("max_20x") {
        Some("Max 20x")
    } else if tier.ends_with("max_5x") {
        Some("Max 5x")
    } else {
        None
    }
}

fn organization_plan(organization: &str) -> Option<&'static str> {
    match organization {
        "claude_max" => Some("Max"),
        "claude_pro" => Some("Pro"),
        "claude_team" => Some("Team"),
        "claude_enterprise" => Some("Enterprise"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn config(account: &str) -> Vec<u8> {
        format!(r#"{{"projects":{{"/x":{{"mcpServers":{{}}}}}},"oauthAccount":{account}}}"#)
            .into_bytes()
    }

    #[test]
    fn identity_reads_email_and_a_known_plan() {
        let cases = [
            (
                r#"{"emailAddress":"a@example.com","organizationType":"claude_team","userRateLimitTier":"default_claude_max_5x","organizationRateLimitTier":"default_raven"}"#,
                Some("Team, Max 5x"),
            ),
            (
                r#"{"emailAddress":"a@example.com","organizationType":"claude_max","userRateLimitTier":"default_claude_max_20x"}"#,
                Some("Max 20x"),
            ),
            (
                r#"{"emailAddress":"a@example.com","organizationType":"claude_pro"}"#,
                Some("Pro"),
            ),
            (
                r#"{"emailAddress":"a@example.com","organizationType":"something_new","seatTier":7}"#,
                None,
            ),
        ];
        for (account, plan) in cases {
            assert_eq!(
                parse(&config(account)),
                Some(Identity {
                    email: "a@example.com".into(),
                    plan,
                })
            );
        }
    }

    #[test]
    fn identity_rejects_missing_or_unsafe_email() {
        for account in [
            "null",
            "{}",
            r#"{"emailAddress":""}"#,
            r#"{"emailAddress":"no-at-sign"}"#,
            r#"{"emailAddress":"a@b.c\u001b[2J"}"#,
            r#"{"emailAddress":"a b@example.com"}"#,
            r#"{"emailAddress":7}"#,
        ] {
            assert_eq!(parse(&config(account)), None);
        }
        assert_eq!(parse(b"not json"), None);
    }

    #[test]
    fn identity_file_must_be_a_private_regular_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(CONFIG);
        std::fs::write(&path, config(r#"{"emailAddress":"a@example.com"}"#)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let dir = File::open(temp.path()).unwrap();
        assert!(read(&dir).is_some());

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o622)).unwrap();
        assert!(read(&dir).is_none());

        std::fs::rename(&path, temp.path().join("real.json")).unwrap();
        symlink(temp.path().join("real.json"), &path).unwrap();
        std::fs::set_permissions(
            temp.path().join("real.json"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        assert!(read(&dir).is_none());
    }

    #[test]
    fn identity_display_and_matching() {
        let identity = Identity {
            email: "A@Example.com".into(),
            plan: Some("Pro"),
        };
        assert_eq!(identity.to_string(), "A@Example.com (Pro)");
        assert!(identity.same_login(&Identity {
            email: "a@example.com".into(),
            plan: None,
        }));
    }
}
