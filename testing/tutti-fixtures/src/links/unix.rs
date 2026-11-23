//! Unix: a published plugin is a symlink to the artifact.

use std::path::Path;

/// Whether `link` already names `real`; a symlink is resolved at each open, so
/// one naming `real` is as fresh as `real`.
pub(crate) fn is_published(link: &Path, real: &Path) -> bool {
    std::fs::read_link(link).is_ok_and(|to| to == real)
}

/// Makes `staging` a symlink to `real`.
pub(crate) fn stage(real: &Path, staging: &Path) {
    std::os::unix::fs::symlink(real, staging).expect("stage the reference plugin symlink");
}

#[cfg(test)]
mod tests {
    use crate::publish_with_extension;
    use std::io::Write;

    /// **A link already naming the artifact is not republished.** The
    /// link's own inode is the witness: a republish renames a fresh symlink
    /// over it, which a second call must not do.
    #[test]
    fn a_published_link_is_left_alone() {
        use std::os::unix::fs::MetadataExt;
        let dir = std::env::temp_dir().join(format!("tfr-link-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let real = dir.join("libplugin.so");
        std::fs::File::create(&real)
            .unwrap()
            .write_all(b"x")
            .unwrap();
        let link = publish_with_extension(&real, "clap");
        assert_eq!(link, dir.join("libplugin.clap"));
        assert_eq!(std::fs::read_link(&link).unwrap(), real);
        let first = std::fs::symlink_metadata(&link).unwrap().ino();
        assert_eq!(publish_with_extension(&real, "clap"), link);
        let second = std::fs::symlink_metadata(&link).unwrap().ino();
        assert_eq!(first, second, "the second publish replaced the link");

        // A link naming something else is replaced.
        let other = dir.join("other.so");
        std::fs::File::create(&other).unwrap();
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&other, &link).unwrap();
        publish_with_extension(&real, "clap");
        assert_eq!(std::fs::read_link(&link).unwrap(), real);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
