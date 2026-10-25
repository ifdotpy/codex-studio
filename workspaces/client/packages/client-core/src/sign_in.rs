//! A native gesture reserves one exact sign-in while server approval completes.
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

/// Maximum time between the native click and use of its sign-in intent. Unit: seconds.
const INTENT_LIFETIME: Duration = Duration::from_secs(60);
/// Maximum number of live sign-in intents in this native process. Unit: intents.
const INTENT_LIMIT: usize = 16;
/// Maximum length of a sign-in card, owner, window, or preparation identity. Unit: UTF-8 bytes.
const IDENTITY_MAX_BYTES: usize = 200;
const REFUSED: &str =
    "Use the sign-in button again. This approval is unavailable or does not match.";

#[derive(Clone, PartialEq, Eq)]
struct Context {
    request: String,
    owner: String,
    window: String,
}
impl Context {
    fn parse(params: &Value) -> Result<Self, String> {
        let owner = match params.get("signInOwner") {
            None | Some(Value::Null) => "local",
            Some(owner) => identity(owner)?,
        };
        Ok(Self {
            request: identity(&params["value"]["requestId"])?.into(),
            owner: owner.into(),
            // Only the native host supplies this provenance.
            window: identity(&params["_nativeWindow"])?.into(),
        })
    }
}
fn identity(value: &Value) -> Result<&str, String> {
    value
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= IDENTITY_MAX_BYTES && !s.contains('\0'))
        .ok_or_else(|| REFUSED.into())
}

#[derive(Clone, PartialEq, Eq)]
struct Scope {
    context: Context,
    url: String,
}
impl Scope {
    fn parse(params: &Value) -> Result<Self, String> {
        Ok(Self {
            context: Context::parse(params)?,
            url: crate::desktop::external_url(&params["value"]["url"])?,
        })
    }
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
    let scope = Scope::parse(params)?;
    let id = crate::server_keys::random_id()?;
    intents()
        .lock()
        .map_err(|_| REFUSED)?
        .prepare(scope, id, now)
}

/// Consume before any forward request or browser action. A later action failure cannot reuse it.
/// A scope mismatch preserves the original intent for its authorized caller.
pub(crate) fn consume(params: &Value, now: Instant) -> Result<(), String> {
    let scope = Scope::parse(params)?;
    let id = identity(&params["value"]["preparation"])?;
    intents()
        .lock()
        .map_err(|_| REFUSED)?
        .consume(&scope, id, now)
}

pub(crate) fn cancel(params: &Value, now: Instant) -> Result<Value, String> {
    let context = Context::parse(params)?;
    let id = identity(&params["value"]["preparation"])?;
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
        Scope::parse(&params()).unwrap()
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
                    .consume(&Scope::parse(&wrong).unwrap(), "intent", now)
                    .is_err()
            );
        }
        assert!(intents.consume(&scope(), "forged", now).is_err());
        let mut normalized = params();
        normalized["signInOwner"] = json!("local");
        normalized["value"]["url"] = json!("HTTPS://EXAMPLE.COM:443/authorize?state=one");
        assert!(
            intents
                .consume(&Scope::parse(&normalized).unwrap(), "intent", now)
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
            ("url", json!("file:///tmp/a")),
        ] {
            let mut invalid = params();
            invalid["value"][key] = bad;
            assert!(Scope::parse(&invalid).is_err());
        }
        for key in ["signInOwner", "_nativeWindow"] {
            let mut invalid = params();
            invalid[key] = json!("");
            assert!(Scope::parse(&invalid).is_err());
            invalid[key] = json!(7);
            assert!(Scope::parse(&invalid).is_err());
        }
        let mut invalid = params();
        invalid.as_object_mut().unwrap().remove("_nativeWindow");
        assert!(Scope::parse(&invalid).is_err());
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
}
