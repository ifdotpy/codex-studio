//! Web Push of sign-in alerts (docs/web-push.md): the server key, the encryption (RFC 8291,
//! aes128gcm), the VAPID authorization (RFC 8292), the subscription store, and the one push
//! per due sign-in card. No test sends a real push: they use a fake transport and a fake
//! push endpoint on the loopback.

use super::*;
use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;

fn base64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

fn unbase64url(text: &str) -> Vec<u8> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text)
        .expect("base64url")
}

/// The phone side of one subscription: its ECDH key and its authentication secret.
struct Phone {
    private: ring::agreement::EphemeralPrivateKey,
    public: Vec<u8>,
    auth: [u8; 16],
}

impl Phone {
    fn new() -> Self {
        let rng = ring::rand::SystemRandom::new();
        let private =
            ring::agreement::EphemeralPrivateKey::generate(&ring::agreement::ECDH_P256, &rng)
                .expect("phone key");
        let public = private
            .compute_public_key()
            .expect("phone public key")
            .as_ref()
            .to_vec();
        let mut auth = [0u8; 16];
        ring::rand::SecureRandom::fill(&rng, &mut auth).expect("auth secret");
        Self {
            private,
            public,
            auth,
        }
    }

    fn subscription(&self, endpoint: &str) -> NewSubscription {
        NewSubscription {
            endpoint: endpoint.into(),
            p256dh: base64url(&self.public),
            auth: base64url(&self.auth),
            device: "Phone".into(),
        }
    }

    /// Decrypt one aes128gcm body as a browser does (RFC 8291 section 3.4).
    fn decrypt(self, body: &[u8]) -> Result<Vec<u8>, &'static str> {
        let salt = &body[..16];
        let record = u32::from_be_bytes(body[16..20].try_into().expect("record size"));
        assert_eq!(record, RECORD_SIZE);
        let id_length = body[20] as usize;
        assert_eq!(id_length, 65);
        let server = &body[21..21 + id_length];
        let mut ciphertext = body[21 + id_length..].to_vec();
        let secret = ring::agreement::agree_ephemeral(
            self.private,
            &ring::agreement::UnparsedPublicKey::new(&ring::agreement::ECDH_P256, server),
            |shared| shared.to_vec(),
        )
        .map_err(|_| "ecdh")?;
        let mut info = b"WebPush: info\0".to_vec();
        info.extend_from_slice(&self.public);
        info.extend_from_slice(server);
        let ikm = expand(&extract(&self.auth, &secret), &info, 32);
        let prk = extract(salt, &ikm);
        let key = expand(&prk, b"Content-Encoding: aes128gcm\0", 16);
        let nonce = expand(&prk, b"Content-Encoding: nonce\0", 12);
        let key = ring::aead::LessSafeKey::new(
            ring::aead::UnboundKey::new(&ring::aead::AES_128_GCM, &key).expect("key"),
        );
        let plain = key
            .open_in_place(
                ring::aead::Nonce::try_assume_unique_for_key(&nonce).expect("nonce"),
                ring::aead::Aad::empty(),
                &mut ciphertext,
            )
            .map_err(|_| "authentication")?;
        let end = plain
            .iter()
            .rposition(|byte| *byte != 0)
            .expect("delimiter");
        assert_eq!(plain[end], 2, "the last record ends with the delimiter 2");
        Ok(plain[..end].to_vec())
    }
}

struct Length(usize);
impl ring::hkdf::KeyType for Length {
    fn len(&self) -> usize {
        self.0
    }
}

fn extract(salt: &[u8], ikm: &[u8]) -> ring::hkdf::Prk {
    ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, salt).extract(ikm)
}

fn expand(prk: &ring::hkdf::Prk, info: &[u8], length: usize) -> Vec<u8> {
    let mut out = vec![0u8; length];
    prk.expand(&[info], Length(length))
        .expect("expand")
        .fill(&mut out)
        .expect("fill");
    out
}

