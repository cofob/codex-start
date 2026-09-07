//! Device identity and domain-separated authentication proofs.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::RngCore;
use sha2::{Digest, Sha256};

#[must_use]
pub fn random_secret() -> String {
    let mut bytes = [0; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Generate a 128-bit invitation or endpoint-group password.
#[must_use]
pub fn random_password() -> String {
    let mut bytes = [0; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Decode a 128-bit invitation or endpoint-group password.
///
/// # Errors
/// Returns an error for a malformed password or unsupported length.
pub fn decode_password(value: &str) -> Result<Vec<u8>, crate::Error> {
    URL_SAFE_NO_PAD
        .decode(value)
        .ok()
        .filter(|v| v.len() == 16)
        .ok_or_else(|| crate::Error::Invalid("invalid password".into()))
}

///
/// # Errors
/// Returns an error if a key, signature, or encoded secret is invalid.
pub fn decode_secret(value: &str) -> Result<[u8; 32], crate::Error> {
    URL_SAFE_NO_PAD
        .decode(value)
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| crate::Error::Invalid("invalid secret".into()))
}

///
/// # Errors
/// Returns an error if a key, signature, or encoded secret is invalid.
pub fn decode32(value: &str) -> Result<[u8; 32], crate::Error> {
    hex::decode(value)
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| crate::Error::Invalid("invalid public key or fingerprint".into()))
}

#[must_use]
pub fn digest(value: &[u8]) -> String {
    hex::encode(Sha256::digest(value))
}

///
/// # Errors
/// Returns an error if a key, signature, or encoded secret is invalid.
pub fn public_key(secret: &str) -> Result<String, crate::Error> {
    Ok(hex::encode(
        SigningKey::from_bytes(&decode_secret(secret)?)
            .verifying_key()
            .to_bytes(),
    ))
}

#[must_use]
pub fn proof_message(nonce: &str, fingerprint: &str) -> Vec<u8> {
    format!("cs.fob.wtf/auth/v1\0{nonce}\0{fingerprint}").into_bytes()
}

///
/// # Errors
/// Returns an error if a key, signature, or encoded secret is invalid.
pub fn sign(secret: &str, nonce: &str, fingerprint: &str) -> Result<String, crate::Error> {
    Ok(hex::encode(
        SigningKey::from_bytes(&decode_secret(secret)?)
            .sign(&proof_message(nonce, fingerprint))
            .to_bytes(),
    ))
}

///
/// # Errors
/// Returns an error if a key, signature, or encoded secret is invalid.
pub fn verify(
    key: &str,
    signature: &str,
    nonce: &str,
    fingerprint: &str,
) -> Result<(), crate::Error> {
    let key = VerifyingKey::from_bytes(&decode32(key)?).map_err(|_| crate::Error::Unauthorized)?;
    let bytes = hex::decode(signature).map_err(|_| crate::Error::Unauthorized)?;
    let signature = Signature::from_slice(&bytes).map_err(|_| crate::Error::Unauthorized)?;
    key.verify(&proof_message(nonce, fingerprint), &signature)
        .map_err(|_| crate::Error::Unauthorized)
}

#[must_use]
pub fn approval_code(device_key: &str, fingerprint: &str, nonce: &str) -> String {
    let hash =
        digest(format!("cs.fob.wtf/pair/v1\0{device_key}\0{fingerprint}\0{nonce}").as_bytes());
    format!("{}-{}-{}", &hash[..5], &hash[5..10], &hash[10..15]).to_uppercase()
}
