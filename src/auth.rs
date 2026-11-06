use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use sha2::{Digest, Sha256};
use subtle::{Choice, ConstantTimeEq};

const SESSION_TTL: Duration = Duration::from_secs(8 * 60 * 60);

fn sha256(value: &[u8]) -> [u8; 32] {
    Sha256::digest(value).into()
}

pub fn random_hex(bytes: usize) -> Result<String> {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer).map_err(|e| anyhow!("system RNG failed: {e}"))?;
    Ok(hex::encode(buffer))
}

/// Configured API keys. Only digests stay in memory, and comparing fixed-size
/// digests in constant time hides both the key contents and their length.
pub struct ApiKeys(Vec<(String, [u8; 32])>);

impl ApiKeys {
    pub fn new(keys: &[(String, String)]) -> Self {
        Self(
            keys.iter()
                .map(|(name, key)| (name.clone(), sha256(key.as_bytes())))
                .collect(),
        )
    }

    /// Returns the name of the matching key. Checks every key, so timing does
    /// not reveal which one matched.
    pub fn authenticate(&self, presented: &str) -> Option<&str> {
        let digest = sha256(presented.as_bytes());
        let mut matched = None;
        for (name, expected) in &self.0 {
            if bool::from(expected.ct_eq(&digest)) {
                matched = Some(name.as_str());
            }
        }
        matched
    }
}

pub struct AdminCredentials {
    user: [u8; 32],
    password: [u8; 32],
}

impl AdminCredentials {
    pub fn new(user: &str, password: &str) -> Self {
        Self {
            user: sha256(user.as_bytes()),
            password: sha256(password.as_bytes()),
        }
    }

    pub fn verify(&self, user: &str, password: &str) -> bool {
        let user_ok: Choice = self.user.ct_eq(&sha256(user.as_bytes()));
        let password_ok: Choice = self.password.ct_eq(&sha256(password.as_bytes()));
        (user_ok & password_ok).into()
    }
}

/// Panel sessions, in memory: restarting the service logs everyone out. Only
/// token digests are kept, so lookups do not depend on the secret's bytes.
#[derive(Default)]
pub struct Sessions(Mutex<HashMap<[u8; 32], Instant>>);

impl Sessions {
    pub fn create(&self) -> Result<String> {
        let token = random_hex(32)?;
        let now = Instant::now();
        let mut sessions = self.0.lock().expect("session lock poisoned");
        sessions.retain(|_, expires| *expires > now);
        sessions.insert(sha256(token.as_bytes()), now + SESSION_TTL);
        Ok(token)
    }

    pub fn is_valid(&self, token: &str) -> bool {
        let sessions = self.0.lock().expect("session lock poisoned");
        sessions
            .get(&sha256(token.as_bytes()))
            .is_some_and(|expires| *expires > Instant::now())
    }

    pub fn remove(&self, token: &str) {
        let mut sessions = self.0.lock().expect("session lock poisoned");
        sessions.remove(&sha256(token.as_bytes()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_resolves_to_its_name() {
        let keys = ApiKeys::new(&[("a".into(), "key-a".into()), ("b".into(), "key-b".into())]);
        assert_eq!(keys.authenticate("key-b"), Some("b"));
        assert_eq!(keys.authenticate("key-c"), None);
        assert_eq!(keys.authenticate(""), None);
    }

    #[test]
    fn admin_needs_both_user_and_password() {
        let admin = AdminCredentials::new("admin", "secret");
        assert!(admin.verify("admin", "secret"));
        assert!(!admin.verify("admin", "wrong"));
        assert!(!admin.verify("root", "secret"));
    }

    #[test]
    fn removed_session_is_no_longer_valid() {
        let sessions = Sessions::default();
        let token = sessions.create().unwrap();
        assert!(sessions.is_valid(&token));
        assert!(!sessions.is_valid("forged"));
        sessions.remove(&token);
        assert!(!sessions.is_valid(&token));
    }
}
