//! Server address choice of the iOS shell. Pure, so desktop unit tests
//! cover the order.
#![cfg_attr(not(target_os = "ios"), allow(dead_code))]

use tauri::Url;

/// Server address compiled in at build time, for an install that must open
/// Studio from the home screen icon. Not set in the repository.
pub const BUILD_DEFAULT: Option<&str> = option_env!("STUDIO_DEFAULT_SERVER_URL");

/// HTTPS servers, or plain HTTP only on this device's loopback (simulator).
pub fn valid_server(value: &str) -> Option<Url> {
    let url: Url = value.trim().parse().ok()?;
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost"));
    let scheme_ok = url.scheme() == "https" || (url.scheme() == "http" && loopback);
    // Path and query stay, so a simulator check can pass `?layoutDebug=1`.
    (scheme_ok && url.host_str().is_some() && url.username().is_empty() && url.password().is_none())
        .then_some(url)
}

/// The address to open: the launch environment value (simulator and device
/// checks), then the address saved after the last successful load, then the
/// build default. Invalid entries are skipped.
pub fn choose(launch: Option<&str>, saved: Option<&str>, default: Option<&str>) -> Option<Url> {
    [launch, saved, default]
        .into_iter()
        .flatten()
        .find_map(valid_server)
}

/// The value saved after a successful load: the server origin only, so a
/// diagnostic query of one launch does not stay.
pub fn saved_value(loaded: &Url) -> Option<String> {
    valid_server(loaded.as_str())?;
    Some(format!("{}/", loaded.origin().ascii_serialization()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAILNET: &str = "https://studio.example.ts.net:8444/";

    #[test]
    fn launch_value_wins_then_saved_then_build_default() {
        let launch = "http://127.0.0.1:8793/?layoutDebug=1";
        assert_eq!(
            choose(
                Some(launch),
                Some(TAILNET),
                Some("https://default.example/")
            )
            .unwrap()
            .as_str(),
            launch
        );
        assert_eq!(
            choose(None, Some(TAILNET), Some("https://default.example/"))
                .unwrap()
                .as_str(),
            TAILNET
        );
        assert_eq!(
            choose(None, None, Some("https://default.example/"))
                .unwrap()
                .as_str(),
            "https://default.example/"
        );
        assert!(choose(None, None, None).is_none());
    }

    #[test]
    fn invalid_entries_fall_through_to_the_next_source() {
        assert_eq!(
            choose(Some(""), Some("not a url"), Some(TAILNET))
                .unwrap()
                .as_str(),
            TAILNET
        );
        // Plain HTTP only on loopback.
        assert_eq!(
            choose(Some("http://studio.example.ts.net/"), None, Some(TAILNET))
                .unwrap()
                .as_str(),
            TAILNET
        );
        assert!(valid_server("https://user:pass@studio.example/").is_none());
        assert!(valid_server("studio-shell://localhost/").is_none());
    }

    #[test]
    fn saves_only_the_server_origin() {
        let loaded: Url = "https://studio.example.ts.net:8444/chat/1?layoutDebug=1"
            .parse()
            .unwrap();
        assert_eq!(saved_value(&loaded).as_deref(), Some(TAILNET));
        let start: Url = "studio-shell://localhost/?failed=1".parse().unwrap();
        assert!(saved_value(&start).is_none());
    }
}
