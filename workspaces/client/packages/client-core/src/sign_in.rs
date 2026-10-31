//! A native gesture reserves one exact sign-in while server approval completes.
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
use studio_client_domain::json_object::{LenientField, optional_field};

/// Maximum time between the native click and use of its sign-in intent. Unit: seconds.
const INTENT_LIFETIME: Duration = Duration::from_secs(60);
/// Maximum number of live sign-in intents in this native process. Unit: intents.
const INTENT_LIMIT: usize = 16;
/// Maximum length of a sign-in card, owner, window, or preparation identity. Unit: UTF-8 bytes.
const IDENTITY_MAX_BYTES: usize = 200;
const REFUSED: &str =
    "Use the sign-in button again. This approval is unavailable or does not match.";

// Each scalar is projected independently so malformed fields retain access-time errors.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SignInFields {
    #[serde(default, deserialize_with = "optional_field")]
    request_id: Option<String>,
    #[serde(default, deserialize_with = "optional_field")]
    url: Option<String>,
    #[serde(default, deserialize_with = "optional_field")]
    preparation: Option<String>,
}
#[derive(Default, Deserialize)]
struct SignInEnvelope {
    #[serde(default, rename = "signInOwner", deserialize_with = "owner")]
    owner: Owner,
    #[serde(default, rename = "_nativeWindow", deserialize_with = "optional_field")]
    window: Option<String>,
}
#[derive(Default)]
enum Owner {
    #[default]
    Local,
    Named(String),
    Invalid,
}
fn owner<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Owner, D::Error> {
    Ok(match Option::<LenientField<String>>::deserialize(d)? {
        None => Owner::Local,
        Some(LenientField::Value(value)) => Owner::Named(value),
        Some(LenientField::Other(_)) => Owner::Invalid,
    })
}
struct SignInRequest {
    envelope: SignInEnvelope,
    fields: SignInFields,
}
impl SignInRequest {
    fn parse(params: &Value) -> Self {
        let envelope = if params.is_object() {
            SignInEnvelope::deserialize(params).unwrap_or_default()
        } else {
            SignInEnvelope::default()
        };
        let value = &params["value"];
        let fields = if value.is_object() {
            SignInFields::deserialize(value).unwrap_or_default()
        } else {
            SignInFields::default()
        };
        Self { envelope, fields }
    }
    fn context(&self) -> Result<Context, String> {
        let owner = match &self.envelope.owner {
            Owner::Local => "local",
            Owner::Named(value) => identity(Some(value))?,
            Owner::Invalid => return Err(REFUSED.into()),
        };
        Ok(Context {
            request: identity(self.fields.request_id.as_deref())?.into(),
            owner: owner.into(),
            window: identity(self.envelope.window.as_deref())?.into(),
        })
    }
    fn scope(&self) -> Result<Scope, String> {
        let context = self.context()?;
        let url = self
            .fields
            .url
            .as_ref()
            .map_or(Value::Null, |url| Value::String(url.clone()));
        Ok(Scope {
            context,
            url: crate::desktop::external_url(&url)?,
        })
    }
    fn preparation(&self) -> Result<&str, String> {
        identity(self.fields.preparation.as_deref())
    }
}
#[derive(Clone, PartialEq, Eq)]
struct Context {
    request: String,
    owner: String,
    window: String,
}
fn identity(value: Option<&str>) -> Result<&str, String> {
    value
        .filter(|s| !s.is_empty() && s.len() <= IDENTITY_MAX_BYTES && !s.contains('\0'))
        .ok_or_else(|| REFUSED.into())
}
#[derive(Clone, PartialEq, Eq)]
struct Scope {
    context: Context,
    url: String,
}
struct Intent {
    scope: Scope,
    expires: Instant,
}
#[derive(Default)]
struct Intents {
    items: HashMap<String, Intent>,
}
impl Intents {
    fn prepare(&mut self, scope: Scope, id: String, now: Instant) -> Result<Value, String> {
        self.items.retain(|_, intent| intent.expires > now);
        if self.items.len() >= INTENT_LIMIT || self.items.contains_key(&id) {
            return Err("Too many sign-in approvals are waiting.".into());
        }
        self.items.insert(
            id.clone(),
            Intent {
                scope,
                expires: now + INTENT_LIFETIME,
            },
        );
        Ok(Value::String(id))
    }
    fn consume(&mut self, scope: &Scope, id: &str, now: Instant) -> Result<(), String> {
        self.items.retain(|_, intent| intent.expires > now);
        if !self
            .items
            .get(id)
            .is_some_and(|intent| intent.scope == *scope)
        {
            return Err(REFUSED.into());
        }
        self.items.remove(id);
        Ok(())
    }
    fn cancel(&mut self, context: &Context, id: &str, now: Instant) -> bool {
        self.items.retain(|_, intent| intent.expires > now);
        if self
            .items
            .get(id)
            .is_some_and(|intent| intent.scope.context == *context)
        {
            self.items.remove(id);
            true
        } else {
            false
        }
    }
}
fn intents() -> &'static Mutex<Intents> {
    static VALUE: OnceLock<Mutex<Intents>> = OnceLock::new();
    VALUE.get_or_init(Mutex::default)
}

