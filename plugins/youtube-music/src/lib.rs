//! Minimal YouTube Music `playback.resolve` guest (ABI v0).
//!
//! Slice 0 scope: resolve one video ID through a four-rung,
//! version-pinned client ladder (`IOS`, two `ANDROID_VR` pins,
//! `VISIONOS`) via host-performed HTTP. No cookies, no signature
//! deciphering, no KV, no search/radio/candidates — see `README.md`.

mod guest;
mod parse;
mod rungs;

use core::alloc::Layout;
use core::slice;

/// ABI-required allocator: hands the host a guest-owned buffer of `len`
/// bytes to write the next step message into.
///
/// The buffer is deliberately never freed — instances are discarded after
/// each invocation, so the leak is bounded by the step budget.
#[no_mangle]
pub extern "C" fn alloc(len: u32) -> u32 {
    let Ok(layout) = Layout::from_size_align(len as usize, 1) else {
        return 0;
    };
    // SAFETY: `layout` is valid; `alloc` either returns a valid buffer or
    // null, which the host treats as an allocation failure.
    unsafe { std::alloc::alloc(layout) as u32 }
}

/// ABI-required step entry: consume the step message at `ptr`/`len`,
/// return `(ptr << 32) | len` of the guest-owned response.
///
/// The response buffer is leaked intentionally so it stays valid until
/// the next `handle`/`alloc` call, per the contract.
#[no_mangle]
pub extern "C" fn handle(ptr: u32, len: u32) -> u64 {
    // SAFETY: per the ABI, `ptr..ptr+len` is initialized guest memory
    // that the host wrote before calling `handle`.
    let input = unsafe { slice::from_raw_parts(ptr as *const u8, len as usize) };
    let out = guest::step(input);
    let out_ptr = out.as_ptr() as u64;
    let out_len = out.len() as u64;
    core::mem::forget(out);
    (out_ptr << 32) | out_len
}
