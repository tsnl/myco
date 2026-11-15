//! Versioned native-client contract, independent of browser transcript projections.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Identity {
    pub protocol: u32,
    pub version: String,
    pub build: String,
    pub instance: Uuid,
    pub profile: String,
    pub workspace: String,
}

impl Identity {
    pub fn new() -> Self {
        Self {
            protocol: VERSION,
            version: env!("CARGO_PKG_VERSION").into(),
            build: env!("MYCO_GIT_COMMIT").into(),
            instance: Uuid::new_v4(),
            profile: "default".into(),
            workspace: std::env::current_dir()
                .unwrap_or_default()
                .display()
                .to_string(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.protocol != VERSION
            || self.version != env!("CARGO_PKG_VERSION")
            || self.build != env!("MYCO_GIT_COMMIT")
        {
            return Err(format!(
                "Incompatible Myco service (protocol {}, version {}, build {}); use the same build for client and server. No local fallback was started.",
                self.protocol, self.version, self.build
            ));
        }
        Ok(())
    }
}

/// Service requests name a session by its full id, never a prefix: the
/// 32 lowercase hex digits of a simple UUID.
pub fn is_full_session_id(id: &str) -> bool {
    id.len() == 32 && id.to_ascii_lowercase() == id && Uuid::parse_str(id).is_ok()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Submit {
    pub instance: Uuid,
    pub request_id: Uuid,
    pub text: String,
    #[serde(default)]
    pub images: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Output {
    pub instance: Uuid,
    pub request_id: Uuid,
    pub revision: u64,
    pub offset: usize,
    pub next_offset: usize,
    pub output: String,
    pub exit_code: Option<u8>,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_requires_exact_protocol_package_and_build_identity() {
        let identity = Identity::new();
        identity.validate().unwrap();
        let mut changed = identity.clone();
        changed.protocol += 1;
        assert!(changed.validate().is_err());
        changed = identity.clone();
        changed.version.push_str("-other");
        assert!(changed.validate().is_err());
        changed = identity;
        changed.build.push_str("-other");
        assert!(changed.validate().is_err());
    }

    #[test]
    fn only_full_lowercase_simple_ids_name_a_service_session() {
        let id = Uuid::new_v4();
        assert!(is_full_session_id(&id.as_simple().to_string()));
        for other in [
            id.as_simple().to_string().to_uppercase(),
            id.as_hyphenated().to_string(),
            id.as_simple().to_string()[..8].to_string(),
            "g".repeat(32),
        ] {
            assert!(!is_full_session_id(&other), "{other}");
        }
    }
}
