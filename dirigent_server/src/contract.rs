//! Types shared between the Dirigent version API and its clients.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct VersionResponse {
    pub channel: String,
    pub version: String,
    pub commit: String,
    pub published_at: String,
    pub artifacts: Vec<VersionArtifact>,
}

/// Application binaries are updater payloads; installers are user-facing downloads
/// (a setup executable on Windows, an installation archive on Linux).
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Application,
    Installer,
}

impl ArtifactKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Application => "application",
            Self::Installer => "installer",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct VersionArtifact {
    pub target: String,
    pub kind: ArtifactKind,
    pub file_name: String,
    pub url: String,
    pub size: u64,
    pub sha256: String,
}

/// Metadata sent in the `manifest` field of the publishing multipart request.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PublishVersion {
    pub version: String,
    pub commit: String,
    pub artifacts: Vec<PublishArtifact>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PublishArtifact {
    pub target: String,
    pub kind: ArtifactKind,
    pub file_name: String,
}

impl PublishArtifact {
    /// Distinguishes installation downloads from updater payloads for the same target.
    pub fn multipart_field(&self) -> String {
        format!("{}-{}", self.target, self.kind.as_str())
    }
}
