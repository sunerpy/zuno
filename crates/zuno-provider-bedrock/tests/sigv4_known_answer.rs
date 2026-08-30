use std::collections::BTreeMap;

use url::Url;
use zuno_provider_bedrock::{AwsCredentials, SigV4Signer};

/// Published by AWS as the S3 SigV4 GET Object test-suite example:
/// https://docs.aws.amazon.com/AmazonS3/latest/developerguide/sig-v4-header-based-auth.html
#[test]
fn aws_s3_get_object_known_answer_matches_byte_for_byte() {
    let credentials = AwsCredentials::new(
        "AKIAIOSFODNN7EXAMPLE",
        "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
    );
    let signer = SigV4Signer::new("us-east-1", "s3");
    let url = Url::parse("https://examplebucket.s3.amazonaws.com/test.txt").expect("valid URL");
    let headers = BTreeMap::from([
        ("range".to_owned(), "bytes=0-9".to_owned()),
        (
            "x-amz-content-sha256".to_owned(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_owned(),
        ),
    ]);

    let signed = signer
        .sign("GET", &url, &headers, b"", &credentials, "20130524T000000Z")
        .expect("the published request signs");

    let expected_canonical_request = concat!(
        "GET\n",
        "/test.txt\n",
        "\n",
        "host:examplebucket.s3.amazonaws.com\n",
        "range:bytes=0-9\n",
        "x-amz-content-sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n",
        "x-amz-date:20130524T000000Z\n",
        "\n",
        "host;range;x-amz-content-sha256;x-amz-date\n",
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    );
    let expected_string_to_sign = concat!(
        "AWS4-HMAC-SHA256\n",
        "20130524T000000Z\n",
        "20130524/us-east-1/s3/aws4_request\n",
        "7344ae5b7ee6c3e7e6b0fe0640412a37625d1fbfff95c48bbb2dc43964946972",
    );
    let expected_signature = "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41";

    assert_eq!(
        signed.canonical_request.as_bytes(),
        expected_canonical_request.as_bytes()
    );
    assert_eq!(
        signed.string_to_sign.as_bytes(),
        expected_string_to_sign.as_bytes()
    );
    assert_eq!(signed.signature.as_bytes(), expected_signature.as_bytes());
    assert_eq!(
        signed.authorization,
        concat!(
            "AWS4-HMAC-SHA256 ",
            "Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, ",
            "SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, ",
            "Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41",
        )
    );
}

#[test]
fn temporary_credentials_sign_the_security_token() {
    let credentials = AwsCredentials::new("AKID", "secret").with_session_token("session-token");
    let signer = SigV4Signer::new("us-east-1", "bedrock");
    let url = Url::parse("https://bedrock-runtime.us-east-1.amazonaws.com/model/m/converse-stream")
        .expect("valid URL");

    let signed = signer
        .sign(
            "POST",
            &url,
            &BTreeMap::new(),
            b"{}",
            &credentials,
            "20260805T120000Z",
        )
        .expect("temporary credentials sign");

    assert!(signed.signed_headers.contains("x-amz-security-token"));
    assert_eq!(
        signed
            .headers
            .get("x-amz-security-token")
            .map(String::as_str),
        Some("session-token")
    );
}
