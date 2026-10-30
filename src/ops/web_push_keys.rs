//! `artifact-mcp web-push-keys`: print a new VAPID (RFC 8292) key pair for ADR-0012 Web Push
//! reminders.
//!
//! Put the `WEB_PUSH_VAPID_PRIVATE_KEY` line in the server environment. The public key is shown
//! for reference only: the server derives it from the private key
//! ([`crate::config::vapid_public_key`]). Rotating the private key makes every stored push
//! subscription unusable, so viewers must turn notifications on again.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use p256::{SecretKey, elliptic_curve::sec1::ToSec1Point as _};

use crate::{config::RandomSource, error::AppError};

/// Draws before giving up. An out-of-range 32-byte draw has a probability near 2^-32, so only a
/// broken entropy source reaches this limit.
const MAX_DRAWS: usize = 64;

/// A new VAPID identity. Both halves are base64url without padding.
#[derive(Clone, PartialEq, Eq)]
pub struct VapidKeyPair {
    /// The raw 32-byte P-256 scalar (`WEB_PUSH_VAPID_PRIVATE_KEY`).
    pub private_key: String,
    /// The uncompressed public point, 65 bytes (`applicationServerKey`).
    pub public_key: String,
}

impl std::fmt::Debug for VapidKeyPair {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VapidKeyPair")
            .field("public_key", &self.public_key)
            .finish_non_exhaustive()
    }
}

impl VapidKeyPair {
    /// Generate a key pair from `random`.
    ///
    /// The scalar is drawn as 32 bytes and redrawn when it is zero or not below the curve
    /// order, the same way the Web Push sender draws its ephemeral keys.
    ///
    /// # Errors
    /// [`AppError::Internal`] when the entropy source fails or never yields a valid scalar.
    pub fn generate(random: &dyn RandomSource) -> Result<Self, AppError> {
        for _ in 0..MAX_DRAWS {
            let mut scalar = [0_u8; 32];
            random.fill_bytes(&mut scalar)?;
            if let Ok(secret) = SecretKey::from_slice(&scalar) {
                let point = secret.public_key().to_sec1_point(false);
                return Ok(Self {
                    private_key: URL_SAFE_NO_PAD.encode(secret.to_bytes()),
                    public_key: URL_SAFE_NO_PAD.encode(point.as_bytes()),
                });
            }
        }
        Err(AppError::Internal)
    }

    /// The two lines `web-push-keys` prints, each ending in a newline. This is the output format
    /// of the former `scripts/web-push-keys.mjs`.
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "WEB_PUSH_VAPID_PRIVATE_KEY={}\n# Public key (applicationServerKey, derived by the server): {}\n",
            self.private_key, self.public_key
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{AppConfig, MapEnv, ScriptedRandom, SeededRandom, vapid_public_key},
        integrations::web_push::VapidSigner,
    };

    #[test]
    fn generated_private_key_is_accepted_by_the_server_parser() {
        for seed in 1..=32 {
            let pair = VapidKeyPair::generate(&SeededRandom::new(seed)).unwrap();
            assert_eq!(pair.private_key.len(), 43);
            assert_eq!(pair.public_key.len(), 87);
            let point = URL_SAFE_NO_PAD.decode(&pair.public_key).unwrap();
            assert_eq!(point.len(), 65);
            assert_eq!(point[0], 0x04, "uncompressed SEC1 point");

            // The configuration parser derives the same public point from the private key.
            assert_eq!(
                vapid_public_key(&pair.private_key).as_deref(),
                Some(pair.public_key.as_str())
            );
            let config = AppConfig::from_source(
                &MapEnv::empty()
                    .with("WEB_PUSH_VAPID_PRIVATE_KEY", &pair.private_key)
                    .with("WEB_PUSH_SUBJECT", "mailto:ops@example.com"),
            )
            .unwrap();
            let vapid = config.web_push.vapid.unwrap();
            assert_eq!(vapid.public_key, pair.public_key);
            assert_eq!(vapid.private_key.expose(), pair.private_key);

            // The sender signs with it and advertises the same key.
            let signer = VapidSigner::new(&pair.private_key, "mailto:ops@example.com").unwrap();
            assert_eq!(signer.public_key(), pair.public_key);
        }
    }

    #[test]
    fn out_of_range_scalars_are_redrawn() {
        // 32 bytes of 0xff exceed the P-256 order; the next 32 bytes are a valid scalar.
        let mut script = vec![0xff_u8; 32];
        script.extend(std::iter::repeat_n(0x11_u8, 32));
        let pair = VapidKeyPair::generate(&ScriptedRandom::new(script)).unwrap();
        assert_eq!(
            URL_SAFE_NO_PAD.decode(&pair.private_key).unwrap(),
            vec![0x11_u8; 32]
        );
    }

    #[test]
    fn a_broken_entropy_source_fails_instead_of_looping() {
        // An empty script yields only zero bytes, and zero is not a valid scalar.
        assert!(VapidKeyPair::generate(&ScriptedRandom::new(Vec::new())).is_err());
    }

    #[test]
    fn leading_zero_bytes_are_kept_in_the_32_byte_scalar() {
        let mut scalar = vec![0_u8; 3];
        scalar.extend(std::iter::repeat_n(0x42_u8, 29));
        let pair = VapidKeyPair::generate(&ScriptedRandom::new(scalar.clone())).unwrap();
        assert_eq!(URL_SAFE_NO_PAD.decode(&pair.private_key).unwrap(), scalar);
        assert!(vapid_public_key(&pair.private_key).is_some());
    }

    #[test]
    fn render_matches_the_former_script_output() {
        let pair = VapidKeyPair {
            private_key: "PRIVATE".to_owned(),
            public_key: "PUBLIC".to_owned(),
        };
        assert_eq!(
            pair.render(),
            "WEB_PUSH_VAPID_PRIVATE_KEY=PRIVATE\n\
             # Public key (applicationServerKey, derived by the server): PUBLIC\n"
        );
        assert!(!format!("{pair:?}").contains("PRIVATE"));
    }
}