/// The caller consumes the native gesture before it creates an intent.
pub(crate) fn prepare(params: &Value, now: Instant) -> Result<Value, String> {
    let request = SignInRequest::parse(params);
    let scope = request.scope()?;
    let id = crate::server_keys::random_id()?;
    intents()
        .lock()
        .map_err(|_| REFUSED)?
        .prepare(scope, id, now)
}

/// Consume before any forward request or browser action. A later action failure cannot reuse it.
/// A scope mismatch preserves the original intent for its authorized caller.
pub(crate) fn consume(params: &Value, now: Instant) -> Result<(), String> {
    let request = SignInRequest::parse(params);
    let scope = request.scope()?;
    let id = request.preparation()?;
    intents()
        .lock()
        .map_err(|_| REFUSED)?
        .consume(&scope, id, now)
}

pub(crate) fn cancel(params: &Value, now: Instant) -> Result<Value, String> {
    let request = SignInRequest::parse(params);
    let context = request.context()?;
    let id = request.preparation()?;
    Ok(Value::Bool(
        intents()
            .lock()
            .map_err(|_| REFUSED)?
            .cancel(&context, id, now),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn params() -> Value {
        json!({"_nativeWindow":"main", "value":{"requestId":"card-1", "url":"https://example.com/authorize?state=one"}})
    }
    fn scope() -> Scope {
        SignInRequest::parse(&params()).scope().unwrap()
    }
    #[test]
    fn intent_survives_approval_delay_and_expires_at_the_native_deadline() {
        let now = Instant::now();
        let mut intents = Intents::default();
        intents.prepare(scope(), "first".into(), now).unwrap();
        assert!(
            intents
                .consume(&scope(), "first", now + Duration::from_secs(2))
                .is_ok()
        );
        assert!(
            intents
                .consume(&scope(), "first", now + Duration::from_secs(2))
                .is_err()
        );
        intents.prepare(scope(), "expired".into(), now).unwrap();
        assert!(
            intents
                .consume(&scope(), "expired", now + INTENT_LIFETIME)
                .is_err()
        );
        intents.prepare(scope(), "last".into(), now).unwrap();
        assert!(
            intents
                .consume(
                    &scope(),
                    "last",
                    now + INTENT_LIFETIME - Duration::from_nanos(1)
                )
                .is_ok()
        );
    }
    #[test]
    fn intent_requires_the_exact_url_card_owner_and_native_window() {
        let now = Instant::now();
        let mut intents = Intents::default();
        intents.prepare(scope(), "intent".into(), now).unwrap();
        for (path, replacement) in [
            (
                vec!["value", "url"],
                "https://example.com/authorize?state=two",
            ),
            (vec!["value", "requestId"], "card-2"),
            (vec!["signInOwner"], "remote"),
            (vec!["_nativeWindow"], "other"),
        ] {
            let mut wrong = params();
            let mut target = &mut wrong;
            for key in &path[..path.len() - 1] {
                target = &mut target[*key];
            }
            target[path[path.len() - 1]] = json!(replacement);
            assert!(
                intents
                    .consume(
                        &SignInRequest::parse(&wrong).scope().unwrap(),
                        "intent",
                        now
                    )
                    .is_err()
            );
        }
        assert!(intents.consume(&scope(), "forged", now).is_err());
        let mut normalized = params();
        normalized["signInOwner"] = json!("local");
        normalized["value"]["url"] = json!("HTTPS://EXAMPLE.COM:443/authorize?state=one");
        assert!(
            intents
                .consume(
                    &SignInRequest::parse(&normalized).scope().unwrap(),
                    "intent",
                    now
                )
                .is_ok()
        );
    }
    #[test]
    fn cancel_requires_the_card_owner_and_window_and_prevents_open() {
        let now = Instant::now();
        let mut intents = Intents::default();
        intents.prepare(scope(), "intent".into(), now).unwrap();
        for wrong in [
            Context {
                request: "other".into(),
                ..scope().context
            },
            Context {
                owner: "remote".into(),
                ..scope().context
            },
            Context {
                window: "other".into(),
                ..scope().context
            },
        ] {
            assert!(!intents.cancel(&wrong, "intent", now));
        }
        assert!(intents.cancel(&scope().context, "intent", now));
        assert!(!intents.cancel(&scope().context, "intent", now));
        assert!(intents.consume(&scope(), "intent", now).is_err());
    }
    #[test]
    fn malformed_scope_never_creates_an_intent() {
        for (key, bad) in [
            ("requestId", json!("")),
            ("requestId", json!(1)),
            ("url", json!("https://user:secret@example.com")),
            ("url", json!("file:///not-a-sign-in-page")),
        ] {
            let mut invalid = params();
            invalid["value"][key] = bad;
            assert!(SignInRequest::parse(&invalid).scope().is_err());
        }
        for key in ["signInOwner", "_nativeWindow"] {
            let mut invalid = params();
            invalid[key] = json!("");
            assert!(SignInRequest::parse(&invalid).scope().is_err());
            invalid[key] = json!(7);
            assert!(SignInRequest::parse(&invalid).scope().is_err());
        }
        let mut invalid = params();
        invalid.as_object_mut().unwrap().remove("_nativeWindow");
        assert!(SignInRequest::parse(&invalid).scope().is_err());
    }
    #[test]
    fn owner_null_is_local_and_other_non_text_values_are_refused() {
        let mut value = params();
        value["signInOwner"] = Value::Null;
        assert_eq!(
            SignInRequest::parse(&value).context().unwrap().owner,
            "local"
        );
        value["signInOwner"] = json!("remote");
        assert_eq!(
            SignInRequest::parse(&value).context().unwrap().owner,
            "remote"
        );
        for owner in [json!(false), json!(0), json!([]), json!({})] {
            value["signInOwner"] = owner;
            assert!(SignInRequest::parse(&value).context().is_err());
        }
    }
    #[test]
    fn native_count_limit_reclaims_expired_intents() {
        let now = Instant::now();
        let mut intents = Intents::default();
        for index in 0..INTENT_LIMIT {
            intents.prepare(scope(), index.to_string(), now).unwrap();
        }
        assert!(intents.prepare(scope(), "overflow".into(), now).is_err());
        assert!(
            intents
                .prepare(scope(), "after-expiry".into(), now + INTENT_LIFETIME)
                .is_ok()
        );
        assert_eq!(intents.items.len(), 1);
    }

    #[tokio::test]
    async fn desktop_intent_preserves_mismatch_and_checks_url_before_preparation() {
        use crate::protocol::Request;
        let mut value = params();
        value["value"]["requestId"] = json!("sign-in-lifecycle-fixture");
        let preparation = prepare(&value, Instant::now()).unwrap();
        value["value"]["preparation"] = preparation;
        let request = |value: Value| Request {
            id: "sign-in-lifecycle-fixture".into(),
            method: crate::desktop::SIGN_IN_OPEN_METHOD.into(),
            params: value,
        };
        let mut invalid = value.clone();
        invalid["value"]["url"] = json!("file:///not-supported");
        invalid["value"]["preparation"] = json!(null);
        let response = crate::desktop::dispatch(request(invalid), None).await;
        assert_eq!(
            response.error.unwrap().message,
            "Only HTTP and HTTPS links without credentials are supported."
        );
        let mut wrong = value.clone();
        wrong["_nativeWindow"] = json!("elsewhere");
        assert!(
            crate::desktop::dispatch(request(wrong), None)
                .await
                .error
                .is_some()
        );
        value["signInOwner"] = Value::Null;
        let cancel = crate::desktop::dispatch(
            Request {
                id: "cancel-fixture".into(),
                method: crate::desktop::SIGN_IN_CANCEL_METHOD.into(),
                params: value,
            },
            None,
        )
        .await;
        assert_eq!(cancel.result, Some(Value::Bool(true)));
    }
}
