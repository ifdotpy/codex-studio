//! Bonjour (DNS-SD) advertisement of this server on the local network, so the
//! iOS shell finds it without a typed address. The TXT record carries the
//! HTTPS remote access origin (`RemoteAccess`). A WKWebView needs a secure
//! context, so the server does not advertise without that origin. Contract
//! owner: `studio_runtime_schema::bonjour_protocol`.
//!
//! The system responder publishes the record: `dns-sd -R` on macOS and
//! `avahi-publish-service` on Linux, as a child process (like the `tailscale`
//! CLI adapter). A shell wrapper stops that child when this process closes the
//! wrapper's stdin, also after a crash. The advertisement uses the startup
//! configuration; a change takes effect at the next server start.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use studio_runtime_schema::bonjour_protocol::{
    INSTANCE_NAME_MAX_BYTES, SERVICE_TYPE, TXT_ENTRY_MAX_BYTES, TXT_NAME_KEY, TXT_SCHEMA_KEY,
    TXT_URL_HOST_SUFFIX, TXT_URL_KEY, TXT_VERSION, TXT_VERSION_KEY,
};

/// Instance name when the server label has no printable characters.
const FALLBACK_NAME: &str = "Studio server";

/// One DNS-SD registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advertisement {
    pub instance: String,
    pub port: u16,
    pub txt: Vec<String>,
}

/// Why the server does not advertise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The remote access file sets the Bonjour key to `false`.
    Disabled,
    /// No HTTPS remote access origin is configured.
    NoSecureOrigin,
}

impl Refusal {
    pub fn reason(self) -> &'static str {
        match self {
            Self::Disabled => "remote-access.json sets \"bonjour\" to false",
            Self::NoSecureOrigin => {
                "no HTTPS remote access origin (remote-access.json or CODEX_CANVAS_PUBLIC_ORIGIN); \
                 the iOS app needs a secure origin"
            }
        }
    }
}

/// The registration for `origin`, the validated `RemoteAccess` origin
/// (`https://host[:port]`, no path).
pub fn advertisement(
    origin: Option<&str>,
    enabled: bool,
    label: &str,
    schema: &str,
) -> Result<Advertisement, Refusal> {
    if !enabled {
        return Err(Refusal::Disabled);
    }
    let origin = origin.ok_or(Refusal::NoSecureOrigin)?;
    let authority = origin
        .strip_prefix("https://")
        .filter(|authority| {
            !authority.is_empty() && !authority.contains(['/', '?', '#', '@', '[', ' '])
        })
        .ok_or(Refusal::NoSecureOrigin)?;
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port.parse().map_err(|_| Refusal::NoSecureOrigin)?),
        None => (authority, 443),
    };
    if !host.ends_with(TXT_URL_HOST_SUFFIX) {
        return Err(Refusal::NoSecureOrigin);
    }
    let name = display_name(label);
    Ok(Advertisement {
        instance: truncate(&name, INSTANCE_NAME_MAX_BYTES),
        port,
        txt: vec![
            format!("{TXT_VERSION_KEY}={TXT_VERSION}"),
            format!("{TXT_URL_KEY}={origin}/"),
            truncate(&format!("{TXT_NAME_KEY}={name}"), TXT_ENTRY_MAX_BYTES),
            format!("{TXT_SCHEMA_KEY}={schema}"),
        ],
    })
}

/// The label without control characters and backslashes (`dns-sd` reads a
/// backslash as an escape).
fn display_name(label: &str) -> String {
    let name: String = label
        .chars()
        .filter(|c| !c.is_control() && *c != '\\')
        .collect();
    match name.trim() {
        "" => FALLBACK_NAME.to_owned(),
        name => name.to_owned(),
    }
}

/// At most `max` bytes, cut at a character boundary.
fn truncate(value: &str, max: usize) -> String {
    let mut end = value.len().min(max);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].trim_end().to_owned()
}

/// The command line tool of the system responder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// macOS `dns-sd -R`.
    DnsSd,
    /// Linux `avahi-publish-service` (Avahi utilities).
    Avahi,
}

