use std::path::{Path, PathBuf};

/// Match production preparation: Seatbelt evaluates resolved filesystem paths.
pub fn redirect_root(path: impl AsRef<Path>) -> PathBuf {
    let root = path
        .as_ref()
        .canonicalize()
        .expect("resolve UMBRA_TEST_REDIRECT_ROOT");
    assert!(
        root.is_absolute(),
        "redirect root must resolve to an absolute path"
    );
    assert!(root.is_dir(), "redirect root must resolve to a directory");
    root
}
