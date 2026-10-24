//! Port of `src/fastbrowse/artifacts.py`: a filesystem `ArtifactSink`. Hosts with their own storage implement the
//! trait instead.

use std::path::{Path, PathBuf};

use crate::BoxFuture;
use crate::models::{Artifact, ArtifactKind, ArtifactSink};
use crate::py::{sha256_hex, text};

#[derive(Debug, Clone)]
pub struct DirectorySink {
    root: PathBuf,
}

impl DirectorySink {
    /// The root made absolute now, as `Path.resolve()` does, so a later change of directory moves nothing.
    pub fn new(root: impl AsRef<Path>) -> std::io::Result<DirectorySink> {
        let root = root.as_ref();
        let root = if root.exists() { root.canonicalize()? } else { std::path::absolute(root)? };
        Ok(DirectorySink { root })
    }
}

/// `Path.as_uri()`: `file://` and the path percent-encoded with `/` kept, as `urllib.parse.quote` encodes it.
fn file_uri(path: &Path) -> String {
    format!("file://{}", text::quote_path(&path.to_string_lossy()))
}

/// The last component of `name`, as `Path(name).name`: a hostile name cannot climb out with `..` or a separator.
fn base_name(name: &str) -> &str {
    match name.rsplit('/').next().unwrap_or("") {
        "." | ".." => "",
        last => last,
    }
}

impl ArtifactSink for DirectorySink {
    fn put<'a>(
        &'a self,
        kind: ArtifactKind,
        name: &'a str,
        mime_type: &'a str,
        content: &'a [u8],
    ) -> BoxFuture<'a, std::io::Result<Artifact>> {
        Box::pin(async move {
            let digest = sha256_hex(content);
            // Content-addressed so a hostile filename can neither escape the root nor overwrite another artifact.
            let file = match base_name(name) {
                "" => "artifact",
                file => file,
            };
            let path = self.root.join(kind.as_str()).join(&digest[..16]).join(file);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, content)?;
            Ok(Artifact {
                kind,
                name: name.to_string(),
                mime_type: mime_type.to_string(),
                size_bytes: content.len() as i64,
                sha256: digest,
                uri: file_uri(&path),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::block_on;

    #[test]
    fn an_artifact_is_stored_by_its_content_under_its_own_name() {
        let root = std::env::temp_dir().join(format!("fastbrowse-artifacts-{}", std::process::id()));
        let sink = DirectorySink::new(&root).unwrap();
        let artifact = block_on(sink.put(ArtifactKind::Download, "../../a b(1).csv", "text/csv", b"x,y\n")).unwrap();
        let digest = sha256_hex(b"x,y\n");
        assert_eq!(artifact.sha256, digest);
        assert_eq!(artifact.size_bytes, 4);
        assert!(artifact.uri.ends_with(&format!("/download/{}/a%20b%281%29.csv", &digest[..16])), "{}", artifact.uri);
        assert!(artifact.uri.starts_with("file:///"));
        let unnamed = block_on(sink.put(ArtifactKind::Download, "..", "text/plain", b"")).unwrap();
        assert!(unnamed.uri.ends_with("/artifact"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
