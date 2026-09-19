//! Standalone plugin artifact validator (ABI v0).
//!
//! `auqw-validate <plugin.wasm> <manifest.json> [--update-digest]`
//!
//! Checks artifact size (<= 5 MiB), zero imports, no start section, and
//! the required exports (`memory`, `alloc`, `handle`) with exact
//! signatures; computes the artifact's sha256 and verifies or updates
//! `manifest.artifact.digest`.
//!
//! Deliberately independent of the auqw host crate: a duplicated ~80-line
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
