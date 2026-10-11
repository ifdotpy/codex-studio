//! Set up a vault from this Mac (docs/design.md, "Set up a vault for a project"). `keys` makes
//! the vault identity and its paper key. `deploy` makes or updates the vault's stack
//! (crates/layr-desk/cloud/vault.yaml) with a person's AWS profile, uploads vault.json and the paper
//! slot, registers the vault with the gateway, and gives the writer its keys. The local files
//! are in ~/.config/studio-vault: gateway.json (from vault-gateway), and per vault recipient.txt
//! and paper.age (from `keys`), facts.json and cloud.json (from `deploy`). As for the AWS CLI,
//! AWS_ENDPOINT_URL_CLOUDFORMATION, AWS_ENDPOINT_URL_IAM and AWS_ENDPOINT_URL_S3 replace the
//! endpoints (for a test; S3 is then path-style).

use crate::profile::Profile;
use crate::s3::{self, Call, S3};
use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use clap::Args;
use desk_core::aws::{self, Credentials};
use desk_core::keys;
use desk_core::sign::Signer;
use desk_core::writer::Bucket;
use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TEMPLATE: &str = include_str!("../cloud/vault.yaml");
const LIMIT: Duration = Duration::from_secs(60);
/// The longest wait for a stack.
const STACK_WAIT: Duration = Duration::from_secs(3600);

fn settings() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var("HOME").context("HOME")?).join(".config/studio-vault"))
}

/// A vault prefix: the reader role studio-vault-reader-<prefix> has at most 64 characters, and
/// the gateway serves /api/ and /assets/ itself.
fn check_prefix(prefix: &str) -> Result<()> {
    let mut chars = prefix.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && prefix.len() <= 44;
    if !valid {
        bail!("{prefix:?} is not a vault name: up to 44 letters, digits, - and _");
    }
    if ["api", "assets"].contains(&prefix) {
        bail!("{prefix} is a gateway path; choose another name");
    }
    Ok(())
}

fn create_private(path: &std::path::Path, data: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| path.display().to_string())?;
    file.write_all(data)?;
    file.sync_all()?;
    Ok(())
}

#[derive(Args)]
pub struct Keys {
    /// The vault (project prefix)
    #[arg(long)]
    project: String,
    /// After the words, wait for Enter, then clear the terminal and its scrollback
    #[arg(long)]
    clear: bool,
}

/// Make the keys of a new vault and show its paper key once, in this terminal only.
pub fn keys(args: Keys) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    check_prefix(&args.project)?;
    let folder = settings()?.join(&args.project);
    if folder.join("recipient.txt").exists() {
        bail!(
            "{} has keys already; a new vault identity would cut off the old states",
            folder.display()
        );
    }
    let vault = keys::generate_identity();
    let paper = keys::generate_identity();
    let slot = keys::seal_slot(&vault, &keys::recipient_of(&paper)?)?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&folder)?;
    // recipient.txt comes last: it marks a folder with complete keys. A paper.age of an earlier
    // run that stopped before it is replaced.
    let _ = std::fs::remove_file(folder.join("paper.age"));
    create_private(&folder.join("paper.age"), &slot)?;
    create_private(
        &folder.join("recipient.txt"),
        format!("{}\n", keys::recipient_of(&vault)?).as_bytes(),
    )?;
    let words = keys::words_of_identity(&paper)?;
    let words: Vec<&str> = words.split(' ').collect();
    let rows: Vec<String> = (0..6)
        .map(|r| {
            [0, 6, 12, 18]
                .iter()
                .map(|c| format!("{:>2}. {:<10}", r + c + 1, words[r + c]))
                .collect::<Vec<_>>()
                .join("  ")
        })
        .collect();
    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        "\nPaper key of the vault \"{}\": 24 words. Write them on paper twice, in this order, and keep\n\
         the copies apart. They are shown once and stored nowhere. Without them and without a passkey\n\
         the vault cannot be read.\n",
        args.project
    )?;
    for row in rows {
        writeln!(out, "    {row}")?;
    }
    writeln!(out)?;
    out.flush()?;
    if args.clear && std::io::stdin().is_terminal() {
        write!(
            out,
            "Press Enter when both copies are written. The terminal is then cleared. "
        )?;
        out.flush()?;
        std::io::stdin().read_line(&mut String::new())?;
        // Clear the screen and the scrollback.
        write!(out, "\x1b[H\x1b[2J\x1b[3J")?;
        writeln!(out, "The keys of the vault \"{}\" are ready.", args.project)?;
    }
    Ok(())
}

