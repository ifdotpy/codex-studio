//! Server address choice of the iOS shell. Pure, so desktop unit tests
//! cover the order.
#![cfg_attr(not(target_os = "ios"), allow(dead_code))]

use std::collections::BTreeMap;
use tauri::Url;

/// The Bonjour contract of the server; its owner is the runtime schema crate.
#[path = "../../../../../runtime/packages/runtime-schema/src/bonjour_protocol.rs"]
#[allow(dead_code)]
pub mod bonjour;

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

/// A server found on the local network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    pub name: String,
    pub url: Url,
}

/// The server in one Bonjour result, from its TXT record. Only an HTTPS
/// tailnet origin is accepted: no loopback HTTP, path, query or user.
pub fn discovered(service_name: &str, txt: &BTreeMap<String, String>) -> Option<Discovered> {
    if txt.get(bonjour::TXT_VERSION_KEY).map(String::as_str) != Some(bonjour::TXT_VERSION) {
        return None;
    }
    let url = valid_server(txt.get(bonjour::TXT_URL_KEY)?)?;
    let origin_only = url.scheme() == "https"
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none()
        && url
            .host_str()
            .is_some_and(|host| host.ends_with(bonjour::TXT_URL_HOST_SUFFIX));
    if !origin_only {
        return None;
    }
    let name = txt
        .get(bonjour::TXT_NAME_KEY)
        .map(|name| name.trim())
        .filter(|name| !name.is_empty())
        .unwrap_or(service_name)
        .to_owned();
    Some(Discovered { name, url })
}

/// The address to open. Order: the launch environment value (simulator and
/// device checks), the address saved after the last successful load, a
/// server found on the local network, the build default. Invalid entries
/// are skipped.
///
/// The network search is asynchronous. While it is not `settled`, the start
/// page waits and the result is `None`. When it settles, one server opens
/// directly. Two or more servers stay on the start page for a choice. With
/// none, the build default opens.
pub fn choose(
    launch: Option<&str>,
    saved: Option<&str>,
    servers: &[Discovered],
    settled: bool,
    default: Option<&str>,
) -> Option<Url> {
    if let Some(url) = [launch, saved].into_iter().flatten().find_map(valid_server) {
        return Some(url);
    }
    if !settled {
        return None;
    }
    match servers {
        [one] => Some(one.url.clone()),
        [] => default.and_then(valid_server),
        _ => None,
    }
}

/// The value saved after a successful load: the server origin only, so a
/// diagnostic query of one launch does not stay.
pub fn saved_value(loaded: &Url) -> Option<String> {
    valid_server(loaded.as_str())?;
    Some(format!("{}/", loaded.origin().ascii_serialization()))
}

/// `nw_browser_state_t` values (Network.framework `browser.h`).
pub const BROWSER_READY: u32 = 1;
pub const BROWSER_FAILED: u32 = 2;
pub const BROWSER_WAITING: u32 = 4;
/// `nw_error_domain_dns` (Network.framework `error.h`).
pub const ERROR_DOMAIN_DNS: u32 = 2;
/// `kDNSServiceErr_PolicyDenied` (`dns_sd.h`): Local Network access is off.
pub const DNS_POLICY_DENIED: i32 = -65570;

