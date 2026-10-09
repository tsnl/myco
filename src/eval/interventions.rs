//! Observer attestations bind to immutable result bytes, separately from machine metrics.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::core::image_store::sha256;

const MAX_ANNOTATION_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Serialize)]
pub(super) struct ResultIdentity {
    pub fingerprint: String,
    pub result_sha256: String,
    pub result_path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u32,
    annotations: Vec<Annotation>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Annotation {
    fingerprint: String,
    result_sha256: String,
    human_interventions: u64,
    observer: String,
    annotated_at: DateTime<Utc>,
    note: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    evidence: Vec<String>,
}

#[derive(Default)]
pub(super) struct Annotations {
    source_sha256: Option<String>,
    entries: BTreeMap<(String, String), Annotation>,
}

pub(super) struct Coverage {
    pub total: Option<u64>,
    pub annotated: usize,
    pub missing: usize,
}

impl Annotations {
    pub(super) fn load(path: Option<&Path>) -> Result<Self, String> {
        let Some(path) = path else {
            return Ok(Self::default());
        };
        let bytes = read_annotations(path)?;
        let document: Document = serde_json::from_slice(&bytes)
            .map_err(|error| format!("parse interventions {}: {error}", path.display()))?;
        if document.version != 1 {
            return Err(format!(
                "unsupported intervention annotation version {}",
                document.version
            ));
        }
        let mut entries = BTreeMap::new();
        for annotation in document.annotations {
            annotation.validate()?;
            let fingerprint = annotation.fingerprint.clone();
            let hash = annotation.result_sha256.clone();
            if entries
                .insert((fingerprint.clone(), hash.clone()), annotation)
                .is_some()
            {
                return Err(format!(
                    "duplicate intervention annotation for {fingerprint} / {hash}"
                ));
            }
        }
        Ok(Self {
            source_sha256: Some(sha256(&bytes)),
            entries,
        })
    }

    pub(super) fn bind(&self, results: &[ResultIdentity]) -> Result<(), String> {
        let mut known = BTreeMap::new();
        for result in results {
            if let Some(previous) =
                known.insert((&result.fingerprint, &result.result_sha256), result)
            {
                return Err(format!(
                    "ambiguous duplicate result identity {} / {}: {} and {}",
                    result.fingerprint,
                    result.result_sha256,
                    previous.result_path,
                    result.result_path
                ));
            }
        }
        for annotation in self.entries.values() {
            if !known.contains_key(&(&annotation.fingerprint, &annotation.result_sha256)) {
                let problem = if known
                    .keys()
                    .any(|(fingerprint, _)| **fingerprint == annotation.fingerprint)
                {
                    "stale or mismatched intervention annotation: result.json SHA-256 differs"
                } else {
                    "intervention annotation names unknown result"
                };
                return Err(format!(
                    "{problem}: {} / {}",
                    annotation.fingerprint, annotation.result_sha256
                ));
            }
        }
        Ok(())
    }

    pub(super) fn coverage<'a>(
        &self,
        identities: impl Iterator<Item = &'a ResultIdentity>,
    ) -> Result<Coverage, String> {
        let (mut sum, mut annotated, mut missing) = (0u64, 0, 0);
        for identity in identities {
            if let Some(annotation) = self
                .entries
                .get(&(identity.fingerprint.clone(), identity.result_sha256.clone()))
            {
                sum = sum
                    .checked_add(annotation.human_interventions)
                    .ok_or("observer-reported human intervention total overflows u64")?;
                annotated += 1;
            } else {
                missing += 1;
            }
        }
        Ok(Coverage {
            total: (missing == 0 && annotated > 0).then_some(sum),
            annotated,
            missing,
        })
    }

    pub(super) fn provenance(&self) -> Option<serde_json::Value> {
        self.source_sha256.as_ref().map(|hash| {
            serde_json::json!({
                "version": 1, "source_sha256": hash,
                "annotations": self.entries.values().collect::<Vec<_>>()
            })
        })
    }
}

impl Annotation {
    fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("fingerprint", &self.fingerprint),
            ("result_sha256", &self.result_sha256),
        ] {
            if value.len() != 64
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(format!(
                    "intervention {name} must contain 64 lowercase hexadecimal characters"
                ));
            }
        }
        if self.observer.trim().is_empty()
            || self.note.trim().is_empty()
            || self.evidence.iter().any(|item| item.trim().is_empty())
        {
            return Err(
                "intervention observer, note, and any evidence references must be nonempty".into(),
            );
        }
        Ok(())
    }
}

fn read_annotations(path: &Path) -> Result<Vec<u8>, String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|error| format!("read interventions {}: {error}", path.display()))?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_ANNOTATION_BYTES {
        return Err("intervention annotations must be a regular file no larger than 4 MiB".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_ANNOTATION_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_ANNOTATION_BYTES {
        return Err("intervention annotations exceed 4 MiB".into());
    }
    Ok(bytes)
}
