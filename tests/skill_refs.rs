//! RFC-0017 §S1 — the drift gate for the builder skill. A skill that lies about flag names is worse
//! than no skill (the same reason stale semantics are worse than none, RFC-0016 §2), so CI enforces
//! two invariants:
//!   1. the committed `cli-reference.md` is byte-identical to what the binary generates now, and
//!   2. every `--flag` mentioned in the *authored* skill files is a real flag (present in the
//!      reference) — no hallucinated flags.

use std::collections::BTreeSet;
use std::path::PathBuf;

fn skill_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(nuthatch::skill::SKILL_DIR)
}

#[test]
fn committed_cli_reference_is_not_stale() {
    let committed = std::fs::read_to_string(skill_dir().join("cli-reference.md"))
        .expect("cli-reference.md must be committed");
    let fresh = nuthatch::skill::generate_cli_reference();
    assert_eq!(
        committed, fresh,
        "cli-reference.md is out of date — run `nuthatch skill-refs` and commit the result"
    );
}

#[test]
fn authored_files_only_mention_real_flags() {
    // Every `--flag` the reference documents (the source of truth).
    let reference = nuthatch::skill::generate_cli_reference();
    let real: BTreeSet<String> = flags_in(&reference);
    assert!(real.contains("--chain") && real.contains("--seal-direct"));

    // Scan every authored skill file (everything except the generated reference).
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(skill_dir()).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if path.extension().and_then(|e| e.to_str()) != Some("md") || name == "cli-reference.md" {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        for flag in flags_in(&text) {
            // `--url` etc. are all real; a flag not in the reference is a hallucination.
            if !real.contains(&flag) {
                offenders.push(format!("{name}: `{flag}` is not a real nuthatch flag"));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "authored skill files reference nonexistent flags:\n{}",
        offenders.join("\n")
    );
}

/// Extract `--flag` tokens (long options) from text. A flag is `--` followed by a lowercase letter and
/// then letters/digits/hyphens; trailing punctuation is trimmed by the character class.
fn flags_in(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == b'-' && bytes[i + 1] == b'-' && bytes[i + 2].is_ascii_lowercase() {
            let start = i;
            i += 2;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-') {
                i += 1;
            }
            out.insert(text[start..i].to_string());
        } else {
            i += 1;
        }
    }
    out
}

/// RFC-0017 §S1, extended per issue #137 (C2): every `nuthatch_*` metric name an authored skill file
/// mentions must be a real series the binary emits. A stale metric name (`nuthatch_tip` for
/// `nuthatch_tip_height`) is the same failure class as a hallucinated flag - an agent greps a scrape
/// for a name that isn't there and concludes the nest is broken. The source of truth is
/// `Metrics::render()`, exactly as `cli-reference.md` is the source of truth for flags.
#[test]
fn authored_files_only_mention_real_metrics() {
    // The canonical set: every `nuthatch_*` name the exposition can emit. Register a nest first so the
    // per-nest `nuthatch_nest_*` series are present in the render too.
    nuthatch::metrics::METRICS.nest("__drift_probe__");
    let real = metric_names_in(&nuthatch::metrics::METRICS.render());
    assert!(real.contains("nuthatch_tip_height") && real.contains("nuthatch_rss_bytes"));

    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(skill_dir()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let text = std::fs::read_to_string(&path).unwrap();
        for metric in metric_names_in(&text) {
            if !real.contains(&metric) {
                offenders.push(format!("{name}: `{metric}` is not a real nuthatch metric"));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "authored skill files reference nonexistent metrics:\n{}",
        offenders.join("\n")
    );
}

/// Extract `nuthatch_<...>` metric-name tokens (lowercase/digit/underscore tail, trailing underscores
/// trimmed so markdown emphasis doesn't leak in). Byte-based so it never slices a multibyte boundary.
fn metric_names_in(text: &str) -> BTreeSet<String> {
    const PREFIX: &[u8] = b"nuthatch_";
    let mut out = BTreeSet::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(PREFIX) {
            let start = i;
            i += PREFIX.len();
            while i < bytes.len()
                && (bytes[i].is_ascii_lowercase() || bytes[i].is_ascii_digit() || bytes[i] == b'_')
            {
                i += 1;
            }
            let mut end = i;
            while end > start + PREFIX.len() && bytes[end - 1] == b'_' {
                end -= 1;
            }
            // The token is pure ASCII by construction, so this slice is always valid UTF-8.
            out.insert(String::from_utf8_lossy(&bytes[start..end]).into_owned());
        } else {
            i += 1;
        }
    }
    out
}
