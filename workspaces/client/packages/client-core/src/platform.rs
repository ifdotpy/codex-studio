//! Pure platform services used by the presentation adapters.
use crate::protocol::{Request, Response};
use serde_json::{Value, json};
use unicode_segmentation::UnicodeSegmentation;

fn execute(request: &Request) -> Result<Value, String> {
    let p = &request.params;
    match request.method.as_str() {
        "text.sentences" => {
            let text = p["text"].as_str().ok_or("Text is required.")?;
            let mut index = 0;
            Ok(Value::Array(
                text.split_sentence_bounds()
                    .map(|segment| {
                        let value = json!({"text":segment,"index":index});
                        index += segment.encode_utf16().count();
                        value
                    })
                    .collect(),
            ))
        }
        "locale.language_name" => {
            let code = p["code"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 128)
                .ok_or("A language code is required.")?;
            let locale = p["locale"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 128)
                .ok_or("A display locale is required.")?;
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            {
                use objc2_foundation::{NSLocale, NSString};
                Ok(objc2::rc::autoreleasepool(|_| {
                    let locale = NSLocale::localeWithLocaleIdentifier(&NSString::from_str(locale));
                    json!(
                        locale
                            .localizedStringForLocaleIdentifier(&NSString::from_str(code))
                            .to_string()
                    )
                }))
            }
            #[cfg(not(any(target_os = "macos", target_os = "ios")))]
            {
                let _ = (locale, code);
                Err("Native locale names require macOS.".into())
            }
        }
        "system.uuid" => {
            let mut bytes = [0u8; 16];
            if unsafe { libc::getentropy(bytes.as_mut_ptr().cast(), bytes.len()) } != 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            bytes[6] = (bytes[6] & 15) | 64;
            bytes[8] = (bytes[8] & 63) | 128;
            let hex = format!("{:032x}", u128::from_be_bytes(bytes));
            Ok(json!(format!(
                "{}-{}-{}-{}-{}",
                &hex[..8],
                &hex[8..12],
                &hex[12..16],
                &hex[16..20],
                &hex[20..]
            )))
        }
        "url.parse" => {
            let value = p["value"].as_str().ok_or("A URL is required.")?;
            let base = p["base"]
                .as_str()
                .map(reqwest::Url::parse)
                .transpose()
                .map_err(|e| e.to_string())?;
            let url = base
                .map(|base| base.join(value))
                .unwrap_or_else(|| reqwest::Url::parse(value))
                .map_err(|e| e.to_string())?;
            Ok(
                json!({"href":url.as_str(),"protocol":format!("{}:",url.scheme()),"origin":url.origin().ascii_serialization(),
                "hostname":url.host_str().unwrap_or(""),"host":match (url.host_str(),url.port()) {(Some(host),Some(port))=>format!("{host}:{port}"),(Some(host),None)=>host.into(),_=>String::new()},
                "port":url.port().map(|v|v.to_string()).unwrap_or_default(),"username":url.username(),"password":url.password().unwrap_or(""),
                "pathname":url.path(),"search":url.query().map(|v|format!("?{v}")).unwrap_or_default(),"hash":url.fragment().map(|v|format!("#{v}")).unwrap_or_default()}),
            )
        }
        "url.query_parse" => {
            let query = p["value"]
                .as_str()
                .ok_or("A query string is required.")?
                .trim_start_matches('?');
            let mut url = reqwest::Url::parse("http://localhost/").unwrap();
            url.set_query(Some(query));
            Ok(json!(url.query_pairs().collect::<Vec<_>>()))
        }
        "url.query_encode" => {
            let pairs: Vec<(String, String)> =
                serde_json::from_value(p["pairs"].clone()).map_err(|e| e.to_string())?;
            let mut url = reqwest::Url::parse("http://localhost/").unwrap();
            url.query_pairs_mut().extend_pairs(pairs);
            Ok(json!(url.query().unwrap_or("")))
        }
        _ => Err("Unknown platform command.".into()),
    }
}

