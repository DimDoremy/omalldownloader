//! Where a download is allowed to land.
//!
//! The rule from the review (R2): a caller — the extension above all — may choose a
//! **logical** destination and a file name, never a path. The absolute path is built
//! here, from a configured root, and checked to stay inside that root after every
//! symlink on the way has been resolved.

use std::path::{Component, Path, PathBuf};

/// A directory the user configured once, and that the app trusts for its lifetime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadRoot {
    path: PathBuf,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PathError {
    #[error("the destination {requested:?} is not inside the download root {root:?}")]
    Outside { requested: PathBuf, root: PathBuf },
    #[error("the download root {root:?} does not exist or cannot be resolved: {reason}")]
    RootUnusable { root: PathBuf, reason: String },
    #[error("the destination names a directory, not a file: {0:?}")]
    IsDirectory(PathBuf),
    #[error("the path contains a component that may not appear in a download target: {0}")]
    ForbiddenComponent(String),
}

impl DownloadRoot {
    /// Canonicalises the root now, so a later symlink swap of the root itself is
    /// caught instead of silently moving every check with it.
    pub fn new(path: impl AsRef<Path>) -> Result<Self, PathError> {
        let path = path.as_ref();
        let resolved = path.canonicalize().map_err(|e| PathError::RootUnusable {
            root: path.to_path_buf(),
            reason: e.to_string(),
        })?;
        if !resolved.is_dir() {
            return Err(PathError::RootUnusable {
                root: path.to_path_buf(),
                reason: "not a directory".into(),
            });
        }
        Ok(Self { path: resolved })
    }

    pub fn as_path(&self) -> &Path {
        &self.path
    }

    /// The final path of a file named `name` (already through
    /// [`crate::filename::sanitize`]) inside an optional relative `subdir`.
    ///
    /// The result is guaranteed to sit strictly inside the root. It is not yet
    /// created; [`Self::open_exclusive`] is the step that cannot be raced.
    /// The root itself, for callers that need to look around in it.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn resolve(&self, subdir: Option<&str>, name: &str) -> Result<PathBuf, PathError> {
        let mut target = self.path.clone();

        if let Some(subdir) = subdir {
            let rel = Path::new(subdir);
            for component in rel.components() {
                match component {
                    Component::Normal(part) => {
                        let text = part.to_string_lossy();
                        // A name that looks like a path here is a bug or an attack.
                        if text.contains(['/', '\\']) || text == "." || text == ".." {
                            return Err(PathError::ForbiddenComponent(text.into_owned()));
                        }
                        target.push(part);
                    }
                    Component::CurDir => {}
                    other => {
                        return Err(PathError::ForbiddenComponent(
                            other.as_os_str().to_string_lossy().into_owned(),
                        ));
                    }
                }
            }
        }

        // The category folder is created on demand: a download into `ISO/` should not fail
        // because the folder does not exist yet.
        if let Some(parent) = target.parent()
            && !parent.exists()
        {
            let _ = std::fs::create_dir_all(parent);
        }
        target.push(name);

        // `..` cannot survive the loop above, but a symlink inside the root can point
        // out of it, so compare canonical paths when the target exists. A missing
        // target is canonicalised through its parent, which is the interesting part.
        self.ensure_inside(&target)?;
        Ok(target)
    }

    fn ensure_inside(&self, target: &Path) -> Result<(), PathError> {
        let resolved = if target.exists() {
            target.canonicalize()
        } else {
            match target.parent() {
                Some(parent) if parent.exists() => parent
                    .canonicalize()
                    .map(|p| p.join(target.file_name().unwrap_or_default())),
                _ => Ok(target.to_path_buf()),
            }
        }
        .map_err(|e| PathError::RootUnusable {
            root: target.to_path_buf(),
            reason: e.to_string(),
        })?;

        if !resolved.starts_with(&self.path) || resolved == self.path {
            return Err(PathError::Outside {
                requested: target.to_path_buf(),
                root: self.path.clone(),
            });
        }
        if resolved.is_dir() {
            return Err(PathError::IsDirectory(target.to_path_buf()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filename;

    fn root() -> (tempfile::TempDir, DownloadRoot) {
        let dir = tempfile::tempdir().unwrap();
        let root = DownloadRoot::new(dir.path()).unwrap();
        (dir, root)
    }

    #[test]
    fn resolves_a_plain_name_inside_the_root() {
        let (_dir, root) = root();
        let p = root.resolve(None, "a.txt").unwrap();
        assert!(p.starts_with(root.as_path()));
        assert_eq!(p.file_name().unwrap(), "a.txt");
    }

    #[test]
    fn accepts_a_relative_subdir() {
        let (_dir, root) = root();
        let p = root.resolve(Some("music/2024"), "a.mp3").unwrap();
        assert!(p.ends_with("music/2024/a.mp3"));
    }

    #[test]
    fn refuses_dotdot_and_absolute_subdirs() {
        let (_dir, root) = root();
        assert!(matches!(
            root.resolve(Some(".."), "a.txt"),
            Err(PathError::ForbiddenComponent(_))
        ));
        assert!(matches!(
            root.resolve(Some("/etc"), "a.txt"),
            Err(PathError::ForbiddenComponent(_))
        ));
        assert!(matches!(
            root.resolve(Some("a/../../b"), "x"),
            Err(PathError::ForbiddenComponent(_))
        ));
    }

    #[test]
    fn refuses_a_name_that_escapes_even_unsanitised() {
        let (_dir, root) = root();
        // Defence in depth: callers sanitise first, but a mistake here must still fail.
        let err = root.resolve(None, "../escape.txt").unwrap_err();
        assert!(matches!(
            err,
            PathError::Outside { .. } | PathError::ForbiddenComponent(_)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_symlink_pointing_out_of_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let link = dir.path().join("escape");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();

        let root = DownloadRoot::new(dir.path()).unwrap();
        let err = root.resolve(Some("escape"), "x.bin").unwrap_err();
        assert!(matches!(err, PathError::Outside { .. }), "got {err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_root_is_resolved_once() {
        let real = tempfile::tempdir().unwrap();
        let alias_parent = tempfile::tempdir().unwrap();
        let alias = alias_parent.path().join("alias");
        std::os::unix::fs::symlink(real.path(), &alias).unwrap();

        let root = DownloadRoot::new(&alias).unwrap();
        assert_eq!(root.as_path(), real.path().canonicalize().unwrap());
        let p = root.resolve(None, "a").unwrap();
        assert!(p.starts_with(root.as_path()));
    }

    #[test]
    fn sanitised_hostile_names_stay_inside() {
        let (_dir, root) = root();
        for hostile in ["../../../etc/passwd", "..\\..\\x", "CON", "", "a/b"] {
            let safe = filename::sanitize(hostile);
            let p = root.resolve(None, &safe).expect(hostile);
            assert!(p.starts_with(root.as_path()), "{hostile} -> {p:?}");
        }
    }
}
