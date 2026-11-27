//! 全局快捷键模块：负责截图热键的持久化、解析与系统级注册。
//!
//! 截图必须在窗口没有焦点时也能触发，因此使用 rgpui 的系统级全局热键
//! （Windows 上底层为 `RegisterHotKey`），而不是仅窗口聚焦时生效的键位绑定。

use rgpui::{App, Keystroke, components::HotkeyValue};

use crate::{
    config,
    i18n::{tr, tr_fmt},
};

/// 截图热键种类：区域、全屏、取色使用全局热键，长截图仅在拖框时触发。
///（窗口吸附已合入区域截图，不再需要独立窗口热键。）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HotkeyKind {
    /// 区域截图（含窗口吸附）。
    Region,
    /// 全屏截图。
    Full,
    /// 屏幕取色。
    Picker,
    /// 长截图。
    Long,
}

impl HotkeyKind {
    /// 系统注册编号（四种热键互不干扰）。
    pub fn id(self) -> u32 {
        match self {
            HotkeyKind::Region => 1,
            HotkeyKind::Full => 2,
            HotkeyKind::Picker => 3,
            HotkeyKind::Long => 4,
        }
    }

    /// 出厂默认按键串（参考 Snipaste / 微信截图的常用组合）。
    pub fn default_text(self) -> &'static str {
        match self {
            HotkeyKind::Region => "insert",
            HotkeyKind::Long => "",
            HotkeyKind::Full | HotkeyKind::Picker => "",
        }
    }

    /// 界面展示名（跟随当前语言）。
    pub fn label(self) -> String {
        match self {
            HotkeyKind::Region => tr("hotkey.region"),
            HotkeyKind::Full => tr("hotkey.full"),
            HotkeyKind::Picker => tr("hotkey.picker"),
            HotkeyKind::Long => tr("hotkey.long"),
        }
    }
}

/// 把录制到的快捷键值转换为 `Keystroke::parse` 可接受的按键串。
///
/// # 参数
/// - `value`: 快捷键录制框给出的按键值。
///
/// # 返回
/// 如 `ctrl-shift-a` 的小写按键串。
pub fn hotkey_value_to_string(value: &HotkeyValue) -> String {
    // 拼装顺序与 `Keystroke::parse` 的解析格式保持一致。
    let mut parts = Vec::new();
    if value.modifiers.control {
        parts.push("ctrl".to_string());
    }
    if value.modifiers.alt {
        parts.push("alt".to_string());
    }
    if value.modifiers.shift {
        parts.push("shift".to_string());
    }
    if value.modifiers.platform {
        parts.push("secondary".to_string());
    }
    parts.push(value.key.to_lowercase());
    parts.join("-")
}

/// 读取用户保存的截图热键，未设置或非法时回退到该种类的默认值。
pub fn load_hotkey_string(kind: HotkeyKind) -> String {
    let cfg = config::load();
    let stored = match kind {
        HotkeyKind::Region => cfg.hotkeys.region,
        HotkeyKind::Full => cfg.hotkeys.full,
        HotkeyKind::Picker => cfg.hotkeys.picker,
        HotkeyKind::Long => cfg.hotkeys.long,
    };
    resolve_hotkey(kind, stored)
}

/// 空字符串明确表示停用，不能被过滤后恢复成默认快捷键。
fn resolve_hotkey(kind: HotkeyKind, stored: Option<String>) -> String {
    stored
        .map(|text| text.trim().to_string())
        .filter(|text| text.is_empty() || Keystroke::parse(text).is_ok())
        .unwrap_or_else(|| kind.default_text().to_string())
}

/// 保存用户录制的截图热键字符串。
///
/// # 参数
/// - `kind`: 热键种类（决定写哪个配置字段）。
/// - `hotkey`: 如 `ctrl-shift-a` 的按键串。
pub fn save_hotkey_string(kind: HotkeyKind, hotkey: &str) {
    // 写入失败仅忽略（不阻断改键流程），调用方已通过返回值提示注册结果。
    config::update(|cfg| match kind {
        HotkeyKind::Region => cfg.hotkeys.region = Some(hotkey.to_string()),
        HotkeyKind::Full => cfg.hotkeys.full = Some(hotkey.to_string()),
        HotkeyKind::Picker => cfg.hotkeys.picker = Some(hotkey.to_string()),
        HotkeyKind::Long => cfg.hotkeys.long = Some(hotkey.to_string()),
    });
}

/// 应用全局截图热键：先注销旧注册，再注册新按键串。
///
/// # 参数
/// - `cx`: rgpui 应用上下文。
/// - `kind`: 热键种类（决定系统注册编号）。
/// - `hotkey`: 如 `ctrl-shift-a` 的按键串。
///
/// # 返回
/// 注册成功返回 `Ok(())`；按键串非法或被系统占用时返回中文错误说明。
pub fn apply_global_hotkey(cx: &mut App, kind: HotkeyKind, hotkey: &str) -> Result<(), String> {
    // 长截图配置保留兼容旧文件，但绝不向系统注册，避免日常输入被截获。
    if kind == HotkeyKind::Long || hotkey.trim().is_empty() {
        cx.unregister_global_hotkey(kind.id());
        return Ok(());
    }
    // Windows 的 RegisterHotKey 只支持部分按键（如 F1-F12、单字符、方向键等），
    // 先解析再注册，错误直接展示给用户。
    let keystroke = Keystroke::parse(hotkey).map_err(|error| {
        tr_fmt(
            "hotkey.parse_fail",
            &[("hotkey", hotkey), ("error", &error.to_string())],
        )
    })?;
    // 同一编号重复注册前必须先注销，否则系统会返回占用错误。
    cx.unregister_global_hotkey(kind.id());
    cx.register_global_hotkey(kind.id(), &keystroke)
        .map_err(|error| {
            tr_fmt(
                "hotkey.register_fail",
                &[
                    ("label", &kind.label()),
                    ("hotkey", hotkey),
                    ("error", &error.to_string()),
                ],
            )
        })
}

#[cfg(test)]
mod tests {
    //! 防止空值被意外注册，或重启后又恢复与其他应用冲突的默认键。
    use super::*;
    #[test]
    fn blank_means_disabled() {
        assert_eq!(resolve_hotkey(HotkeyKind::Region, None), "insert");
        assert_eq!(resolve_hotkey(HotkeyKind::Full, None), "");
        assert_eq!(resolve_hotkey(HotkeyKind::Picker, None), "");
        assert_eq!(resolve_hotkey(HotkeyKind::Long, None), "");
        assert_eq!(resolve_hotkey(HotkeyKind::Region, Some("  ".into())), "");
        assert_eq!(
            resolve_hotkey(HotkeyKind::Full, Some("ctrl-alt-f".into())),
            "ctrl-alt-f"
        );
    }
}
