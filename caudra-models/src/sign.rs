use base64::{Engine, engine::general_purpose::STANDARD};
use ring::{
    rand::SystemRandom,
    signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::hex;

const KEY_ID_BYTES: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Signature {
    pub key_id: String,
    pub signature: String,
}

#[derive(Debug, Error)]
pub enum SignatureError {
    #[error("could not generate an Ed25519 signing key")]
    KeyGeneration,
    #[error("invalid Ed25519 PKCS#8 signing key: {0}")]
    InvalidKey(String),
    #[error("no trusted signing key matches key id {0:?}")]
    UntrustedKey(String),
    #[error("signature is not valid base64: {0}")]
    InvalidBase64(#[from] base64::DecodeError),
    #[error("Ed25519 signature does not match the model-file bytes")]
    VerificationFailed,
}

pub fn keygen() -> Result<(Vec<u8>, [u8; 32]), SignatureError> {
    let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
        .map_err(|_| SignatureError::KeyGeneration)?;
    let key = parse_key(document.as_ref())?;
    let mut public = [0; 32];
    public.copy_from_slice(key.public_key().as_ref());
    Ok((document.as_ref().to_vec(), public))
}

pub fn key_id(public_key: &[u8; 32]) -> String {
    hex(&Sha256::digest(public_key)[..KEY_ID_BYTES])
}

pub fn sign(bytes: &[u8], pkcs8: &[u8]) -> Result<Signature, SignatureError> {
    let key = parse_key(pkcs8)?;
    Ok(Signature {
        key_id: hex(&Sha256::digest(key.public_key().as_ref())[..KEY_ID_BYTES]),
        signature: STANDARD.encode(key.sign(bytes).as_ref()),
    })
}

pub fn verify(
    bytes: &[u8],
    signature: &Signature,
    trusted: &[[u8; 32]],
) -> Result<(), SignatureError> {
    let candidates: Vec<_> = trusted
        .iter()
        .filter(|key| key_id(key) == signature.key_id)
        .collect();
    if candidates.is_empty() {
        return Err(SignatureError::UntrustedKey(signature.key_id.clone()));
    }
    let decoded = STANDARD.decode(&signature.signature)?;
    if candidates.iter().any(|key| {
        UnparsedPublicKey::new(&ED25519, key.as_slice())
            .verify(bytes, &decoded)
            .is_ok()
    }) {
        Ok(())
    } else {
        Err(SignatureError::VerificationFailed)
    }
}

fn parse_key(pkcs8: &[u8]) -> Result<Ed25519KeyPair, SignatureError> {
    Ed25519KeyPair::from_pkcs8(pkcs8).map_err(|error| SignatureError::InvalidKey(error.to_string()))
}

#[cfg(test)]
pub(crate) mod tests {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use test_case::test_case;

    use super::{Signature, SignatureError, key_id, keygen, sign, verify};

    pub(crate) const TEST_SIGNING_KEY_PKCS8_B64: &str = "MFMCAQEwBQYDK2VwBCIEIJ1hsZ3v/VpguoRK9JLsLMREScVpezJpGXA7rAMcrn9goSMDIQDXWpgBgrEKt9VL/tPJZAc6DuFy89qmIyWvAhpo9wdRGg==";
    pub(crate) const TEST_PUBLIC_KEY_B64: &str = "11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=";
    const TEST_KEY_ID: &str = "21fe31dfa154a261";
    const EMPTY_SIGNATURE_B64: &str =
        "5VZDAMNgrHKQhuLMgG6CioSHfx645dl02HPgZSJJAVVfuIIVkKM7rMYeOXAc+bRr0lv18FlbviRlUUFDjnoQCw==";
    const MESSAGE: &[u8] = b"model-file bytes\n";
    const VERIFICATION_MESSAGE: &str = "Ed25519 signature does not match the model-file bytes";
    const UNTRUSTED_MESSAGE: &str = "no trusted signing key matches key id \"21fe31dfa154a261\"";
    const PRODUCTION_PUBLIC_KEY: &str = "OmFa847Jcms51wq0p3iNnb3Mp012XcdYD00EYH3hKpg=";
    const PRODUCTION_KEY_ID: &str = "5370f4ad7d3650ee";

    #[test]
    fn production_trust_matches_publishing_key() {
        let public: [u8; 32] = STANDARD
            .decode(PRODUCTION_PUBLIC_KEY)
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(crate::TRUSTED_KEYS, &[public]);
        assert_eq!(key_id(&public), PRODUCTION_KEY_ID);
    }

    fn public_key() -> [u8; 32] {
        STANDARD
            .decode(TEST_PUBLIC_KEY_B64)
            .unwrap()
            .try_into()
            .unwrap()
    }

    fn signed(bytes: &[u8]) -> Signature {
        sign(bytes, &STANDARD.decode(TEST_SIGNING_KEY_PKCS8_B64).unwrap()).unwrap()
    }

    #[test]
    fn fixed_key_matches_rfc8032_vector() {
        let signature = signed(b"");
        assert_eq!(signature.key_id, TEST_KEY_ID);
        assert_eq!(signature.signature, EMPTY_SIGNATURE_B64);
        assert_eq!(key_id(&public_key()), TEST_KEY_ID);
        verify(b"", &signature, &[public_key()]).unwrap();
    }

    #[test]
    fn signature_round_trips_and_verifies() {
        let signature = signed(MESSAGE);
        let parsed: Signature =
            serde_json::from_slice(&serde_json::to_vec(&signature).unwrap()).unwrap();
        assert_eq!(signature, parsed);
        verify(MESSAGE, &parsed, &[[0; 32], public_key()]).unwrap();
    }

    #[test_case(b"model-file bytes"; "removed_newline")]
    #[test_case(b"Model-file bytes\n"; "changed_byte")]
    fn tampered_bytes_fail(bytes: &[u8]) {
        let error = verify(bytes, &signed(MESSAGE), &[public_key()]).unwrap_err();
        assert!(matches!(error, SignatureError::VerificationFailed));
        assert_eq!(error.to_string(), VERIFICATION_MESSAGE);
    }

    #[test]
    fn untrusted_key_fails() {
        let error = verify(MESSAGE, &signed(MESSAGE), &[[0; 32]]).unwrap_err();
        assert!(matches!(error, SignatureError::UntrustedKey(_)));
        assert_eq!(error.to_string(), UNTRUSTED_MESSAGE);
    }

    #[test]
    fn malformed_base64_fails() {
        let mut signature = signed(MESSAGE);
        signature.signature = "!".to_owned();
        assert!(matches!(
            verify(MESSAGE, &signature, &[public_key()]),
            Err(SignatureError::InvalidBase64(_))
        ));
    }

    #[test]
    fn malformed_pkcs8_fails_without_panicking() {
        assert!(matches!(
            sign(MESSAGE, b"not a key"),
            Err(SignatureError::InvalidKey(_))
        ));
    }

    #[test]
    fn generated_key_can_sign_and_verify() {
        let (pkcs8, public) = keygen().unwrap();
        verify(MESSAGE, &sign(MESSAGE, &pkcs8).unwrap(), &[public]).unwrap();
    }
}
