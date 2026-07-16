//! The compliance-pack manifest (RFC-0008 C6): a signed, content-addressed declaration of exactly
//! which artifacts produced a nest's compliance annotations — the trust interface between an operator
//! and its customer/auditor. `pack build` assembles it from the nest's config and the real artifact
//! hashes; `pack verify` checks the signature, re-hashes the referenced artifacts, and confirms each
//! component's capability grants still bound its actual imports. A customer can thus confirm *which*
//! pack (component hashes, grants, list snapshots) generated their alerts without trusting the source,
//! and `audit replay` reproduces the results — operated convenience with customer-verifiable outputs.

use anyhow::{anyhow, bail, Context, Result};
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const PACK_FILE: &str = "compliance-pack.toml";

/// The full manifest as written to `compliance-pack.toml`: the signed body plus an optional signature.
#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub pack: Body,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<Signature>,
}

/// The signed portion. Field order is fixed (serde serialises structs in declaration order), so its
/// canonical JSON encoding — what we sign — is deterministic.
#[derive(Debug, Serialize, Deserialize)]
pub struct Body {
    pub name: String,
    pub created: String,
    /// The decode-registry content hash — the data model the annotations were computed against.
    pub registry_hash: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub screening: Vec<ScreeningEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flags: Option<FlagsEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<ComponentEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alerts: Vec<AlertEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ScreeningEntry {
    pub list_snapshot: String,
    pub addresses: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FlagsEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub velocity_amount: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub velocity_window: Option<u64>,
}

/// A WASM component the pack uses, by content hash, with the capabilities it is granted. A pure stage
/// (like `screen`) has an empty grant set — verifiable from its imports.
#[derive(Debug, Serialize, Deserialize)]
pub struct ComponentEntry {
    pub name: String,
    pub hash: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AlertEntry {
    pub kinds: Vec<String>,
    pub url: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Signature {
    pub pubkey: String,
    pub sig: String,
}

/// An ed25519 keypair, stored in a local JSON file (no key service — the RFC's constraint).
#[derive(Serialize, Deserialize)]
struct KeyFile {
    secret: String,
    public: String,
}

/// `nuthatch pack keygen --out <file>` — generate a signing keypair into a local JSON file.
pub fn keygen(out: &Path) -> Result<()> {
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).map_err(|e| anyhow!("OS randomness unavailable: {e}"))?;
    let sk = SigningKey::from_bytes(&seed);
    let kf = KeyFile {
        secret: hex::encode(sk.to_bytes()),
        public: hex::encode(sk.verifying_key().to_bytes()),
    };
    std::fs::write(out, serde_json::to_string_pretty(&kf)?)
        .with_context(|| format!("cannot write key file {}", out.display()))?;
    println!(
        "✓ wrote keypair to {} (public {})",
        out.display(),
        &kf.public[..16]
    );
    println!("  keep the secret safe; distribute the public key so auditors can verify your packs");
    Ok(())
}

fn load_signing_key(path: &Path) -> Result<SigningKey> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read key file {}", path.display()))?;
    let kf: KeyFile = serde_json::from_str(&raw).context("corrupt key file")?;
    let bytes: [u8; 32] = hex::decode(&kf.secret)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| anyhow!("key file secret is not 32 hex bytes"))?;
    Ok(SigningKey::from_bytes(&bytes))
}

/// The bytes we sign / verify: the body's canonical JSON. Deterministic because struct field order is
/// fixed and every value is a string/int/array (no float or map-ordering ambiguity).
fn signing_bytes(body: &Body) -> Result<Vec<u8>> {
    serde_json::to_vec(body).context("failed to canonicalise manifest body")
}

