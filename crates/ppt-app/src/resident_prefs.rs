//! 开机自启动与「后台留多久」。
//!
//! # 为什么要「后台启动」
//!
//! 老师开机后第一件事是打开课件。如果每次都要先把应用完整拉起来，
//! 那个「双击课件 → 白窗口 → 等两秒」的观感就白瞎了。开机时先在托盘里待命，
//! 双击课件只是把窗口叫出来，首屏几乎瞬间就到了。
//!
//! 代价是「一个看不见的进程一直挂着」。所以这里给一个**存活时长**：
//! 窗口藏起来多久之后自己退出（0 = 一直留着，由老师自己关）。
//!
//! # 存两处，各管一件事
//!
//! - `%LOCALAPPDATA%\OpenPPTView\resident.json`：老师的意图（开不开自启动、留多久）；
//! - `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`：Windows 实际会不会拉起它。
//!
//! 界面显示以**注册表为准**（那才是真的会发生的事），开关则同时写两处。

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// `Run` 键下这个值的名字。
const RUN_VALUE: &str = "OpenPPTView";
/// 自启动时带的参数：只进托盘，不弹窗。
pub const BACKGROUND_FLAG: &str = "--background";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Prefs {
    /// 老师的意图。实际是否生效看注册表（见 [`registry_enabled`]）。
    #[serde(default)]
    pub auto_start: bool,
    /// 窗口藏起来多久后退出进程；`0` = 常驻。
    #[serde(default)]
    pub idle_exit_minutes: u64,
}

impl Default for Prefs {
    fn default() -> Self {
        // 默认不自启动、藏 1 小时后退出 —— 与之前写死的行为一致，
        // 升级上来的机器什么都不会变。
        Prefs {
            auto_start: false,
            idle_exit_minutes: 60,
        }
    }
}

fn path() -> std::path::PathBuf {
    crate::data_dir().join("resident.json")
}

pub fn load() -> Prefs {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|t| serde_json::from_str::<Prefs>(&t).ok())
        .unwrap_or_default()
}

pub fn save(p: &Prefs) -> Result<(), String> {
    let dir = crate::data_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("建不出目录：{e}"))?;
    let text = serde_json::to_string_pretty(p).map_err(|e| e.to_string())?;
    std::fs::write(path(), text).map_err(|e| format!("写设置失败：{e}"))
}

/// 窗口藏起来之后多久退出。`None` = 常驻。
pub fn idle_duration() -> Option<Duration> {
    idle_duration_of(load().idle_exit_minutes)
}

/// 分钟 → `Duration`；0 表示常驻。上限一个月，挡住手滑填进来的离谱数字。
fn idle_duration_of(minutes: u64) -> Option<Duration> {
    if minutes == 0 {
        None
    } else {
        Some(Duration::from_secs(minutes.min(43_200) * 60))
    }
}

/// 注册表里那条自启动项现在在不在。
pub fn registry_enabled() -> bool {
    #[cfg(windows)]
    {
        use winreg::enums::HKEY_CURRENT_USER;
        use winreg::RegKey;
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        if let Ok(run) = hkcu.open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run") {
            return run.get_value::<String, _>(RUN_VALUE).is_ok();
        }
    }
    false
}

/// 打开/关闭开机自启动。返回注册表现在的状态。
///
/// 写的是 `HKCU` —— 不需要管理员权限，也不会因为换账号就失效（每台机器的登录用户各自生效）。
pub fn set_autostart(on: bool) -> Result<bool, String> {
    #[cfg(windows)]
    {
        use winreg::enums::{HKEY_CURRENT_USER, KEY_WRITE};
        use winreg::RegKey;
        let exe = std::env::current_exe().map_err(|e| format!("读不到程序位置：{e}"))?;
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (run, _) = hkcu
            .create_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run")
            .map_err(|e| format!("打不开 Run 键：{e}"))?;
        if on {
            let cmd = format!("\"{}\" {BACKGROUND_FLAG}", exe.display());
            run.set_value(RUN_VALUE, &cmd)
                .map_err(|e| format!("写自启动失败：{e}"))?;
        } else if let Err(e) = run.delete_value(RUN_VALUE) {
            // 本来就没有这条：不算失败
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(format!("删不掉自启动项：{e}"));
            }
        }
        let mut mine = load();
        mine.auto_start = registry_enabled();
        let _ = save(&mine);
        return Ok(registry_enabled());
    }
    #[allow(unreachable_code)]
    Err("只有 Windows 支持开机自启动".into())
}

/// 这次启动是不是「后台启动」（不弹窗口）。
pub fn started_in_background(args: &[String]) -> bool {
    args.iter().skip(1).any(|a| a == BACKGROUND_FLAG)
}

/// 自启动那条命令（测试与排障用）。
pub fn autostart_command(exe: &Path) -> String {
    format!("\"{}\" {BACKGROUND_FLAG}", exe.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_keep_the_old_behaviour() {
        let p = Prefs::default();
        assert!(!p.auto_start, "默认不自启动");
        assert_eq!(p.idle_exit_minutes, 60, "默认藏 1 小时后退出（与旧版一致）");
    }

    #[test]
    fn zero_minutes_means_stay_forever() {
        assert!(idle_duration_of(0).is_none());
        assert_eq!(idle_duration_of(30), Some(Duration::from_secs(1800)));
    }

    #[test]
    fn background_flag_is_recognised() {
        let args = vec!["app.exe".to_string(), BACKGROUND_FLAG.to_string()];
        assert!(started_in_background(&args));
        assert!(!started_in_background(&["app.exe".to_string()]));
        // 文件名里带这几个字不算
        assert!(!started_in_background(&["--background.exe".to_string()]));
    }

    #[test]
    fn autostart_command_quotes_the_path() {
        let cmd = autostart_command(Path::new("C:\\Program Files\\OpenPPTView\\OpenPPTView.exe"));
        assert!(cmd.starts_with('"'));
        assert!(cmd.contains(BACKGROUND_FLAG));
    }
}
