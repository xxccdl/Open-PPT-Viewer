//! Windows 文件关联（「默认打开方式」自动注册）。
//!
//! # 为什么不能真的「一键静默设为默认」
//!
//! 从 Windows 8 起，用户级默认程序由
//! `HKCU\...\Explorer\FileExts\<.ext>\UserChoice` 决定，而它的值里带一个
//! 由系统密钥算出的哈希。任何外部程序直接写入都会被判定为篡改而失效 ——
//! 这是微软为防止软件劫持关联设的硬限制，WPS、PotPlayer 等同样绕不过，
//! 所以它们也都要让用户点一次确认。
//!
//! 因此本模块做的是**合规且真正有效**的三件事：
//!
//! 1. 在 `HKCU` 下注册每扩展名独立的 ProgID（图标 + 打开命令）；
//! 2. 写 `OpenWithProgids` 与 `Capabilities`/`RegisteredApplications`，
//!    让本应用出现在右键「打开方式」以及系统「默认应用」页里；
//! 3. 写入 `HKCU\Software\Classes\<.ext>` 的默认值 —— 在用户**从未**
//!    为该扩展名选过默认程序时，这就是生效的默认值。
//!
//! 只有当系统已经锁定了 `UserChoice`（例如被 WPS 占着）时，
//! 才需要调起系统设置界面让老师点一次「设为默认值」。
//!
//! 全程只写 `HKCU`，不需要管理员权限，也不碰 `UserChoice`。
//!
//! # 同一台机器上装过两次会怎样
//!
//! ProgID 是**分档**写的：按机器装（`C:\Program Files`）写 HKLM，按用户装
//! （`%LOCALAPPDATA%\Programs\OpenPPTView`）写 HKCU —— 而 HKCR 的解析顺序是
//! **HKCU 优先**。于是「先按用户装过一次、后来按机器升级」的机器上，
//! 旧副本留在 HKCU 的那条命令一直压着新的：老师升级到最新版、双击课件，
//! 起来的还是那个旧副本，他看到的是「打开的还是老版本」。
//!
//! 所以 [`status`] 不只看 ProgID 在不在，还要看它的命令**指着谁**；
//! [`repair_if_needed`] 把「指着别的副本」也一并改回来。

use std::path::{Path, PathBuf};

/// 应用在注册表里使用的名字（`RegisteredApplications` 的键名、
/// 以及系统设置里显示的名字）。
pub const APP_KEY: &str = "OpenPPTView";

/// 应用显示名（出现在「打开方式」菜单与默认应用列表里）。
const APP_DISPLAY: &str = "OpenPPTView 课件讲演器";

/// 一个要关联的扩展名。
#[derive(Debug, Clone, Copy)]
struct Spec {
    ext: &'static str,
    /// 在「打开方式」里显示的友好名称。
    label: &'static str,
}

/// 本应用**当前真正能渲染**的格式。
///
/// 这里刻意只列已实现的格式：如果 .ppt / .docx 出现在关联列表里而打开会报错，
/// 老师双击文件后看到的是错误弹窗 —— 那比「不关联」更糟。
/// 等旧版二进制解析与其它格式落地后，把对应项加到这里即可。
const SPECS: &[Spec] = &[
    Spec {
        ext: "pptx",
        label: "PowerPoint 演示文稿",
    },
    Spec {
        ext: "ppsx",
        label: "PowerPoint 放映",
    },
    Spec {
        ext: "pdf",
        label: "PDF 文档",
    },
];

/// 某个扩展名的关联状态。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssocStatus {
    /// 不含点号的扩展名。
    pub ext: String,
    /// 友好名称。
    pub label: String,
    /// ProgID 是否已登记（「打开方式」候选项里有我们）。
    pub registered: bool,
    /// 那条 ProgID 的打开命令指着的是不是**当前这一份**程序。
    ///
    /// 与 `registered` 分开是因为它们是两件事：键在、但命令指着另一个副本，
    /// 双击课件起来的就不是你现在用的这个版本（见 [`repair_if_needed`]）。
    pub points_here: bool,
    /// 当前的系统默认程序是否就是本应用，**并且真的能打开**。
    pub is_default: bool,
    /// 当前默认程序的名字（若非本应用，用于告知老师现状）。
    pub current_handler: Option<String>,
}

