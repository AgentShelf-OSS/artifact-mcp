//! Signature vectors for every JWS algorithm family the server verifies.
//!
//! The keys and tokens were generated with Python `cryptography` 46.0.3, independently of
//! jsonwebtoken, on 2026-10-30. They pin that the verification path (JWK document -> key
//! selection -> `DecodingKey` -> `jsonwebtoken::crypto::verify`) accepts each genuine signature and
//! rejects a changed payload or a swapped key, whatever crypto backend jsonwebtoken uses.

use artifact_mcp::security::jwks::{JwkDocument, JwksProvider, StaticJwks};

const KEYS: &str = r#"{
    "keys": [
        {
            "kty": "EC",
            "crv": "P-256",
            "kid": "es256",
            "alg": "ES256",
            "x": "gG5HKekGIuy1WGQD1Ux6yQKnCviovEbqU-777EPILeQ",
            "y": "IEXbz6zS6VfxgR-rX3RvkBj_g21qmztdgqRDqk5CM44"
        },
        {
            "kty": "OKP",
            "crv": "Ed25519",
            "kid": "eddsa",
            "alg": "EdDSA",
            "x": "Mfw1ANlX6-nd9NTXwGcUTVLltqk9jYY6kuLd9JccnSU"
        },
        {
            "kty": "RSA",
            "kid": "rs256",
            "alg": "RS256",
            "n": "yWCoZCnkB2YPZfc6WDHXuPwX8_ZVJFQ_MKWbj24Zh9CybaL3Nz-C2voulWTkjZQ2A2KPDJZL-VHkwzof57zNpGEFwjZZy70swFU8h-5eFBcNCnO8pFLYe8ea_vClavYcHZRh1gxGzzVstEXavZu9p-rhglJpRsquU81WKg3T7CFTOgAwDSaz2C3wfdOWI_hhHDZ4e2qwBMkQOrkflG2PRgHTB9r9wBPwGGY5Y5SKSHJh_Ze1UHzsK6EsHo7QZ3duMYDJeFZXYn435vTTDQU0B_D_BmXANEsSqXdmFZpDZyTnsishbbMhWoY54zOZ81h8X0dbPGJiNgYzDiSHHmW3-Q",
            "e": "AQAB"
        },
        {
            "kty": "RSA",
            "kid": "ps256",
            "alg": "PS256",
            "n": "yWCoZCnkB2YPZfc6WDHXuPwX8_ZVJFQ_MKWbj24Zh9CybaL3Nz-C2voulWTkjZQ2A2KPDJZL-VHkwzof57zNpGEFwjZZy70swFU8h-5eFBcNCnO8pFLYe8ea_vClavYcHZRh1gxGzzVstEXavZu9p-rhglJpRsquU81WKg3T7CFTOgAwDSaz2C3wfdOWI_hhHDZ4e2qwBMkQOrkflG2PRgHTB9r9wBPwGGY5Y5SKSHJh_Ze1UHzsK6EsHo7QZ3duMYDJeFZXYn435vTTDQU0B_D_BmXANEsSqXdmFZpDZyTnsishbbMhWoY54zOZ81h8X0dbPGJiNgYzDiSHHmW3-Q",
            "e": "AQAB"
        }
    ]
}"#;

