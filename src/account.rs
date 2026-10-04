//! Exact configured account namespaces. No alias, case or punctuation inference.
use sha2::{Digest, Sha256};

/// Stable privacy-preserving key for one configured login and provider realm.
#[must_use]
pub(crate) fn namespace(username: &str, realm: &str) -> String {
    format!(
        "account-v1-{}",
        fingerprint("configured-account-v1", &[realm, username])
    )
}

#[must_use]
pub(crate) fn provider_fingerprint(realm: &str, dsid: &str) -> String {
    fingerprint("authenticated-provider-v1", &[realm, dsid])
}

fn fingerprint(kind: &str, values: &[&str]) -> String {
    let mut hash = Sha256::new();
    hash.update(kind.as_bytes());
    for value in values {
        hash.update(value.len().to_string().as_bytes());
        hash.update(b":");
        hash.update(value.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

#[cfg(test)]
mod tests {
    use super::namespace;

    #[test]
    fn exact_account_namespace_preserves_distinctions() {
        let key = namespace("first.last@example.invalid", "com");
        for (username, realm) in [
            ("firstlast@example.invalid", "com"),
            ("First.last@example.invalid", "com"),
            ("first.last@example.invalid", "cn"),
            ("é@example.invalid", "com"),
            ("é@example.invalid", "com"),
        ] {
            assert_ne!(key, namespace(username, realm));
        }
        assert_ne!(
            namespace("é@example.invalid", "com"),
            namespace("é@example.invalid", "com")
        );
        assert_eq!(key, namespace("first.last@example.invalid", "com"));
        let long_a = format!("{}first.last@example.invalid", "a".repeat(200));
        let long_b = format!("{}firstlast@example.invalid", "a".repeat(200));
        assert_eq!(
            crate::auth::session::sanitize_username(&long_a),
            crate::auth::session::sanitize_username(&long_b)
        );
        assert_ne!(namespace(&long_a, "com"), namespace(&long_b, "com"));
        assert_eq!(key.len(), 75);
        assert!(!key.contains("example"));
        assert_ne!(namespace("@.+-!", "com"), namespace("", "com"));
    }
}
