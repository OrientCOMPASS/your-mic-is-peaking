//! MicYou native plugin ABI bindings (mirrors `micyou_plugin_abi.h`, ABI v1).
//!
//! Layout rules honored here (see MicYou docs/plugins/api-reference.md):
//! * The 7 function pointers before `ctx` are frozen — declared non-optional
//!   exactly like the official `native-soundpad` example.
//! * Everything appended after `ctx` is declared `Option<fn>` and null-checked
//!   before every call, so a host that leaves an extension slot empty degrades
//!   gracefully.
//! * We deliberately do NOT declare the API-version-2 control-plane fields
//!   (`set_muted` … `set_dsp_settings`) we don't use: reading past the end of
//!   a shorter host table would be out-of-bounds, so the struct stops at
//!   `set_panel_icon` — the last field a v1 host is guaranteed to have.
//!
//! Threading contract (enforced by the crate, not just documented):
//! host callbacks are ONLY invoked from `init`/`deinit`/`handle_message`
//! contexts (host-dispatched threads). The real-time `process` path never
//! touches [`HOST`].

#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::Mutex;

pub const MPL_ABI_VERSION: u32 = 1;
pub const MPL_API_VERSION: u32 = 1;

/// Result codes returned by every plugin entry point (`mpl_result_t`).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum mpl_result_t {
    MPL_OK = 0,
    MPL_ERR_NOT_IMPLEMENTED = 1,
    MPL_ERR_INVALID_ARG = 2,
    MPL_ERR_RUNTIME = 3,
    MPL_ERR_BUFFER_TOO_SMALL = 4,
    MPL_ERR_PERMISSION = 5,
}

/// Log levels (`mpl_log_level_t`), passed as plain i32 across the boundary
/// (repr(C) enum == C int == i32; matches the host table's ABI).
pub const LOG_ERROR: i32 = 0;
pub const LOG_WARN: i32 = 1;
pub const LOG_INFO: i32 = 2;

/// Host callback table (`mpl_host_api_t`). Field order MUST match the C header
/// byte-for-byte; new host fields are only ever appended after `ctx`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct mpl_host_api_t {
    pub log: unsafe extern "C" fn(*mut c_void, i32, *const c_char),
    pub get_config:
        unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_char, *mut u32) -> mpl_result_t,
    pub set_config: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> mpl_result_t,
    pub emit_event: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> mpl_result_t,
    pub send_message:
        unsafe extern "C" fn(*mut c_void, *const c_char, *const u8, u32) -> mpl_result_t,
    pub audio_state: unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> mpl_result_t,
    pub connected_devices: unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> mpl_result_t,
    pub ctx: *mut c_void,
    // ── Appended extensions (all optional / null-checked) ──
    pub play_sound: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> mpl_result_t>,
    pub plugin_dir:
        Option<unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> mpl_result_t>,
    pub register_hotkey:
        Option<unsafe extern "C" fn(*mut c_void, *const c_char, *mut u64) -> mpl_result_t>,
    pub open_window: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> mpl_result_t>,
    pub fs_read:
        Option<unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_char, *mut u32) -> mpl_result_t>,
    pub fs_write: Option<unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> mpl_result_t>,
    pub set_timeout:
        Option<unsafe extern "C" fn(*mut c_void, u64, *const c_char, *mut u64) -> mpl_result_t>,
    pub clear_timeout: Option<unsafe extern "C" fn(*mut c_void, u64) -> mpl_result_t>,
    pub http_request: Option<
        unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, *const c_char, *const c_char, *mut u64) -> mpl_result_t,
    >,
    pub set_interval:
        Option<unsafe extern "C" fn(*mut c_void, u64, *const c_char, *mut u64) -> mpl_result_t>,
    pub clear_interval: Option<unsafe extern "C" fn(*mut c_void, u64) -> mpl_result_t>,
    pub open_url: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> mpl_result_t>,
    pub notify:
        Option<unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> mpl_result_t>,
    pub locale:
        Option<unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> mpl_result_t>,
    pub host_info:
        Option<unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> mpl_result_t>,
    pub clipboard_read:
        Option<unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> mpl_result_t>,
    pub clipboard_write: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> mpl_result_t>,
    pub set_panel_icon:
        Option<unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> mpl_result_t>,
}

// The table is a plain-old-data function-pointer bundle; the host keeps the
// backing context alive for the whole plugin lifetime, so it is safe to store
// and to move across the threads the host itself dispatches us on.
unsafe impl Send for mpl_host_api_t {}
unsafe impl Sync for mpl_host_api_t {}

/// Static plugin identity (`mpl_plugin_info_t`).
#[repr(C)]
pub struct mpl_plugin_info_t {
    pub abi_version: u32,
    pub api_version: u32,
    pub id: *const c_char,
    pub version: *const c_char,
}
unsafe impl Sync for mpl_plugin_info_t {}

