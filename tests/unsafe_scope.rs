//! `unsafe` is confined to one audited file; anywhere else the word fails the build.

use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
};

const ALLOWED: &str = "src/sources/hid_feature.rs";

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), Box<dyn Error>> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            rust_files(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

#[test]
fn unsafe_appears_only_in_the_feature_report_module() -> Result<(), Box<dyn Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files)?;
    let mut offenders = Vec::new();
    for file in files {
        let rel = file.strip_prefix(root)?.to_string_lossy().into_owned();
        if rel == ALLOWED {
            continue;
        }
        let text = fs::read_to_string(&file)?;
        let hit = text
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .any(|word| word == "unsafe");
        if hit {
            offenders.push(rel);
        }
    }
    assert!(
        offenders.is_empty(),
        "`unsafe` outside {ALLOWED}: {offenders:?}"
    );
    Ok(())
}
