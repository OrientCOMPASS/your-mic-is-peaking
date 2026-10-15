//! Hotkey string parsing / validation (keyboard combinations only).
//!
//! 复用自 Focus-Capture（同为 Unlicense）：在把快捷键字符串交给宿主
//! `register_hotkey` 之前先行校验，给用户精确的错误提示（而不是宿主的
//! 笼统失败）。词法与宿主 global-hotkey 解析器对齐：
//! ctrl/alt/shift/super/cmd/win + a–z、0–9、f1–f24、常用命名键。
//! 鼠标侧键仅用于识别并给出「暂不支持」的可操作错误。

pub const MOD_CTRL: u8 = 0b0001;
pub const MOD_ALT: u8 = 0b0010;
pub const MOD_SHIFT: u8 = 0b0100;
pub const MOD_WIN: u8 = 0b1000;

/// A parsed keyboard-only hotkey combination (`vk` = Win32 virtual-key code).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Spec {
    pub mods: u8,
    pub vk: u16,
}

/// Parse a shortcut string like `ctrl+shift+f9`.
pub fn parse(text: &str) -> Result<Spec, String> {
    let mut mods = 0u8;
    let mut vk: Option<u16> = None;
    for raw in text.split('+') {
        let tok = raw.trim().to_ascii_lowercase();
        if tok.is_empty() {
            continue;
        }
        match tok.as_str() {
            "ctrl" | "control" => mods |= MOD_CTRL,
            "alt" | "option" => mods |= MOD_ALT,
            "shift" => mods |= MOD_SHIFT,
            "win" | "super" | "meta" | "cmd" | "command" => mods |= MOD_WIN,
            "mouse4" | "xbutton1" | "x1" | "back" | "mouse5" | "xbutton2" | "x2" | "forward" => {
                return Err(
                    "鼠标侧键暂不支持：宿主 register_hotkey 仅支持键盘（鼠标监听计划由宿主 API 提供）".into(),
                )
            }
            other => {
                if vk.is_some() {
                    return Err(format!("快捷键只能有一个主键（多余 token: {other}）"));
                }
                vk = Some(key_to_vk(other).ok_or_else(|| format!("无法识别的键名: {other}"))?);
            }
        }
    }
    let vk = vk.ok_or("快捷键缺少主键（如 f9、a、space）".to_string())?;
    // Safety guard: a bare letter/digit would hijack normal typing system-wide.
    if mods == 0 {
        let is_char = (0x30..=0x39).contains(&vk) || (0x41..=0x5A).contains(&vk);
        if is_char {
            return Err("字母/数字主键必须搭配至少一个修饰键（ctrl/alt/shift/win）".into());
        }
    }
    Ok(Spec { mods, vk })
}

/// Key name → Win32 virtual-key code (US-layout VK semantics, which is what
/// RegisterHotKey-style APIs expect for A–Z / 0–9).
fn key_to_vk(name: &str) -> Option<u16> {
    let b = name.as_bytes();
    if b.len() == 1 {
        return match b[0] {
            b'a'..=b'z' => Some(0x41 + (b[0] - b'a') as u16),
            b'0'..=b'9' => Some(0x30 + (b[0] - b'0') as u16),
            _ => None,
        };
    }
    if let Some(n) = name.strip_prefix('f') {
        if let Ok(n) = n.parse::<u16>() {
            if (1..=24).contains(&n) {
                return Some(0x70 + n - 1); // VK_F1..VK_F24
            }
        }
    }
    Some(match name {
        "space" => 0x20,
        "tab" => 0x09,
        "enter" | "return" => 0x0D,
        "backspace" => 0x08,
        "escape" | "esc" => 0x1B,
        "pause" => 0x13,
        "insert" => 0x2D,
        "delete" => 0x2E,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "up" => 0x26,
        "left" => 0x25,
        "down" => 0x28,
        "right" => 0x27,
        "minus" => 0xBD,
        "equal" | "equals" => 0xBB,
        "bracketleft" | "leftbracket" => 0xDB,
        "bracketright" | "rightbracket" => 0xDD,
        "backslash" => 0xDC,
        "semicolon" => 0xBA,
        "quote" | "apostrophe" => 0xDE,
        "comma" => 0xBC,
        "period" | "dot" => 0xBE,
        "slash" => 0xBF,
        "backquote" | "grave" => 0xC0,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_keyboard_combos() {
        let s = parse("ctrl+shift+f9").unwrap();
        assert_eq!(s.mods, MOD_CTRL | MOD_SHIFT);
        assert_eq!(s.vk, 0x78); // VK_F9
    }

    #[test]
    fn key_names_map_to_vk() {
        assert!(parse("a").is_err()); // bare letter rejected
        assert_eq!(parse("ctrl+a").unwrap().vk, 0x41);
        assert_eq!(parse("ctrl+9").unwrap().vk, 0x39);
        assert_eq!(parse("f13").unwrap().vk, 0x7C);
        assert_eq!(parse("ctrl+up").unwrap().vk, 0x26);
        assert!(parse("ctrl+nope").is_err());
        assert!(parse("ctrl").is_err()); // no main key
    }

    #[test]
    fn rejects_duplicate_main_keys() {
        assert!(parse("ctrl+a+b").is_err());
    }

    #[test]
    fn mouse_tokens_get_actionable_error() {
        let e = parse("ctrl+mouse4").unwrap_err();
        assert!(e.contains("鼠标侧键暂不支持"), "{e}");
        assert!(parse("mouse5").unwrap_err().contains("宿主"));
    }
}
