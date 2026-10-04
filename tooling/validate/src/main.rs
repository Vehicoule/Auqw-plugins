//! Standalone plugin artifact validator (ABI v0).
//!
//! `auqw-validate <plugin.wasm> <manifest.json> [--update-digest]`
//!
//! Checks artifact size (<= 5 MiB), full module validity, zero imports,
//! no start section, and the required exports (`memory`, `alloc`,
//! `handle`) with exact signatures; enforces the manifest schema
//! (`sdk/contract/manifest.schema.json`) plus the manifest-vs-guest
//! alignment (declared capabilities served, granted permissions used);
//! computes the artifact's sha256 and verifies or updates
//! `manifest.artifact.digest`.
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
    if let Err(e) = check_guest_alignment(&manifest, &wasm) {
        eprintln!("validate: {e}");
        return ExitCode::FAILURE;
    }
    if let Err(e) = check_capability_support(&manifest, &wasm) {
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
    // abi: the 0.1.x line — the tier matrix existed for an installed
    // base the prerelease never had; one line serves every
    // capability (0.1.1 is additive: it adds only the http_batch
    // host_request kind, which needs no capability bit). Revisions
    // stay immutable: a future ABI's capabilities never become valid
    // on 0.1.0.
    let abi = field_str("abi")?;
    let allowed_caps: &[&str] = match abi {
        "0.1.0" | "0.1.1" => &[
            "catalog.artwork",
            "catalog.entity",
            "catalog.metadata",
            "catalog.search",
            "catalog.search.kinds",
            "catalog.suggest",
            "lyrics.plain",
            "lyrics.synced",
            "playback.candidates",
            "playback.resolve",
            "radio.seed",
        ],
        _ => {
            return Err("manifest.abi must be \"0.1.0\" or \"0.1.1\"".into());
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
    // permissions: ^(network:(\*\.)?[a-z0-9.-]+|pot-provider|kv)$
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
            continue;
        }
        let rest = p
            .strip_prefix("network:")
            .ok_or_else(|| format!("manifest.permissions entry {p:?} is malformed"))?;
        let wildcarded = rest.starts_with("*.");
        let body = if wildcarded { &rest[2..] } else { rest };
        if body.is_empty() {
            return Err(format!("manifest.permissions entry {p:?} is malformed"));
        }
        if !body
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
        {
            return Err(format!("manifest.permissions entry {p:?} is malformed"));
        }
        // Mirrors plugin-host `validate`: a dotted DNS name or a
        // loopback literal — non-loopback IPs, bare labels, empty
        // labels, and wildcarded loopback forms (`*.localhost`,
        // `*.127.0.0.1`) are not grantable destinations.
        let is_loopback = !wildcarded
            && (body == "localhost"
                || body
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback()));
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

/// A permission the guest never exercises is silent scope — the
/// sandbox grant is read from the manifest, so granting beyond what the
/// code uses asks the host for authority it cannot spend.
///
/// Use is read off the module's data segments: the SDK's `host_call`
/// kind strings are `&'static str` literals and live in rodata —
/// `kv` → `kv_get`/`kv_set`, `pot-provider` → `pot_token`,
/// `network:<dest>` → the guest must fetch (`http_request`) AND carry
/// the destination literal — guests format their fetch URLs, so a
/// declared host that never appears in rodata is dead scope. A
/// wildcard destination (`*.host`) is exempt: its URLs arrive in
/// provider payloads rather than literals. (`resume` is the
/// paginated-fetch continue call — only ever compiled in alongside
/// `http_request`, so it witnesses nothing extra.) A destination can
/// still be over-granted at fetch time — granularity stops where the
/// literals do.
fn check_guest_alignment(manifest: &serde_json::Value, wasm: &[u8]) -> Result<(), String> {
    let mut segments: Vec<&[u8]> = Vec::new();
    for payload in Parser::new(0).parse_all(wasm) {
        if let Payload::DataSection(reader) = payload.map_err(|e| format!("data section: {e}"))? {
            for segment in reader {
                let segment = segment.map_err(|e| format!("data section: {e}"))?;
                segments.push(segment.data);
            }
        }
    }
    let has = |needle: &str| {
        segments
            .iter()
            .any(|seg| seg.windows(needle.len()).any(|w| w == needle.as_bytes()))
    };
    // The host grants a non-wildcard destination on exact host
    // equality (`host == pattern`), so the literal must stand on
    // hostname boundaries: a longer name that merely embeds the
    // declared one (`xapi.deezer.com`, `api.deezer.com.evil.tld`) can
    // never be fetched through the grant. Neighbor bytes are compared
    // directly — alphanumerics (DNS is case-insensitive), `.`, `-`,
    // `_` continue a name; anything else (or the segment edge) is a
    // boundary.
    let host_byte = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_');
    let has_host = |host: &str| {
        let needle = host.as_bytes();
        segments.iter().any(|seg| {
            seg.windows(needle.len()).enumerate().any(|(i, w)| {
                w == needle
                    && (i == 0 || !host_byte(seg[i - 1]))
                    && (i + needle.len() == seg.len() || !host_byte(seg[i + needle.len()]))
            })
        })
    };

    let perms = manifest["permissions"]
        .as_array()
        .ok_or_else(|| "manifest.permissions must be an array".to_string())?;
    for perm in perms {
        let Some(p) = perm.as_str() else {
            return Err("manifest.permissions entries must be strings".into());
        };
        let used = if p == "kv" {
            has("kv_get") || has("kv_set")
        } else if p == "pot-provider" {
            has("pot_token")
        } else if let Some(dest) = p.strip_prefix("network:") {
            // The guest must fetch at all — via `http_request` or, on
            // the 0.1.1 line, `http_batch` — and a non-wildcard
            // destination must appear as a literal (its URLs are
            // formatted in the guest). Wildcards are exempt: their
            // URLs arrive in provider payloads.
            (has("http_request") || has("http_batch")) && (dest.contains('*') || has_host(dest))
        } else {
            // Permission kinds without a statically-decidable use.
            true
        };
        if !used {
            return Err(format!(
                "manifest.permissions entry {p:?} is granted but the guest never uses it"
            ));
        }
    }
    Ok(())
}

/// A capability no plugin can legitimately declare — probing it must
/// yield the canonical dispatch fallback, proving the guest routes by
/// capability name at all. Without the contract a guest that ignores
/// `capability` and fails every request looks "served" for any
/// declaration.
const DISPATCH_SENTINEL: &str = "auqw.sentinel";

/// A declared capability the guest does not serve is silent scope —
/// the host would route invocations that always fail `not-applicable`.
/// The dispatch match's string literals never reach the artifact (LLVM
/// folds them into immediate compares), so the guest itself is asked:
/// feed one `invoke` step per declared capability and read the first
/// reply. The undeclared sentinel is probed first — its reply must be
/// the canonical `not-applicable` fallback, or the probe cannot trust
/// what a declared reply means. Fuel is capped so a looped guest
/// cannot hang the check.
fn check_capability_support(manifest: &serde_json::Value, wasm: &[u8]) -> Result<(), String> {
    let caps = manifest["capabilities"]
        .as_array()
        .ok_or_else(|| "manifest.capabilities must be an array".to_string())?;
    if caps.is_empty() {
        return Ok(());
    }
    let mut config = wasmi::Config::default();
    config.consume_fuel(true);
    let engine = wasmi::Engine::new(&config);
    let module = wasmi::Module::new(&engine, wasm)
        .map_err(|e| format!("cannot load module for the capability probe: {e}"))?;
    let sentinel = probe_capability(&engine, &module, DISPATCH_SENTINEL)?;
    if !is_dispatch_fallback(&sentinel, DISPATCH_SENTINEL) {
        return Err(format!(
            "guest does not answer {DISPATCH_SENTINEL:?} with the not-applicable \
             dispatch fallback — capability routing cannot be verified"
        ));
    }
    for cap in caps {
        let Some(name) = cap.as_str() else {
            return Err("manifest.capabilities entries must be strings".into());
        };
        let reply = probe_capability(&engine, &module, name)?;
        if is_dispatch_fallback(&reply, name) {
            return Err(format!(
                "manifest.capabilities entry {name:?} is not implemented by the guest"
            ));
        }
    }
    Ok(())
}

/// The reply every SDK dispatch produces for an unrouted capability:
/// `fail` + `not-applicable` + `capability <name> not supported` (the
/// SDK prefixes every error message with its kind). An
/// implementation's *own* `not-applicable` (a provider/kind mismatch)
/// carries a different message and is never confused for the fallback.
fn is_dispatch_fallback(reply: &serde_json::Value, cap: &str) -> bool {
    reply["type"].as_str() == Some("fail")
        && reply["error"]["kind"].as_str() == Some("not-applicable")
        && reply["error"]["message"].as_str()
            == Some(&format!("not-applicable: capability {cap} not supported"))
}

/// One fresh instance per probe — an invocation's state does not carry
/// across `handle` entries. Sends `{"type":"invoke",...,"payload":{}}`
/// and returns the guest's first reply. A served capability answers
/// `invalid-response` (the empty payload fails its own schema first)
/// or a host-call request.
fn probe_capability(
    engine: &wasmi::Engine,
    module: &wasmi::Module,
    cap: &str,
) -> Result<serde_json::Value, String> {
    const FUEL: u64 = 20_000_000;
    const MEMORY_LIMIT: usize = 64 * 1024 * 1024;
    // Fuel caps instruction count only — a memory.grow can still reach
    // the Wasm32 limit and OOM the validator before fuel runs out.
    // The limiter enforces the host's 64 MiB memory budget too.
    struct ProbeState {
        limits: wasmi::StoreLimits,
    }
    let mut store = wasmi::Store::new(
        engine,
        ProbeState {
            limits: wasmi::StoreLimitsBuilder::default()
                .memory_size(MEMORY_LIMIT)
                .trap_on_grow_failure(true)
                .build(),
        },
    );
    store.limiter(|s| &mut s.limits);
    store
        .set_fuel(FUEL)
        .map_err(|e| format!("cannot cap probe fuel: {e}"))?;
    let linker = wasmi::Linker::new(engine);
    let instance = linker
        .instantiate_and_start(&mut store, module)
        .map_err(|e| format!("cannot instantiate guest: {e}"))?;
    let memory = instance
        .get_memory(&store, "memory")
        .ok_or_else(|| "missing memory export".to_string())?;
    let alloc = instance
        .get_typed_func::<u32, u32>(&store, "alloc")
        .map_err(|e| format!("guest alloc is not callable: {e}"))?;
    let handle = instance
        .get_typed_func::<(u32, u32), u64>(&store, "handle")
        .map_err(|e| format!("guest handle is not callable: {e}"))?;

    let invoke = serde_json::json!({
        "type": "invoke",
        "request_id": "auqw-validate",
        "capability": cap,
        "payload": {},
    });
    let input =
        serde_json::to_vec(&invoke).map_err(|e| format!("cannot serialize probe step: {e}"))?;
    let len = u32::try_from(input.len()).map_err(|_| "probe step too large".to_string())?;
    let ptr = alloc
        .call(&mut store, len)
        .map_err(|e| format!("guest alloc trapped: {e}"))?;
    let start = usize::try_from(ptr).unwrap_or(usize::MAX);
    let end = start.saturating_add(input.len());
    if end > memory.data(&store).len() {
        return Err("guest alloc returned an out-of-bounds pointer".into());
    }
    memory.data_mut(&mut store)[start..end].copy_from_slice(&input);
    let packed = handle
        .call(&mut store, (ptr, len))
        .map_err(|e| format!("guest handle trapped while probing {cap:?}: {e}"))?;
    let out_ptr = usize::try_from(packed >> 32).unwrap_or(usize::MAX);
    let out_len = usize::try_from(packed & 0xffff_ffff).unwrap_or(usize::MAX);
    let data = memory.data(&store);
    let Some(out_end) = out_ptr.checked_add(out_len) else {
        return Err("guest reply lies outside its linear memory".into());
    };
    if out_end > data.len() {
        return Err("guest reply lies outside its linear memory".into());
    }
    serde_json::from_slice(&data[out_ptr..out_end])
        .map_err(|e| format!("guest reply is not JSON: {e}"))
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
    fn abi_0_1_serves_the_full_capability_set() {
        // The one ABI serves every capability — the tier matrix went
        // with the installed base that never shipped.
        assert_eq!(
            check_manifest(&manifest("0.1.0", "[\"playback.resolve\"]", "[]")),
            Ok(())
        );
        assert_eq!(
            check_manifest(&manifest(
                "0.1.0",
                "[\"catalog.entity\",\"catalog.search.kinds\",\"lyrics.plain\",\"lyrics.synced\",\"radio.seed\"]",
                "[]"
            )),
            Ok(())
        );
        assert!(check_manifest(&manifest("0.1.0", "[\"bogus.cap\"]", "[]")).is_err());
        // Any other abi string is refused outright.
        assert!(check_manifest(&manifest("0.2.0", "[\"playback.resolve\"]", "[]")).is_err());
        assert!(check_manifest(&manifest("0.4.0", "[\"playback.resolve\"]", "[]")).is_err());
    }

    #[test]
    fn kv_and_network_permissions() {
        assert_eq!(
            check_manifest(&manifest("0.1.0", "[\"playback.resolve\"]", "[\"kv\"]")),
            Ok(())
        );
        assert_eq!(
            check_manifest(&manifest(
                "0.1.0",
                "[\"playback.resolve\"]",
                "[\"pot-provider\",\"network:*.googlevideo.com\",\"kv\"]"
            )),
            Ok(())
        );
        assert!(check_manifest(&manifest("0.1.0", "[\"playback.resolve\"]", "[\"fs\"]")).is_err());
        assert_eq!(
            check_manifest(&manifest("0.1.0", "[\"lyrics.plain\"]", "[\"kv\"]")),
            Ok(())
        );
        // Loopback literals grant only as bare literals — a wildcarded
        // form would cover every `*.localhost`/`*.127.x` destination.
        for (perm, ok) in [
            ("network:localhost", true),
            ("network:127.0.0.1", true),
            ("network:*.localhost", false),
            ("network:*.127.0.0.1", false),
        ] {
            let perms = format!("[\"{perm}\"]");
            let result = check_manifest(&manifest("0.1.0", "[\"playback.resolve\"]", &perms));
            assert_eq!(result.is_ok(), ok, "{perm}: {result:?}");
        }
    }

    #[test]
    fn abi_0_1_allows_kv_and_pot_permissions() {
        assert_eq!(
            check_manifest(&manifest(
                "0.1.0",
                "[\"playback.resolve\"]",
                "[\"pot-provider\",\"network:x.test\",\"kv\"]"
            )),
            Ok(())
        );
    }

    /// A module whose data segments carry each given literal — the
    /// shape capability names and host-call kinds take in a real guest.
    fn wasm_with_literals(literals: &[&str]) -> Vec<u8> {
        let data = literals
            .iter()
            .enumerate()
            .map(|(i, s)| format!("(data (i32.const {}) \"{s}\")", i * 1024))
            .collect::<Vec<_>>()
            .join("\n            ");
        wat::parse_str(format!(
            "(module
                (memory (export \"memory\") 1)
                {data}
                (func (export \"alloc\") (param i32) (result i32) (i32.const 0))
                (func (export \"handle\") (param i32 i32) (result i64) (i64.const 0)))"
        ))
        .unwrap_or_default()
    }

    #[test]
    fn alignment_passes_when_claims_match_guest() {
        let wasm = wasm_with_literals(&[
            "playback.resolve",
            "http_request",
            "x.test",
            "kv_get",
            "pot_token",
        ]);
        assert_eq!(
            check_guest_alignment(
                &manifest(
                    "0.3.0",
                    "[\"playback.resolve\"]",
                    "[\"network:x.test\",\"kv\",\"pot-provider\"]"
                ),
                &wasm
            ),
            Ok(())
        );
    }

    /// The canonical reply an SDK dispatch gives an unrouted probe.
    const FALLBACK_SENTINEL: &str = "{\"type\":\"fail\",\"error\":{\"kind\":\"not-applicable\",\
        \"message\":\"not-applicable: capability auqw.sentinel not supported\"}}";

    /// A module whose `handle` answers `sentinel_reply` when the probed
    /// capability starts with `sentinel_first` and `other_reply`
    /// otherwise — the capability name sits at a fixed offset inside
    /// the serialized invoke step (located dynamically so serde_json's
    /// key order cannot silently move it).
    fn wasm_dispatcher(sentinel_first: u8, sentinel_reply: &str, other_reply: &str) -> Vec<u8> {
        let step = serde_json::json!({
            "type": "invoke",
            "request_id": "auqw-validate",
            "capability": "Q",
            "payload": {},
        })
        .to_string();
        let off = step.find('Q').unwrap_or_default();
        let s_esc = sentinel_reply.replace('\\', "\\\\").replace('"', "\\\"");
        let o_esc = other_reply.replace('\\', "\\\\").replace('"', "\\\"");
        wat::parse_str(format!(
            "(module
                (memory (export \"memory\") 1)
                (data (i32.const 2048) \"{s_esc}\")
                (data (i32.const 4096) \"{o_esc}\")
                (func (export \"alloc\") (param i32) (result i32) (i32.const 16))
                (func (export \"handle\") (param i32 i32) (result i64)
                    (select
                        (i64.const {})
                        (i64.const {})
                        (i32.eq
                            (i32.load8_u (i32.add (local.get 0) (i32.const {off})))
                            (i32.const {sentinel_first})))))",
            (2048u64 << 32) | sentinel_reply.len() as u64,
            (4096u64 << 32) | other_reply.len() as u64,
        ))
        .unwrap_or_default()
    }

    #[test]
    fn capability_probe_detects_dispatch_fallback() {
        let wasm = wasm_dispatcher(
            b'a',
            FALLBACK_SENTINEL,
            "{\"type\":\"fail\",\"error\":{\"kind\":\"not-applicable\",\
             \"message\":\"not-applicable: capability bogus.cap not supported\"}}",
        );
        assert!(
            check_capability_support(&manifest("0.3.0", "[\"bogus.cap\"]", "[]"), &wasm)
                .is_err_and(|e| e.contains("bogus.cap"))
        );
    }

    #[test]
    fn capability_probe_accepts_served_replies() {
        // A served capability fails the empty payload with its own
        // error kind, a host-call request, or a result — and an
        // implementation's own not-applicable (a provider/kind
        // mismatch) is not the dispatch fallback.
        for reply in [
            "{\"type\":\"fail\",\"error\":{\"kind\":\"invalid-response\",\"message\":\"payload: missing\"}}",
            "{\"type\":\"kv_get\",\"id\":1,\"key\":\"x\"}",
            "{\"type\":\"done\",\"result\":{}}",
            "{\"type\":\"fail\",\"error\":{\"kind\":\"not-applicable\",\"message\":\"ref is not a deezer track ref\"}}",
        ] {
            let wasm = wasm_dispatcher(b'a', FALLBACK_SENTINEL, reply);
            assert_eq!(
                check_capability_support(&manifest("0.3.0", "[\"playback.resolve\"]", "[]"), &wasm),
                Ok(()),
                "{reply}"
            );
        }
    }

    #[test]
    fn capability_probe_rejects_absent_dispatch_contract() {
        // A guest that ignores `capability` and fails uniformly has no
        // routing — without the sentinel check every declaration would
        // look served.
        let wasm = wasm_dispatcher(
            b'a',
            "{\"type\":\"fail\",\"error\":{\"kind\":\"invalid-response\",\"message\":\"bad payload\"}}",
            "{\"type\":\"fail\",\"error\":{\"kind\":\"invalid-response\",\"message\":\"bad payload\"}}",
        );
        assert!(check_capability_support(
            &manifest("0.3.0", "[\"playback.resolve\"]", "[]"),
            &wasm
        )
        .is_err_and(|e| e.contains("cannot be verified")));
    }

    #[test]
    fn alignment_checks_network_destination_literals() {
        // A declared non-wildcard destination must appear as a literal.
        let wasm = wasm_with_literals(&["http_request", "api.deezer.com"]);
        assert!(check_guest_alignment(
            &manifest("0.3.0", "[]", "[\"network:itunes.apple.com\"]"),
            &wasm
        )
        .is_err_and(|e| e.contains("itunes.apple.com")));
        assert_eq!(
            check_guest_alignment(
                &manifest("0.3.0", "[]", "[\"network:api.deezer.com\"]"),
                &wasm
            ),
            Ok(())
        );
        // Wildcard destinations ride on payload URLs — undecidable.
        let wasm = wasm_with_literals(&["http_request"]);
        assert_eq!(
            check_guest_alignment(
                &manifest("0.3.0", "[]", "[\"network:*.googlevideo.com\"]"),
                &wasm
            ),
            Ok(())
        );
    }

    #[test]
    fn alignment_network_destination_anchors_on_hostname_boundaries() {
        // The host grants on exact host equality, so a longer name
        // that merely embeds the declared one is dead scope — case,
        // `_`, and label extensions all fail the boundary.
        let wasm = wasm_with_literals(&[
            "http_request",
            "xapi.deezer.com",
            "api.deezer.com.evil.tld",
            "Wapi.deezer.com",
            "_api.deezer.com",
        ]);
        assert!(check_guest_alignment(
            &manifest("0.3.0", "[]", "[\"network:api.deezer.com\"]"),
            &wasm
        )
        .is_err_and(|e| e.contains("api.deezer.com")));
        // The same host inside a formatted URL is a real use.
        let wasm = wasm_with_literals(&["http_request", "https://api.deezer.com/search"]);
        assert_eq!(
            check_guest_alignment(
                &manifest("0.3.0", "[]", "[\"network:api.deezer.com\"]"),
                &wasm
            ),
            Ok(())
        );
    }

    #[test]
    fn alignment_rejects_unused_permissions() {
        let wasm = wasm_with_literals(&["playback.resolve", "http_request"]);
        for perm in ["kv", "pot-provider"] {
            assert!(
                check_guest_alignment(
                    &manifest("0.3.0", "[\"playback.resolve\"]", &format!("[\"{perm}\"]")),
                    &wasm
                )
                .is_err_and(|e| e.contains(perm)),
                "{perm}"
            );
        }
        // kv_set alone satisfies the kv grant; pot_token alone the pot grant.
        let wasm = wasm_with_literals(&["playback.resolve", "http_request", "kv_set", "pot_token"]);
        assert_eq!(
            check_guest_alignment(
                &manifest(
                    "0.3.0",
                    "[\"playback.resolve\"]",
                    "[\"kv\",\"pot-provider\"]"
                ),
                &wasm
            ),
            Ok(())
        );
        // A network grant with no fetch host call is dead scope.
        let wasm = wasm_with_literals(&["playback.resolve", "kv_get"]);
        assert!(check_guest_alignment(
            &manifest("0.3.0", "[\"playback.resolve\"]", "[\"network:x.test\"]"),
            &wasm
        )
        .is_err_and(|e| e.contains("network:x.test")));
    }
}