#[derive(Args)]
pub struct Deploy {
    /// The vault (project prefix)
    #[arg(long)]
    project: String,
    /// The AWS profile of the vault's account, which may be a client's
    #[arg(long)]
    profile: String,
    #[arg(long, default_value = "eu-central-1")]
    region: String,
    /// A shell command that takes the writer configuration (bucket, region, prefix, recipient,
    /// the Desk site, a new access key and a new signing key) as JSON on standard input and
    /// stores it where the writer runs; it runs when the writer has no key yet
    #[arg(long)]
    writer: Option<String>,
    /// Give the writer a new access key and a new signing key; this also starts signatures on a
    /// vault made before them
    #[arg(long)]
    new_writer_key: bool,
    /// The gateway role of another person who serves this vault from their own gateway
    #[arg(long)]
    also_gateway: Vec<String>,
}

/// gateway.json of vault-gateway: the gateway's role, its artifacts bucket and its account.
#[derive(Deserialize)]
struct Gateway {
    role: String,
    artifacts: String,
    profile: String,
    region: String,
    /// The site's name; a gateway on a tailnet (vault-gateway --tailscale) has none.
    #[serde(default)]
    host: Option<String>,
}

/// The tailnet name of a gateway without a domain, once it has joined the tailnet.
fn tailnet_host() -> Result<String> {
    let output = Command::new("tailscale")
        .args(["status", "--json"])
        .output()
        .context("tailscale status")?;
    let status: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    status["Peer"]
        .as_object()
        .into_iter()
        .flat_map(|peers| peers.values())
        .find(|p| p["HostName"] == "vault-gateway" && p["DNSName"].is_string())
        .and_then(|p| p["DNSName"].as_str())
        .map(|name| name.trim_end_matches('.').to_string())
        .ok_or_else(|| anyhow!("the gateway has not joined the tailnet yet; run this again later"))
}

fn endpoint(variable: &str, default: String) -> String {
    std::env::var(variable)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or(default)
        .trim_end_matches('/')
        .to_string()
}

/// One account of a person: the Query APIs of CloudFormation and IAM, and S3.
struct Account {
    http: reqwest::Client,
    credentials: Credentials,
    region: String,
}

/// What a failed Query call says: its code and message, which hold no secret.
fn failure(what: &str, status: u16, reply: &str) -> anyhow::Error {
    let code = aws::xml_text(reply, "Code").unwrap_or_default();
    let message = aws::xml_text(reply, "Message").unwrap_or_default();
    anyhow!("{what}: HTTP {status} {code} {message}")
}

/// A stack as DescribeStacks shows it.
struct Stack {
    status: String,
    reason: String,
    outputs: BTreeMap<String, String>,
}

impl Account {
    fn new(credentials: Credentials, region: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent("layr-desk")
            .connect_timeout(Duration::from_secs(30))
            .build()?;
        Ok(Account {
            http,
            credentials,
            region: region.to_string(),
        })
    }

    fn bucket(&self, bucket: &str, region: &str) -> Result<S3> {
        let endpoint = std::env::var("AWS_ENDPOINT_URL_S3")
            .ok()
            .filter(|e| !e.is_empty());
        S3::new(
            bucket,
            region,
            self.credentials.clone(),
            endpoint.as_deref(),
        )
    }

    /// One Query API call (form-encoded POST): the status and the XML reply.
    async fn query(
        &self,
        service: &str,
        region: &str,
        base: &str,
        params: &[(&str, &str)],
    ) -> Result<(u16, String)> {
        let body = params
            .iter()
            .map(|(k, v)| {
                format!(
                    "{}={}",
                    aws::uri_encode(k, false),
                    aws::uri_encode(v, false)
                )
            })
            .collect::<Vec<_>>()
            .join("&");
        let call = Call {
            method: Method::POST,
            base,
            path: "/",
            query: &[],
            headers: &[(
                "content-type",
                "application/x-www-form-urlencoded; charset=utf-8",
            )],
            body: body.into_bytes(),
        };
        s3::send(&self.http, call, &self.credentials, region, service, LIMIT).await
    }

    async fn cloudformation(&self, params: &[(&str, &str)]) -> Result<(u16, String)> {
        let base = endpoint(
            "AWS_ENDPOINT_URL_CLOUDFORMATION",
            format!("https://cloudformation.{}.amazonaws.com", self.region),
        );
        let mut params = params.to_vec();
        params.push(("Version", "2010-05-15"));
        self.query("cloudformation", &self.region.clone(), &base, &params)
            .await
    }

