//! `layr-desk push`: put the main head of a layr project into its vault (docs/design.md,
//! "Writer"). The host of the project runs it as the owner of the layr store. The configuration
//! holds the vault recipient, a key that may only put new objects and list them, and the
//! writer's signing key (docs/design.md, "Signatures").

use crate::s3::S3;
use anyhow::{anyhow, bail, Context, Result};
use clap::Args;
use desk_core::aws::Credentials;
use desk_core::keys;
use desk_core::sign::Signer;
use desk_core::writer::{Layr, Writer};
use serde::Deserialize;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;

#[derive(Args)]
pub struct Push {
    /// The vault configuration, JSON with bucket, region, prefix, recipient, accessKeyId,
    /// secretAccessKey, signingKey (optional) and, for a test store, endpoint; without it, or
    /// with "-", it is read from standard input
    #[arg(long)]
    config: Option<PathBuf>,
    /// The layr project
    #[arg(long)]
    project: String,
    /// The state folder of the writer: writer.json, tree.json and the export folders a and b
    #[arg(long)]
    state_dir: PathBuf,
    /// The layr command
    #[arg(long, default_value = "layr")]
    layr: PathBuf,
}

/// The configuration of one vault, with the names of the host's `config.json`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    bucket: String,
    region: String,
    prefix: String,
    recipient: String,
    access_key_id: String,
    secret_access_key: String,
    #[serde(default)]
    endpoint: Option<String>,
    /// Standard base64 of the writer's 32-byte Ed25519 seed; without it the trees are unsigned.
    #[serde(default)]
    signing_key: Option<String>,
}

/// layr for one project.
struct Command {
    program: PathBuf,
    project: String,
}

impl Layr for Command {
    fn run(&self, args: &[&str]) -> Result<String> {
        let output = std::process::Command::new(&self.program)
            .arg("--project")
            .arg(&self.project)
            .args(args)
            .current_dir("/")
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("running {}", self.program.display()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "layr {} failed: {}",
                args[0],
                stderr.chars().take(300).collect::<String>().trim()
            );
        }
        String::from_utf8(output.stdout)
            .map_err(|_| anyhow!("layr {} wrote text that is not UTF-8", args[0]))
    }
}

impl Config {
    /// The names that go into the S3 host and keys, as the host of the project checks them when
    /// it keeps the configuration.
    fn check(&self) -> Result<()> {
        let bucket = &self.bucket;
        let bucket_ok = (3..=63).contains(&bucket.len())
            && bucket
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
            && !bucket.starts_with(['.', '-'])
            && !bucket.ends_with(['.', '-']);
        if !bucket_ok {
            bail!("the bucket name {bucket:?} is not valid");
        }
        // Two letters, words, a digit: eu-central-1, us-gov-west-1.
        let parts: Vec<&str> = self.region.split('-').collect();
        let region_ok = parts.len() >= 3
            && parts[0].len() == 2
            && parts[..parts.len() - 1]
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase()))
            && parts[parts.len() - 1].len() == 1
            && parts[parts.len() - 1].bytes().all(|b| b.is_ascii_digit());
        if !region_ok {
            bail!("the region {:?} is not valid", self.region);
        }
        // A letter or a digit, then letters, digits, `.`, `_` and `-`.
        let prefix = &self.prefix;
        let prefix_ok = (1..=100).contains(&prefix.len())
            && prefix.starts_with(|c: char| c.is_ascii_alphanumeric())
            && prefix
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if !prefix_ok {
            bail!("the prefix {prefix:?} is not valid");
        }
        if self.access_key_id.is_empty() || self.secret_access_key.is_empty() {
            bail!("the access key is missing");
        }
        if let Some(endpoint) = &self.endpoint {
            let url = reqwest::Url::parse(endpoint).ok();
            if !url.is_some_and(|u| matches!(u.scheme(), "http" | "https") && u.has_host()) {
                bail!("the endpoint {endpoint:?} is not http(s)://host");
            }
        }
        keys::encrypt(&self.recipient, b"").context("the vault recipient")?;
        self.signer()?;
        Ok(())
    }

    fn signer(&self) -> Result<Option<Signer>> {
        self.signing_key
            .as_deref()
            .map(Signer::from_base64)
            .transpose()
    }
}

pub async fn run(args: Push) -> Result<()> {
    let text = match &args.config {
        Some(path) if path != Path::new("-") => {
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?
        }
        _ => {
            let mut text = String::new();
            std::io::stdin().read_to_string(&mut text)?;
            text
        }
    };
    let config: Config =
        serde_json::from_str(&text).context("the configuration is not a vault configuration")?;
    config.check()?;
    let signer = config.signer()?;
    let s3 = S3::new(
        &config.bucket,
        &config.region,
        Credentials {
            key_id: config.access_key_id,
            secret: config.secret_access_key,
            token: None,
        },
        config.endpoint.as_deref(),
    )?;
    let layr = Command {
        program: args.layr,
        project: args.project,
    };
    let head = layr.run(&["rev-parse", "main"])?.trim().to_string();
    if head.is_empty() {
        bail!("layr names no main head");
    }
    let recipient = config.recipient;
    let mut writer = Writer::new(
        &args.state_dir,
        &config.prefix,
        layr,
        s3,
        move |data: &[u8]| keys::encrypt(&recipient, data),
    )?;
    if let Some(signer) = signer {
        writer = writer.signed_by(signer);
    }
    let summary = writer.push(&head).await?;
    println!("{}", serde_json::to_string(&summary)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Config;

    fn config(field: &str, value: &str) -> Config {
        let mut json = serde_json::json!({
            "bucket": "vault-1", "region": "eu-central-1", "prefix": "proj",
            "recipient": desk_core::keys::recipient_of(&desk_core::keys::generate_identity()).unwrap(),
            "accessKeyId": "AKIAIOSFODNN7EXAMPLE", "secretAccessKey": "secret-example-0000",
        });
        if !field.is_empty() {
            json[field] = value.into();
        }
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn names_that_would_change_the_host_or_the_keys_are_refused() {
        assert!(config("", "").check().is_ok());
        for (field, value) in [
            ("bucket", "my.vault"),
            ("region", "us-gov-west-1"),
            ("prefix", "Proj_1.x-y"),
            ("endpoint", "http://127.0.0.1:9000"),
            ("signingKey", "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA="),
        ] {
            assert!(config(field, value).check().is_ok(), "{field} {value}");
        }
        for (field, value) in [
            ("bucket", "Upper"),
            ("bucket", "ab"),
            ("bucket", "b@evil.com/"),
            ("bucket", "-vault"),
            ("region", "eu-central-1.evil.com"),
            ("region", "eu-central"),
            ("region", "e-central-1"),
            ("prefix", ""),
            ("prefix", "-a"),
            ("prefix", "a/b"),
            ("prefix", "caf\u{e9}"),
            ("endpoint", "ftp://x"),
            ("endpoint", "127.0.0.1:9000"),
            ("recipient", "age1nothing"),
            ("secretAccessKey", ""),
            ("signingKey", "AQID"),
            ("signingKey", "not base64"),
        ] {
            assert!(config(field, value).check().is_err(), "{field} {value}");
        }
    }
}
