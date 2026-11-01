//! The `host-agent` build that runner VMs download and install at boot.
//!
//! Runner VMs boot a stock Debian image, so the startup script fetches the static `host-agent` binary from
//! this repository's GitHub releases and refuses to continue unless its SHA-256 matches a value pinned here.
//!
//! `host-agent/release-build.sh` builds the binaries reproducibly, so the same source always gives the same
//! checksums. The `Host agent release` workflow rebuilds them on every pull request and fails when the
//! checksums below no longer match the source, so a change to `host-agent/` must update them in the same
//! pull request. On a push to `main` it publishes the binaries as the release named by [`release_tag`].

/// Repository whose releases hold the binaries.
pub const RELEASE_REPOSITORY: &str = "swack-tools/spot-github-runners";

/// SHA-256 of `host-agent-x86_64-linux-musl`, as printed by `host-agent/release-build.sh`.
pub const SHA256_X86_64: &str = "111c2f7b0f6a777fd1b4d2805815191dfcb74e4539729554f6468c55e01c2707";

/// SHA-256 of `host-agent-aarch64-linux-musl`, as printed by `host-agent/release-build.sh`.
pub const SHA256_AARCH64: &str = "e2c3f4acd03e15164371e891386cfa026aa0096dc63bfb1fd94f5343e8c68705";

/// The pinned binaries, checked for shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedHostAgent {
    pub sha256_x86_64: String,
    pub sha256_aarch64: String,
}

impl PinnedHostAgent {
    /// Release tag: `host-agent-` and the first 12 hex digits of the x86_64 checksum. Content-addressed, so
    /// a new build gets a new tag and an existing tag never changes meaning.
    pub fn release_tag(&self) -> String {
        format!("host-agent-{}", &self.sha256_x86_64[..12])
    }

    /// Directory URL the startup script appends an asset name to.
    pub fn download_base_url(&self) -> String {
        format!(
            "https://github.com/{}/releases/download/{}",
            RELEASE_REPOSITORY,
            self.release_tag()
        )
    }
}

pub fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Validates a pair of checksums (lower-case hex only, because the startup script compares them as text).
pub fn validate(sha256_x86_64: &str, sha256_aarch64: &str) -> Result<PinnedHostAgent, String> {
    if !is_sha256_hex(sha256_x86_64) {
        return Err("host-agent x86_64 checksum is not 64 lower-case hex digits".to_string());
    }
    if !is_sha256_hex(sha256_aarch64) {
        return Err("host-agent aarch64 checksum is not 64 lower-case hex digits".to_string());
    }
    Ok(PinnedHostAgent {
        sha256_x86_64: sha256_x86_64.to_string(),
        sha256_aarch64: sha256_aarch64.to_string(),
    })
}

/// The release this Worker build installs on runner VMs.
pub fn current() -> Result<PinnedHostAgent, String> {
    validate(SHA256_X86_64, SHA256_AARCH64)
}

/// [`PinnedHostAgent::release_tag`] for the current pin.
pub fn release_tag() -> Result<String, String> {
    current().map(|p| p.release_tag())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_pin_is_valid() {
        let pin = current().expect("pinned checksums must be valid");
        assert!(pin.release_tag().starts_with("host-agent-"));
        assert_eq!(pin.release_tag().len(), "host-agent-".len() + 12);
        assert!(pin.download_base_url().starts_with(
            "https://github.com/swack-tools/spot-github-runners/releases/download/host-agent-"
        ));
    }

    #[test]
    fn rejects_bad_checksums() {
        let good = "a".repeat(64);
        assert!(validate(&good, &good).is_ok());
        assert!(validate("", &good).is_err());
        assert!(validate(&good, "").is_err());
        assert!(validate(&"A".repeat(64), &good).is_err());
        assert!(validate(&"a".repeat(63), &good).is_err());
        assert!(validate(&format!("{}g", "a".repeat(63)), &good).is_err());
        assert!(validate(&format!("{}'", "a".repeat(63)), &good).is_err());
    }
}
