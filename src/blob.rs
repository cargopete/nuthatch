//! Content-addressed nest packaging (RFC-0012 §4): turn a nest directory into an **egg** — its
//! *authored inputs* canonicalised and pinned by a Merkle-root hash, so a nest becomes a portable
//! deploy unit. `nest egg` lays a single `.egg` file (`egg` here); `nest hatch` verifies + installs
//! one (`hatch`), resolving a `.egg` file, an `http(s)` URL, or an unpacked blob dir.
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

/// `nuthatch nest egg <dir> [--out <path>] [--as-dir]`: lay an *egg* — a single portable,
/// content-addressed `.egg` file holding the nest's authored inputs plus `manifest.json`. With
/// `--as-dir`, write the old unpacked blob *directory* instead (handy for inspecting contents). Prints
/// the egg's content address. Default output is `<nest-name>-<hash12>.egg` beside the nest.
pub fn egg(dir: &Path, out: Option<&Path>, as_dir: bool) -> Result<()> {
    let manifest = build_manifest(dir, None)?;
    let hash = manifest.blob_hash();
    let default_out = |ext: &str| {
        let parent = dir.parent().unwrap_or_else(|| Path::new("."));
        parent.join(format!("{}-{}.{ext}", manifest.nest_name, &hash[..12]))
    };

    if as_dir {
        let out_dir = out
            .map(Path::to_path_buf)
            .unwrap_or_else(|| default_out("nest"));
        // If the chosen output dir is *inside* the nest, rebuild the manifest excluding it (so the blob
        // doesn't try to pack itself). Rare, but a foot-gun worth closing.
        let (manifest, hash) = if out_dir.starts_with(dir) {
            let m = build_manifest(dir, Some(&out_dir))?;
            let h = m.blob_hash();
            (m, h)
        } else {
            (manifest, hash)
        };
        write_blob_dir(dir, &out_dir, &manifest)?;
        println!("laid egg (unpacked) for nest '{}'", manifest.nest_name);
        println!("  dir:      {}", out_dir.display());
        println!("  hash:     {hash}");
        println!("  registry: {}", manifest.registry_hash);
        println!("  files:    {}", manifest.files.len());
        return Ok(());
    }

    let out_file = out
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_out("egg"));
    write_egg(dir, &manifest, &out_file)?;
    println!("laid egg for nest '{}'", manifest.nest_name);
    println!("  egg:      {}", out_file.display());
    println!("  hash:     {hash}");
    println!("  registry: {}", manifest.registry_hash);
    println!("  files:    {}", manifest.files.len());
    println!();
    println!("tip: share this .egg (a URL, or the file). Anyone can run your exact nest with");
    println!(
        "     `nuthatch nest hatch <file-or-url>` — every file is verified against the manifest,"
    );
    println!(
        "     and the decode registry is reproduced from the inputs. Pin it with `--expect {}`.",
        &hash[..12]
    );
    Ok(())
}

/// Materialise a blob's files (from the nest `src`) plus `manifest.json` into `out_dir` — the unpacked
/// on-disk layout shared by the `--as-dir` form and, tarred, the `.egg`.
fn write_blob_dir(src: &Path, out_dir: &Path, manifest: &Manifest) -> Result<()> {
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("creating blob dir {}", out_dir.display()))?;
    for f in &manifest.files {
        let dst = out_dir.join(&f.path);
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::copy(src.join(&f.path), &dst).with_context(|| format!("copying {}", f.path))?;
    }
    // Pretty-print the *stored* manifest for human readability; the blob hash is over the canonical
    // (compact) bytes, so on-disk formatting never affects identity.
    std::fs::write(
        out_dir.join("manifest.json"),
        serde_json::to_string_pretty(manifest)?,
    )
    .context("writing manifest.json")?;
    Ok(())
}