/// 注册结果。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssocOutcome {
    /// 状态列表（注册后重新查询）。
    pub items: Vec<AssocStatus>,
    /// 是否**已经**全部为默认（此时无需再弹系统设置）。
    pub all_default: bool,
}

/// 每种扩展名对应的 ProgID。
fn prog_id(ext: &str) -> String {
    format!("{APP_KEY}.{ext}")
}

/// 把当前可执行文件注册为这些格式的「打开方式」。
///
/// 幂等：重复调用只会覆盖成同样的值。
#[cfg(windows)]
pub fn register_all(exe: &Path) -> Result<AssocOutcome, String> {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let exe_str = exe.to_string_lossy().to_string();
    let command = format!("\"{exe_str}\" \"%1\"");
    let icon = format!("{exe_str},0");

    let classes = hkcu
        .open_subkey_with_flags("Software\\Classes", KEY_READ | winreg::enums::KEY_WRITE)
        .or_else(|_| hkcu.create_subkey("Software\\Classes").map(|(k, _)| k))
        .map_err(|e| format!("无法打开 HKCU\\Software\\Classes：{e}"))?;

    for spec in SPECS {
        let pid = prog_id(spec.ext);

        // ① ProgID：友好名称、图标、打开命令
        let (prog, _) = classes
            .create_subkey(&pid)
            .map_err(|e| format!("无法创建 ProgID {pid}：{e}"))?;
        prog.set_value("", &spec.label).map_err(err)?;
        prog.set_value("FriendlyTypeName", &spec.label).map_err(err)?;
        let (icon_key, _) = prog.create_subkey("DefaultIcon").map_err(err)?;
        icon_key.set_value("", &icon).map_err(err)?;
        let (cmd_key, _) = prog
            .create_subkey("shell\\open\\command")
            .map_err(err)?;
        cmd_key.set_value("", &command).map_err(err)?;

        // ② 出现在右键「打开方式」的候选项里
        let (owp, _) = classes
            .create_subkey(format!(".{}\\OpenWithProgids", spec.ext))
            .map_err(err)?;
        // 值名才是关键，内容留空即可（资源管理器只检查名字是否存在）
        owp.set_value(&pid, &"").map_err(err)?;

        // ③ 让 .ext 在没有 UserChoice 时直接落到本应用。
        //    系统已锁定 UserChoice 时这个值会被忽略，不会造成冲突。
        let (ext_key, _) = classes
            .create_subkey(format!(".{}", spec.ext))
            .map_err(err)?;
        ext_key.set_value("", &pid).map_err(err)?;
    }

    // ④ Capabilities + RegisteredApplications：在系统「默认应用」页里出现
    let (caps, _) = hkcu
        .create_subkey(format!("Software\\{APP_KEY}\\Capabilities"))
        .map_err(err)?;
    caps.set_value("ApplicationName", &APP_DISPLAY).map_err(err)?;
    caps.set_value(
        "ApplicationDescription",
        &"为讲课场景打造的课件放映器，支持 .pptx 与 .pdf".to_string(),
    )
    .map_err(err)?;
    let (fa, _) = caps.create_subkey("FileAssociations").map_err(err)?;
    for spec in SPECS {
        fa.set_value(format!(".{}", spec.ext), &prog_id(spec.ext))
            .map_err(err)?;
    }

    let (reg_apps, _) = hkcu
        .create_subkey("Software\\RegisteredApplications")
        .map_err(err)?;
    reg_apps
        .set_value(APP_KEY, &format!("Software\\{APP_KEY}\\Capabilities"))
        .map_err(err)?;

    let items = status();
    let all_default = !items.is_empty() && items.iter().all(|i| i.is_default);
    Ok(AssocOutcome { items, all_default })
}

