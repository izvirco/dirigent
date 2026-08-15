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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct VersionArtifact {
    pub target: String,
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
    /// Also used as the multipart field name for this artifact.
    pub target: String,
    pub file_name: String,
}
