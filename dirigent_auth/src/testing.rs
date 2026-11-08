//! For tests (feature `testing`): a fixed RSA key that signs access tokens the way the
//! forge does, and the JWKS that verifies them. No network.

use base64::Engine;
use base64::prelude::BASE64_URL_SAFE_NO_PAD;
use ring::signature::{KeyPair, RSA_PKCS1_SHA256, RsaKeyPair, RsaPublicKeyComponents};
use serde_json::{Value, json};

/// A 2048-bit key made with `openssl genpkey` for these tests, PKCS#8 DER.
const KEY: &[u8] = include_bytes!("test_key.der");

pub struct Signer {
    key: RsaKeyPair,
}

impl Default for Signer {
    fn default() -> Self {
        Self::new()
    }
}

impl Signer {
    pub fn new() -> Self {
        Self { key: RsaKeyPair::from_pkcs8(KEY).expect("test key") }
    }

    pub fn kid(&self) -> &'static str {
        "test-key"
    }

    /// The JWKS with the key's public half, as `/api/oauth/jwks` serves it.
    pub fn jwks(&self) -> String {
        let public: RsaPublicKeyComponents<Vec<u8>> = self.key.public_key().into();
        let b64 = |b: &[u8]| BASE64_URL_SAFE_NO_PAD.encode(b);
        json!({ "keys": [{ "kty": "RSA", "use": "sig", "alg": "RS256", "kid": self.kid(), "n": b64(&public.n), "e": b64(&public.e) }] })
            .to_string()
    }

    /// `claims` as an RS256 `at+jwt`.
    pub fn sign(&self, claims: &Value) -> String {
        let header = json!({ "alg": "RS256", "typ": "at+jwt", "kid": self.kid() });
        let input = format!("{}.{}", BASE64_URL_SAFE_NO_PAD.encode(header.to_string()), BASE64_URL_SAFE_NO_PAD.encode(claims.to_string()));
        let mut sig = vec![0; self.key.public().modulus_len()];
        self.key.sign(&RSA_PKCS1_SHA256, &ring::rand::SystemRandom::new(), input.as_bytes(), &mut sig).expect("sign");
        format!("{input}.{}", BASE64_URL_SAFE_NO_PAD.encode(sig))
    }
}

/// An access token's claims as the forge makes them (`preferred_username` "seb").
pub fn claims(iss: &str, sub: &str, aud: &str, client_id: &str, exp: i64) -> Value {
    json!({
        "iss": iss, "sub": sub, "aud": aud, "client_id": client_id, "scope": "profile",
        "iat": exp - 600, "exp": exp, "jti": "j", "preferred_username": "seb",
    })
}
