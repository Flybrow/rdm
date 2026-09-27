use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

fn main() {
    embed_extension();
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/rdm.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/rdm.ico").set("ProductName", "RDM").set("FileDescription", "Rust Download Manager");
        res.compile().expect("embed Windows resources");
    }
}

/// The browser extension (`extension/`, tests excluded) goes into the executable: RDM installs it
/// into the browsers and keeps the installed copy up to date (see `src/extension.rs`).
fn embed_extension() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../extension");
    println!("cargo:rerun-if-changed={}", root.display());
    let mut files = Vec::new();
    collect(&root, &root, &mut files);
    files.sort();
    assert!(files.iter().any(|(name, _)| name == "manifest.json"), "extension/manifest.json not found");
    let mut out = String::from("/// Every file of the extension: path inside it (with `/`), contents.\npub const FILES: &[(&str, &[u8])] = &[\n");
    for (name, path) in &files {
        writeln!(out, "    ({name:?}, include_bytes!({:?})),", path.display().to_string()).unwrap();
    }
    out.push_str("];\n");
    fs::write(PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("extension_files.rs"), out).unwrap();
}

fn collect(root: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) {
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        let name = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
        if path.is_dir() {
            if name != "test" {
                collect(root, &path, files);
            }
        } else {
            files.push((name, path));
        }
    }
}
