use super::*;

#[tokio::test]
async fn oidc_authentication_accepts_an_optional_typ_without_weakening_access_tokens() {
    let (_, keys) = verifier_with(config(), Source::new());
    let verifier =
        OidcIdTokenVerifier::new(config().authority(), CLIENT.to_owned(), 30, keys.clone())
            .unwrap();
    let now = jsonwebtoken::get_current_timestamp();
    let claims = json!({
        "iss":config().issuer(),"sub":"browser-subject","aud":CLIENT,
        "iat":now-1,"exp":now+300,"nonce":"transaction-nonce",
    });
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("test-key-0".to_owned());
    header.typ = None;
    let token = sign_with_header(0, &header, &claims);
    let identity = verifier
        .verify(&token, "transaction-nonce", None, None)
        .await
        .unwrap();
    assert_eq!(identity.subject(), "browser-subject");
    let api = EntraVerifier::with_keys(config(), keys).unwrap();
    assert!(
        api.verify(&token).await.is_err(),
        "ID tokens cannot authorize an API"
    );
}

#[tokio::test]
async fn oidc_nonce_audience_authorized_party_and_token_hash_are_bound_to_the_login() {
    use sha2::{Digest, Sha256};
    let (_, keys) = verifier_with(config(), Source::new());
    let verifier =
        OidcIdTokenVerifier::new(config().authority(), CLIENT.to_owned(), 30, keys).unwrap();
    let now = jsonwebtoken::get_current_timestamp();
    let digest = Sha256::digest(b"resource-access-token");
    let claims = json!({
        "iss":config().issuer(),"sub":"browser-subject","aud":[CLIENT,OTHER],"azp":CLIENT,
        "iat":now-1,"exp":now+300,"nonce":"transaction-nonce","auth_time":now-5,
        "at_hash":URL_SAFE_NO_PAD.encode(&digest[..digest.len()/2]),
    });
    assert!(
        verifier
            .verify(
                &signed(0, &claims),
                "transaction-nonce",
                Some("resource-access-token"),
                Some(60)
            )
            .await
            .is_ok()
    );
    for (field, value) in [
        ("nonce", json!("another-login")),
        ("azp", json!(OTHER)),
        ("aud", json!(OTHER)),
        ("auth_time", json!(now - 600)),
        ("at_hash", json!("incorrect")),
    ] {
        let mut changed = claims.clone();
        changed[field] = value;
        assert!(
            verifier
                .verify(
                    &signed(0, &changed),
                    "transaction-nonce",
                    Some("resource-access-token"),
                    Some(60)
                )
                .await
                .is_err(),
            "{field}"
        );
    }
    let mut missing_party = claims;
    missing_party.as_object_mut().unwrap().remove("azp");
    assert!(
        verifier
            .verify(
                &signed(0, &missing_party),
                "transaction-nonce",
                Some("resource-access-token"),
                None
            )
            .await
            .is_err()
    );
}
