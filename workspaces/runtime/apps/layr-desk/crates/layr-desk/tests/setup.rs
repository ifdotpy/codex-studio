//! `layr-desk keys` and `layr-desk deploy` end to end against a fake AWS: an SSO profile whose
//! token has ended is renewed with its refresh token, the vault stack is created, vault.json, the
//! paper slot and the gateway registration are stored, and the writer gets a new access key and a
//! signing key through its command, after which the old key is deleted and vault.json names the
//! writer from the state after the highest one. A profile without a sign-in exits with 3.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, Response};
use percent_encoding::percent_decode_str;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Fake {
    /// Device-code polls before the person confirms.
    polls: u32,
    objects: BTreeMap<String, Vec<u8>>,
    stack: bool,
    keys: Vec<String>,
    deleted: Vec<String>,
    template: String,
    parameters: BTreeMap<String, String>,
}

type Shared = Arc<Mutex<Fake>>;

fn reply(status: u16, body: impl Into<Bytes>) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(body.into()));
    *response.status_mut() = hyper::StatusCode::from_u16(status).unwrap();
    response
}

fn decode(text: &str) -> String {
    percent_decode_str(text).decode_utf8().unwrap().into_owned()
}

fn form(text: &str) -> BTreeMap<String, String> {
    text.split('&')
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (decode(k), decode(v)))
        .collect()
}

fn error(code: &str, message: &str) -> Response<Full<Bytes>> {
    reply(
        400,
        format!("<ErrorResponse><Error><Code>{code}</Code><Message>{message}</Message></Error></ErrorResponse>"),
    )
}