impl Tool {
    pub fn system() -> Option<Self> {
        if cfg!(target_os = "macos") {
            Some(Self::DnsSd)
        } else if cfg!(target_os = "linux") {
            Some(Self::Avahi)
        } else {
            None
        }
    }

    fn program(self) -> &'static str {
        match self {
            Self::DnsSd => "/usr/bin/dns-sd",
            Self::Avahi => "avahi-publish-service",
        }
    }

    pub fn arguments(self, advertisement: &Advertisement) -> Vec<String> {
        let port = advertisement.port.to_string();
        let mut arguments: Vec<String> = match self {
            Self::DnsSd => vec![
                "-R".into(),
                advertisement.instance.clone(),
                SERVICE_TYPE.into(),
                "local".into(),
                port,
            ],
            Self::Avahi => vec![advertisement.instance.clone(), SERVICE_TYPE.into(), port],
        };
        arguments.extend(advertisement.txt.iter().cloned());
        arguments
    }
}

/// The program path, or `None` when it is not installed.
fn find(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        return Path::new(program).is_file().then(|| program.into());
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|path| path.is_file())
}

/// Runs `"$@"` and stops it when stdin closes. An asynchronous list reads
/// `/dev/null`, so stdin is kept as fd 3 for the watcher.
const WRAPPER: &str = r#"exec 2>&1 3<&0
"$@" 3<&- </dev/null &
child=$!
{ read -r _ <&3; kill "$child"; } >/dev/null 2>&1 &
exec 3<&-
wait "$child""#;

/// A running advertisement. Drop it to withdraw the record.
pub struct Running {
    _stdin: ChildStdin,
}

/// Start `program` with `arguments` under the wrapper. Its output goes to
/// stderr with a `bonjour:` prefix; stdout stays free for the readiness line.
pub fn run(program: &Path, arguments: &[String]) -> std::io::Result<Running> {
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(WRAPPER)
        .arg("studio-bonjour")
        .arg(program)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("no stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("no stdout"))?;
    std::thread::Builder::new()
        .name("bonjour-advertisement".to_owned())
        .spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                eprintln!("bonjour: {line}");
            }
            match child.wait() {
                Ok(status) => eprintln!("bonjour: advertisement stopped ({status})"),
                Err(error) => eprintln!("bonjour: advertisement stopped: {error}"),
            }
        })?;
    Ok(Running { _stdin: stdin })
}

