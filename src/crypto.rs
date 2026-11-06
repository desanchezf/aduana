use std::sync::Arc;

use age::x25519::{Identity, Recipient};
use anyhow::{Context, Result, anyhow};

/// Encrypts bodies for storage with age (X25519 + ChaCha20-Poly1305), so a
/// copy of the database can be read with the `age` CLI and the private key.
///
/// Bodies can reach tens of MB, so the work runs on the blocking pool rather
/// than on the async workers.
#[derive(Clone)]
pub struct Crypto {
    identity: Arc<Identity>,
    recipient: Arc<Recipient>,
}

impl Crypto {
    pub fn new(identity: Identity) -> Self {
        let recipient = Arc::new(identity.to_public());
        Self {
            identity: Arc::new(identity),
            recipient,
        }
    }

    pub async fn encrypt(&self, plaintext: Vec<u8>) -> Result<Vec<u8>> {
        let recipient = self.recipient.clone();
        tokio::task::spawn_blocking(move || {
            age::encrypt(recipient.as_ref(), &plaintext).map_err(|e| anyhow!("age encrypt: {e}"))
        })
        .await
        .context("encryption task failed")?
    }

    pub async fn decrypt(&self, ciphertext: Vec<u8>) -> Result<Vec<u8>> {
        let identity = self.identity.clone();
        tokio::task::spawn_blocking(move || {
            age::decrypt(identity.as_ref(), &ciphertext).map_err(|e| anyhow!("age decrypt: {e}"))
        })
        .await
        .context("decryption task failed")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trip_hides_plaintext() {
        let crypto = Crypto::new(Identity::generate());
        let sealed = crypto.encrypt(b"top secret prompt".to_vec()).await.unwrap();
        assert!(!sealed.windows(6).any(|w| w == b"secret"));
        assert_eq!(crypto.decrypt(sealed).await.unwrap(), b"top secret prompt");
    }

    #[tokio::test]
    async fn another_key_cannot_decrypt() {
        let sealed = Crypto::new(Identity::generate())
            .encrypt(b"data".to_vec())
            .await
            .unwrap();
        assert!(
            Crypto::new(Identity::generate())
                .decrypt(sealed)
                .await
                .is_err()
        );
    }
}