async fn aws(fake: Shared, request: Request<hyper::body::Incoming>) -> Response<Full<Bytes>> {
    let (parts, body) = request.into_parts();
    let data = body.collect().await.unwrap().to_bytes().to_vec();
    let body = String::from_utf8_lossy(&data).into_owned();
    let path = decode(parts.uri.path());
    let mut fake = fake.lock().unwrap();
    // IAM Identity Center: the OIDC token service and the portal.
    if path == "/client/register" {
        let asked: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(asked["scopes"], json!(["sso:account:access"]));
        return reply(
            200,
            r#"{"clientId":"client","clientSecret":"client-secret","clientSecretExpiresAt":4070908800}"#,
        );
    }
    if path == "/device_authorization" {
        let asked: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(asked["startUrl"], "https://example.awsapps.com/start");
        return reply(
            200,
            r#"{"deviceCode":"device","userCode":"ABCD-EFGH","verificationUri":"https://device.example","verificationUriComplete":"https://device.example?code=ABCD-EFGH","expiresIn":60,"interval":1}"#,
        );
    }
    if path == "/token" && body.contains("device_code") {
        fake.polls += 1;
        if fake.polls < 2 {
            return reply(400, r#"{"error":"authorization_pending"}"#);
        }
        return reply(
            200,
            r#"{"accessToken":"fresh","expiresIn":3600,"refreshToken":"refresh-1"}"#,
        );
    }
    if path == "/token" {
        let asked: Value = serde_json::from_str(&body).unwrap();
        if asked["grantType"] != "refresh_token" || asked["refreshToken"] != "refresh-1" {
            return reply(400, r#"{"error":"invalid_grant"}"#);
        }
        return reply(
            200,
            r#"{"accessToken":"fresh","expiresIn":3600,"refreshToken":"refresh-2"}"#,
        );
    }
    if path == "/federation/credentials" {
        let bearer = parts.headers.get("x-amz-sso_bearer_token");
        if bearer.map(|v| v.as_bytes()) != Some(b"fresh") {
            return reply(401, r#"{"message":"Session token not found or invalid"}"#);
        }
        return reply(
            200,
            r#"{"roleCredentials":{"accessKeyId":"ASIAROLE","secretAccessKey":"role-secret","sessionToken":"role-token","expiration":0}}"#,
        );
    }
    let authorization = parts
        .headers
        .get("authorization")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    if !authorization.contains("Credential=ASIAROLE/") {
        return reply(403, "<Error><Code>InvalidClientTokenId</Code></Error>");
    }
    let service = authorization
        .split("/aws4_request")
        .next()
        .and_then(|s| s.rsplit('/').next())
        .unwrap_or_default()
        .to_string();
    match service.as_str() {
        "cloudformation" => {
            let asked = form(&body);
            match asked["Action"].as_str() {
                "DescribeStacks" if !fake.stack => {
                    error("ValidationError", "Stack with id studio-vault-demo does not exist")
                }
                "DescribeStacks" => reply(
                    200,
                    "<DescribeStacksResponse><DescribeStacksResult><Stacks><member>\
                     <StackName>studio-vault-demo</StackName><StackStatus>CREATE_COMPLETE</StackStatus>\
                     <Outputs><member><OutputKey>BucketName</OutputKey><OutputValue>vault-bucket</OutputValue></member>\
                     <member><OutputKey>WriterUserName</OutputKey><OutputValue>writer-user</OutputValue></member>\
                     <member><OutputKey>ReaderRoleArn</OutputKey><OutputValue>arn:aws:iam::1:role/reader</OutputValue></member>\
                     </Outputs></member></Stacks></DescribeStacksResult></DescribeStacksResponse>",
                ),
                "CreateStack" => {
                    assert_eq!(asked["OnFailure"], "DELETE");
                    fake.template = asked["TemplateBody"].clone();
                    for n in 1..=2 {
                        fake.parameters.insert(
                            asked[&format!("Parameters.member.{n}.ParameterKey")].clone(),
                            asked[&format!("Parameters.member.{n}.ParameterValue")].clone(),
                        );
                    }
                    fake.stack = true;
                    reply(200, "<CreateStackResponse/>")
                }
                "UpdateStack" => error("ValidationError", "No updates are to be performed."),
                other => panic!("CloudFormation {other}"),
            }
        }
        "iam" => {
            let asked = form(&body);
            assert_eq!(asked["UserName"], "writer-user");
            match asked["Action"].as_str() {
                "ListAccessKeys" => reply(
                    200,
                    format!(
                        "<ListAccessKeysResponse><ListAccessKeysResult><AccessKeyMetadata>{}</AccessKeyMetadata></ListAccessKeysResult></ListAccessKeysResponse>",
                        fake.keys.iter().map(|k| format!("<member><AccessKeyId>{k}</AccessKeyId></member>")).collect::<String>()
                    ),
                ),
                "CreateAccessKey" => {
                    fake.keys.push("AKIANEW".into());
                    reply(200, "<CreateAccessKeyResponse><CreateAccessKeyResult><AccessKey><AccessKeyId>AKIANEW</AccessKeyId><SecretAccessKey>new-secret</SecretAccessKey></AccessKey></CreateAccessKeyResult></CreateAccessKeyResponse>")
                }
                "DeleteAccessKey" => {
                    let id = asked["AccessKeyId"].clone();
                    fake.keys.retain(|k| *k != id);
                    fake.deleted.push(id);
                    reply(200, "<DeleteAccessKeyResponse/>")
                }
                other => panic!("IAM {other}"),
            }
        }
        "s3" => {
            let key = path.trim_start_matches('/').to_string();
            let query = form(parts.uri.query().unwrap_or(""));
            if parts.method == hyper::Method::PUT {
                let none_match = parts.headers.get("if-none-match").is_some();
                if none_match && fake.objects.contains_key(&key) {
                    return reply(412, "<Error><Code>PreconditionFailed</Code></Error>");
                }
                fake.objects.insert(key, data);
                reply(200, "")
            } else if query.get("list-type").map(String::as_str) == Some("2") {
                let prefix = format!("{}/{}", key.trim_end_matches('/'), query["prefix"]);
                let keys: String = fake
                    .objects
                    .keys()
                    .filter(|k| k.starts_with(&prefix))
                    .map(|k| {
                        format!(
                            "<Contents><Key>{}</Key></Contents>",
                            k.split_once('/').unwrap().1
                        )
                    })
                    .collect();
                reply(200, format!("<ListBucketResult>{keys}</ListBucketResult>"))
            } else {
                match fake.objects.get(&key) {
                    Some(data) => reply(200, data.clone()),
                    None => reply(404, "<Error><Code>NoSuchKey</Code></Error>"),
                }
            }
        }
        other => panic!("service {other}"),
    }
}

async fn listen(fake: Shared) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let fake = fake.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |request| {
                    let fake = fake.clone();
                    async move { Ok::<_, std::convert::Infallible>(aws(fake, request).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    url
}

/// A home with an SSO profile `p` (session `s`), the gateway settings, and no token yet.
struct Home {
    root: PathBuf,
    endpoint: String,
}

impl Home {
    fn new(name: &str, endpoint: String) -> Home {
        let root = std::env::temp_dir().join(format!("desk-setup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".aws/sso/cache")).unwrap();
        std::fs::create_dir_all(root.join(".config/studio-vault")).unwrap();
        std::fs::write(
            root.join(".aws/config"),
            "[profile p]\nsso_session = s\nsso_account_id = 111122223333\nsso_role_name = Admin\nregion = eu-central-1\n\
             [sso-session s]\nsso_start_url = https://example.awsapps.com/start\nsso_region = eu-central-1\nsso_registration_scopes = sso:account:access\n",
        )
        .unwrap();
        std::fs::write(
            root.join(".config/studio-vault/gateway.json"),
            json!({"role": "arn:aws:iam::1:role/gateway", "artifacts": "artifacts", "profile": "p",
                   "region": "eu-central-1", "host": "desk.test"})
            .to_string(),
        )
        .unwrap();
        Home { root, endpoint }
    }

    fn token_cache(&self) -> PathBuf {
        self.root
            .join(".aws/sso/cache")
            .join(format!("{}.json", hex::encode(Sha1::digest(b"s"))))
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_layr-desk"));
        command.args(args).env("HOME", &self.root);
        command.env_remove("AWS_CONFIG_FILE");
        command.env(
            "AWS_SHARED_CREDENTIALS_FILE",
            self.root.join("no-credentials"),
        );
        for service in ["SSO", "SSO_OIDC", "CLOUDFORMATION", "IAM", "S3"] {
            command.env(format!("AWS_ENDPOINT_URL_{service}"), &self.endpoint);
        }
        command.output().unwrap()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn ok(output: &Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn keys_then_deploy_make_the_vault_and_give_the_writer_its_keys() {
    let fake = Shared::default();
    let home = Home::new("deploy", listen(fake.clone()).await);
    // A token that has ended, with a refresh token of a registration that has not.
    std::fs::write(
        home.token_cache(),
        json!({"startUrl": "https://example.awsapps.com/start", "region": "eu-central-1",
               "accessToken": "old", "expiresAt": "2020-01-01T00:00:00Z", "clientId": "client",
               "clientSecret": "client-secret", "registrationExpiresAt": "2099-01-01T00:00:00Z",
               "refreshToken": "refresh-1"})
        .to_string(),
    )
    .unwrap();

    let shown = ok(&home.run(&["keys", "--project", "demo"]));
    // The numbered words, in their order: column by column, six rows.
    let mut numbered: Vec<(usize, String)> = Vec::new();
    for line in shown.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        for pair in parts.chunks(2) {
            if let [n, word] = pair {
                if let Some(n) = n.strip_suffix('.').and_then(|n| n.parse().ok()) {
                    numbered.push((n, word.to_string()));
                }
            }
        }
    }
    numbered.sort();
    assert_eq!(numbered.len(), 24, "{shown}");
    let words: Vec<String> = numbered.into_iter().map(|(_, w)| w).collect();
    let keys = home.root.join(".config/studio-vault/demo");
    let recipient = std::fs::read_to_string(keys.join("recipient.txt")).unwrap();
    // The words open the paper slot, and it holds the identity of the recipient.
    let paper = desk_core::keys::identity_from_paper(&words.join(" ")).unwrap();
    let vault = desk_core::keys::open_slot(&std::fs::read(keys.join("paper.age")).unwrap(), &paper)
        .unwrap();
    assert_eq!(
        desk_core::keys::recipient_of(&vault).unwrap(),
        recipient.trim()
    );
    let again = home.run(&["keys", "--project", "demo"]);
    assert!(
        !again.status.success(),
        "a second keys run must not replace the identity"
    );

    // Earlier unsigned states: signatures start after the highest one.
    {
        let mut fake = fake.lock().unwrap();
        fake.objects.insert(
            "vault-bucket/demo/states/0000000003.0123abcd.tree.age".into(),
            b"x".to_vec(),
        );
        fake.objects.insert(
            "vault-bucket/demo/states/0000000002.0123abcd.tree.age".into(),
            b"x".to_vec(),
        );
        fake.keys.push("AKIAOLD".into());
    }
    let configuration = home.root.join("writer.json");
    let writer = format!("cat > '{}'", configuration.display());
    let summary: Value = serde_json::from_str(&ok(&home.run(&[
        "deploy",
        "--project",
        "demo",
        "--profile",
        "p",
        "--writer",
        &writer,
        "--new-writer-key",
    ])))
    .unwrap();
    assert_eq!(summary["site"], "https://desk.test/demo/");
    assert_eq!(summary["bucket"], "vault-bucket");

    let state = fake.lock().unwrap();
    assert!(state.template.contains("AWS::S3::Bucket"));
    assert_eq!(state.parameters["Prefix"], "demo");
    assert_eq!(
        state.parameters["GatewayRoleArns"],
        "arn:aws:iam::1:role/gateway"
    );
    assert_eq!(
        state.objects["vault-bucket/demo/slots/paper.age"],
        std::fs::read(keys.join("paper.age")).unwrap()
    );
    let registration: Value =
        serde_json::from_slice(&state.objects["artifacts/gateway/projects/demo.json"]).unwrap();
    assert_eq!(registration["role"], "arn:aws:iam::1:role/reader");
    assert_eq!(registration["recipient"], recipient.trim());

    let given: Value =
        serde_json::from_str(&std::fs::read_to_string(&configuration).unwrap()).unwrap();
    assert_eq!(given["accessKeyId"], "AKIANEW");
    assert_eq!(given["secretAccessKey"], "new-secret");
    assert_eq!(given["site"], "https://desk.test/");
    assert_eq!(state.keys, ["AKIANEW"]);
    assert_eq!(state.deleted, ["AKIAOLD"]);
    let record: Value =
        serde_json::from_slice(&state.objects["vault-bucket/demo/vault.json"]).unwrap();
    let signer =
        desk_core::sign::Signer::from_base64(given["signingKey"].as_str().unwrap()).unwrap();
    assert_eq!(record["writer"], signer.public());
    assert_eq!(record["signedFrom"], 4);
    assert_eq!(record["recipient"], recipient.trim());

    // The renewed token is where the AWS CLI reads it.
    let cached: Value =
        serde_json::from_str(&std::fs::read_to_string(home.token_cache()).unwrap()).unwrap();
    assert_eq!(cached["accessToken"], "fresh");
    assert_eq!(cached["refreshToken"], "refresh-2");
    drop(state);

    // A later run without a new writer key runs no writer command and keeps the writer and its
    // signatures.
    let output = home.run(&[
        "deploy",
        "--project",
        "demo",
        "--profile",
        "p",
        "--writer",
        "exit 1",
    ]);
    ok(&output);
    let fake = fake.lock().unwrap();
    let kept: Value =
        serde_json::from_slice(&fake.objects["vault-bucket/demo/vault.json"]).unwrap();
    assert_eq!(kept, record);
    assert_eq!(fake.keys, ["AKIANEW"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_profile_without_a_sign_in_exits_with_3() {
    let fake = Shared::default();
    let home = Home::new("signin", listen(fake.clone()).await);
    ok(&home.run(&["keys", "--project", "demo"]));
    let output = home.run(&["deploy", "--project", "demo", "--profile", "p"]);
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("layr-desk aws-login --profile p"));
    let profiles: Value =
        serde_json::from_str(&ok(&home.run(&["aws-profiles", "--json"]))).unwrap();
    assert_eq!(profiles["profiles"][0]["name"], "p");
    assert_eq!(profiles["profiles"][0]["signedIn"], false);
    assert!(
        !fake.lock().unwrap().stack,
        "nothing is made without credentials"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn aws_login_shows_the_page_and_keeps_the_token_where_the_cli_reads_it() {
    let fake = Shared::default();
    let home = Home::new("login", listen(fake.clone()).await);
    let output = home.run(&["aws-login", "--profile", "p", "--json"]);
    let lines: Vec<Value> = ok(&output)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines[0]["page"], "https://device.example?code=ABCD-EFGH");
    assert_eq!(lines[0]["code"], "ABCD-EFGH");
    assert_eq!(lines[1]["signedIn"], true);
    assert_eq!(fake.lock().unwrap().polls, 2);
    let cached: Value =
        serde_json::from_str(&std::fs::read_to_string(home.token_cache()).unwrap()).unwrap();
    assert_eq!(cached["accessToken"], "fresh");
    assert_eq!(cached["refreshToken"], "refresh-1");
    assert_eq!(cached["registrationExpiresAt"], "2099-01-01T00:00:00Z");
    // The profile now gives credentials: deploy gets past the sign-in.
    let profiles: Value =
        serde_json::from_str(&ok(&home.run(&["aws-profiles", "--json"]))).unwrap();
    assert_eq!(profiles["profiles"][0]["signedIn"], true);
}