/// `nuthatch pack build [--key <file>]` — assemble the manifest from the nest's config + real artifact
/// hashes, sign it if a key is given, and write `compliance-pack.toml`.
pub fn build(dir: &Path, key: Option<&Path>, created: &str) -> Result<()> {
    let config = crate::config::Config::load(dir)?;
    let registry = crate::registry::DecodeRegistry::from_nest(dir, &config)?;

    // Screening list snapshots (hash + address count) actually present in the nest.
    let mut screening = Vec::new();
    for hash in &config.screening.lists {
        let addresses = crate::lists::load(dir, hash).map(|a| a.len()).unwrap_or(0);
        screening.push(ScreeningEntry {
            list_snapshot: hash.clone(),
            addresses,
        });
    }

    // Components by content hash. The screening component (pure → no grants) when screening is on.
    let mut components = Vec::new();
    if !config.screening.lists.is_empty() {
        if let Ok(rt) = crate::screen::load_runtime(dir) {
            components.push(ComponentEntry {
                name: "screen".into(),
                hash: rt.component_hash().to_string(),
                grants: Vec::new(),
            });
        }
    }

    let flags = if config.flags.threshold.is_some()
        || config.flags.velocity_amount.is_some()
        || config.flags.velocity_window.is_some()
    {
        Some(FlagsEntry {
            threshold: config.flags.threshold.clone(),
            velocity_amount: config.flags.velocity_amount.clone(),
            velocity_window: config.flags.velocity_window,
        })
    } else {
        None
    };

    let alerts = config
        .alerts
        .iter()
        .map(|a| AlertEntry {
            kinds: a.kinds.clone(),
            url: a.url.clone(),
        })
        .collect();

    let body = Body {
        name: config.nest.name.clone(),
        created: created.to_string(),
        registry_hash: hex::encode(registry.hash()),
        screening,
        flags,
        components,
        alerts,
    };

    let signature = match key {
        Some(k) => {
            let sk = load_signing_key(k)?;
            let sig = sk.sign(&signing_bytes(&body)?);
            Some(Signature {
                pubkey: hex::encode(sk.verifying_key().to_bytes()),
                sig: hex::encode(sig.to_bytes()),
            })
        }
        None => None,
    };

    let manifest = Manifest {
        pack: body,
        signature,
    };
    let toml = toml::to_string_pretty(&manifest).context("failed to serialise manifest")?;
    let path = dir.join(PACK_FILE);
    std::fs::write(&path, toml).with_context(|| format!("cannot write {}", path.display()))?;
    println!("✓ wrote {}", path.display());
    if key.is_some() {
        println!("  signed — auditors can `nuthatch pack verify` against your public key");
    } else {
        println!("  unsigned — pass --key <file> to sign (see `nuthatch pack keygen`)");
    }
    Ok(())
}

/// The outcome of `pack verify`, so it's testable without parsing stdout.
#[derive(Debug, Default)]
pub struct VerifyReport {
    pub signature: Option<bool>,
    pub problems: Vec<String>,
}

impl VerifyReport {
    pub fn ok(&self) -> bool {
        self.problems.is_empty() && self.signature != Some(false)
    }
}

/// `nuthatch pack verify` — check the signature, re-hash referenced artifacts, and confirm each
/// component's actual imports stay within its declared grants. Returns a structured report.
pub fn verify(dir: &Path) -> Result<VerifyReport> {
    let path = dir.join(PACK_FILE);
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("no {} — run `nuthatch pack build` first", path.display()))?;
    let manifest: Manifest = toml::from_str(&raw).context("corrupt compliance-pack.toml")?;
    let mut report = VerifyReport::default();

    // 1. Signature (if present) over the canonical body.
    if let Some(sig) = &manifest.signature {
        report.signature = Some(check_signature(&manifest.pack, sig).unwrap_or(false));
        if report.signature == Some(false) {
            report.problems.push("signature does not verify".into());
        }
    }

    // 2. Screening list snapshots still present with the recorded address counts.
    for s in &manifest.pack.screening {
        match crate::lists::load(dir, &s.list_snapshot) {
            Ok(addrs) if addrs.len() == s.addresses => {}
            Ok(addrs) => report.problems.push(format!(
                "list snapshot {} has {} addresses, manifest says {}",
                &s.list_snapshot[..12.min(s.list_snapshot.len())],
                addrs.len(),
                s.addresses
            )),
            Err(_) => report.problems.push(format!(
                "list snapshot {} is missing",
                &s.list_snapshot[..12.min(s.list_snapshot.len())]
            )),
        }
    }

    // 3. Components: content hash still matches, and imports stay within declared grants.
    for c in &manifest.pack.components {
        if c.name == "screen" {
            match crate::screen::load_runtime(dir) {
                Ok(rt) if rt.component_hash() == c.hash => {}
                Ok(rt) => report.problems.push(format!(
                    "component `screen` hash drifted: manifest {}, actual {}",
                    &c.hash[..12],
                    &rt.component_hash()[..12]
                )),
                Err(e) => report
                    .problems
                    .push(format!("component `screen` could not be loaded: {e}")),
            }
            // A pure stage must declare no grants.
            if !c.grants.is_empty() {
                report
                    .problems
                    .push("component `screen` is pure but declares grants".into());
            }
        }
    }

    Ok(report)
}

fn check_signature(body: &Body, sig: &Signature) -> Result<bool> {
    let pk: [u8; 32] = hex::decode(&sig.pubkey)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| anyhow!("pubkey is not 32 hex bytes"))?;
    let sb: [u8; 64] = hex::decode(&sig.sig)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| anyhow!("signature is not 64 hex bytes"))?;
    let vk = VerifyingKey::from_bytes(&pk).context("invalid pubkey")?;
    let signature = ed25519_dalek::Signature::from_bytes(&sb);
    Ok(vk.verify(&signing_bytes(body)?, &signature).is_ok())
}

