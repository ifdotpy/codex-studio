//! The keys against vectors that the JavaScript implementation made (js-vectors.json): the same
//! words, identities, recipients and slots in both.

use base64::Engine;
use desk_core::keys;
use serde_json::Value;

fn vectors() -> Value {
    serde_json::from_str(include_str!("js-vectors.json")).unwrap()
}

fn fixture_identity() -> String {
    include_str!("../../../js/vault/test/fixture/identity.txt")
        .trim()
        .to_string()
}

#[test]
fn words_and_identities_match_javascript() {
    let v = vectors();
    let id = fixture_identity();
    assert_eq!(keys::words_of_identity(&id).unwrap(), v["fixtureWords"]);
    assert_eq!(
        keys::identity_from_paper(v["fixtureWords"].as_str().unwrap()).unwrap(),
        id
    );
    assert_eq!(keys::recipient_of(&id).unwrap(), v["fixtureRecipient"]);
    let secret: Vec<u8> = (1..=32).collect();
    let prf = keys::identity_from_secret(&secret).unwrap();
    assert_eq!(prf, v["secret1to32"]);
    assert_eq!(keys::recipient_of(&prf).unwrap(), v["secret1to32Recipient"]);
    assert_eq!(keys::secret_of_identity(&prf).unwrap().to_vec(), secret);
}

#[test]
fn a_slot_sealed_in_javascript_opens() {
    let v = vectors();
    let slot = base64::engine::general_purpose::STANDARD
        .decode(v["slotB64"].as_str().unwrap())
        .unwrap();
    assert_eq!(
        keys::open_slot(&slot, v["opener"].as_str().unwrap()).unwrap(),
        fixture_identity()
    );
    let other = keys::generate_identity();
    assert!(keys::open_slot(&slot, &other).is_err());
}

#[test]
fn a_sealed_slot_opens_only_for_its_opener() {
    let vault = keys::generate_identity();
    let opener = keys::generate_identity();
    let slot = keys::seal_slot(&vault, &keys::recipient_of(&opener).unwrap()).unwrap();
    assert_eq!(keys::open_slot(&slot, &opener).unwrap(), vault);
    assert!(keys::open_slot(&slot, &vault).is_err());
}

#[test]
fn paper_words_allow_numbers_short_words_and_the_age_form() {
    let v = vectors();
    let words: Vec<&str> = v["fixtureWords"].as_str().unwrap().split(' ').collect();
    let numbered: Vec<String> = words
        .iter()
        .enumerate()
        .map(|(i, w)| format!("{}. {}", i + 1, w))
        .collect();
    let id = fixture_identity();
    assert_eq!(keys::identity_from_paper(&numbered.join("\n")).unwrap(), id);
    let short: Vec<String> = words
        .iter()
        .map(|w| w.chars().take(4).collect::<String>().to_uppercase())
        .collect();
    assert_eq!(keys::identity_from_paper(&short.join(", ")).unwrap(), id);
    let spaced = format!(
        "  {} \n",
        id.to_lowercase()
            .chars()
            .collect::<Vec<_>>()
            .chunks(6)
            .map(|c| c.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join(" ")
    );
    assert_eq!(keys::identity_from_paper(&spaced).unwrap(), id);
    let mut swapped = words.clone();
    swapped.swap(0, 1);
    let err = keys::identity_from_paper(&swapped.join(" "))
        .unwrap_err()
        .to_string();
    assert!(err.contains("checksum"), "{err}");
    assert!(keys::identity_from_paper("zzzz")
        .unwrap_err()
        .to_string()
        .contains("not a word"));
    assert!(keys::identity_from_paper(&words[..23].join(" ")).is_err());
}

#[test]
fn the_prf_salt_names_the_vault() {
    assert_eq!(keys::prf_salt("proj"), b"layr-vault/v1/proj");
}
