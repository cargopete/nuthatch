//! Content-addressed nest packaging (RFC-0012 §4): turn a nest directory into a **blob** — its
//! *authored inputs* canonicalised and pinned by a Merkle-root hash, so a nest becomes a deploy unit
//! (`nest pack` here; `nest mount` verifies + installs one, a later slice).
//!
//! The blob pins **inputs** (`nuthatch.toml`, ABIs, views, labels, skills, `schema.json`, `llms.txt`),
//! never build artifacts (the generated decode registry) or sealed data (`segments/`, `nuthatch.redb`).
//! Instead the manifest records the *expected* `registry_hash`; a `mount` regenerates the registry from
//! the packed inputs and asserts it matches — extending determinism from the data path (RFC-0009's
//! content-addressed segments) to the *authoring* path: same inputs + same generator → same blob →
//! same decode, verifiably. The blob hash is `sha256` over the **canonical** manifest (fixed field
//! order, files sorted by path, compact encoding), reusing the seal-manifest discipline, not new crypto.

use crate::config::{Config, DB_FILE};
use crate::registry::DecodeRegistry;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Blob manifest schema version. Bumped only on an incompatible manifest-shape change; a blob whose
/// version this build doesn't understand is rejected on mount (like `schema_version` in `config.rs`).
pub const BLOB_FORMAT_VERSION: u32 = 1;

/// Files/dirs never included in a blob: the hot store and sealed data are *derived*, not authored, and
/// including them would make the hash depend on runtime state instead of inputs. Matched by exact name
/// at any depth.
const EXCLUDE: &[&str] = &[DB_FILE, "segments", ".git", ".DS_Store"];

/// One packed input file: its path relative to the nest root and the hash of its bytes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    pub sha256: String,
}

/// The blob manifest — the content-addressed declaration of a nest's inputs. Field order here IS the
/// canonical order (serde preserves declaration order); `files` is sorted by path. Do not reorder
/// fields without bumping [`BLOB_FORMAT_VERSION`] — the order is load-bearing for the blob hash.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub blob_format_version: u32,
    pub nest_name: String,
    pub schema_version: u32,
    /// The nuthatch version that produced (and can reproduce) this blob's decode registry.
    pub generator_version: String,
    /// The expected decode-registry hash — a mount regenerates the registry from `files` and asserts
    /// it equals this. Hex, no `0x` (matches the seal manifest's convention).
    pub registry_hash: String,
    /// Every authored input, sorted by `path`. A Merkle layer: each file hashed, the sorted list
    /// then folded into the blob hash via the canonical manifest.
    pub files: Vec<FileEntry>,
}

impl Manifest {
    /// The canonical byte serialization the blob hash is taken over: compact JSON (no incidental
    /// whitespace), fixed field order, `files` pre-sorted. Deterministic across machines.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        // `to_vec` is compact (no pretty whitespace); struct field order is fixed; `files` is sorted at
        // build time. serde_json preserves map/struct key order as declared, so this is stable.
        serde_json::to_vec(self).expect("manifest serializes")
    }

    /// The blob hash: `sha256` of the canonical manifest bytes, hex-encoded. This is the nest's
    /// content address — the thing `mount <hash>` resolves.
    pub fn blob_hash(&self) -> String {
        hex::encode(Sha256::digest(self.canonical_bytes()))
    }
}

/// Recursively collect the authored input files under `root`, relative-pathed and sorted, skipping the
/// [`EXCLUDE`] set (and `skip`, e.g. the output dir when it sits inside the nest). Deterministic order.
fn collect_files(root: &Path, skip: Option<&Path>) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .with_context(|| format!("reading {}", dir.display()))?
            .collect::<std::io::Result<_>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if EXCLUDE.iter().any(|x| *x == name) {
                continue;
            }
            if let Some(skip) = skip {
                if path == skip {
                    continue;
                }
            }
            let ft = entry.file_type()?;
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                out.push(path);
            }
            // Symlinks are deliberately ignored — a blob must be self-contained.
        }
    }
    out.sort();
    Ok(out)
}