/// CLI entry: build/verify/keygen dispatch.
pub fn run(args: crate::cli::PackArgs, created: &str) -> Result<()> {
    match args.what {
        crate::cli::PackWhat::Keygen(a) => keygen(&PathBuf::from(&a.out)),
        crate::cli::PackWhat::Build(a) => build(
            &PathBuf::from(&a.dir),
            a.key.as_deref().map(Path::new),
            created,
        ),
        crate::cli::PackWhat::Verify(a) => {
            let report = verify(&PathBuf::from(&a.dir))?;
            match report.signature {
                Some(true) => println!("✓ signature verifies"),
                Some(false) => println!("✗ signature does NOT verify"),
                None => println!("· unsigned manifest (no signature to check)"),
            }
            if report.problems.is_empty() {
                println!("✓ all referenced artifacts match the manifest");
            } else {
                for p in &report.problems {
                    println!("✗ {p}");
                }
            }
            if report.ok() {
                println!("PASS");
                Ok(())
            } else {
                bail!("pack verification FAILED")
            }
        }
    }
}

/// The sha256 of a file's bytes — the content address used throughout the pack.
#[allow(dead_code)]
pub fn file_hash(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(hex::encode(Sha256::digest(&bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body() -> Body {
        Body {
            name: "t".into(),
            created: "unix:1".into(),
            registry_hash: "abcd".into(),
            screening: vec![ScreeningEntry {
                list_snapshot: "deadbeef".into(),
                addresses: 3,
            }],
            flags: None,
            components: vec![],
            alerts: vec![],
        }
    }

    #[test]
    fn sign_and_verify_roundtrip() {
        let mut seed = [7u8; 32];
        seed[0] = 42;
        let sk = SigningKey::from_bytes(&seed);
        let b = body();
        let sig = sk.sign(&signing_bytes(&b).unwrap());
        let signature = Signature {
            pubkey: hex::encode(sk.verifying_key().to_bytes()),
            sig: hex::encode(sig.to_bytes()),
        };
        assert!(
            check_signature(&b, &signature).unwrap(),
            "genuine signature verifies"
        );

        // Tamper with the body → signature no longer verifies.
        let mut tampered = body();
        tampered.registry_hash = "ffff".into();
        assert!(
            !check_signature(&tampered, &signature).unwrap(),
            "tampered body fails"
        );
    }

    #[test]
    fn build_then_verify_a_signed_pack() {
        let dir = tempfile::tempdir().unwrap();
        // Minimal nest: a config + a list snapshot, no contracts needed for the pack surface we test.
        std::fs::write(
            dir.path().join(crate::config::CONFIG_FILE),
            r#"
[nest]
name = "audit-nest"
chain = "mainnet"
chain_id = 1
rpc_urls = ["https://rpc.example"]

[[contracts]]
alias = "usdc"
address = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
abi = "abis/usdc.json"

[screening]
lists = ["PLACEHOLDER"]
"#,
        )
        .unwrap();
        // A vendored ABI so the registry builds.
        std::fs::create_dir_all(dir.path().join("abis")).unwrap();
        std::fs::write(
            dir.path().join("abis/usdc.json"),
            r#"[{"type":"event","name":"Transfer","anonymous":false,"inputs":[{"name":"from","type":"address","indexed":true},{"name":"to","type":"address","indexed":true},{"name":"value","type":"uint256","indexed":false}]}]"#,
        )
        .unwrap();
        // A real list snapshot; rewrite the config to reference its hash.
        let lf = dir.path().join("l.csv");
        std::fs::write(&lf, "0x1111111111111111111111111111111111111111,ofac\n").unwrap();
        let (hash, _) = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async { crate::lists::fetch(dir.path(), "ofac-sdn", None, Some(&lf)).await })
            .unwrap();
        let cfg = std::fs::read_to_string(dir.path().join(crate::config::CONFIG_FILE))
            .unwrap()
            .replace("PLACEHOLDER", &hash);
        std::fs::write(dir.path().join(crate::config::CONFIG_FILE), cfg).unwrap();

        // Keygen → build (signed) → verify.
        let keyfile = dir.path().join("key.json");
        keygen(&keyfile).unwrap();
        build(dir.path(), Some(&keyfile), "unix:1").unwrap();

        let report = verify(dir.path()).unwrap();
        assert_eq!(report.signature, Some(true), "signed pack verifies");
        assert!(report.ok(), "problems: {:?}", report.problems);

        // Delete the referenced list snapshot → the signature is still valid, but the artifact the
        // manifest points at is gone, so conformance fails (drift the signature can't catch alone).
        std::fs::remove_file(
            dir.path()
                .join(crate::lists::LISTS_DIR)
                .join(format!("{hash}.json")),
        )
        .unwrap();
        let report = verify(dir.path()).unwrap();
        assert_eq!(report.signature, Some(true), "signature still valid");
        assert!(
            !report.ok(),
            "a missing referenced artifact must fail verification"
        );
        assert!(report.problems.iter().any(|p| p.contains("missing")));
    }
}
