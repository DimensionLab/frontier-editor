// SPDX-License-Identifier: Apache-2.0
//! Provenance hooks: what the app records when a document is saved or a proposal is
//! approved. The core produces the events; the host persists them.

use crate::diff::ChangeSet;
use crate::kind::FileKind;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// `sha256:<hex>` of the UTF-8 text.
pub fn content_hash(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    format!("sha256:{}", hex::encode(h.finalize()))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Author {
    User { id: String },
    Agent { id: String, session: Option<String> },
    System,
}

/// Emitted by [`crate::Editor::save`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct EditEvent {
    pub path: String,
    pub kind: FileKind,
    pub before_hash: String,
    pub after_hash: String,
    /// Hunks between the last saved text and this one.
    pub hunks: Vec<crate::diff::Hunk>,
    pub author: Author,
    /// Milliseconds since the Unix epoch, supplied by the host.
    pub time_ms: u64,
    /// Number of edits (undo nodes) since the previous save.
    #[serde(default)]
    pub edit_count: usize,
}

/// Emitted when a reviewer accepts a subset of a proposal's hunks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ApprovalEvent {
    pub path: String,
    pub kind: FileKind,
    /// Hash of the text the proposal was made against.
    pub base_hash: String,
    /// Hash of the full proposal.
    pub proposed_hash: String,
    /// Hash of the text after applying the accepted hunks.
    pub result_hash: String,
    pub accepted_hunks: Vec<String>,
    pub rejected_hunks: Vec<String>,
    pub approver: Author,
    pub note: Option<String>,
    pub time_ms: u64,
}

impl ApprovalEvent {
    #[allow(clippy::too_many_arguments)]
    pub fn from_changeset(
        path: &str,
        kind: FileKind,
        cs: &ChangeSet,
        result_text: &str,
        accepted: &[&str],
        approver: Author,
        note: Option<String>,
        time_ms: u64,
    ) -> Self {
        let accepted_hunks: Vec<String> = cs
            .hunks
            .iter()
            .filter(|h| accepted.contains(&h.id.as_str()))
            .map(|h| h.id.clone())
            .collect();
        let rejected_hunks = cs
            .hunks
            .iter()
            .filter(|h| !accepted.contains(&h.id.as_str()))
            .map(|h| h.id.clone())
            .collect();
        Self {
            path: path.to_string(),
            kind,
            base_hash: cs.base_hash.clone(),
            proposed_hash: cs.proposed_hash.clone(),
            result_hash: content_hash(result_text),
            accepted_hunks,
            rejected_hunks,
            approver,
            note,
            time_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable() {
        assert_eq!(
            content_hash(""),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