/// True when a browser error means that the person turned off Local Network
/// access for the app.
pub fn permission_denied(error_domain: u32, error_code: i32) -> bool {
    error_domain == ERROR_DOMAIN_DNS && error_code == DNS_POLICY_DENIED
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAILNET: &str = "https://studio.example.ts.net:8444/";

    fn server(name: &str, url: &str) -> Discovered {
        Discovered {
            name: name.into(),
            url: url.parse().unwrap(),
        }
    }

    fn txt(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn launch_value_wins_then_saved_then_one_found_then_build_default() {
        let launch = "http://127.0.0.1:8793/?layoutDebug=1";
        let found = [server("Mac", "https://found.example.ts.net/")];
        let default = Some("https://default.example/");
        let open = |launch, saved, servers: &[Discovered], settled| {
            choose(launch, saved, servers, settled, default).map(|url| url.to_string())
        };
        assert_eq!(
            open(Some(launch), Some(TAILNET), &found, true).as_deref(),
            Some(launch)
        );
        assert_eq!(
            open(None, Some(TAILNET), &found, false).as_deref(),
            Some(TAILNET),
            "a saved address does not wait for the network search"
        );
        assert_eq!(
            open(None, None, &found, true).as_deref(),
            Some("https://found.example.ts.net/")
        );
        assert_eq!(
            open(None, None, &[], true).as_deref(),
            Some("https://default.example/")
        );
        assert!(choose(None, None, &[], true, None).is_none());
    }

    #[test]
    fn the_search_must_settle_and_two_servers_wait_for_a_choice() {
        let found = [server("Mac", "https://found.example.ts.net/")];
        let default = Some("https://default.example/");
        assert!(choose(None, None, &found, false, default).is_none());
        assert!(choose(None, None, &[], false, default).is_none());
        let two = [
            server("Mac", "https://found.example.ts.net/"),
            server("Mini", "https://mini.example.ts.net/"),
        ];
        assert!(choose(None, None, &two, true, default).is_none());
    }

    #[test]
    fn invalid_entries_fall_through_to_the_next_source() {
        assert_eq!(
            choose(Some(""), Some("not a url"), &[], true, Some(TAILNET))
                .unwrap()
                .as_str(),
            TAILNET
        );
        // Plain HTTP only on loopback.
        assert_eq!(
            choose(
                Some("http://studio.example.ts.net/"),
                None,
                &[],
                true,
                Some(TAILNET)
            )
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

    #[test]
    fn a_result_needs_the_txt_version_and_an_https_tailnet_origin() {
        let good = txt(&[("txtvers", "1"), ("url", TAILNET), ("name", " Home Mac ")]);
        assert_eq!(
            discovered("Home Mac (2)", &good),
            Some(server("Home Mac", TAILNET))
        );
        let unnamed = txt(&[("txtvers", "1"), ("url", TAILNET)]);
        assert_eq!(
            discovered("Home Mac (2)", &unnamed).unwrap().name,
            "Home Mac (2)"
        );
        for url in [
            "http://127.0.0.1:8793/",
            "http://studio.example.ts.net/",
            "https://evil.example/",
            "https://studio.example.ts.net/chat",
            "https://studio.example.ts.net/?x=1",
            "https://user@studio.example.ts.net/",
            "not a url",
        ] {
            let record = txt(&[("txtvers", "1"), ("url", url)]);
            assert!(discovered("Mac", &record).is_none(), "{url}");
        }
        assert!(discovered("Mac", &txt(&[("txtvers", "2"), ("url", TAILNET)])).is_none());
        assert!(discovered("Mac", &txt(&[("url", TAILNET)])).is_none());
    }

    #[test]
    fn only_the_dns_policy_error_means_local_network_access_is_off() {
        assert!(permission_denied(ERROR_DOMAIN_DNS, DNS_POLICY_DENIED));
        assert!(!permission_denied(1, -65570));
        assert!(!permission_denied(2, -65537));
    }

    #[test]
    fn the_ios_plist_declares_the_service_type_and_the_local_network_reason() {
        for plist in [
            include_str!("../Info.ios.plist"),
            include_str!("../gen/apple/studio-native-client_iOS/Info.plist"),
        ] {
            let services = plist
                .split("<key>NSBonjourServices</key>")
                .nth(1)
                .and_then(|rest| rest.split("</array>").next())
                .expect("NSBonjourServices");
            assert!(
                services.contains(&format!("<string>{}</string>", bonjour::SERVICE_TYPE)),
                "{services}"
            );
            assert!(plist.contains("<key>NSLocalNetworkUsageDescription</key>"));
        }
    }
}
