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
/// Guest linear memory cap (64 MiB) expressed in 64 KiB pages; the
/// host runtime enforces the same bound.
const MAX_MEMORY_PAGES: u64 = 64 * 1024 * 1024 / 65536;

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
    // Indexed by the module's full type-index space, which is what the
    // function section references — non-func (GC) types occupy real
    // slots, so a filtered func-only vec would shift every later index.
    let mut types: Vec<Option<(Vec<ValType>, Vec<ValType>)>> = Vec::new();
    let mut func_type_idx: Vec<u32> = Vec::new();
    let mut exports: Vec<(String, ExternalKind, u32)> = Vec::new();
    let mut memories: Vec<wasmparser::MemoryType> = Vec::new();
    let mut import_count = 0usize;
    let mut has_start = false;

    for payload in Parser::new(0).parse_all(wasm) {
        let payload = payload.map_err(|e| format!("malformed wasm: {e}"))?;
        match payload {
            Payload::TypeSection(reader) => {
                for group in reader {
                    let group = group.map_err(|e| format!("type section: {e}"))?;
                    for subtype in group.types() {
                        let ty = match &subtype.composite_type.inner {
                            CompositeInnerType::Func(ft) => {
                                Some((ft.params().to_vec(), ft.results().to_vec()))
                            }
                            _ => None,
                        };
                        types.push(ty);
                    }
                }
            }
            Payload::ImportSection(reader) => import_count += reader.count() as usize,
            Payload::MemorySection(reader) => {
                for m in reader {
                    memories.push(m.map_err(|e| format!("memory section: {e}"))?);
                }
            }
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
    if memories.len() > 1 {
        return Err(format!(
            "module declares {} memories; v0 allows one",
            memories.len()
        ));
    }
    for mem in &memories {
        // `page_size_log2` overrides the 64 KiB page unit when present.
        let page = 1u64 << mem.page_size_log2.unwrap_or(16);
        if mem.initial.saturating_mul(page) > MAX_MEMORY_PAGES * 65536 {
            return Err("declared memory initial exceeds 64 MiB".into());
        }
        if mem
            .maximum
            .is_some_and(|m| m.saturating_mul(page) > MAX_MEMORY_PAGES * 65536)
        {
            return Err("declared memory maximum exceeds 64 MiB".into());
        }
    }

    let find = |name: &str| exports.iter().find(|(n, _, _)| n == name);
    match find("memory") {
        Some((_, ExternalKind::Memory, _)) => {}
        _ => return Err("missing memory export".into()),
    }
    check_func(
        &exports,
        &types,
        &func_type_idx,
        "alloc",
        &[ValType::I32],
        &[ValType::I32],
    )?;
    check_func(
        &exports,
        &types,
        &func_type_idx,
        "handle",
        &[ValType::I32, ValType::I32],
        &[ValType::I64],
    )?;
    Ok(())
}

fn check_func(
    exports: &[(String, ExternalKind, u32)],
    types: &[Option<(Vec<ValType>, Vec<ValType>)>],
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
    let (p, r) = match types.get(*type_idx as usize) {
        Some(Some((p, r))) => (p, r),
        Some(None) => return Err(format!("{name} references a non-function type")),
        None => return Err(format!("{name} type index out of range")),
    };
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
    // abi: "0.1.0", "0.2.0", or "0.3.0"; the capability set is
    // version-specific and revisions are immutable — a newer ABI's
    // capabilities never become valid on an older ABI.
    let abi = field_str("abi")?;
    let allowed_caps: &[&str] = match abi {
        "0.1.0" => &["playback.resolve"],
        "0.2.0" => &[
            "catalog.search",
            "catalog.metadata",
            "catalog.artwork",
            "playback.resolve",
            "playback.candidates",
        ],
        "0.3.0" => &[
            "catalog.artwork",
            "catalog.entity",
            "catalog.metadata",
            "catalog.search",
            "lyrics.plain",
            "lyrics.synced",
            "playback.candidates",
            "playback.resolve",
            "radio.seed",
        ],
        _ => {
            return Err("manifest.abi must be \"0.1.0\", \"0.2.0\", or \"0.3.0\"".into());
        }
    };
    // capabilities: non-empty subset of the ABI's set
    let caps = manifest["capabilities"]
        .as_array()
        .ok_or_else(|| "manifest.capabilities must be an array".to_string())?;
    if caps.is_empty() {
        return Err("manifest.capabilities must not be empty".into());
    }
    if caps
        .iter()
        .any(|c| !c.as_str().is_some_and(|s| allowed_caps.contains(&s)))
    {
        return Err("manifest.capabilities outside the set this ABI serves".into());
    }
    // permissions: ^(network:(\*\.)?[a-z0-9.-]+|pot-provider|kv)$ —
    // `kv` is a 0.2 permission and forbidden on a 0.1 manifest.
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
        if p == "kv" {
            if abi == "0.1.0" {
                return Err("manifest.permissions entry \"kv\" requires abi \"0.2.0\"".into());
            }
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
        // Mirrors plugin-host `validate`: a dotted DNS name or a
        // loopback literal — non-loopback IPs, bare labels, empty
        // labels, and `*.localhost` are not grantable destinations.
        let is_loopback = body == "localhost"
            || body
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        if !is_loopback
            && (body.parse::<std::net::IpAddr>().is_ok()
                || !body.contains('.')
                || body.split('.').any(str::is_empty)
                || body.ends_with(".localhost"))
        {
            return Err(format!(
                "manifest.permissions entry {p:?} is not a public DNS name"
            ));
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

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_WAT: &str = "(module
        (memory (export \"memory\") 1)
        (func (export \"alloc\") (param i32) (result i32) (i32.const 0))
        (func (export \"handle\") (param i32 i32) (result i64) (i64.const 0)))";

    fn manifest(abi: &str, caps: &str, perms: &str) -> serde_json::Value {
        serde_json::from_str(&format!(
            "{{\"id\":\"p\",\"version\":\"0.1.0\",\"abi\":\"{abi}\",\
             \"capabilities\":{caps},\"permissions\":{perms},\
             \"artifact\":{{\"path\":\"p.wasm\",\"digest\":\"sha256:{}\"}}}}",
            "a".repeat(64)
        ))
        .unwrap_or(serde_json::Value::Null)
    }

    fn shape(wat: &str) -> Result<(), String> {
        check_shape(&wat::parse_str(wat).unwrap_or_default())
    }

    #[test]
    fn valid_module_passes() {
        assert_eq!(shape(VALID_WAT), Ok(()));
    }

    #[test]
    fn imports_rejected() {
        let wat = "(module
            (import \"env\" \"f\" (func))
            (memory (export \"memory\") 1)
            (func (export \"alloc\") (param i32) (result i32) (i32.const 0))
            (func (export \"handle\") (param i32 i32) (result i64) (i64.const 0)))";
        assert!(shape(wat).is_err_and(|e| e.contains("import")));
    }

    #[test]
    fn start_section_rejected() {
        let wat = "(module
            (memory (export \"memory\") 1)
            (func (export \"alloc\") (param i32) (result i32) (i32.const 0))
            (func (export \"handle\") (param i32 i32) (result i64) (i64.const 0))
            (func $noop)
            (start $noop))";
        assert!(shape(wat).is_err_and(|e| e.contains("start")));
    }

    #[test]
    fn memory_over_cap_rejected() {
        // 1025 pages of 64 KiB initial exceeds the 64 MiB bound.
        let wat = "(module
            (memory (export \"memory\") 1025)
            (func (export \"alloc\") (param i32) (result i32) (i32.const 0))
            (func (export \"handle\") (param i32 i32) (result i64) (i64.const 0)))";
        assert!(shape(wat).is_err_and(|e| e.contains("memory")));
    }

    #[test]
    fn memory_max_over_cap_rejected() {
        let wat = "(module
            (memory (export \"memory\") 1 1025)
            (func (export \"alloc\") (param i32) (result i32) (i32.const 0))
            (func (export \"handle\") (param i32 i32) (result i64) (i64.const 0)))";
        assert!(shape(wat).is_err_and(|e| e.contains("memory")));
    }

    #[test]
    fn missing_exports_rejected() {
        let wat = "(module (memory (export \"memory\") 1))";
        assert!(shape(wat).is_err());
    }

    #[test]
    fn non_func_types_do_not_shift_signature_lookup() {
        // A GC struct occupies a slot in the type-index space the
        // function section references; resolving through a filtered
        // func-only list would mis-check every later func.
        let good = "(module
            (type (struct))
            (type (func (param i32) (result i32)))
            (type (func (param i32 i32) (result i64)))
            (memory (export \"memory\") 1)
            (func (export \"alloc\") (type 1) (param i32) (result i32) (i32.const 0))
            (func (export \"handle\") (type 2) (param i32 i32) (result i64) (i64.const 0)))";
        assert_eq!(shape(good), Ok(()));

        // `alloc` is declared with a decoy signature; filtered indexing
        // would land on the real alloc type and let it through.
        let bad = "(module
            (type (struct))
            (type (func (param i32 i32) (result i32)))
            (type (func (param i32) (result i32)))
            (type (func (param i32 i32) (result i64)))
            (memory (export \"memory\") 1)
            (func (export \"alloc\") (type 1) (param i32 i32) (result i32) (i32.const 0))
            (func (export \"handle\") (type 3) (param i32 i32) (result i64) (i64.const 0)))";
        assert!(shape(bad).is_err_and(|e| e.contains("signature")));
    }

    #[test]
    fn abi_versions_and_capability_sets() {
        assert_eq!(
            check_manifest(&manifest("0.1.0", "[\"playback.resolve\"]", "[]")),
            Ok(())
        );
        assert!(check_manifest(&manifest("0.1.0", "[\"catalog.search\"]", "[]")).is_err());
        assert_eq!(
            check_manifest(&manifest(
                "0.2.0",
                "[\"catalog.search\",\"playback.candidates\"]",
                "[]"
            )),
            Ok(())
        );
        assert!(check_manifest(&manifest("0.2.0", "[\"bogus.cap\"]", "[]")).is_err());
        // 0.3.0 adds catalog.entity, lyrics.*, radio.seed on top of 0.2.0.
        assert_eq!(
            check_manifest(&manifest(
                "0.3.0",
                "[\"catalog.entity\",\"lyrics.plain\",\"lyrics.synced\",\"radio.seed\"]",
                "[]"
            )),
            Ok(())
        );
        assert_eq!(
            check_manifest(&manifest("0.3.0", "[\"playback.resolve\"]", "[]")),
            Ok(())
        );
        // Immutable revisions: 0.3.0 capabilities are invalid on 0.1.0/0.2.0.
        for cap in [
            "catalog.entity",
            "lyrics.plain",
            "lyrics.synced",
            "radio.seed",
        ] {
            let caps = format!("[\"{cap}\"]");
            assert!(check_manifest(&manifest("0.1.0", &caps, "[]")).is_err());
            assert!(check_manifest(&manifest("0.2.0", &caps, "[]")).is_err());
        }
        assert!(check_manifest(&manifest("0.3.0", "[\"bogus.cap\"]", "[]")).is_err());
        assert!(check_manifest(&manifest("0.4.0", "[\"playback.resolve\"]", "[]")).is_err());
    }

    #[test]
    fn kv_and_network_permissions() {
        assert_eq!(
            check_manifest(&manifest("0.2.0", "[\"playback.resolve\"]", "[\"kv\"]")),
            Ok(())
        );
        assert_eq!(
            check_manifest(&manifest(
                "0.2.0",
                "[\"playback.resolve\"]",
                "[\"pot-provider\",\"network:*.googlevideo.com\",\"kv\"]"
            )),
            Ok(())
        );
        assert!(check_manifest(&manifest("0.2.0", "[\"playback.resolve\"]", "[\"fs\"]")).is_err());
        assert_eq!(
            check_manifest(&manifest("0.3.0", "[\"lyrics.plain\"]", "[\"kv\"]")),
            Ok(())
        );
    }

    #[test]
    fn abi_0_1_forbids_kv_permission() {
        assert!(check_manifest(&manifest("0.1.0", "[\"playback.resolve\"]", "[\"kv\"]")).is_err());
        assert_eq!(
            check_manifest(&manifest(
                "0.1.0",
                "[\"playback.resolve\"]",
                "[\"pot-provider\",\"network:x.test\"]"
            )),
            Ok(())
        );
    }
}