/// Build the manifest for the nest at `dir` without writing anything — hashes every authored input and
/// records the regenerated `registry_hash`. Shared by `pack` and (later) `mount`'s verify.
pub fn build_manifest(dir: &Path, skip_out: Option<&Path>) -> Result<Manifest> {
    let config = Config::load(dir).context("loading nest config for pack")?;
    // Regenerate the decode registry from the *inputs* (toml + ABIs) so the manifest pins what a mount
    // must reproduce — never a stored artifact.
    let registry = DecodeRegistry::from_nest(dir, &config).context("building decode registry")?;
    let registry_hash = hex::encode(registry.hash());

    let files = collect_files(dir, skip_out)?
        .into_iter()
        .map(|path| {
            let bytes =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            let rel = path
                .strip_prefix(dir)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/"); // stable path separator across platforms
            Ok(FileEntry {
                path: rel,
                sha256: hex::encode(Sha256::digest(&bytes)),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    if files.is_empty() {
        bail!("nothing to pack in {} (no input files)", dir.display());
    }

    Ok(Manifest {
        blob_format_version: BLOB_FORMAT_VERSION,
        nest_name: config.nest.name,
        schema_version: config.nest.schema_version,
        generator_version: env!("CARGO_PKG_VERSION").to_string(),
        registry_hash,
        files,
    })
}

/// `nuthatch nest pack <dir> [--out <path>]`: write a content-addressed blob of the nest — a directory
/// holding the authored inputs plus `manifest.json`. Prints the blob hash. Default output dir is
/// `<nest-name>-<hash12>.nest/` beside the nest.
pub fn pack(dir: &Path, out: Option<&Path>) -> Result<()> {
    // Compute the output path first so it can be excluded from the walk when it sits inside `dir`.
    let manifest = build_manifest(dir, None)?;
    let hash = manifest.blob_hash();

    let out_dir = match out {
        Some(p) => p.to_path_buf(),
        None => {
            let parent = dir.parent().unwrap_or_else(|| Path::new("."));
            parent.join(format!("{}-{}.nest", manifest.nest_name, &hash[..12]))
        }
    };

    // If the chosen output dir is *inside* the nest, rebuild the manifest excluding it (so the blob
    // doesn't try to pack itself). Rare, but a foot-gun worth closing.
    let (manifest, hash) = if out_dir.starts_with(dir) {
        let m = build_manifest(dir, Some(&out_dir))?;
        let h = m.blob_hash();
        (m, h)
    } else {
        (manifest, hash)
    };

    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("creating blob dir {}", out_dir.display()))?;
    for f in &manifest.files {
        let src = dir.join(&f.path);
        let dst = out_dir.join(&f.path);
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::copy(&src, &dst).with_context(|| format!("copying {}", f.path))?;
    }
    // Pretty-print the *stored* manifest for human readability; the blob hash is over the canonical
    // (compact) bytes, so on-disk formatting never affects identity.
    std::fs::write(
        out_dir.join("manifest.json"),
        serde_json::to_string_pretty(&manifest)?,
    )
    .context("writing manifest.json")?;

    println!("packed nest '{}'", manifest.nest_name);
    println!("  blob:     {hash}");
    println!("  registry: {}", manifest.registry_hash);
    println!("  files:    {}", manifest.files.len());
    println!("  out:      {}", out_dir.display());
    Ok(())
}

/// Verify that a nest dir's inputs reproduce the `registry_hash` a manifest claims — the check `mount`
/// will run. Kept here so `pack` and mount share one definition of "does this blob decode as promised".
pub fn verify_registry_reproduces(dir: &Path, manifest: &Manifest) -> Result<()> {
    let config = Config::load(dir)?;
    let regen = hex::encode(DecodeRegistry::from_nest(dir, &config)?.hash());
    if regen != manifest.registry_hash {
        bail!(
            "registry hash mismatch: manifest claims {}, inputs regenerate {} — the blob was authored \
             by a different generator version",
            manifest.registry_hash,
            regen
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CONFIG_FILE;

    /// A minimal nest dir (config + one ABI) for exercising pack.
    fn write_nest(dir: &Path) {
        std::fs::write(
            dir.join(CONFIG_FILE),
            r#"[nest]
name = "t"
chain = "arbitrum-one"
chain_id = 42161
rpc_urls = ["https://x"]
schema_version = 1

[[contracts]]
alias = "c"
address = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
abi = "abis/c.json"
"#,
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("abis")).unwrap();
        std::fs::write(
            dir.join("abis/c.json"),
            r#"[{"type":"event","name":"Transfer","anonymous":false,"inputs":[{"name":"from","type":"address","indexed":true},{"name":"to","type":"address","indexed":true},{"name":"value","type":"uint256","indexed":false}]}]"#,
        )
        .unwrap();
        std::fs::write(dir.join("llms.txt"), "how to query this nest\n").unwrap();
    }

    #[test]
    fn manifest_is_deterministic_and_pins_the_registry_hash() {
        let a = tempfile::tempdir().unwrap();
        write_nest(a.path());
        let m1 = build_manifest(a.path(), None).unwrap();
        let m2 = build_manifest(a.path(), None).unwrap();
        // Same inputs → byte-identical canonical manifest → identical blob hash (the determinism rule).
        assert_eq!(m1.blob_hash(), m2.blob_hash());
        assert_eq!(m1.canonical_bytes(), m2.canonical_bytes());
        // The manifest pins the regenerated decode registry, and it verifies against the inputs.
        let config = Config::load(a.path()).unwrap();
        let expected = hex::encode(DecodeRegistry::from_nest(a.path(), &config).unwrap().hash());
        assert_eq!(m1.registry_hash, expected);
        verify_registry_reproduces(a.path(), &m1).unwrap();
        // Files are sorted and exclude nothing authored (config + abi + llms.txt = 3).
        assert_eq!(m1.files.len(), 3);
        assert!(m1.files.windows(2).all(|w| w[0].path <= w[1].path));
    }

    #[test]
    fn a_changed_input_changes_the_blob_hash() {
        let a = tempfile::tempdir().unwrap();
        write_nest(a.path());
        let before = build_manifest(a.path(), None).unwrap().blob_hash();
        // Touch an authored input.
        std::fs::write(a.path().join("llms.txt"), "different docs\n").unwrap();
        let after = build_manifest(a.path(), None).unwrap().blob_hash();
        assert_ne!(
            before, after,
            "the blob hash is content-addressed over its inputs"
        );
    }

    #[test]
    fn derived_files_are_excluded() {
        let a = tempfile::tempdir().unwrap();
        write_nest(a.path());
        // Simulate a run: a hot store + sealed segments appear. Neither must enter the blob.
        std::fs::write(a.path().join(DB_FILE), b"redb bytes").unwrap();
        std::fs::create_dir_all(a.path().join("segments")).unwrap();
        std::fs::write(a.path().join("segments/x.parquet"), b"parquet").unwrap();
        let m = build_manifest(a.path(), None).unwrap();
        assert!(m
            .files
            .iter()
            .all(|f| f.path != DB_FILE && !f.path.starts_with("segments/")));
        assert_eq!(m.files.len(), 3, "still just the 3 authored inputs");
    }
}