/// 自愈：默认打开方式指着我们、但那条 ProgID 不能把我们叫起来，就修回来。
///
/// # 两种要修的情况
///
/// 一、**ProgID 被删了**。卸载会删掉 `OpenPPTView.pptx`，而
/// `FileExts\...\UserChoice` 是 **Windows 保护的键**（值里带哈希，
/// 程序写不进去）—— 它会一直指着那个已经不存在的 ProgID。
/// 于是老师双击课件**毫无反应**：系统找不到处理程序，也没有任何提示。
/// 重装一次本来能好，但「重装之后第一次双击仍然没反应」谁都会以为是程序坏了。
///
/// 二、**ProgID 指着另一个副本**（见模块开头那段）：按用户装过的那份留在
/// HKCU 的命令压着 HKLM 里的新的，双击课件起来的是旧版本。
///
/// 两种情况都不是「老师选了别的程序」，所以每次启动顺手看一眼，
/// 命中就把整套关联重新登记一遍（`register_all` 是幂等的）。
/// 返回是否真的修补过，便于记日志。
///
/// # `may_repoint`：谁能改「指着别人的那一条」
///
/// 键被删了谁都能补（补的是我们自己的键）；但把一条**已经存在、只是指着
/// 另一个副本**的命令改指到自己，只有「装在这台机器上的那一份」才该做 ——
/// 否则开发时直接跑编译产物（`target\release\ppt-app.exe`）也会顺手把老师的
/// `.pptx` 抢过去，而这一整段代码本来就是为了消灭「打开的是另一个版本」。
#[cfg(windows)]
pub fn repair_if_needed(exe: &Path, may_repoint: bool) -> bool {
    let need = status().iter().any(|s| {
        // 老师选了别的程序就完全不插手
        if s.current_handler.as_deref() != Some(prog_id(&s.ext).as_str()) {
            return false;
        }
        if !s.registered {
            return true;
        }
        !s.points_here && may_repoint
    });
    if !need {
        return false;
    }
    register_all(exe).is_ok()
}

/// 非 Windows：没有关联可修。
#[cfg(not(windows))]
pub fn repair_if_needed(_exe: &Path, _may_repoint: bool) -> bool {
    false
}

/// 从一条打开命令里取出可执行文件路径。
///
/// 命令形如 `"C:\…\OpenPPTView.exe" "%1"`（[`register_all`] 与安装程序都这么写）。
/// 解析失败返回 `None` —— 宁可当「不是我们」，也不要误判成「就是这一份」。
fn command_exe(cmd: &str) -> Option<String> {
    let cmd = cmd.trim();
    // 带引号就取引号里那一段（路径里有空格时必然带引号）
    let path = if let Some(rest) = cmd.strip_prefix('"') {
        rest.split('"').next()?
    } else {
        cmd.split_whitespace().next()?
    };
    if path.is_empty() {
        None
    } else {
        Some(path.to_string())
    }
}

/// 我们的 ProgID 登记过没有（HKCU 与 HKLM 都算）。
///
/// 两档都要看：安装程序在 `Program Files` 下装的时候写的是 **HKLM**，
/// 而只在老师点过「注册到本应用」之后 HKCU 才会有。只看 HKCU 会把
/// 「按机器装好、还没点过那个按钮」的机器误判成「尚未注册」——
/// 明明右键「打开方式」里已经有我们了。
#[cfg(windows)]
fn prog_id_registered(pid: &str) -> bool {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;

    let rel = format!("Software\\Classes\\{pid}");
    [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE]
        .iter()
        .any(|root| RegKey::predef(*root).open_subkey(&rel).is_ok())
}