    /// IAM is global; its endpoint signs in us-east-1.
    async fn iam(&self, what: &str, params: &[(&str, &str)]) -> Result<String> {
        let base = endpoint("AWS_ENDPOINT_URL_IAM", "https://iam.amazonaws.com".into());
        let mut params = params.to_vec();
        params.push(("Version", "2010-05-08"));
        let (status, reply) = self.query("iam", "us-east-1", &base, &params).await?;
        if status != 200 {
            return Err(failure(what, status, &reply));
        }
        Ok(reply)
    }

    /// The stack, or None when there is none.
    async fn describe(&self, name: &str) -> Result<Option<Stack>> {
        let (status, reply) = self
            .cloudformation(&[("Action", "DescribeStacks"), ("StackName", name)])
            .await?;
        if status == 400
            && aws::xml_text(&reply, "Message").is_some_and(|m| m.contains("does not exist"))
        {
            return Ok(None);
        }
        if status != 200 {
            return Err(failure(&format!("DescribeStacks {name}"), status, &reply));
        }
        let outputs = match (reply.find("<Outputs>"), reply.find("</Outputs>")) {
            (Some(start), Some(end)) if start < end => {
                let section = &reply[start..end];
                aws::xml_texts(section, "OutputKey")
                    .into_iter()
                    .zip(aws::xml_texts(section, "OutputValue"))
                    .collect()
            }
            _ => BTreeMap::new(),
        };
        Ok(Some(Stack {
            status: aws::xml_text(&reply, "StackStatus")
                .ok_or_else(|| anyhow!("DescribeStacks {name} gave no status"))?,
            reason: aws::xml_text(&reply, "StackStatusReason").unwrap_or_default(),
            outputs,
        }))
    }