/// Advertise this server, or log why not.
pub fn start(origin: Option<&str>, enabled: bool, label: &str, schema: &str) -> Option<Running> {
    let advertisement = match advertisement(origin, enabled, label, schema) {
        Ok(advertisement) => advertisement,
        Err(refusal) => {
            eprintln!("bonjour: no advertisement: {}", refusal.reason());
            return None;
        }
    };
    let Some(tool) = Tool::system() else {
        eprintln!("bonjour: no advertisement: unsupported platform");
        return None;
    };
    let Some(program) = find(tool.program()) else {
        eprintln!(
            "bonjour: no advertisement: {} is not installed",
            tool.program()
        );
        return None;
    };
    match run(&program, &tool.arguments(&advertisement)) {
        Ok(running) => {
            eprintln!(
                "bonjour: advertising \"{}\" {SERVICE_TYPE} port {} ({})",
                advertisement.instance,
                advertisement.port,
                advertisement.txt.join(" ")
            );
            Some(running)
        }
        Err(error) => {
            eprintln!("bonjour: no advertisement: {error}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const ORIGIN: &str = "https://studio.example.ts.net:8444";

    #[test]
    fn the_txt_record_carries_the_https_origin_name_and_schema() {
        let advertisement =
            advertisement(Some(ORIGIN), true, "Home Mac", "abc123").expect("advertised");
        assert_eq!(
            advertisement,
            Advertisement {
                instance: "Home Mac".into(),
                port: 8444,
                txt: vec![
                    "txtvers=1".into(),
                    "url=https://studio.example.ts.net:8444/".into(),
                    "name=Home Mac".into(),
                    "schema=abc123".into(),
                ],
            }
        );
        let default_port = advertisement_port("https://studio.example.ts.net");
        assert_eq!(default_port, Some(443));
    }

    fn advertisement_port(origin: &str) -> Option<u16> {
        advertisement(Some(origin), true, "Studio", "s")
            .ok()
            .map(|a| a.port)
    }

    #[test]
    fn no_secure_origin_or_the_off_switch_refuses_the_advertisement() {
        assert_eq!(
            advertisement(None, true, "Studio", "s"),
            Err(Refusal::NoSecureOrigin)
        );
        for origin in [
            "http://studio.example.ts.net",
            "https://",
            "https://studio.example.ts.net/path",
            "https://studio.example.ts.net:http",
            "https://studio.example.com",
            "studio.example.ts.net",
        ] {
            assert_eq!(
                advertisement(Some(origin), true, "Studio", "s"),
                Err(Refusal::NoSecureOrigin),
                "{origin}"
            );
        }
        assert_eq!(
            advertisement(Some(ORIGIN), false, "Studio", "s"),
            Err(Refusal::Disabled)
        );
    }

    #[test]
    fn the_name_fits_dns_sd_limits() {
        let long = "Ж".repeat(80);
        let advertisement = advertisement(Some(ORIGIN), true, &long, "s").expect("advertised");
        assert!(advertisement.instance.len() <= INSTANCE_NAME_MAX_BYTES);
        assert_eq!(advertisement.instance, "Ж".repeat(31));
        assert!(
            advertisement
                .txt
                .iter()
                .all(|t| t.len() <= TXT_ENTRY_MAX_BYTES)
        );
        let cleaned = super::advertisement(Some(ORIGIN), true, " a\\b\nc ", "s").expect("ok");
        assert_eq!(cleaned.instance, "abc");
        let empty = super::advertisement(Some(ORIGIN), true, "\n", "s").expect("ok");
        assert_eq!(empty.instance, FALLBACK_NAME);
    }

    #[test]
    fn both_tools_receive_the_name_type_port_and_txt() {
        let advertisement = advertisement(Some(ORIGIN), true, "Mac", "s").expect("advertised");
        let txt = &advertisement.txt;
        assert_eq!(
            Tool::DnsSd.arguments(&advertisement)[..5],
            ["-R", "Mac", "_layr-studio._tcp", "local", "8444"]
        );
        assert_eq!(Tool::DnsSd.arguments(&advertisement)[5..], txt[..]);
        assert_eq!(
            Tool::Avahi.arguments(&advertisement)[..3],
            ["Mac", "_layr-studio._tcp", "8444"]
        );
        assert_eq!(Tool::Avahi.arguments(&advertisement)[3..], txt[..]);
    }

    fn alive(pid: &str) -> bool {
        Command::new("kill")
            .args(["-0", pid])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn wait_until(mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn closing_stdin_stops_the_responder_tool() {
        let dir = tempfile::tempdir().expect("temp dir");
        let pid_file = dir.path().join("pid");
        let tool = dir.path().join("fake-dns-sd");
        std::fs::write(
            &tool,
            format!(
                "#!/bin/sh\necho \"$@\"\necho $$ > {}\nexec sleep 30\n",
                pid_file.display()
            ),
        )
        .expect("fake tool");
        Command::new("chmod")
            .arg("+x")
            .arg(&tool)
            .status()
            .expect("chmod");
        let running = run(&tool, &["-R".into(), "name=a b".into()]).expect("started");
        assert!(wait_until(
            || std::fs::read_to_string(&pid_file).is_ok_and(|pid| !pid.trim().is_empty())
        ));
        let pid = std::fs::read_to_string(&pid_file).expect("pid");
        let pid = pid.trim();
        assert!(alive(pid), "the tool runs while the server holds stdin");
        drop(running);
        assert!(wait_until(|| !alive(pid)), "closing stdin stops the tool");
    }
}
