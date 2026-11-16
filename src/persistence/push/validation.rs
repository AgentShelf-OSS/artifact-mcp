use super::*;

/// One-column form of the `WEBHOOK_ENC_KEY` record: `v1:<nonce>:<ciphertext>:<tag>` (standard
/// base64), matching the packed endpoint ciphertext format.
#[must_use]
pub fn pack_endpoint_ciphertext(record: &EncryptedUrl) -> String {
    format!("v1:{}:{}:{}", record.nonce, record.ciphertext, record.tag)
}

/// Inverse of [`pack_endpoint_ciphertext`].
#[must_use]
pub fn unpack_endpoint_ciphertext(value: &str) -> Option<EncryptedUrl> {
    let mut parts = value.split(':');
    let (Some("v1"), Some(nonce), Some(ciphertext), Some(tag), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return None;
    };
    let valid = |part: &str| BASE64_STANDARD.decode(part).is_ok();
    (valid(nonce) && valid(ciphertext) && valid(tag)).then(|| EncryptedUrl {
        ciphertext: ciphertext.to_owned(),
        nonce: nonce.to_owned(),
        tag: tag.to_owned(),
    })
}