/// Write a single-file `.egg`: a tar of `manifest.json` + every manifest file, read from the nest
/// `src`. The egg's *identity* is `manifest.blob_hash()` (over the canonical manifest), so the tar's
/// own byte layout is immaterial — a hatch re-verifies each file against the manifest regardless.
fn write_egg(src: &Path, manifest: &Manifest, out_file: &Path) -> Result<()> {
    let file = std::fs::File::create(out_file)
        .with_context(|| format!("creating egg {}", out_file.display()))?;
    let mut ar = tar::Builder::new(file);

    let manifest_bytes = serde_json::to_vec_pretty(manifest)?;
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest_bytes.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_cksum();
    ar.append_data(&mut header, "manifest.json", &manifest_bytes[..])
        .context("adding manifest.json to egg")?;

    for f in &manifest.files {
        ar.append_path_with_name(src.join(&f.path), Path::new(&f.path))
            .with_context(|| format!("adding {} to egg", f.path))?;
    }
    ar.finish().context("finalising egg")?;
    Ok(())
}

/// Read + parse a blob's `manifest.json`.
pub fn load_manifest(blob_dir: &Path) -> Result<Manifest> {
    let raw = std::fs::read_to_string(blob_dir.join("manifest.json"))
        .with_context(|| format!("reading blob manifest in {}", blob_dir.display()))?;
    serde_json::from_str(&raw).context("parsing blob manifest")
}

/// Join a **manifest-declared** relative path onto `base`, refusing anything that could escape it — an
/// egg is a distributable, hash-resolved deploy unit (RFC-0012 §4/§5), so its file paths are untrusted
/// input. Only `Normal` path components are allowed: an absolute path (which `Path::join` would let
/// *replace* the base), a `..` parent, a root/prefix, or a bare `.` are all rejected. This is the
/// zip-slip / absolute-path-escape guard for `hatch`.
fn checked_join(base: &Path, rel: &str) -> Result<PathBuf> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        bail!("blob file path {rel:?} is absolute — refusing (path-traversal guard)");
    }
    for comp in rel_path.components() {
        if !matches!(comp, std::path::Component::Normal(_)) {
            bail!(
                "blob file path {rel:?} has an illegal component — refusing (path-traversal guard)"
            );
        }
    }
    Ok(base.join(rel_path))
}

/// `nuthatch nest hatch <egg> [--dir <target>] [--expect <hash>]`: hatch an egg into a runnable nest.
/// `egg` may be a `.egg` file, an `http(s)://` URL to one, or an already-unpacked blob directory. A URL
/// or file is resolved to a local blob dir (fetch + untar), then verified and installed by
/// [`install_verified`]. The fetch is the *only* network touch and only when you pass a URL — a local
/// `.egg` or dir hatches fully offline.
pub async fn hatch(egg: &str, target: Option<&Path>, expect: Option<&str>) -> Result<()> {
    if egg.starts_with("http://") || egg.starts_with("https://") {
        let bytes = reqwest::get(egg)
            .await
            .with_context(|| format!("fetching egg from {egg}"))?
            .error_for_status()
            .with_context(|| format!("fetching egg from {egg}"))?
            .bytes()
            .await
            .context("reading fetched egg bytes")?;
        let tmp = tempfile::tempdir().context("temp dir for fetched egg")?;
        let egg_file = tmp.path().join("fetched.egg");
        std::fs::write(&egg_file, &bytes).context("writing fetched egg")?;
        let blob_dir = tmp.path().join("unpacked");
        extract_egg(&egg_file, &blob_dir)?;
        return install_verified(&blob_dir, target, expect);
    }
    let path = Path::new(egg);
    if path.is_dir() {
        // Already an unpacked blob directory (e.g. `egg --as-dir` output) — install straight from it.
        return install_verified(path, target, expect);
    }
    if path.is_file() {
        let tmp = tempfile::tempdir().context("temp dir for egg")?;
        let blob_dir = tmp.path().join("unpacked");
        extract_egg(path, &blob_dir)?;
        return install_verified(&blob_dir, target, expect);
    }
    bail!("nothing to hatch at '{egg}' — expected a .egg file, an http(s):// URL, or a blob directory");
}

/// Untar a `.egg` into `dest`. `tar`'s `unpack` refuses entries that would escape `dest` (its own
/// zip-slip guard); [`install_verified`] then re-checks every *manifest-declared* file with
/// [`checked_join`], so extraction and install are both bounded — defence in depth on untrusted input.
fn extract_egg(egg_file: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(egg_file)
        .with_context(|| format!("opening egg {}", egg_file.display()))?;
    std::fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    tar::Archive::new(file)
        .unpack(dest)
        .with_context(|| format!("unpacking egg {}", egg_file.display()))?;
    Ok(())
}