pub fn dispatch(request: &Request) -> Response {
    match execute(request) {
        Ok(value) => Response::success(request.id.clone(), value),
        Err(error) => Response::failure(request.id.clone(), "platform_error", error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    #[test]
    fn native_language_names_use_the_requested_display_locale() {
        let name = |locale| {
            execute(&Request {
                id: "locale".into(),
                method: "locale.language_name".into(),
                params: json!({"code":"ru-RU","locale":locale}),
            })
            .unwrap()
            .as_str()
            .unwrap()
            .to_lowercase()
        };
        assert!(name("en-US").contains("russian"));
        assert!(name("ru-RU").contains("рус"));
    }
    #[test]
    fn sentences_keep_utf16_offsets_and_incomplete_text() {
        let value = execute(&Request {
            id: "test".into(),
            method: "text.sentences".into(),
            params: json!({"text":"🦀 Привет. Hello! Ещё"}),
        })
        .unwrap();
        assert_eq!(
            value,
            json!([{"text":"🦀 Привет. ","index":0},{"text":"Hello! ","index":11},{"text":"Ещё","index":18}])
        );
    }
    #[test]
    fn url_service_preserves_repeated_and_unicode_query_values() {
        let value = execute(&Request {
            id: "test".into(),
            method: "url.query_parse".into(),
            params: json!({"value":"?q=%D0%B0+%F0%9F%A6%80&q=two&empty="}),
        })
        .unwrap();
        assert_eq!(value, json!([["q", "а 🦀"], ["q", "two"], ["empty", ""]]));
        let encoded = execute(&Request {
            id: "test".into(),
            method: "url.query_encode".into(),
            params: json!({"pairs":value}),
        })
        .unwrap();
        assert_eq!(encoded, "q=%D0%B0+%F0%9F%A6%80&q=two&empty=");
    }
}

/// Hidden Attar checks must not show system prompts, notifications or sound.
pub fn hidden_check() -> bool {
    static VALUE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *VALUE.get_or_init(|| std::env::args().any(|arg| arg == "--attar-check-scenario"))
}

/// Separate explicit desktop profiles in WebKit as well as SQLite. The host
/// must reject unsupported systems: WebKit otherwise silently uses its default.
pub fn webkit_profile_identifier(directory: &std::path::Path) -> Result<[u8; 16], String> {
    #[cfg(target_os = "macos")]
    if objc2_foundation::NSProcessInfo::processInfo()
        .operatingSystemVersion()
        .majorVersion
        < 14
    {
        return Err("Separate persistent desktop profiles require macOS 14 or newer. Use the default profile or an ephemeral test profile.".into());
    }
    use sha2::{Digest, Sha256};
    let directory = directory
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let mut hash = Sha256::new();
    hash.update(b"codex-studio-webkit-profile-v1\0");
    hash.update(directory.as_os_str().as_encoded_bytes());
    let digest = hash.finalize();
    let mut identifier = [0; 16];
    identifier.copy_from_slice(&digest[..16]);
    identifier[6] = (identifier[6] & 0x0f) | 0x50;
    identifier[8] = (identifier[8] & 0x3f) | 0x80;
    Ok(identifier)
}

#[cfg(test)]
mod webkit_profile_tests {
    use super::*;
    #[test]
    fn explicit_profiles_have_stable_separate_canonical_webkit_stores() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let id = webkit_profile_identifier(first.path()).unwrap();
        assert_eq!(
            id,
            webkit_profile_identifier(&first.path().join(".")).unwrap()
        );
        assert_ne!(id, webkit_profile_identifier(second.path()).unwrap());
        assert!(webkit_profile_identifier(&first.path().join("missing")).is_err());
        #[cfg(unix)]
        {
            let link = second.path().join("alias");
            std::os::unix::fs::symlink(first.path(), &link).unwrap();
            assert_eq!(id, webkit_profile_identifier(&link).unwrap());
        }
    }
}
