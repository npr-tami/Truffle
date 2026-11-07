//! Writes `assets/licences.txt`, the list Settings > About shows under "Open-source licences": every
//! crate built into truffle.exe for Windows (its normal dependencies, followed all the way down) with
//! its version and licence, from `cargo metadata`; then the font and the programs Truffle uses that
//! aren't crates. Run it again after changing dependencies:
//!
//! ```text
//! cargo run --example licences
//! ```
//!
//! The file has one line per entry, name, version and licence split by tabs; lines starting with
//! `#` are notes.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::process::Command;

use serde_json::Value;

/// Truffle is built for this target only.
const TARGET: &str = "x86_64-pc-windows-msvc";

/// Not crates: the font (assets/fonts/Inter-LICENSE.txt) and the programs YouTube sound runs.
const OTHERS: &[(&str, &str, &str)] = &[("Inter", "4.001", "OFL-1.1"), ("yt-dlp", "", "Unlicense"), ("Node.js", "", "MIT")];

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let out = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--filter-platform", TARGET])
        .current_dir(root)
        .output()
        .expect("cargo metadata didn't start");
    assert!(out.status.success(), "cargo metadata failed: {}", String::from_utf8_lossy(&out.stderr));
    let meta: Value = serde_json::from_slice(&out.stdout).expect("cargo metadata's answer");

    let text = licences(&meta);
    let path = root.join("assets").join("licences.txt");
    std::fs::write(&path, &text).expect("couldn't write assets/licences.txt");
    println!("{} entries written to {}", text.lines().filter(|l| !l.starts_with('#')).count(), path.display());
}

/// The file's text: the notes, `OTHERS`, then the crates by name.
fn licences(meta: &Value) -> String {
    let packages: HashMap<&str, &Value> =
        meta["packages"].as_array().expect("packages").iter().map(|p| (p["id"].as_str().expect("package id"), p)).collect();
    let nodes: HashMap<&str, &Value> =
        meta["resolve"]["nodes"].as_array().expect("nodes").iter().map(|n| (n["id"].as_str().expect("node id"), n)).collect();
    let root = meta["resolve"]["root"].as_str().expect("the root package");

    // Truffle's normal dependencies and theirs (build and dev dependencies aren't in the exe).
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut todo = vec![root];
    while let Some(id) = todo.pop() {
        if !seen.insert(id) {
            continue;
        }
        for dep in nodes[id]["deps"].as_array().into_iter().flatten() {
            let normal = dep["dep_kinds"].as_array().into_iter().flatten().any(|k| k["kind"].is_null());
            if normal {
                todo.push(dep["pkg"].as_str().expect("dependency id"));
            }
        }
    }
    seen.remove(root);

    let mut crates: Vec<(String, String, String)> = seen
        .iter()
        .map(|id| {
            let p = packages[id];
            let licence = p["license"].as_str().map(str::to_owned).unwrap_or_else(|| "see its licence file".to_owned());
            (p["name"].as_str().unwrap_or_default().to_owned(), p["version"].as_str().unwrap_or_default().to_owned(), licence)
        })
        .collect();
    crates.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()).then_with(|| a.1.cmp(&b.1)));

    let mut text = String::from(
        "# The open-source code and font in Truffle, and the programs its YouTube sound uses, with their\n\
         # licences: name, version and licence, split by tabs. Made by `cargo run --example licences`.\n",
    );
    for (name, version, licence) in OTHERS {
        text.push_str(&format!("{name}\t{version}\t{licence}\n"));
    }
    for (name, version, licence) in crates {
        text.push_str(&format!("{name}\t{version}\t{licence}\n"));
    }
    text
}