/// Verify a resolved blob directory and install it as a runnable nest. Verification is three-fold, all
/// local (RFC-0012 §5): the manifest's format version is understood, every file's bytes hash to what
/// the manifest claims (integrity), and — after install — the decode registry *regenerated from the
/// installed inputs* equals the manifest's `registry_hash` (the nest decodes exactly as authored). With
/// `expect`, the blob's own content address is asserted too, so you hatch *that* egg and no other.
pub fn install_verified(
    blob_dir: &Path,
    target: Option<&Path>,
    expect: Option<&str>,
) -> Result<()> {
    let manifest = load_manifest(blob_dir)?;

    // Format gate — reject a blob authored by a newer nuthatch, exactly as `config.rs` rejects a newer
    // schema_version. A too-new manifest may hash/verify by rules this build doesn't know.
    if manifest.blob_format_version > BLOB_FORMAT_VERSION {
        bail!(
            "blob needs manifest format v{} but this build understands up to v{} — upgrade nuthatch",
            manifest.blob_format_version,
            BLOB_FORMAT_VERSION
        );
    }

    // Content-address check (optional): the blob you asked for is the blob you got.
    let hash = manifest.blob_hash();
    if let Some(want) = expect {
        if hash != want {
            bail!("blob hash mismatch: expected {want}, got {hash}");
        }
    }

    // Integrity: every file's bytes hash to the manifest's claim.
    for f in &manifest.files {
        let bytes = std::fs::read(checked_join(blob_dir, &f.path)?)
            .with_context(|| format!("blob is missing declared file {}", f.path))?;
        let got = hex::encode(Sha256::digest(&bytes));
        if got != f.sha256 {
            bail!(
                "blob file {} is corrupt: manifest {}, actual {got}",
                f.path,
                f.sha256
            );
        }
    }

    let target = match target {
        Some(t) => t.to_path_buf(),
        None => PathBuf::from(&manifest.nest_name),
    };
    if target.exists() && std::fs::read_dir(&target)?.next().is_some() {
        bail!("target {} exists and is not empty", target.display());
    }
    for f in &manifest.files {
        let dst = checked_join(&target, &f.path)?;
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::copy(checked_join(blob_dir, &f.path)?, &dst)
            .with_context(|| format!("installing {}", f.path))?;
    }

    // The load-bearing check: regenerate the decode registry from the *installed* inputs and assert it
    // matches the manifest. Same inputs + same generator → same decode, verifiably.
    verify_registry_reproduces(&target, &manifest)?;

    println!(
        "hatched nest '{}' → {}",
        manifest.nest_name,
        target.display()
    );
    println!("  hash:     {hash}");
    println!(
        "  registry: {} (reproduced from inputs ✓)",
        manifest.registry_hash
    );
    println!();
    println!(
        "tip: it's yours to run — `nuthatch dev --dir {}`. It decodes byte-for-byte as the author",
        target.display()
    );
    println!("     laid it (every file hashed, registry reproduced from inputs).");
    Ok(())
}