/// 某个 ProgID 的打开命令里写的是哪个 exe。
///
/// 两档都查，顺序与 HKCR 一致：**HKCU 优先，没有才看 HKLM**。
/// 这个顺序不能反：按用户装过的那份留在 HKCU 的旧命令，正是
/// 「双击课件起来的是老版本」的成因（见模块开头那段），
/// 让 HKLM 里那条新的先说话就把它盖过去了。
#[cfg(windows)]
fn registered_exe(pid: &str) -> Option<PathBuf> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;

    let rel = format!("Software\\Classes\\{pid}\\shell\\open\\command");
    for root in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        let Some(cmd) = RegKey::predef(root)
            .open_subkey(&rel)
            .ok()
            .and_then(|k| k.get_value::<String, _>("").ok())
        else {
            continue;
        };
        if let Some(path) = command_exe(&cmd) {
            return Some(PathBuf::from(path));
        }
    }
    None
}

/// 这条命令指着的是不是当前这一份程序。
///
/// 注册表里存的可能是另一种写法（大小写、短名、链接），所以先规范化再比，
/// 规范化不了才退回不区分大小写的字符串比较。
#[cfg(windows)]
fn points_at(path: &Path, exe: &Path) -> bool {
    if path == exe {
        return true;
    }
    match (path.canonicalize(), exe.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => path.to_string_lossy().eq_ignore_ascii_case(&exe.to_string_lossy()),
    }
}

/// 查询当前关联状态。
#[cfg(windows)]
pub fn status() -> Vec<AssocStatus> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);

    // 当前这一份程序是谁：关联指着别处时，界面得能如实说出来
    let me = std::env::current_exe().ok();

    SPECS
        .iter()
        .map(|spec| {
            let pid = prog_id(spec.ext);

            // HKCU 与 HKLM 都算（安装程序按机器装时写的是 HKLM）
            let registered = prog_id_registered(&pid);

            // 键在 ≠ 能叫起我们来：命令可能指着另一个副本（见模块开头那段）
            let points_here = registered
                && me
                    .as_deref()
                    .zip(registered_exe(&pid).as_deref())
                    .is_some_and(|(me, target)| points_at(target, me));

            // UserChoice 优先；没有它时才看 Classes 的默认值
            let user_choice = hkcu
                .open_subkey(format!(
                    "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\FileExts\\.{}\\UserChoice",
                    spec.ext
                ))
                .ok()
                .and_then(|k| k.get_value::<String, _>("ProgId").ok());

            let fallback = hkcu
                .open_subkey(format!("Software\\Classes\\.{}", spec.ext))
                .ok()
                .and_then(|k| k.get_value::<String, _>("").ok())
                .filter(|s| !s.is_empty());

            let handler = user_choice.or(fallback);
            // 「已经是默认」要三件事同时成立：系统指着我们、那条 ProgID 还在、
            // 而且它真的能把我们叫起来。
            //
            // 少一条都会撒谎，而这两种状态都真实出现过：卸载删掉 ProgID 之后
            // `FileExts\...\UserChoice` 还指着它（双击毫无反应）；按用户装过的
            // 那份把命令改成了旧副本（双击起来的是老版本）。
            // 界面应该说「还没设好」，而不是「已经是默认了」。
            let is_default = registered && points_here && handler.as_deref() == Some(pid.as_str());

            AssocStatus {
                ext: spec.ext.to_string(),
                label: spec.label.to_string(),
                registered,
                points_here,
                is_default,
                current_handler: handler,
            }
        })
        .collect()
}

/// 调起系统「默认应用」界面。
///
/// Win10 1709+ 与 Win11 都识别 `ms-settings:` 协议。
/// `registeredAppUser` 参数在支持的版本上会把列表定位到本应用，
/// 不支持的版本会自动忽略它并显示整页 —— 两种情况下老师都能完成任务。
#[cfg(windows)]
pub fn open_default_apps_page() -> Result<(), String> {
    use std::process::Command;

    let target = format!("ms-settings:defaultapps?registeredAppUser={APP_KEY}");
    Command::new("cmd")
        .args(["/C", "start", "", &target])
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("无法打开系统「默认应用」设置：{e}"))
}

