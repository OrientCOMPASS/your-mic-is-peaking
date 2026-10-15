//! YourMicIsPeaking — MicYou 原生 DSP 插件：一键炸麦（响度保持的削波失真）。
//!
//! * 按下全局快捷键（默认 `ctrl+shift+f9`）或点面板按钮 → 麦克风信号被
//!   湿推入硬削波，产生「炸麦」破音失真；
//! * **输出响度自动匹配输入**（每帧 RMS 对齐，见 `dsp.rs`）——只炸不变大声；
//! * 再按一次 → 当帧立即旁路恢复干净信号（瞬时开关，无淡变）。
//!
//! 线程纪律（对应 api-reference.md「调用约定与并发安全约束」）：
//!
//! | 线程 | 触碰的资源 | Host API |
//! | --- | --- | --- |
//! | 宿主总线线程（handle_message / init / deinit） | SESSION 状态、原子参数 | ✅ 仅此处调用 |
//! | 宿主实时音频线程（process） | PEAKER、原子参数只读 | ❌ 绝不调用，零分配 |

#![allow(non_camel_case_types)]

use std::cell::UnsafeCell;
use std::ffi::c_char;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};

mod abi;
mod dsp;
mod hotkey;

use abi::{mpl_host_api_t, mpl_plugin_info_t, mpl_result_t};

const PLUGIN_ID: &[u8] = b"opss.your-mic-is-peaking\0";
const PLUGIN_VERSION: &[u8] = concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes();

/// 快捷键配置缺失/不可读时的回退值。
const DEFAULT_HOTKEY: &str = "ctrl+shift+f9";
/// 炸裂强度（湿推倍数）默认值与合法范围。
const DEFAULT_DRIVE: f64 = 24.0;
const DRIVE_MIN: f64 = 1.0;
const DRIVE_MAX: f64 = 128.0;
/// 干湿混合默认值。
const DEFAULT_MIX: f64 = 1.0;

/// 炸麦开关（音频线程快速路径标志）。仅在总线线程持 SESSION 锁时写入。
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// 湿推倍数，f32 bits（音频线程无锁读取）。
static DRIVE_BITS: AtomicU32 = AtomicU32::new(24.0f32.to_bits());
/// 干湿混合比例，f32 bits。
static MIX_BITS: AtomicU32 = AtomicU32::new(1.0f32.to_bits());
/// 状态变化是否弹系统通知。
static NOTIFY_ON: AtomicBool = AtomicBool::new(true);

/// 炸麦处理器状态 — 仅宿主实时音频线程触碰（process 在单线程上串行），
/// 故 UnsafeCell 是健全的；init/deinit 的重置发生在链注册/注销静默窗口内
/// （宿主保证 DSP 注销会等待在途 process 帧结束）。
struct AudioCell(UnsafeCell<dsp::Peaker>);
// SAFETY: single-thread (audio) ownership documented above.
unsafe impl Sync for AudioCell {}
static PEAKER: AudioCell = AudioCell(UnsafeCell::new(dsp::Peaker::new()));

/// 总线线程侧的会话状态（快捷键注册簿记 + 最近错误）。
struct Session {
    /// register_hotkey 返回的句柄（0 = 无）。
    hotkey_id: u64,
    hotkey_text: String,
    hotkey_ok: bool,
    /// 当前 `hotkey_id` 注册时所用的快捷键字符串。
    host_hotkey_sc: String,
    /// 用户改动快捷键后置 false（旧注册无法释放——ABI 没有 unregister——
    /// 其消息按句柄 id 过滤掉）。
    host_hotkey_active: bool,
    last_error: String,
}

impl Session {
    const fn new() -> Self {
        Self {
            hotkey_id: 0,
            hotkey_text: String::new(),
            hotkey_ok: false,
            host_hotkey_sc: String::new(),
            host_hotkey_active: false,
            last_error: String::new(),
        }
    }
}

static SESSION: Mutex<Session> = Mutex::new(Session::new());

fn session() -> MutexGuard<'static, Session> {
    SESSION.lock().unwrap_or_else(|p| p.into_inner())
}

/// Catch every panic at the FFI boundary — unwinding across `extern "C"` is UB
/// and would take the whole host process down.
fn guard<F: FnOnce() -> mpl_result_t>(f: F) -> mpl_result_t {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(mpl_result_t::MPL_ERR_RUNTIME)
}

// ────────────────────────── required entry points ──────────────────────────

#[no_mangle]
pub extern "C" fn micyou_plugin_info() -> *const mpl_plugin_info_t {
    static INFO: mpl_plugin_info_t = mpl_plugin_info_t {
        abi_version: abi::MPL_ABI_VERSION,
        api_version: abi::MPL_API_VERSION,
        id: PLUGIN_ID.as_ptr() as *const std::ffi::c_char,
        version: PLUGIN_VERSION.as_ptr() as *const std::ffi::c_char,
    };
    &INFO
}