/// Verify that a nest dir's inputs reproduce the `registry_hash` a manifest claims — the check `hatch`
/// runs. Kept here so `egg` and hatch share one definition of "does this blob decode as promised".
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

    /// SEC-1: a blob is untrusted, distributable input — `mount` must refuse a manifest whose file
    /// paths would escape the target (zip-slip `../` or an absolute path, which `Path::join` would let
    /// *replace* the base). Otherwise mounting a hostile blob is an arbitrary file write → RCE.
    #[test]
    fn mount_refuses_path_traversal() {
        for evil_path in [
            "../escaped.txt",
            "/tmp/nuthatch-escape.txt",
            "a/../../escaped.txt",
        ] {
            let blob = tempfile::tempdir().unwrap();
            let manifest = Manifest {
                blob_format_version: BLOB_FORMAT_VERSION,
                nest_name: "evil".into(),
                schema_version: 1,
                generator_version: "x".into(),
                registry_hash: "00".into(),
                files: vec![FileEntry {
                    path: evil_path.into(),
                    sha256: "0".repeat(64),
                }],
            };
            std::fs::write(
                blob.path().join("manifest.json"),
                serde_json::to_string(&manifest).unwrap(),
            )
            .unwrap();
            let target = tempfile::tempdir().unwrap();
            let err = install_verified(blob.path(), Some(target.path()), None)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("path-traversal"),
                "path {evil_path:?} should be refused, got: {err}"
            );
        }
    }

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
    fn pack_then_mount_round_trips_and_verifies() {
        let src = tempfile::tempdir().unwrap();
        write_nest(src.path());
        let blob = tempfile::tempdir().unwrap();
        egg(src.path(), Some(blob.path()), true).unwrap();

        let manifest = load_manifest(blob.path()).unwrap();
        let target = tempfile::tempdir().unwrap();
        // Install with the correct expected hash → a runnable nest whose registry reproduces.
        install_verified(
            blob.path(),
            Some(target.path()),
            Some(&manifest.blob_hash()),
        )
        .unwrap();
        assert!(target.path().join(CONFIG_FILE).exists());
        assert!(target.path().join("abis/c.json").exists());
        verify_registry_reproduces(target.path(), &manifest).unwrap();
    }

    #[tokio::test]
    async fn egg_file_hatches_and_verifies() {
        // The headline path: lay a single-file `.egg`, then hatch it from that file → a runnable nest,
        // hash-verified, registry reproduced. Exercises write_egg → extract_egg → install_verified.
        let src = tempfile::tempdir().unwrap();
        write_nest(src.path());
        let out = tempfile::tempdir().unwrap();
        let egg_file = out.path().join("t.egg");
        let manifest = build_manifest(src.path(), None).unwrap();
        write_egg(src.path(), &manifest, &egg_file).unwrap();
        assert!(egg_file.is_file(), "egg is a single file");

        let target = tempfile::tempdir().unwrap();
        let installed = target.path().join("nest");
        hatch(
            egg_file.to_str().unwrap(),
            Some(&installed),
            Some(&manifest.blob_hash()),
        )
        .await
        .unwrap();
        assert!(installed.join(CONFIG_FILE).exists());
        assert!(installed.join("abis/c.json").exists());
        verify_registry_reproduces(&installed, &manifest).unwrap();

        // A wrong --expect is refused even via the file→hatch path.
        let t2 = tempfile::tempdir().unwrap();
        assert!(hatch(
            egg_file.to_str().unwrap(),
            Some(t2.path()),
            Some("deadbeef")
        )
        .await
        .is_err());
    }

    #[test]
    fn mount_rejects_a_tampered_file_and_a_wrong_hash() {
        let src = tempfile::tempdir().unwrap();
        write_nest(src.path());
        let blob = tempfile::tempdir().unwrap();
        egg(src.path(), Some(blob.path()), true).unwrap();

        // Wrong expected hash → refuse before touching disk.
        let t0 = tempfile::tempdir().unwrap();
        assert!(install_verified(blob.path(), Some(t0.path()), Some("deadbeef")).is_err());

        // Tamper a file's bytes without updating the manifest → integrity check fails.
        std::fs::write(blob.path().join("llms.txt"), "tampered\n").unwrap();
        let t1 = tempfile::tempdir().unwrap();
        let err = install_verified(blob.path(), Some(t1.path()), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("corrupt"), "got: {err}");
    }

    #[test]
    fn mount_rejects_a_newer_blob_format() {
        let src = tempfile::tempdir().unwrap();
        write_nest(src.path());
        let blob = tempfile::tempdir().unwrap();
        egg(src.path(), Some(blob.path()), true).unwrap();
        // Rewrite the manifest claiming a future format version.
        let mut m = load_manifest(blob.path()).unwrap();
        m.blob_format_version = BLOB_FORMAT_VERSION + 1;
        std::fs::write(
            blob.path().join("manifest.json"),
            serde_json::to_string_pretty(&m).unwrap(),
        )
        .unwrap();
        let t = tempfile::tempdir().unwrap();
        let err = install_verified(blob.path(), Some(t.path()), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("format v"), "got: {err}");
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
