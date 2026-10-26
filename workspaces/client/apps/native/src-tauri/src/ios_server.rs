//! Server address choice of the iOS shell. Pure, so desktop unit tests
//! cover the order and the rules for servers found on the local network.
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
/// Custom scheme of the bundled start page.
pub const START_SCHEME: &str = "studio-shell";

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

/// An address that a found server may have: an HTTPS tailnet origin, with
/// no loopback HTTP, path, query or user.
pub fn found_url(value: &str) -> Option<Url> {
    let url = valid_server(value)?;
    let origin_only = url.scheme() == "https"
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none()
        && url
            .host_str()
            .is_some_and(|host| host.ends_with(bonjour::TXT_URL_HOST_SUFFIX));
    origin_only.then_some(url)
}

/// The server in one Bonjour result, from its TXT record.
pub fn discovered(service_name: &str, txt: &BTreeMap<String, String>) -> Option<Discovered> {
    if txt.get(bonjour::TXT_VERSION_KEY).map(String::as_str) != Some(bonjour::TXT_VERSION) {
        return None;
    }
    let url = found_url(txt.get(bonjour::TXT_URL_KEY)?)?;
    let name = txt
        .get(bonjour::TXT_NAME_KEY)
        .map(|name| name.trim())
        .filter(|name| !name.is_empty())
        .unwrap_or(service_name)
        .to_owned();
    Some(Discovered { name, url })
}

/// Where the launch address comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The `STUDIO_SERVER_URL` launch environment value.
    Launch,
    /// The address saved after an explicit choice.
    Saved,
    /// The build default.
    BuildDefault,
}

/// The address to open at launch, first valid of: the launch environment
/// value (simulator and device checks), the saved address, the build
/// default. Servers found on the local network never open by themselves:
/// with none of these, the start page lists them, and the person taps one.
pub fn choose(
    launch: Option<&str>,
    saved: Option<&str>,
    _found: &[Discovered],
    default: Option<&str>,
) -> Option<(Url, Source)> {
    [
        (launch, Source::Launch),
        (saved, Source::Saved),
        (default, Source::BuildDefault),
    ]
    .into_iter()
    .find_map(|(value, source)| Some((valid_server(value?)?, source)))
}

/// The server origin to save.
pub fn saved_value(url: &Url) -> Option<String> {
    valid_server(url.as_str())?;
    Some(format!("{}/", url.origin().ascii_serialization()))
}

/// The value to save after a page load. Only the origin that the launch
/// value or the build default opened (`opened`) is saved after its load. A
/// tap or a Connect submit saves before the load (`connect`). Any other
/// origin, for example a link, a redirect or a found server that the person
/// did not choose, does not change the saved address.
pub fn save_after_load(loaded: &Url, opened: Option<&Url>) -> Option<String> {
    let value = saved_value(loaded)?;
    (saved_value(opened?)? == value).then_some(value)
}

/// The tailnet of an address: the host without its first label, for a host
/// that ends in `.ts.net` (`mac.tail1234.ts.net` -> `tail1234.ts.net`).
pub fn tailnet(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    let (_, rest) = host.split_once('.')?;
    (rest.ends_with(bonjour::TXT_URL_HOST_SUFFIX) && rest.contains('.')).then(|| rest.to_owned())
}

/// True when a found server is not on the tailnet of the saved address.
/// Before the first choice (nothing saved), no server is marked.
pub fn other_network(server: &Url, saved: Option<&str>) -> bool {
    match saved.and_then(valid_server) {
        Some(saved) => tailnet(server) != tailnet(&saved),
        None => false,
    }
}

/// One row of the start page list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub url: Url,
    pub other_network: bool,
}

/// The start page rows, by name, with the other-network mark.
pub fn entries(servers: &[Discovered], saved: Option<&str>) -> Vec<Entry> {
    let mut entries: Vec<_> = servers
        .iter()
        .map(|server| Entry {
            name: server.name.clone(),
            url: server.url.clone(),
            other_network: other_network(&server.url, saved),
        })
        .collect();
    entries.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.url.as_str().cmp(b.url.as_str()))
    });
    entries
}

/// The result of a start page request to open a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Connect {
    /// Save the address and open it.
    Open(Url),
    /// A found server on another tailnet: ask for a confirm tap first.
    Confirm(Url),
    /// Not an address that the shell opens.
    Invalid,
}

/// Decide a start page request. `found` is a tap on a found server;
/// `confirmed` is the confirm tap for another tailnet. A typed address is
/// the person's own choice and opens without a confirm tap.
pub fn connect(value: &str, found: bool, confirmed: bool, saved: Option<&str>) -> Connect {
    if !found {
        return valid_server(value).map_or(Connect::Invalid, Connect::Open);
    }
    match found_url(value) {
        Some(url) if other_network(&url, saved) && !confirmed => Connect::Confirm(url),
        Some(url) => Connect::Open(url),
        None => Connect::Invalid,
    }
}