/// By-value copy of the host table, stored during `init` (the pointer handed
/// to `init` must never be retained — api-reference.md §调用约定).
static HOST: Mutex<Option<mpl_host_api_t>> = Mutex::new(None);

pub fn store_host(host: mpl_host_api_t) {
    if let Ok(mut slot) = HOST.lock() {
        *slot = Some(host);
    }
}

pub fn clear_host() {
    if let Ok(mut slot) = HOST.lock() {
        *slot = None;
    }
}

/// Run `f` with the stored host table. Returns `None` before `init` / after
/// `deinit`, or when the (poisoned) lock is unavailable. Never call this from
/// the real-time audio thread.
pub fn with_host<R>(f: impl FnOnce(&mpl_host_api_t) -> R) -> Option<R> {
    HOST.lock()
        .ok()
        .and_then(|slot| slot.as_ref().map(f))
}

pub fn log(level: i32, msg: &str) {
    with_host(|h| {
        if let Ok(c) = CString::new(msg) {
            unsafe { (h.log)(h.ctx, level, c.as_ptr()) };
        }
    });
}

pub fn log_info(msg: &str) {
    log(LOG_INFO, msg);
}
pub fn log_warn(msg: &str) {
    log(LOG_WARN, msg);
}
#[allow(dead_code)] // 保留完整日志等级，便于后续排障细化
pub fn log_error(msg: &str) {
    log(LOG_ERROR, msg);
}

/// `get_config(key)` following the out/out_size buffer contract:
/// MPL_OK + NUL-terminated JSON in the buffer, or empty string when the key
/// does not exist / the call fails. A fixed 16 KiB buffer covers every value
/// this plugin stores (status blobs are a few hundred bytes).
pub fn get_config(key: &str) -> String {
    with_host(|h| {
        let Ok(k) = CString::new(key) else {
            return String::new();
        };
        let mut buf = [0u8; 16384];
        let mut size = buf.len() as u32;
        let code = unsafe {
            (h.get_config)(
                h.ctx,
                k.as_ptr(),
                buf.as_mut_ptr() as *mut c_char,
                &mut size,
            )
        };
        if code == mpl_result_t::MPL_OK {
            let len = (size as usize).min(buf.len().saturating_sub(1));
            String::from_utf8_lossy(&buf[..len]).into_owned()
        } else {
            String::new()
        }
    })
    .unwrap_or_default()
}

/// `set_config(key, json_value)` — `json_value` must be valid JSON text.
pub fn set_config(key: &str, json_value: &str) -> bool {
    with_host(|h| {
        let (Ok(k), Ok(v)) = (CString::new(key), CString::new(json_value)) else {
            return false;
        };
        let code = unsafe { (h.set_config)(h.ctx, k.as_ptr(), v.as_ptr()) };
        code == mpl_result_t::MPL_OK
    })
    .unwrap_or(false)
}

/// `notify(title, body)` — no capability required; silently skipped when the
/// host table lacks the extension slot.
pub fn notify(title: &str, body: &str) -> bool {
    with_host(|h| {
        let Some(f) = h.notify else { return false };
        let (Ok(t), Ok(b)) = (CString::new(title), CString::new(body)) else {
            return false;
        };
        (unsafe { f(h.ctx, t.as_ptr(), b.as_ptr()) }) == mpl_result_t::MPL_OK
    })
    .unwrap_or(false)
}

/// `register_hotkey(shortcut)` -> handle id (0 = unavailable/failed).
pub fn register_hotkey(shortcut: &str) -> u64 {
    with_host(|h| {
        let Some(f) = h.register_hotkey else { return 0 };
        let Ok(sc) = CString::new(shortcut) else { return 0 };
        let mut id: u64 = 0;
        if (unsafe { f(h.ctx, sc.as_ptr(), &mut id) }) == mpl_result_t::MPL_OK {
            id
        } else {
            0
        }
    })
    .unwrap_or(0)
}

/// `set_panel_icon(panel_id, icon)` — best-effort cosmetics.
pub fn set_panel_icon(panel_id: &str, icon: &str) {
    with_host(|h| {
        let Some(f) = h.set_panel_icon else { return };
        if let (Ok(p), Ok(i)) = (CString::new(panel_id), CString::new(icon)) {
            unsafe { f(h.ctx, p.as_ptr(), i.as_ptr()) };
        }
    });
}

/// Read a NUL-terminated C string argument defensively (never panics).
pub unsafe fn cstr_or<'a>(p: *const c_char, fallback: &'a str) -> &'a str {
    if p.is_null() {
        return fallback;
    }
    unsafe { CStr::from_ptr(p) }.to_str().unwrap_or(fallback)
}