fn mode(path: &std::path::Path) -> u32 {
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

#[test]
fn the_server_key_is_owner_only_and_stays_the_same() {
    let state = tempfile::tempdir().expect("state");
    let first = ServerKey::load_or_create(state.path()).expect("key");
    let folder = state.path().join(FOLDER);
    assert_eq!(mode(&folder), 0o700);
    assert_eq!(mode(&folder.join(KEY_FILE)), 0o600);
    let public = unbase64url(&first.public_key());
    assert_eq!(
        (public.len(), public[0]),
        (65, 4),
        "an uncompressed P-256 point"
    );
    let again = ServerKey::load_or_create(state.path()).expect("key");
    assert_eq!(again.public_key(), first.public_key());
}

#[test]
fn a_payload_is_encrypted_for_the_subscription_and_only_its_keys_read_it() {
    let payload = br#"{"title":"Sign-in needed","body":"id.example.com for Main"}"#;
    let phone = Phone::new();
    let body = encrypt(payload, &phone.public, &phone.auth).expect("encrypt");
    assert_eq!(phone.decrypt(&body).expect("decrypt"), payload);
    // Another secret cannot read it.
    let phone = Phone::new();
    let other = Phone::new();
    let body = encrypt(payload, &phone.public, &other.auth).expect("encrypt");
    assert_eq!(phone.decrypt(&body), Err("authentication"));
}

#[test]
fn the_vapid_authorization_signs_the_push_origin() {
    let state = tempfile::tempdir().expect("state");
    let key = ServerKey::load_or_create(state.path()).expect("key");
    let header = key
        .authorization(
            "https://push.example.net/send/abc",
            "https://studio.example.org",
            1_000,
        )
        .expect("authorization");
    let (token, public) = header
        .strip_prefix("vapid t=")
        .and_then(|rest| rest.split_once(", k="))
        .expect("vapid t=..., k=...");
    assert_eq!(public, key.public_key());
    let parts: Vec<&str> = token.split('.').collect();
    assert_eq!(parts.len(), 3);
    let head: Value = serde_json::from_slice(&unbase64url(parts[0])).expect("header");
    assert_eq!(head, json!({"typ": "JWT", "alg": "ES256"}));
    let claims: Value = serde_json::from_slice(&unbase64url(parts[1])).expect("claims");
    assert_eq!(
        claims,
        json!({"aud": "https://push.example.net", "exp": 1_000 + VAPID_TOKEN_SECONDS,
               "sub": "https://studio.example.org"})
    );
    ring::signature::UnparsedPublicKey::new(
        &ring::signature::ECDSA_P256_SHA256_FIXED,
        unbase64url(public),
    )
    .verify(
        format!("{}.{}", parts[0], parts[1]).as_bytes(),
        &unbase64url(parts[2]),
    )
    .expect("the server key signed the token");
}

#[test]
fn only_https_push_endpoints_of_public_hosts_are_accepted() {
    for good in [
        "https://web.push.apple.com/abc",
        "https://fcm.googleapis.com/fcm/send/x",
    ] {
        assert!(push_endpoint(good, false).is_ok(), "{good}");
    }
    for bad in [
        "http://web.push.apple.com/abc",
        "https://user:secret@web.push.apple.com/abc",
        "https://127.0.0.1/abc",
        "https://localhost/abc",
        "https://10.0.0.5/abc",
        "https://[::1]/abc",
        "not a url",
    ] {
        assert!(push_endpoint(bad, false).is_err(), "{bad}");
    }
    // The fake endpoint of a test may use http on the loopback.
    assert!(push_endpoint("http://127.0.0.1:4242/push", true).is_ok());
}

#[test]
fn subscriptions_are_owner_only_one_per_endpoint_and_removable() {
    let state = tempfile::tempdir().expect("state");
    let store = Subscriptions::new(state.path());
    let phone = Phone::new();
    let first = store
        .add(
            phone.subscription("https://web.push.apple.com/a"),
            false,
            10.0,
        )
        .expect("add");
    let again = store
        .add(
            phone.subscription("https://web.push.apple.com/a"),
            false,
            11.0,
        )
        .expect("add");
    assert_eq!(again.id, first.id, "a device subscribes once per endpoint");
    assert_eq!(
        mode(&state.path().join(FOLDER).join(SUBSCRIPTIONS_FILE)),
        0o600
    );
    let other = Phone::new();
    store
        .add(
            other.subscription("https://web.push.apple.com/b"),
            false,
            12.0,
        )
        .expect("add");
    assert_eq!(store.list().expect("list").len(), 2);
    assert!(store.remove(&first.id).expect("remove"));
    assert!(!store.remove(&first.id).expect("remove"));
    assert_eq!(store.list().expect("list").len(), 1);
    // A key that is not a P-256 point, or a short secret, is refused.
    let mut bad = phone.subscription("https://web.push.apple.com/c");
    bad.auth = base64url(&[1, 2, 3]);
    assert!(store.add(bad, false, 13.0).is_err());
}

/// One push as the fake transport saw it: endpoint, headers and encrypted body.
type SentPush = (String, Vec<(String, String)>, Vec<u8>);

/// A transport that keeps every push and answers each endpoint with a fixed status.
struct FakeTransport {
    status: std::collections::BTreeMap<String, u16>,
    sent: Mutex<Vec<SentPush>>,
}

impl PushTransport for FakeTransport {
    fn post(
        &self,
        endpoint: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<u16, String> {
        self.sent.lock().expect("test step").push((
            endpoint.to_owned(),
            headers.to_vec(),
            body.to_vec(),
        ));
        Ok(*self.status.get(endpoint).unwrap_or(&201))
    }
}

fn card(created: f64, expires: f64) -> DueCard {
    DueCard {
        id: "card-1".into(),
        host: "id.example.com".into(),
        chat: "Main".into(),
        created_at: created,
        expires_at: Some(expires),
        waiting: false,
    }
}

#[test]
fn a_due_sign_in_card_is_pushed_once_and_a_gone_subscription_is_removed() {
    let state = tempfile::tempdir().expect("state");
    let store = Subscriptions::new(state.path());
    let key = ServerKey::load_or_create(state.path()).expect("key");
    let kept = Phone::new();
    let gone = Phone::new();
    store
        .add(
            kept.subscription("https://web.push.apple.com/kept"),
            false,
            1.0,
        )
        .expect("add");
    store
        .add(
            gone.subscription("https://web.push.apple.com/gone"),
            false,
            1.0,
        )
        .expect("add");
    let transport = FakeTransport {
        status: [("https://web.push.apple.com/gone".to_owned(), 410)].into(),
        sent: Mutex::new(Vec::new()),
    };
    let sender = Sender {
        state: state.path().to_path_buf(),
        key,
        subject: "https://studio.example.org".into(),
    };
    let cards = vec![card(1000.0, 1600.0)];
    // Within the grace the browser of the Mac may still finish the sign-in.
    sender.deliver(&transport, &cards, 1005.0).expect("deliver");
    assert!(transport.sent.lock().expect("test step").is_empty());
    sender.deliver(&transport, &cards, 1010.0).expect("deliver");
    let sent = std::mem::take(&mut *transport.sent.lock().expect("test step"));
    assert_eq!(sent.len(), 2);
    let (endpoint, headers, body) = sent
        .iter()
        .find(|(endpoint, _, _)| endpoint.ends_with("/kept"))
        .cloned()
        .expect("push to the kept device");
    assert_eq!(endpoint, "https://web.push.apple.com/kept");
    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    assert_eq!(header("Content-Encoding").as_deref(), Some("aes128gcm"));
    assert_eq!(header("TTL").as_deref(), Some("600"));
    assert!(header("Authorization").is_some_and(|value| value.starts_with("vapid t=")));
    let payload: Value =
        serde_json::from_slice(&kept.decrypt(&body).expect("decrypt")).expect("payload");
    assert_eq!(
        payload,
        json!({"title": "Sign-in needed", "body": "id.example.com for Main",
               "tag": "studio-sign-in:card-1"}),
        "the payload holds only the text of the alert"
    );
    let left: Vec<String> = store
        .list()
        .expect("list")
        .into_iter()
        .map(|row| row.endpoint)
        .collect();
    assert_eq!(
        left,
        ["https://web.push.apple.com/kept"],
        "a gone device is removed"
    );
    // One push per card.
    sender.deliver(&transport, &cards, 1100.0).expect("deliver");
    assert!(transport.sent.lock().expect("test step").is_empty());
    // One reminder before the card closes, not while a phone sign-in waits.
    let waiting = vec![DueCard {
        waiting: true,
        ..card(1000.0, 1600.0)
    }];
    sender
        .deliver(&transport, &waiting, 1300.0)
        .expect("deliver");
    assert!(transport.sent.lock().expect("test step").is_empty());
    sender.deliver(&transport, &cards, 1300.0).expect("deliver");
    assert_eq!(transport.sent.lock().expect("test step").len(), 1);
}

#[test]
fn the_https_transport_posts_to_a_fake_push_endpoint_on_the_loopback() {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("fake endpoint");
    let port = listener.local_addr().expect("address").port();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("push");
        let mut reader = BufReader::new(stream.try_clone().expect("stream"));
        let mut head = Vec::new();
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("line");
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().expect("length");
            }
            if line == "\r\n" {
                break;
            }
            head.push(line);
        }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).expect("body");
        let mut stream = stream;
        stream
            .write_all(b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .expect("reply");
        (head, body)
    });
    let transport = UreqTransport::for_test_loopback();
    let status = transport
        .post(
            &format!("http://127.0.0.1:{port}/push"),
            &[("TTL".into(), "600".into())],
            b"encrypted",
        )
        .expect("post");
    assert_eq!(status, 201);
    let (head, body) = server.join().expect("server");
    assert!(head[0].starts_with("POST /push "), "{head:?}");
    assert_eq!(body, b"encrypted");
}