/// # Safety
/// `host` must point to a valid `mpl_host_api_t` for the duration of the call;
/// we copy it by value and never retain the pointer (per api-reference.md).
#[no_mangle]
pub unsafe extern "C" fn micyou_plugin_init(host: *const mpl_host_api_t) -> mpl_result_t {
    guard(|| {
        if host.is_null() {
            return mpl_result_t::MPL_ERR_INVALID_ARG;
        }
        // By-value copy — the host may free/reuse the table after init.
        abi::store_host(unsafe { *host });

        // 干净的音频状态（init 运行于链注册之前，音频线程不可能在 process 内）。
        ACTIVE.store(false, Ordering::Release);
        unsafe { (*PEAKER.0.get()).reset() };

        let mut s = session();
        s.last_error.clear();

        // 恢复用户配置（首次启用时由 manifest `config` 播种默认值）。
        s.hotkey_text = config_string("hotkey", DEFAULT_HOTKEY);
        let drive = config_f64("drive", DEFAULT_DRIVE).clamp(DRIVE_MIN, DRIVE_MAX) as f32;
        DRIVE_BITS.store(drive.to_bits(), Ordering::Relaxed);
        let mix = config_f64("mix", DEFAULT_MIX).clamp(0.0, 1.0) as f32;
        MIX_BITS.store(mix.to_bits(), Ordering::Relaxed);
        NOTIFY_ON.store(config_bool("notify", true), Ordering::Relaxed);

        // 注册全局快捷键（先本地校验，错误信息更精确；配置变更时活体重规划）。
        plan_hotkey(&mut s);
        if s.hotkey_ok {
            abi::log_info(&format!(
                "YourMicIsPeaking v{} ready — hotkey [{}] registered (id {})",
                env!("CARGO_PKG_VERSION"),
                s.hotkey_text,
                s.hotkey_id
            ));
        } else {
            abi::log_warn(&format!(
                "hotkey unavailable for [{}]: {} — use the panel button to toggle",
                s.hotkey_text, s.last_error
            ));
        }

        abi::set_panel_icon("status", "💥");
        write_status(&s);
        mpl_result_t::MPL_OK
    })
}

#[no_mangle]
pub extern "C" fn micyou_plugin_deinit() {
    let _ = guard(|| {
        // 先灭音频路径：卸载后任何在途帧都必须旁路。
        ACTIVE.store(false, Ordering::Release);
        unsafe { (*PEAKER.0.get()).reset() };
        let mut s = session();
        *s = Session::new();
        drop(s);
        abi::clear_host();
        mpl_result_t::MPL_OK
    });
}

// ────────────────────────── optional entry points ──────────────────────────

/// 实时 DSP：响度保持的硬削波炸麦。
///
/// Contract: `data` holds `samples` interleaved f32 values (`channels` per
/// frame); everything here is allocation-free, lock-free and bounded —
/// 3 atomic loads + two passes of cheap math over the frame.
///
/// # Safety
/// `data` must point to `samples` writable f32s, `bypass` to a u32.
#[no_mangle]
pub unsafe extern "C" fn micyou_plugin_process(
    data: *mut f32,
    samples: u32,
    channels: u32,
    _queued_ms: f64,
    bypass: *mut u32,
) -> mpl_result_t {
    guard(|| {
        if data.is_null() || bypass.is_null() || channels == 0 || samples == 0 {
            if !bypass.is_null() {
                unsafe { *bypass = 1 };
            }
            return mpl_result_t::MPL_ERR_INVALID_ARG;
        }
        let params = dsp::Params {
            active: ACTIVE.load(Ordering::Acquire),
            drive: f32::from_bits(DRIVE_BITS.load(Ordering::Relaxed)),
            mix: f32::from_bits(MIX_BITS.load(Ordering::Relaxed)),
        };
        let slice = unsafe { std::slice::from_raw_parts_mut(data, samples as usize) };
        let processed = unsafe { (*PEAKER.0.get()).process(slice, channels as usize, &params) };
        unsafe { *bypass = if processed { 0 } else { 1 } };
        mpl_result_t::MPL_OK
    })
}

/// Bus messages: hotkey toggles and panel actions.
///
/// Runs on a host-dispatched thread — the ONLY context where Host API calls
/// are legal (besides init/deinit). Everything here is O(1) and cheap.
///
/// # Safety
/// `topic` is a NUL-terminated string; `payload` spans `payload_len` bytes.
#[no_mangle]
pub unsafe extern "C" fn micyou_plugin_handle_message(
    _source: *const c_char,
    topic: *const c_char,
    _payload: *const u8,
    _payload_len: u32,
) -> mpl_result_t {
    guard(|| {
        let topic = unsafe { abi::cstr_or(topic, "") };
        if let Some(id_str) = topic.strip_prefix("hotkey:") {
            let pressed: u64 = id_str.parse().unwrap_or(0);
            let mut s = session();
            // 只认当前有效的宿主注册（旧快捷键字符串的陈旧注册按句柄 id 过滤）。
            if !s.host_hotkey_active || pressed == 0 || pressed != s.hotkey_id {
                return mpl_result_t::MPL_OK;
            }
            toggle(&mut s);
            return mpl_result_t::MPL_OK;
        }
        match topic {
            "ui:toggle" => {
                let mut s = session();
                toggle(&mut s);
            }
            "ui:on" => {
                let mut s = session();
                set_active(&mut s, true);
            }
            "ui:off" => {
                let mut s = session();
                set_active(&mut s, false);
            }
            "ui:apply" => {
                let mut s = session();
                apply_config(&mut s);
            }
            _ => {}
        }
        mpl_result_t::MPL_OK
    })
}