    /// Wait until the stack has no change in progress; its outputs after a create or update.
    async fn settle(&self, name: &str) -> Result<BTreeMap<String, String>> {
        let start = Instant::now();
        let mut shown = String::new();
        loop {
            let Some(stack) = self.describe(name).await? else {
                bail!("CloudFormation deleted the stack {name} after a failed create; its events say why");
            };
            if stack.status != shown {
                eprintln!("layr-desk: {name}: {}", stack.status);
                shown = stack.status.clone();
            }
            match stack.status.as_str() {
                "CREATE_COMPLETE" | "UPDATE_COMPLETE" => return Ok(stack.outputs),
                status if status.ends_with("_IN_PROGRESS") => {}
                status => bail!("the stack {name} is {status}: {}", stack.reason),
            }
            if start.elapsed() > STACK_WAIT {
                bail!("the stack {name} takes more than an hour");
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    /// Create or update the vault stack and give its outputs.
    async fn deploy_stack(
        &self,
        name: &str,
        parameters: &[(&str, &str)],
    ) -> Result<BTreeMap<String, String>> {
        let existing = self.describe(name).await?;
        if existing
            .as_ref()
            .is_some_and(|s| s.status.ends_with("_IN_PROGRESS"))
        {
            self.settle(name).await?;
        }
        let keys: Vec<(String, String)> = parameters
            .iter()
            .enumerate()
            .flat_map(|(i, (key, value))| {
                [
                    (
                        format!("Parameters.member.{}.ParameterKey", i + 1),
                        key.to_string(),
                    ),
                    (
                        format!("Parameters.member.{}.ParameterValue", i + 1),
                        value.to_string(),
                    ),
                ]
            })
            .collect();
        let action = if existing.is_some() {
            "UpdateStack"
        } else {
            "CreateStack"
        };
        let mut params: Vec<(&str, &str)> = vec![
            ("Action", action),
            ("StackName", name),
            ("TemplateBody", TEMPLATE),
            ("Capabilities.member.1", "CAPABILITY_IAM"),
            ("Capabilities.member.2", "CAPABILITY_NAMED_IAM"),
        ];
        params.extend(keys.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        if existing.is_none() {
            params.push(("OnFailure", "DELETE"));
        }
        let (status, reply) = self.cloudformation(&params).await?;
        if status == 400
            && aws::xml_text(&reply, "Message")
                .is_some_and(|m| m.contains("No updates are to be performed"))
        {
            return Ok(self
                .describe(name)
                .await?
                .ok_or_else(|| anyhow!("the stack {name} is gone"))?
                .outputs);
        }
        if status != 200 {
            return Err(failure(&format!("{action} {name}"), status, &reply));
        }
        eprintln!(
            "layr-desk: {} {name}",
            if existing.is_some() {
                "updating"
            } else {
                "creating"
            }
        );
        self.settle(name).await
    }

    async fn access_keys(&self, user: &str) -> Result<Vec<String>> {
        let reply = self
            .iam(
                &format!("ListAccessKeys {user}"),
                &[("Action", "ListAccessKeys"), ("UserName", user)],
            )
            .await?;
        Ok(aws::xml_texts(&reply, "AccessKeyId"))
    }

    async fn delete_access_key(&self, user: &str, id: &str) -> Result<()> {
        self.iam(
            &format!("DeleteAccessKey {user}"),
            &[
                ("Action", "DeleteAccessKey"),
                ("UserName", user),
                ("AccessKeyId", id),
            ],
        )
        .await
        .map(drop)
    }
}

/// The highest seq with a tree in the vault, 0 for none.
async fn highest_seq(bucket: &S3, prefix: &str) -> Result<u64> {
    let keys = bucket.list(&format!("{prefix}/states/")).await?;
    Ok(keys
        .iter()
        .filter_map(|key| {
            let name = key.rsplit('/').next()?;
            let parts: Vec<&str> = name.split('.').collect();
            match parts.as_slice() {
                [seq, attempt, "tree", "age"]
                    if seq.len() == 10
                        && attempt.len() == 8
                        && attempt.chars().all(|c| c.is_ascii_hexdigit()) =>
                {
                    seq.parse().ok()
                }
                _ => None,
            }
        })
        .max()
        .unwrap_or(0))
}

fn write_json(path: PathBuf, value: &Value) -> Result<()> {
    std::fs::write(&path, serde_json::to_string_pretty(value)? + "\n")
        .with_context(|| path.display().to_string())
}

pub async fn deploy(args: Deploy) -> Result<()> {
    check_prefix(&args.project)?;
    let settings = settings()?;
    let keys = settings.join(&args.project);
    let read = |name: &str| {
        std::fs::read(keys.join(name)).with_context(|| {
            format!(
                "{}: make the vault keys first (layr-desk keys --project {})",
                keys.join(name).display(),
                args.project
            )
        })
    };
    let recipient = String::from_utf8(read("recipient.txt")?)?
        .trim()
        .to_string();
    let paper_slot = read("paper.age")?;
    let gateway: Gateway = serde_json::from_slice(
        &std::fs::read(settings.join("gateway.json"))
            .context("no gateway yet (gateway.json); run vault-gateway first")?,
    )?;
    let host = match &gateway.host {
        Some(host) => host.clone(),
        None => tailnet_host()?,
    };
    let profile = Profile::load(&args.profile)?;
    let http = reqwest::Client::new();
    let account = Account::new(profile.credentials(&http).await?, &args.region)?;
    let stack = format!("studio-vault-{}", args.project);

    // 1. The vault stack.
    let roles = std::iter::once(gateway.role.as_str())
        .chain(args.also_gateway.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(",");
    let outputs = account
        .deploy_stack(
            &stack,
            &[("Prefix", &args.project), ("GatewayRoleArns", &roles)],
        )
        .await?;
    let output = |name: &str| {
        outputs
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow!("the stack {stack} has no output {name}"))
    };
    let (bucket_name, writer_user, reader_role) = (
        output("BucketName")?,
        output("WriterUserName")?,
        output("ReaderRoleArn")?,
    );

    // 2. The public vault record and the paper slot. A run without a new writer key keeps the
    // writer key and signedFrom of an earlier run.
    let bucket = account.bucket(&bucket_name, &args.region)?;
    let record_key = format!("{}/vault.json", args.project);
    let earlier: Option<Value> = bucket
        .get_text(&record_key)
        .await?
        .map(|text| serde_json::from_str(&text))
        .transpose()
        .context("vault.json")?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let mut record = json!({
        "format": 1,
        "recipient": recipient,
        "created": earlier.as_ref().and_then(|e| e["created"].as_u64()).unwrap_or(now),
    });
    if let Some(earlier) = earlier.as_ref().filter(|e| e["writer"].is_string()) {
        record["writer"] = earlier["writer"].clone();
        record["signedFrom"] = earlier["signedFrom"].clone();
    }
    bucket
        .put(
            &record_key,
            serde_json::to_vec(&record)?,
            "application/json",
        )
        .await?;
    bucket
        .put_new(&format!("{}/slots/paper.age", args.project), paper_slot)
        .await?;

    // 3. The gateway serves the vault through its reader role; it reads this within 5 minutes.
    let gateway_account = if gateway.profile == args.profile {
        Account::new(account.credentials.clone(), &gateway.region)?
    } else {
        let profile = Profile::load(&gateway.profile)?;
        Account::new(profile.credentials(&http).await?, &gateway.region)?
    };
    gateway_account
        .bucket(&gateway.artifacts, &gateway.region)?
        .put(
            &format!("gateway/projects/{}.json", args.project),
            serde_json::to_vec(&json!({
                "bucket": bucket_name,
                "region": args.region,
                "role": reader_role,
                "recipient": recipient,
            }))?,
            "application/json",
        )
        .await?;

    // 4. A write-only key and a signing key for the writer, passed through a pipe.
    if let Some(command) = &args.writer {
        let listed = account.access_keys(&writer_user).await?;
        if listed.is_empty() || args.new_writer_key {
            let reply = account
                .iam(
                    &format!("CreateAccessKey {writer_user}"),
                    &[("Action", "CreateAccessKey"), ("UserName", &writer_user)],
                )
                .await?;
            let key_id = aws::xml_text(&reply, "AccessKeyId")
                .ok_or_else(|| anyhow!("CreateAccessKey gave no key"))?;
            let secret = aws::xml_text(&reply, "SecretAccessKey")
                .ok_or_else(|| anyhow!("CreateAccessKey gave no secret"))?;
            // The writer signs each tree with this Ed25519 key; the seed exists only in this
            // process and in the pipe.
            let mut seed = [0u8; 32];
            getrandom::getrandom(&mut seed)?;
            let signing_key = B64.encode(seed);
            let writer_key = Signer::from_base64(&signing_key)?.public();
            let configuration = json!({
                "bucket": bucket_name,
                "region": args.region,
                "prefix": args.project,
                "recipient": recipient,
                "site": format!("https://{host}/"),
                "accessKeyId": key_id,
                "secretAccessKey": secret,
                "signingKey": signing_key,
            });
            // The writer's output goes to stderr, so stdout keeps only this command's summary.
            let taken = (|| -> Result<bool> {
                let mut child = Command::new("/bin/sh")
                    .args(["-c", command])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::from(std::io::stderr()))
                    .spawn()
                    .context("the writer command")?;
                let mut stdin = child.stdin.take().expect("a piped stdin");
                let written = stdin.write_all(configuration.to_string().as_bytes());
                drop(stdin);
                let status = child.wait()?;
                Ok(written.is_ok() && status.success())
            })();
            if !matches!(taken, Ok(true)) {
                account.delete_access_key(&writer_user, &key_id).await?;
                bail!(
                    "the writer command did not take the key; the key is deleted again{}",
                    taken.err().map(|e| format!(" ({e:#})")).unwrap_or_default()
                );
            }
            for old in &listed {
                account.delete_access_key(&writer_user, old).await?;
            }
            // Signatures start after the highest state in the bucket now. The order leaves no
            // gap: the writer command has returned, so the writer holds the signing key, and
            // every state that this listing does not show comes from a push with the key. A
            // listing before the command could miss a state that an unsigned push adds in
            // between, and vault.json before the command would ask for signatures on states that
            // the writer still pushes without a key; readers would refuse both. This needs a
            // writer command that returns only after a push with the old configuration has ended.
            let signed = async {
                record["writer"] = json!(writer_key);
                record["signedFrom"] = json!(highest_seq(&bucket, &args.project).await? + 1);
                bucket
                    .put(
                        &record_key,
                        serde_json::to_vec(&record)?,
                        "application/json",
                    )
                    .await
            };
            signed.await.map_err(|e| {
                anyhow!("the writer signs with its new key, but vault.json does not name it ({e:#}); run again with --new-writer-key")
            })?;
        }
    }

    // Public facts for another person's gateway (vault-register), and for this Mac.
    let site = format!("https://{host}/{}/", args.project);
    write_json(
        keys.join("facts.json"),
        &json!({
            "prefix": args.project,
            "bucket": bucket_name,
            "region": args.region,
            "role": reader_role,
            "recipient": recipient,
        }),
    )?;
    write_json(
        keys.join("cloud.json"),
        &json!({
            "profile": args.profile,
            "region": args.region,
            "bucket": bucket_name,
            "site": site,
            "stack": stack,
        }),
    )?;
    println!(
        "{}",
        json!({"stack": stack, "site": site, "bucket": bucket_name})
    );
    Ok(())
}
