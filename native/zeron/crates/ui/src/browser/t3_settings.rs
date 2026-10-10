//! T3 owns account auth and device pairing; native code only bootstraps the host session.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum T3SettingsRoute {
    General,
    Connections,
    Providers,
    PullRequests,
    Usage,
    /// Clerk sign-in and the T3 Connect account pages are modals in the settings sidebar.
    Account,
}

impl T3SettingsRoute {
    pub fn path(self) -> &'static str {
        match self {
            Self::General => "/settings",
            Self::Connections | Self::Account => "/settings/connections",
            Self::Providers => "/settings/providers",
            Self::PullRequests => "/pull-requests",
            Self::Usage => "/usage",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::General => "T3 Settings",
            Self::Connections | Self::Account => "T3 Connect",
            Self::Providers => "T3 Providers",
            Self::PullRequests => "Pull Requests",
            Self::Usage => "Usage",
        }
    }
}

// Deliberately no Debug: the host credential must never appear in error output.
#[cfg(target_os = "linux")]
pub(super) struct HostSession<'a> {
    pub origin: String,
    pub url: String,
    pub title: &'static str,
    pub cookie_name: &'a str,
    pub access_token: &'a str,
}

#[cfg(target_os = "linux")]
impl<'a> HostSession<'a> {
    pub fn parse(session: &'a serde_json::Value, route: T3SettingsRoute) -> Option<Self> {
        let origin = session["origin"].as_str()?;
        let mut base = url::Url::parse(origin).ok()?;
        if !super::model::allowed_navigation(origin)
            || origin.chars().any(|c| c.is_control() || c.is_whitespace())
            || base.path() != "/"
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return None;
        }
        let cookie_name = session["cookieName"].as_str()?;
        let access_token = session["accessToken"].as_str()?;
        if cookie_name.is_empty()
            || cookie_name.len() > 100
            || !cookie_name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
            || access_token.is_empty()
            || access_token.len() > 16384
            || !access_token
                .bytes()
                .all(|c| (0x21..=0x7e).contains(&c) && c != b';')
        {
            return None;
        }
        let origin = base.origin().ascii_serialization();
        base.set_path(route.path());
        Some(Self {
            origin,
            url: base.into(),
            title: route.title(),
            cookie_name,
            access_token,
        })
    }
}

#[cfg(target_os = "linux")]
pub(super) fn prepare_profile(path: &std::path::Path) -> Result<(), &'static str> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let error = "Could not open the private T3 Connect browser profile.";
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => return Err(error),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .recursive(true)
                .create(path)
                .map_err(|_| error)?;
        }
        Err(_) => return Err(error),
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|_| error)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use serde_json::json;

    fn session(origin: &str) -> serde_json::Value {
        json!({"origin": origin, "cookieName": "t3_session", "accessToken": "synthetic-host-session"})
    }

    #[test]
    fn routes_keep_credentials_out_of_the_address() {
        for (route, path) in [
            (T3SettingsRoute::General, "/settings"),
            (T3SettingsRoute::Connections, "/settings/connections"),
            (T3SettingsRoute::Providers, "/settings/providers"),
            (T3SettingsRoute::Account, "/settings/connections"),
            (T3SettingsRoute::PullRequests, "/pull-requests"),
            (T3SettingsRoute::Usage, "/usage"),
        ] {
            let input = session("http://127.0.0.1:3773/");
            let host = HostSession::parse(&input, route).unwrap();
            assert_eq!(host.url, format!("http://127.0.0.1:3773{path}"));
        }
    }

    #[test]
    fn rejects_non_origins_and_embedded_credentials() {
        for origin in [
            "file:///tmp/settings",
            "https://user:password@example.test",
            "https://example.test/other",
            "https://example.test/?token=synthetic",
            "https://example.test/#synthetic",
            "https://example.test/\n",
        ] {
            assert!(HostSession::parse(&session(origin), T3SettingsRoute::Account).is_none());
        }
    }

    #[test]
    fn rejects_invalid_or_missing_bootstrap_credentials() {
        for (key, value) in [
            ("cookieName", ""),
            ("cookieName", "session;other"),
            ("accessToken", ""),
            ("accessToken", "synthetic;other"),
            ("accessToken", "synthetic\nother"),
        ] {
            let mut input = session("https://example.test");
            input[key] = value.into();
            assert!(HostSession::parse(&input, T3SettingsRoute::Connections).is_none());
        }
        assert!(HostSession::parse(&json!({}), T3SettingsRoute::Account).is_none());
    }

    #[test]
    fn rejects_oversized_bootstrap_credentials() {
        for (key, value) in [
            ("cookieName", "a".repeat(101)),
            ("accessToken", "a".repeat(16385)),
        ] {
            let mut input = session("https://example.test");
            input[key] = value.into();
            assert!(HostSession::parse(&input, T3SettingsRoute::Account).is_none());
        }
    }

    #[test]
    fn profile_is_private_and_does_not_follow_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("native-data/profile");
        prepare_profile(&profile).unwrap();
        assert_eq!(
            std::fs::metadata(&profile).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let link = dir.path().join("link");
        symlink(&profile, &link).unwrap();
        assert!(prepare_profile(&link).is_err());
    }
}