// ─────────────────────────── state machine (bus thread) ────────────────────

fn toggle(s: &mut Session) {
    let on = !ACTIVE.load(Ordering::Relaxed);
    set_active(s, on);
}

fn set_active(s: &mut Session, on: bool) {
    if ACTIVE.load(Ordering::Relaxed) == on {
        return; // 幂等：面板连点 / 快捷键抖动不产生重复通知
    }
    ACTIVE.store(on, Ordering::Release);
    if on {
        abi::log_info("peaking ON — mic is blowing up (loudness matched to input)");
        notify_if_enabled("💥 炸麦开启", "麦克风已破音失真（音量与输入保持一致）；再按一次即可恢复");
    } else {
        abi::log_info("peaking OFF");
        notify_if_enabled("🎙️ 炸麦关闭", "麦克风已恢复正常");
    }
    write_status(s);
}

fn apply_config(s: &mut Session) {
    let drive = config_f64("drive", DEFAULT_DRIVE).clamp(DRIVE_MIN, DRIVE_MAX) as f32;
    DRIVE_BITS.store(drive.to_bits(), Ordering::Relaxed);
    let mix = config_f64("mix", DEFAULT_MIX).clamp(0.0, 1.0) as f32;
    MIX_BITS.store(mix.to_bits(), Ordering::Relaxed);
    NOTIFY_ON.store(config_bool("notify", true), Ordering::Relaxed);
    let hk = config_string("hotkey", DEFAULT_HOTKEY);
    if hk != s.hotkey_text {
        s.hotkey_text = hk;
        s.last_error.clear();
        // 活体重规划：无需重载插件。旧的宿主注册（上一个组合键）不再被响应——
        // 其句柄 id 已不匹配，且 ABI 无法注销它。
        plan_hotkey(s);
        abi::log_info(&format!(
            "hotkey re-planned: [{}] (ok={})",
            s.hotkey_text, s.hotkey_ok
        ));
    }
    write_status(s);
}

fn plan_hotkey(s: &mut Session) {
    // 同一组合键的既有注册直接复活（A→B→A 往返不得泄漏重复注册——
    // ABI 无法释放旧的，复用既干净又是即时的）。
    if s.hotkey_id != 0 && s.host_hotkey_sc == s.hotkey_text {
        s.host_hotkey_active = true;
        s.hotkey_ok = true;
        s.last_error.clear();
        return;
    }
    s.host_hotkey_active = false;
    s.hotkey_ok = false;

    let text = s.hotkey_text.clone();
    // 前置校验给出精确错误（如鼠标 token），而非宿主的笼统解析失败。
    if let Err(e) = hotkey::parse(&text) {
        s.last_error = format!("快捷键不可用：{e}");
        return;
    }
    let id = abi::register_hotkey(&text);
    if id != 0 {
        s.hotkey_id = id;
        s.host_hotkey_sc = text;
        s.host_hotkey_active = true;
        s.hotkey_ok = true;
    } else {
        s.last_error = "快捷键注册失败：可能与其他程序冲突、格式非法，或当前平台不支持全局快捷键（如 Linux Wayland 会话）——可改用面板按钮".to_string();
    }
}

fn write_status(s: &Session) {
    let status = serde_json::json!({
        "active": ACTIVE.load(Ordering::Relaxed),
        "drive": f32::from_bits(DRIVE_BITS.load(Ordering::Relaxed)),
        "mix": f32::from_bits(MIX_BITS.load(Ordering::Relaxed)),
        "hotkey": s.hotkey_text,
        "hotkeyOk": s.hotkey_ok,
        "error": s.last_error,
    });
    abi::set_config("ymip_status", &status.to_string());
}

fn notify_if_enabled(title: &str, body: &str) {
    if NOTIFY_ON.load(Ordering::Relaxed) {
        abi::notify(title, body);
    }
}

// ─────────────────────────── config helpers (bus thread) ───────────────────

fn config_string(key: &str, default: &str) -> String {
    let raw = abi::get_config(key);
    serde_json::from_str::<String>(&raw).unwrap_or_else(|_| default.to_string())
}
fn config_f64(key: &str, default: f64) -> f64 {
    let raw = abi::get_config(key);
    serde_json::from_str::<f64>(&raw).unwrap_or(default)
}
fn config_bool(key: &str, default: bool) -> bool {
    let raw = abi::get_config(key);
    serde_json::from_str::<bool>(&raw).unwrap_or(default)
}