/// The network search runs only while the start page is shown: no Local
/// Network prompt for a person who never opens it.
pub fn discovery_on(page: &Url) -> bool {
    page.scheme() == START_SCHEME
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

    fn url(value: &str) -> Url {
        value.parse().unwrap()
    }

    fn server(name: &str, value: &str) -> Discovered {
        Discovered {
            name: name.into(),
            url: url(value),
        }
    }

    fn txt(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn launch_value_wins_then_saved_then_build_default() {
        let launch = "http://127.0.0.1:8793/?layoutDebug=1";
        let default = Some("https://default.example/");
        let open = |launch, saved| {
            choose(launch, saved, &[], default).map(|(url, source)| (url.to_string(), source))
        };
        assert_eq!(
            open(Some(launch), Some(TAILNET)),
            Some((launch.into(), Source::Launch))
        );
        assert_eq!(
            open(None, Some(TAILNET)),
            Some((TAILNET.into(), Source::Saved))
        );
        assert_eq!(
            open(None, None),
            Some(("https://default.example/".into(), Source::BuildDefault))
        );
        assert!(choose(None, None, &[], None).is_none());
    }

    #[test]
    fn a_found_server_never_opens_without_a_tap() {
        let one = [server("Mac", "https://mac.tail1.ts.net/")];
        assert!(
            choose(None, None, &one, None).is_none(),
            "one found server and nothing saved: the start page lists it"
        );
        assert_eq!(
            choose(None, None, &one, Some("https://default.example/"))
                .unwrap()
                .1,
            Source::BuildDefault
        );
    }

    #[test]
    fn invalid_entries_fall_through_to_the_next_source() {
        let open = |launch, saved, default| {
            choose(launch, saved, &[], default).map(|(url, _)| url.to_string())
        };
        assert_eq!(
            open(Some(""), Some("not a url"), Some(TAILNET)).as_deref(),
            Some(TAILNET)
        );
        // Plain HTTP only on loopback.
        assert_eq!(
            open(Some("http://studio.example.ts.net/"), None, Some(TAILNET)).as_deref(),
            Some(TAILNET)
        );
        assert!(valid_server("https://user:pass@studio.example/").is_none());
        assert!(valid_server("studio-shell://localhost/").is_none());
    }

    #[test]
    fn a_load_saves_only_the_origin_that_the_launch_value_or_default_opened() {
        let opened = url("https://studio.example.ts.net:8444/?layoutDebug=1");
        assert_eq!(
            save_after_load(
                &url("https://studio.example.ts.net:8444/chat/1"),
                Some(&opened)
            )
            .as_deref(),
            Some(TAILNET)
        );
        // An unconfirmed server, a link or a redirect keeps the saved address.
        assert!(save_after_load(&url("https://mac.tail1.ts.net/"), Some(&opened)).is_none());
        assert!(save_after_load(&url("https://mac.tail1.ts.net/"), None).is_none());
        assert!(
            save_after_load(&url("studio-shell://localhost/?failed=1"), Some(&opened)).is_none()
        );
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
        for value in [
            "http://127.0.0.1:8793/",
            "http://studio.example.ts.net/",
            "https://evil.example/",
            "https://studio.example.ts.net/chat",
            "https://studio.example.ts.net/?x=1",
            "https://user@studio.example.ts.net/",
            "not a url",
        ] {
            let record = txt(&[("txtvers", "1"), ("url", value)]);
            assert!(discovered("Mac", &record).is_none(), "{value}");
        }
        assert!(discovered("Mac", &txt(&[("txtvers", "2"), ("url", TAILNET)])).is_none());
        assert!(discovered("Mac", &txt(&[("url", TAILNET)])).is_none());
    }

    #[test]
    fn a_found_server_on_another_tailnet_needs_a_confirm_tap() {
        let saved = Some("https://home.tail1.ts.net/");
        let same = "https://mini.tail1.ts.net/";
        let other = "https://mac.tail2.ts.net/";
        assert_eq!(connect(same, true, false, saved), Connect::Open(url(same)));
        assert_eq!(
            connect(other, true, false, saved),
            Connect::Confirm(url(other))
        );
        assert_eq!(connect(other, true, true, saved), Connect::Open(url(other)));
        // Before the first choice, nothing is marked.
        assert_eq!(connect(other, true, false, None), Connect::Open(url(other)));
        // A typed address is the person's own choice.
        assert_eq!(
            connect(other, false, false, saved),
            Connect::Open(url(other))
        );
        // A found link accepts only a tailnet origin.
        assert_eq!(
            connect("https://evil.example/", true, true, saved),
            Connect::Invalid
        );
        assert_eq!(
            entries(&[server("Z", other), server("A", same)], saved),
            vec![
                Entry {
                    name: "A".into(),
                    url: url(same),
                    other_network: false
                },
                Entry {
                    name: "Z".into(),
                    url: url(other),
                    other_network: true
                },
            ]
        );
        assert_eq!(
            tailnet(&url("https://mac.tail2.ts.net:8444/")).as_deref(),
            Some("tail2.ts.net")
        );
        assert!(tailnet(&url("https://ts.net/")).is_none());
    }

    #[test]
    fn the_search_runs_only_on_the_start_page() {
        assert!(discovery_on(&url("studio-shell://localhost/")));
        assert!(discovery_on(&url("studio-shell://localhost/?failed=1")));
        assert!(discovery_on(&url("studio-shell://localhost/connect?x=1")));
        assert!(!discovery_on(&url("https://mac.tail1.ts.net/")));
        assert!(!discovery_on(&url("http://127.0.0.1:8793/")));
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
