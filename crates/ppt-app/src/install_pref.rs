//! 安装向导留下的偏好。
//!
//! 安装程序会把老师选的「操作方式」（自动 / 触摸屏 / 鼠标键盘）写进
//! `HKLM\Software\OpenPPTView`。应用首启读它，就不必再自己猜设备 ——
//! `(pointer: coarse)` 在一体机、带触摸屏的笔记本上并不可靠：
//! 插上鼠标它说自己是鼠标，拔掉又说自己是触摸屏。
//!
//! # 优先级
//!
//! 「老师后来在应用里改过」> 「安装时选的」> 「自动识别」。
//! 所以这里只负责把安装时的选择交出去，应用里的改动由前端记着（见 `loadInputMode`）。

use std::path::PathBuf;

use serde::Serialize;

/// 注册表里放我们这一摊的键。
pub const KEY: &str = "Software\\OpenPPTView";

/// 安装时的选择。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallPreference {
    /// `auto` / `touch` / `mouse`。
    pub input_mode: String,
    /// 安装时是否勾了「在桌面上放一个快捷方式」。
    pub desktop_shortcut: bool,
    /// 装在哪。
    ///
    /// 应用用它回答一个很具体的问题：**我是不是装在这台机器上的那一份**。
    /// 只有那一份才有资格把文件关联改指到自己（见
    /// [`crate::associations::repair_if_needed`]）—— 否则直接跑编译产物
    /// 也会顺手把老师的 `.pptx` 抢过去。
    pub install_location: Option<PathBuf>,
}

/// 读安装时的选择。
///
/// 没读到就返回 `None` —— 开发时直接跑 exe、或者用绿色版解压出来的，
/// 本来就没有这一步，前端会退回到自动识别。
#[cfg(windows)]
pub fn read() -> Option<InstallPreference> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;

    // 安装程序写的是机器级；用户级兜底，方便以后做免管理员的安装方式
    for root in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        let Ok(key) = RegKey::predef(root).open_subkey(KEY) else {
            continue;
        };
        let mode: String = key.get_value("InputMode").unwrap_or_default();
        if mode.is_empty() {
            continue;
        }
        // 认得出是这三种才用；写坏了就当没有，别把界面推进奇怪的状态
        if !matches!(mode.as_str(), "auto" | "touch" | "mouse") {
            continue;
        }
        let desktop: u32 = key.get_value("DesktopShortcut").unwrap_or(1);
        let dir: String = key.get_value("InstallLocation").unwrap_or_default();
        let dir = dir.trim().to_string();
        return Some(InstallPreference {
            input_mode: mode,
            desktop_shortcut: desktop != 0,
            install_location: if dir.is_empty() {
                None
            } else {
                Some(PathBuf::from(dir))
            },
        });
    }
    None
}

#[cfg(not(windows))]
pub fn read() -> Option<InstallPreference> {
    None
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn read_does_not_panic_on_a_clean_profile() {
        // 只要求「读不到时安静地返回」，不依赖机器上装没装过
        let _ = read();
    }
}
