//! Standalone plugin artifact validator (ABI v0).
//!
//! `auqw-validate <plugin.wasm> <manifest.json> [--update-digest]`
//!
//! Checks artifact size (<= 5 MiB), full module validity, zero imports,
//! no start section, and the required exports (`memory`, `alloc`,
//! `handle`) with exact signatures; enforces the manifest schema
//! (`sdk/contract/manifest.schema.json`); computes the artifact's
//! sha256 and verifies or updates `manifest.artifact.digest`.
//!
//! Deliberately independent of the auqw host crate: a duplicated
//! inspection is the intended cost of keeping the repos decoupled.

use std::process::ExitCode;

use sha2::Digest;
use wasmparser::{CompositeInnerType, ExternalKind, Parser, Payload, ValType};

const MAX_ARTIFACT_BYTES: usize = 5 * 1024 * 1024;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let (Some(wasm_path), Some(manifest_path)) = (args.next(), args.next()) else {
        eprintln!("usage: auqw-validate <plugin.wasm> <manifest.json> [--update-digest]");
        return ExitCode::FAILURE;
    };
    let update = args.any(|a| a == "--update-digest");

    let wasm = match std::fs::read(&wasm_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("validate: cannot read {wasm_path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let manifest_text = match std::fs::read_to_string(&manifest_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("validate: cannot read {manifest_path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = check_size(&wasm) {
        eprintln!("validate: {e}");
        return ExitCode::FAILURE;
    }
    if let Err(e) = check_shape(&wasm) {
        eprintln!("validate: {e}");
        return ExitCode::FAILURE;
    }
    let digest = format!("sha256:{:x}", sha2::Sha256::digest(&wasm));

    let mut manifest: serde_json::Value = match serde_json::from_str(&manifest_text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("validate: manifest is not JSON: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = check_manifest(&manifest) {
        eprintln!("validate: {e}");
        return ExitCode::FAILURE;
    }
    let pinned = manifest
        .get("artifact")
        .and_then(|a| a.get("digest"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();

    if update {
        manifest["artifact"]["digest"] = serde_json::Value::String(digest.clone());
        let pretty = match serde_json::to_string_pretty(&manifest) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("validate: cannot serialize manifest: {e}");
                return ExitCode::FAILURE;
            }
        };
        if let Err(e) = std::fs::write(&manifest_path, format!("{pretty}\n")) {
            eprintln!("validate: cannot write {manifest_path}: {e}");
            return ExitCode::FAILURE;
        }
    } else if pinned != digest {
        eprintln!("validate: digest mismatch: manifest={pinned} artifact={digest}");
        return ExitCode::FAILURE;
    }

    println!(
        "validate: ok — {} bytes, zero imports, exports present, {digest}",
        wasm.len()
    );
    ExitCode::SUCCESS
}

fn check_size(wasm: &[u8]) -> Result<(), String> {
    if wasm.len() > MAX_ARTIFACT_BYTES {
        return Err(format!(
            "artifact {} bytes exceeds {} byte cap",
            wasm.len(),
            MAX_ARTIFACT_BYTES
        ));
    }
    Ok(())
}

/// Zero imports, no start section, required exports with exact
/// signatures.
fn check_shape(wasm: &[u8]) -> Result<(), String> {
    let mut func_types: Vec<(Vec<ValType>, Vec<ValType>)> = Vec::new();
    let mut func_type_idx: Vec<u32> = Vec::new();
    let mut exports: Vec<(String, ExternalKind, u32)> = Vec::new();
    let mut import_count = 0usize;
    let mut has_start = false;

    for payload in Parser::new(0).parse_all(wasm) {
        let payload = payload.map_err(|e| format!("malformed wasm: {e}"))?;
        match payload {
            Payload::TypeSection(reader) => {
                for group in reader {
                    let group = group.map_err(|e| format!("type section: {e}"))?;
                    for subtype in group.types() {
                        if let CompositeInnerType::Func(ft) = &subtype.composite_type.inner {
                            func_types.push((ft.params().to_vec(), ft.results().to_vec()));
                        }
                    }
                }
            }
            Payload::ImportSection(reader) => import_count += reader.count() as usize,
            Payload::FunctionSection(reader) => {
                for f in reader {
                    func_type_idx.push(f.map_err(|e| format!("function section: {e}"))?);
                }
            }
            Payload::ExportSection(reader) => {
                for e in reader {
                    let e = e.map_err(|e| format!("export section: {e}"))?;
                    exports.push((e.name.to_string(), e.kind, e.index));
                }
            }
            Payload::StartSection { .. } => has_start = true,
            _ => {}
        }
    }

    // Structural checks inspect sections without executing anything —
    // full validation (type-checking bodies, export-name uniqueness,
    // index resolution) is wasmparser's job.
    wasmparser::Validator::new()
        .validate_all(wasm)
        .map_err(|e| format!("invalid module: {e}"))?;

    if has_start {
        return Err("module has a start section".into());
    }
    if import_count > 0 {
        return Err(format!(
            "module declares {import_count} import(s); v0 allows none"
        ));
    }

    let find = |name: &str| exports.iter().find(|(n, _, _)| n == name);
    match find("memory") {
        Some((_, ExternalKind::Memory, _)) => {}
        _ => return Err("missing memory export".into()),
    }
    check_func(
        &exports,
        &func_types,
        &func_type_idx,
        "alloc",
        &[ValType::I32],
        &[ValType::I32],
    )?;
    check_func(
        &exports,
        &func_types,
        &func_type_idx,
        "handle",
        &[ValType::I32, ValType::I32],
        &[ValType::I64],
    )?;
    Ok(())
}

fn check_func(
    exports: &[(String, ExternalKind, u32)],
    func_types: &[(Vec<ValType>, Vec<ValType>)],
    func_type_idx: &[u32],
    name: &str,
    params: &[ValType],
    results: &[ValType],
) -> Result<(), String> {
    let Some((_, ExternalKind::Func, idx)) = exports.iter().find(|(n, _, _)| n == name) else {
        return Err(format!("missing {name} export"));
    };
    let type_idx = func_type_idx
        .get(*idx as usize)
        .ok_or_else(|| format!("{name} export index out of range"))?;
    let (p, r) = func_types
        .get(*type_idx as usize)
        .ok_or_else(|| format!("{name} type index out of range"))?;
    if p.as_slice() != params || r.as_slice() != results {
        return Err(format!("{name} has wrong signature"));
    }
    Ok(())
}

/// The manifest schema (`sdk/contract/manifest.schema.json`), enforced
/// field by field: required keys, no unknown keys, and each field's
/// grammar. Kept in sync by hand — the schema is the contract.
fn check_manifest(manifest: &serde_json::Value) -> Result<(), String> {
    const FIELDS: [&str; 6] = [
        "id",
        "version",
        "abi",
        "capabilities",
        "permissions",
        "artifact",
    ];
    let obj = manifest
        .as_object()
        .ok_or_else(|| "manifest is not an object".to_string())?;
    for key in obj.keys() {
        if !FIELDS.contains(&key.as_str()) {
            return Err(format!("manifest: unknown field {key:?}"));
        }
    }
    for field in FIELDS {
        if !obj.contains_key(field) {
            return Err(format!("manifest: missing required field {field:?}"));
        }
    }

    let field_str = |name: &str| -> Result<&str, String> {
        manifest[name]
            .as_str()
            .ok_or_else(|| format!("manifest.{name} must be a string"))
    };

    // id: ^[a-z0-9][a-z0-9-]*$
    let id = field_str("id")?;
    let mut chars = id.chars();
    let id_ok = matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !id_ok {
        return Err("manifest.id must match ^[a-z0-9][a-z0-9-]*$".into());
    }
    // version: ^[0-9]+\.[0-9]+\.[0-9]+$
    let version = field_str("version")?;
    let version_ok = version.split('.').count() == 3
        && version
            .split('.')
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    if !version_ok {
        return Err("manifest.version must be semver x.y.z".into());
    }
    // abi: const "0.1.0"
    if field_str("abi")? != "0.1.0" {
        return Err("manifest.abi must be \"0.1.0\"".into());
    }
    // capabilities: non-empty subset of [playback.resolve]
    let caps = manifest["capabilities"]
        .as_array()
        .ok_or_else(|| "manifest.capabilities must be an array".to_string())?;
    if caps.is_empty() {
        return Err("manifest.capabilities must not be empty".into());
    }
    if caps.iter().any(|c| c.as_str() != Some("playback.resolve")) {
        return Err("manifest.capabilities must be a subset of [playback.resolve]".into());
    }
    // permissions: each ^(network:(\*\.)?[a-z0-9.-]+|pot-provider)$
    let perms = manifest["permissions"]
        .as_array()
        .ok_or_else(|| "manifest.permissions must be an array".to_string())?;
    for perm in perms {
        let Some(p) = perm.as_str() else {
            return Err("manifest.permissions entries must be strings".into());
        };
        if p == "pot-provider" {
            continue;
        }
        let body = p
            .strip_prefix("network:")
            .map(|rest| rest.strip_prefix("*.").unwrap_or(rest))
            .filter(|body| !body.is_empty())
            .ok_or_else(|| format!("manifest.permissions entry {p:?} is malformed"))?;
        if !body
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
        {
            return Err(format!("manifest.permissions entry {p:?} is malformed"));
        }
    }
    // artifact: exactly {path: non-empty string, digest: sha256:<64 hex>}
    let artifact = manifest["artifact"]
        .as_object()
        .ok_or_else(|| "manifest.artifact must be an object".to_string())?;
    for key in artifact.keys() {
        if key != "path" && key != "digest" {
            return Err(format!("manifest.artifact: unknown field {key:?}"));
        }
    }
    let path = artifact
        .get("path")
        .and_then(serde_json::Value::as_str)
        .filter(|p| !p.is_empty());
    if path.is_none() {
        return Err("manifest.artifact.path must be a non-empty string".into());
    }
    let digest_ok = artifact
        .get("digest")
        .and_then(serde_json::Value::as_str)
        .and_then(|d| d.strip_prefix("sha256:"))
        .is_some_and(|h| {
            h.len() == 64
                && h.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        });
    if !digest_ok {
        return Err("manifest.artifact.digest must be sha256:<64 lowercase hex>".into());
    }
    Ok(())
}