// ---------- 非 Windows 平台的占位实现 ----------
//
// 本项目只发布 Windows 版，但保留可编译的空实现，
// 以免将来做跨平台适配时整个 crate 编译不过。

/// 非 Windows 平台：文件关联由系统包管理器负责，这里不做任何事。
#[cfg(not(windows))]
pub fn register_all(_exe: &Path) -> Result<AssocOutcome, String> {
    Ok(AssocOutcome {
        items: Vec::new(),
        all_default: false,
    })
}

/// 非 Windows 平台：没有可查询的关联状态。
#[cfg(not(windows))]
pub fn status() -> Vec<AssocStatus> {
    Vec::new()
}

/// 非 Windows 平台：无系统设置页可调起。
#[cfg(not(windows))]
pub fn open_default_apps_page() -> Result<(), String> {
    Err("设置默认打开方式仅支持 Windows".to_string())
}

#[cfg(windows)]
fn err(e: std::io::Error) -> String {
    format!("写入注册表失败：{e}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prog_id_is_namespaced() {
        // ProgID 必须带应用前缀，否则会和 WPS/Office 抢同一个键名
        assert_eq!(prog_id("pptx"), "OpenPPTView.pptx");
        assert!(prog_id("pdf").starts_with(APP_KEY));
    }

    #[test]
    fn specs_cover_the_formats_we_can_actually_open() {
        // 关联列表必须与 App 真正支持的格式一致：
        // 多关联一个打不开的格式，等于让老师双击后看错误弹窗
        let exts: Vec<&str> = SPECS.iter().map(|s| s.ext).collect();
        assert!(exts.contains(&"pptx"));
        assert!(exts.contains(&"ppsx"));
        assert!(exts.contains(&"pdf"));
        assert!(
            !exts.contains(&"ppt"),
            "旧版 .ppt 尚未实现解析，不应出现在关联列表里"
        );
        assert!(!exts.contains(&"docx"));

        // 扩展名不带点号，避免注册表路径拼出两个点
        for s in SPECS {
            assert!(!s.ext.starts_with('.'), "扩展名不应带点：{}", s.ext);
            assert!(!s.label.is_empty());
        }
    }

    #[test]
    fn command_exe_survives_paths_with_spaces() {
        // 这条解析错了，就会把「指着另一个副本」当成「指着自己」——
        // 也就是这次报的那个问题：双击课件起来的是老版本，界面还说已是默认。
        assert_eq!(
            command_exe("\"C:\\Program Files\\OpenPPTView\\OpenPPTView.exe\" \"%1\"").as_deref(),
            Some("C:\\Program Files\\OpenPPTView\\OpenPPTView.exe")
        );
        assert_eq!(
            command_exe("\"C:\\Users\\a b\\AppData\\Local\\Programs\\OpenPPTView\\OpenPPTView.exe\" \"%1\"").as_deref(),
            Some("C:\\Users\\a b\\AppData\\Local\\Programs\\OpenPPTView\\OpenPPTView.exe")
        );
        // 不带引号的写法也得认
        assert_eq!(
            command_exe("C:\\OpenPPTView.exe %1").as_deref(),
            Some("C:\\OpenPPTView.exe")
        );
        assert_eq!(command_exe("   "), None);
        assert_eq!(command_exe(""), None);
    }

    #[test]
    fn status_does_not_panic_on_a_clean_profile() {
        // 只做「能跑通且字段自洽」的断言：注册表内容依赖具体机器，
        // 断言具体值会让测试在不同环境下随机失败
        for item in status() {
            assert!(!item.ext.starts_with('.'));
            assert!(!item.label.is_empty());
            if item.is_default {
                assert!(item.registered, "默认程序必然已注册：{}", item.ext);
            }
        }
    }
}
