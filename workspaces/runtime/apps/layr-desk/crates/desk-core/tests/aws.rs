//! SigV4 against the worked examples of the S3 documentation
//! (docs.aws.amazon.com/AmazonS3/latest/API/sig-v4-header-based-auth.html) and against the
//! signer of the AWS JavaScript SDK (@smithy/signature-v4) for temporary credentials and STS.

use desk_core::aws::{self, Credentials, Request, EMPTY_SHA256};

const HOST: &str = "examplebucket.s3.amazonaws.com";
/// 2013-05-24T00:00:00Z
const DOC_TIME: u64 = 1_369_353_600;

fn doc_credentials() -> Credentials {
    Credentials {
        key_id: "AKIAIOSFODNN7EXAMPLE".into(),
        secret: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
        token: None,
    }
}

fn signature(headers: &[(String, String)]) -> String {
    let auth = &headers.last().unwrap();
    assert_eq!(auth.0, "authorization");
    auth.1.rsplit_once('=').unwrap().1.to_string()
}

#[test]
fn signatures_match_the_s3_documentation() {
    let get = aws::sign(
        &Request {
            method: "GET",
            host: HOST,
            path: "/test.txt",
            query: &[],
            headers: &[("Range", "bytes=0-9")],
            payload_sha256: EMPTY_SHA256,
        },
        &doc_credentials(),
        "us-east-1",
        "s3",
        DOC_TIME,
    );
    assert_eq!(
        signature(&get),
        "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
    );
    let put = aws::sign(
        &Request {
            method: "PUT",
            host: HOST,
            path: "/test$file.text",
            query: &[],
            headers: &[
                ("date", "Fri, 24 May 2013 00:00:00 GMT"),
                ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
            ],
            payload_sha256: "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072",
        },
        &doc_credentials(),
        "us-east-1",
        "s3",
        DOC_TIME,
    );
    assert_eq!(
        signature(&put),
        "98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd"
    );
    let list = aws::sign(
        &Request {
            method: "GET",
            host: HOST,
            path: "/",
            query: &[("prefix", "J"), ("max-keys", "2")],
            headers: &[],
            payload_sha256: EMPTY_SHA256,
        },
        &doc_credentials(),
        "us-east-1",
        "s3",
        DOC_TIME,
    );
    assert_eq!(
        signature(&list),
        "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
    );
    let names: Vec<&str> = list.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        names,
        ["x-amz-content-sha256", "x-amz-date", "authorization"]
    );
}

#[test]
fn temporary_credentials_and_sts_match_the_javascript_sdk() {
    let credentials = Credentials {
        key_id: "ASIAEXAMPLE".into(),
        secret: "secret/EXAMPLE+key".into(),
        token: Some("token/EXAMPLE+=".into()),
    };
    // 2026-10-11T12:34:56Z
    let time = 1_791_722_096;
    let body = format!(
        "Action=AssumeRole&RoleArn={}&RoleSessionName=vault-gateway&Version=2011-06-15",
        aws::uri_encode("arn:aws:iam::123456789012:role/vault-reader-p", false)
    );
    let payload = {
        use sha2::Digest;
        hex::encode(sha2::Sha256::digest(body.as_bytes()))
    };
    let sts = aws::sign(
        &Request {
            method: "POST",
            host: "sts.eu-central-1.api.aws",
            path: "/",
            query: &[],
            headers: &[(
                "content-type",
                "application/x-www-form-urlencoded; charset=utf-8",
            )],
            payload_sha256: &payload,
        },
        &credentials,
        "eu-central-1",
        "sts",
        time,
    );
    assert_eq!(
        sts.last().unwrap().1,
        "AWS4-HMAC-SHA256 Credential=ASIAEXAMPLE/20261011/eu-central-1/sts/aws4_request, \
         SignedHeaders=content-type;host;x-amz-date;x-amz-security-token, \
         Signature=0922add9824b9d87ebcfba80e23ef41e11a329141f47b2f9b9d62f16b1acbc1f"
    );
    let list = aws::sign(
        &Request {
            method: "GET",
            host: "b.s3.dualstack.eu-central-1.amazonaws.com",
            path: "/",
            query: &[
                ("list-type", "2"),
                ("prefix", "p/slots/"),
                ("continuation-token", "1/ab+c="),
            ],
            headers: &[],
            payload_sha256: EMPTY_SHA256,
        },
        &credentials,
        "eu-central-1",
        "s3",
        time,
    );
    assert_eq!(
        signature(&list),
        "6faaefd414edd60f141d57d939b64a7c6839872e8e4ecb733a8ce28cf7db16c6"
    );
    assert!(list
        .iter()
        .any(|(k, v)| k == "x-amz-security-token" && v == "token/EXAMPLE+="));
}

#[test]
fn s3_keys_are_at_the_dual_stack_endpoint() {
    let key = "p/states/0000000001.0a1b2c3d.tree.age";
    assert_eq!(
        aws::s3_location("vault-1", "eu-central-1", None, key),
        (
            "https://vault-1.s3.dualstack.eu-central-1.amazonaws.com".to_string(),
            format!("/{key}")
        )
    );
    assert_eq!(
        aws::s3_location("my.vault", "eu-central-1", None, key),
        (
            "https://s3.dualstack.eu-central-1.amazonaws.com".to_string(),
            format!("/my.vault/{key}")
        )
    );
    assert_eq!(
        aws::s3_location("b", "us-east-1", Some("http://127.0.0.1:9/"), ""),
        ("http://127.0.0.1:9".to_string(), "/b/".to_string())
    );
}

#[test]
fn dates_queries_and_xml() {
    assert_eq!(aws::amz_date(0), "19700101T000000Z");
    assert_eq!(aws::amz_date(DOC_TIME), "20130524T000000Z");
    assert_eq!(aws::amz_date(951_782_400), "20000229T000000Z");
    assert_eq!(aws::amz_date(1_791_722_096), "20261011T123456Z");
    assert_eq!(
        aws::query_string(&[("prefix", "a b/ü"), ("list-type", "2")]),
        "list-type=2&prefix=a%20b%2F%C3%BC"
    );
    let xml = "<?xml version=\"1.0\"?><ListBucketResult xmlns=\"http://s3\"><Contents><Key>p/a&amp;b</Key></Contents>\
               <Contents><Key>p/&#x3C;c&#62;</Key></Contents><NextContinuationToken>t&apos;1</NextContinuationToken></ListBucketResult>";
    assert_eq!(aws::xml_texts(xml, "Key"), ["p/a&b", "p/<c>"]);
    assert_eq!(
        aws::xml_text(xml, "NextContinuationToken").as_deref(),
        Some("t'1")
    );
    assert_eq!(aws::xml_text(xml, "Code"), None);
}