/// (alg, kid, compact JWS)
const VECTORS: &[(&str, &str, &str)] = &[
    (
        "ES256",
        "es256",
        "eyJhbGciOiJFUzI1NiIsImtpZCI6ImVzMjU2In0.eyJpc3MiOiJodHRwczovL2lzc3Vlci5leGFtcGxlLnRlc3QiLCJhdWQiOiJhcnRpZmFjdC1tY3AiLCJzdWIiOiJ2ZWN0b3IifQ.A_EFzNg7b4tQhQejrdEPZ4FKAfBoR_5l5U_a4TZYNwq-Ah6WGxKGMWHBRtEF3Dz7fQjlaWZiUJHag3LJY7xyyQ",
    ),
    (
        "EdDSA",
        "eddsa",
        "eyJhbGciOiJFZERTQSIsImtpZCI6ImVkZHNhIn0.eyJpc3MiOiJodHRwczovL2lzc3Vlci5leGFtcGxlLnRlc3QiLCJhdWQiOiJhcnRpZmFjdC1tY3AiLCJzdWIiOiJ2ZWN0b3IifQ.GHjVkln5PEyVNHKxjMsYimXdn5sZwohNZnzOB8xjzhErpbVSJlwDdzY8_RvLsAdxr8Q6_HC-_2GChRVjNz5nCw",
    ),
    (
        "RS256",
        "rs256",
        "eyJhbGciOiJSUzI1NiIsImtpZCI6InJzMjU2In0.eyJpc3MiOiJodHRwczovL2lzc3Vlci5leGFtcGxlLnRlc3QiLCJhdWQiOiJhcnRpZmFjdC1tY3AiLCJzdWIiOiJ2ZWN0b3IifQ.rptFq8Bj0J31uvr7lQAdrGtUzmmdrEDavXesx1BbVi5B0RklZn0OAWVq_vF3pGp8wZsMJ7w4XmAACG_eM9sECUso0eEYXbEK-ewadyMw3RrIJf5vgH-ksUvlx2dfAnRvA1_0DyUVT_ulayaPXCXJHGDoVunytgjzavMJFmRPnoXZ4xn61_DL5KJXkqPseEHzoRT8NitN8rvhkxsGbeKc32P3RU4tYShYUmMEXZt4psiDCHKO8MM3jL3Z-aMATIwH_gzr77SH4J_oO8zr8JeqbqqCE8o6EuIlzjgSX78PlsiKKeTOmzyD0JrpMgFzAZJujszgF1EnJNLdkQopsCBrFg",
    ),
    (
        "PS256",
        "ps256",
        "eyJhbGciOiJQUzI1NiIsImtpZCI6InBzMjU2In0.eyJpc3MiOiJodHRwczovL2lzc3Vlci5leGFtcGxlLnRlc3QiLCJhdWQiOiJhcnRpZmFjdC1tY3AiLCJzdWIiOiJ2ZWN0b3IifQ.gnMcgBFT_8fQ4JVip_XO7hsc0khUb-nvjOSatoiXkmANVntxG195yq3l9cBdvQOw_vMTE7m5E16orUVi_0pvQjv-PeOR2synqUSrL4MTTVFfE0PH-3-fTbee3dWNeWNcazffVHpccX_IB78tm6Z5pKaCRhnp1OIId1V4NE3KRs5iMsBPoCYAzVBBmJMkxVzCv8pUANW9tM1rgkZktxJ9ceExmAsE3MhWnR8-3iy-OxXrYalsq_vgZCIAbSM3FMBc21btewwex9LXmccfy8G3Qxi3mlstHauc3sd50vUQpA3dgeO6u7zoKUqeEJXL5T4_M0REJLFBeEMfrJ7Tgc5VBg",
    ),
];

fn provider() -> StaticJwks {
    let value: serde_json::Value = serde_json::from_str(KEYS).expect("vector key set parses");
    StaticJwks::new(JwkDocument::from_json(&value).expect("vector key set is a JWK set"))
}

fn split(token: &str) -> (String, &str) {
    let (signing_input, signature) = token.rsplit_once('.').expect("compact JWS");
    (signing_input.to_owned(), signature)
}

async fn verifies(
    provider: &StaticJwks,
    alg: &str,
    kid: &str,
    signing_input: &str,
    signature: &str,
) -> bool {
    let key = provider
        .resolve(alg, Some(kid))
        .await
        .expect("vector key resolves");
    jsonwebtoken::crypto::verify(
        signature,
        signing_input.as_bytes(),
        key.key(),
        key.algorithm(),
    )
    .unwrap_or(false)
}

#[tokio::test]
async fn every_algorithm_family_accepts_its_genuine_signature() {
    let provider = provider();
    for (alg, kid, token) in VECTORS {
        let (signing_input, signature) = split(token);
        assert!(
            verifies(&provider, alg, kid, &signing_input, signature).await,
            "{alg}"
        );
    }
}

#[tokio::test]
async fn a_changed_payload_or_a_swapped_key_never_verifies() {
    let provider = provider();
    for (alg, kid, token) in VECTORS {
        let (signing_input, signature) = split(token);
        // Flip the last character of the payload segment.
        let mut tampered = signing_input.clone().into_bytes();
        let last = tampered.last_mut().expect("payload");
        *last = if *last == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(tampered).expect("ascii");
        assert!(
            !verifies(&provider, alg, kid, &tampered, signature).await,
            "{alg} tampered"
        );
    }
    // RS256 and PS256 share an RSA key but not a padding scheme: each signature fails the other.
    let rs = VECTORS
        .iter()
        .find(|v| v.0 == "RS256")
        .expect("RS256 vector");
    let ps = VECTORS
        .iter()
        .find(|v| v.0 == "PS256")
        .expect("PS256 vector");
    let (rs_input, rs_signature) = split(rs.2);
    let (ps_input, ps_signature) = split(ps.2);
    assert!(!verifies(&provider, "PS256", "ps256", &rs_input, rs_signature).await);
    assert!(!verifies(&provider, "RS256", "rs256", &ps_input, ps_signature).await);
}
