//! 安装 / 卸载的实际动作：解包、拷文件、写注册表、建快捷方式、装运行环境。
//!
//! # 我们为什么自己写安装程序
//!
//! 用 NSIS / Inno 这类通用打包器，界面是它给的；想要 WPS 那种「一屏、一个大按钮」
//! 的定制观感，只能在外壳里塞自绘控件，处处别扭。这个安装程序是我们自己的程序：
//! 界面自己画（见 `ppt-installer-ui`），安装动作自己写。
//!
//! # 载荷放在哪
//!
//! 安装包是**单文件**：`OpenPPTView-Setup.exe` = 本程序 + 尾部追加的载荷。
//! 布局固定，打包脚本按它拼、这里按它读：
//!
//! ```text
//! [安装器 exe][载荷][meta: payload_len u64 | MAGIC u64]
//! 载荷 = [app_len u32][boot_len u32][主程序][WebView2 引导器（可选）]
//! ```
//!
//! 元信息只认**文件最后 16 字节**，不去全文件搜魔数 ——
//! 否则会先搜到代码里那个同样的常量。

use std::fs;
use std::path::{Path, PathBuf};

use windows::core::{Interface, PCWSTR};
use windows::Win32::System::Com::Urlmon::URLDownloadToFileW;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{IShellLinkW, ShellExecuteW, ShellLink};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// 载荷元信息的魔数（只认文件末尾）。
const TAIL_MAGIC: u64 = 0x3159_4150_5650_504F; // "OPPVPAY1" 的小端写法

/// 产品信息。装到哪里、写哪个键，全在这几个常量上。
pub const PRODUCT: &str = "OpenPPTView";
pub const PUBLISHER: &str = "OpenPPTView";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const EXE_NAME: &str = "OpenPPTView.exe";
pub const UNINST_EXE: &str = "uninstall.exe";

/// 主程序的注册表项（应用首启会读这里的 InputMode）。
pub const PREF_KEY: &str = "Software\\OpenPPTView";
/// 卸载信息的注册表项。
pub const UNINST_KEY: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\OpenPPTView";

/// 进度回调：返回 `false` 表示用户按了取消。
pub trait Progress {
    fn step(&mut self, text: &str, percent: f32) -> bool;
}

/// 安装选项（来自第一屏上的选择）。
#[derive(Debug, Clone)]
pub struct Options {
    pub dir: PathBuf,
    pub desktop_shortcut: bool,
    pub input_mode: String,
}

/// 从自身尾部取出载荷。
pub fn read_payload(self_exe: &Path) -> Result<(Vec<u8>, Option<Vec<u8>>), String> {
    let bytes = fs::read(self_exe).map_err(|e| format!("无法读取安装包：{e}"))?;
    if bytes.len() < 16 {
        return Err("安装包不完整：文件长度不足。请重新下载后再试。".into());
    }
    let tail = &bytes[bytes.len() - 16..];
    let payload_len = u64::from_le_bytes(tail[0..8].try_into().unwrap()) as usize;
    let magic = u64::from_le_bytes(tail[8..16].try_into().unwrap());
    if magic != TAIL_MAGIC {
        // 最常见的两种情况：下载/拷贝过程中文件损坏，或者拿错了文件
        // （`cargo build` 产出的 `ppt-installer.exe` 就是这个壳，尾部没有载荷）。
        return Err("安装包中缺少程序数据，文件可能已损坏或经过修改。请重新下载后再试。".into());
    }
    if payload_len == 0 || payload_len + 16 > bytes.len() {
        return Err("安装包中的程序数据不完整。请重新下载后再试。".into());
    }
    let payload = &bytes[bytes.len() - 16 - payload_len..bytes.len() - 16];
    if payload.len() < 8 {
        return Err("安装包中的程序数据不完整。请重新下载后再试。".into());
    }
    let app_len = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
    let boot_len = u32::from_le_bytes(payload[4..8].try_into().unwrap()) as usize;
    let app_start = 8;
    let app_end = app_start + app_len;
    let boot_end = app_end + boot_len;
    if app_end > payload.len() || boot_end > payload.len() {
        return Err("安装包中的程序数据不完整。请重新下载后再试。".into());
    }
    let app = payload[app_start..app_end].to_vec();
    let boot = if boot_len > 0 {
        Some(payload[app_end..boot_end].to_vec())
    } else {
        None
    };
    Ok((app, boot))
}

/// 打出「完整的安装包」（打包脚本用同一套格式）。
#[cfg(test)]
pub fn build_setup(installer: &[u8], app: &[u8], boot: Option<&[u8]>) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&(app.len() as u32).to_le_bytes());
    payload.extend_from_slice(&((boot.map(|b| b.len()).unwrap_or(0)) as u32).to_le_bytes());
    payload.extend_from_slice(app);
    if let Some(b) = boot {
        payload.extend_from_slice(b);
    }
    let mut out = installer.to_vec();
    out.extend_from_slice(&payload);
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(&TAIL_MAGIC.to_le_bytes());
    out
}

// ---------------------------------------------------------------------------
// 安装
// ---------------------------------------------------------------------------

/// 装。出错就返回人话，界面上会照原样显示给老师。
pub fn install(
    payload: &[u8],
    bootstrapper: Option<&[u8]>,
    opts: &Options,
    self_exe: &Path,
    p: &mut dyn Progress,
) -> Result<(), String> {
    // 1. 目录
    if !p.step("正在准备安装位置…", 4.0) {
        return Err("已取消".into());
    }
    fs::create_dir_all(&opts.dir)
        .map_err(|e| format!("无法创建安装文件夹 {}：{e}", opts.dir.display()))?;

    // 2. 主程序
    if !p.step("正在把程序复制进去…", 18.0) {
        return Err("已取消".into());
    }
    let exe = opts.dir.join(EXE_NAME);
    write_file(&exe, payload)?;

    // 3. 卸载程序：把自己复制一份过去，卸载时以 `--uninstall` 身份运行
    if !p.step("正在准备卸载程序…", 46.0) {
        return Err("已取消".into());
    }
    let me = fs::read(self_exe).map_err(|e| format!("无法读取安装包：{e}"))?;
    write_file(&opts.dir.join(UNINST_EXE), &me)?;

    // 4. 快捷方式
    if !p.step("正在创建快捷方式…", 58.0) {
        return Err("已取消".into());
    }
    if opts.desktop_shortcut {
        if let Err(e) = create_shortcut(&shortcut_path(true), &exe) {
            log::warn!("桌面快捷方式没建成：{e}");
        }
    }
    if let Err(e) = create_shortcut(&shortcut_path(false), &exe) {
        log::warn!("开始菜单快捷方式没建成：{e}");
    }

    // 5. 注册表：卸载信息、文件关联、老师选的操作方式
    if !p.step("正在登记到系统…", 68.0) {
        return Err("已取消".into());
    }
    write_registry(&opts.dir, &exe, opts)?;

    // 6. 运行环境（WebView2）：新系统上一般已经有了，没有才装
    if !p.step("正在检查运行环境…", 78.0) {
        return Err("已取消".into());
    }
    if !webview2_installed() {
        if !p.step("正在安装运行环境，第一次可能要等一两分钟…", 84.0) {
            return Err("已取消".into());
        }
        ensure_webview2(bootstrapper, p)?;
    }

    p.step("装好了", 100.0);
    Ok(())
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    fs::write(path, bytes).map_err(|e| write_error(&e, path))
}

/// 把「写文件失败」翻译成老师看得懂、并且知道下一步做什么的一句话。
///
/// 「拒绝访问」几乎永远是同一个原因：那个文件夹要管理员权限
/// （`C:\Program Files` 一类），而当前这份安装程序没提权。
/// 原样抛 `os error 5` 对老师没有任何帮助 —— 他只会觉得程序坏了。
///
/// 完整路径仍然会写进安装日志，排查时用得上。
fn write_error(e: &std::io::Error, path: &Path) -> String {
    log::error!("写入 {} 失败：{e}", path.display());
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        return "没有权限写入这个文件夹。请换一个位置（比如「我的文档」下面）再装。".to_string();
    }
    format!("无法写入文件 {}：{e}", path.display())
}

/// 快捷方式路径（桌面、开始菜单）。
fn shortcut_path(desktop: bool) -> PathBuf {
    let name = format!("{PRODUCT}.lnk");
    if desktop {
        desktop_dir().join(name)
    } else {
        start_menu_dir().join(name)
    }
}

fn desktop_dir() -> PathBuf {
    shell_folder(windows::Win32::UI::Shell::FOLDERID_Desktop)
}

fn start_menu_dir() -> PathBuf {
    shell_folder(windows::Win32::UI::Shell::FOLDERID_Programs)
}

/// 用 `SHGetKnownFolderPath` 取系统目录，别去拼 `%USERPROFILE%`。
fn shell_folder(id: windows::core::GUID) -> PathBuf {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::SHGetKnownFolderPath;
    unsafe {
        match SHGetKnownFolderPath(&id, Default::default(), None) {
            Ok(p) => {
                let s = p.to_string().unwrap_or_default();
                CoTaskMemFree(Some(p.0 as *const _));
                PathBuf::from(s)
            }
            Err(_) => PathBuf::new(),
        }
    }
}

/// 建一个 .lnk 指向主程序。
fn create_shortcut(lnk: &Path, target: &Path) -> Result<(), String> {
    unsafe {
        // COM 可能已经被初始化过（重复初始化返回 RPC_E_CHANGED_MODE 也无所谓）
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| format!("建快捷方式对象失败：{e}"))?;

        let target_w = wide(target.as_os_str());
        link.SetPath(PCWSTR(target_w.as_ptr()))
            .map_err(|e| format!("设置快捷方式目标失败：{e}"))?;
        if let Some(dir) = target.parent() {
            let dir_w = wide(dir.as_os_str());
            let _ = link.SetWorkingDirectory(PCWSTR(dir_w.as_ptr()));
        }
        link.SetIconLocation(PCWSTR(target_w.as_ptr()), 0)
            .map_err(|e| format!("设置快捷方式图标失败：{e}"))?;
        let desc = wide_str("OpenPPTView 课件讲演器");
        let _ = link.SetDescription(PCWSTR(desc.as_ptr()));

        let file: IPersistFile = link
            .cast()
            .map_err(|e| format!("保存快捷方式失败：{e}"))?;
        let lnk_w = wide(lnk.as_os_str());
        file.Save(PCWSTR(lnk_w.as_ptr()), true)
            .map_err(|e| format!("保存快捷方式失败：{e}"))?;
    }
    Ok(())
}

fn remove_shortcut(path: &Path) {
    let _ = fs::remove_file(path);
}

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().chain(std::iter::once(0)).collect()
}

fn wide_str(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 注册表写在哪一档。
///
/// 装在 `Program Files` 下是「给这台电脑装」，写 HKLM（进程已经提权，写得了）；
/// 装在用户目录下（免管理员的用法）写 HKCU —— 应用那边两处都会读，
/// 所以两条路都成立。分清这一档还有个好处在开发上：不动系统目录也能把
/// 安装、注册、卸载整条链路跑一遍。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Machine,
    User,
}

impl Scope {
    pub fn for_dir(dir: &Path) -> Scope {
        let pf = std::env::var("ProgramFiles").unwrap_or_default();
        let pf86 = std::env::var("ProgramFiles(x86)").unwrap_or_default();
        let d = dir.to_string_lossy().to_lowercase();
        if (!pf.is_empty() && d.starts_with(&pf.to_lowercase()))
            || (!pf86.is_empty() && d.starts_with(&pf86.to_lowercase()))
        {
            Scope::Machine
        } else {
            Scope::User
        }
    }
}

// ---------------------------------------------------------------------------
// 注册表
// ---------------------------------------------------------------------------

fn write_registry(dir: &Path, exe: &Path, opts: &Options) -> Result<(), String> {
    use winreg::enums::KEY_WRITE;
    use winreg::RegKey;

    let scope = Scope::for_dir(dir);
    let root = RegKey::predef(match scope {
        Scope::Machine => winreg::enums::HKEY_LOCAL_MACHINE,
        Scope::User => winreg::enums::HKEY_CURRENT_USER,
    });
    let exe_s = exe.to_string_lossy().to_string();
    let dir_s = dir.to_string_lossy().to_string();

    // 卸载信息：让「设置 → 应用」里能看见、能卸掉
    let (key, _) = root
        .create_subkey(UNINST_KEY)
        .map_err(|e| format!("无法写入注册表：{e}"))?;
    let _ = key.set_value("DisplayName", &PRODUCT);
    let _ = key.set_value("DisplayVersion", &VERSION);
    let _ = key.set_value("Publisher", &PUBLISHER);
    let _ = key.set_value("DisplayIcon", &format!("\"{exe_s}\",0"));
    let _ = key.set_value("InstallLocation", &format!("\"{dir_s}\""));
    let _ = key.set_value(
        "UninstallString",
        &format!("\"{}\" --uninstall", dir.join(UNINST_EXE).to_string_lossy()),
    );
    let _ = key.set_value("NoModify", &1u32);
    let _ = key.set_value("NoRepair", &1u32);
    let _ = key.set_value("EstimatedSize", &(8u32 * 1024)); // 8 MB，够估算用

    // 老师选的操作方式：应用首启会读
    let (pref, _) = root
        .create_subkey(PREF_KEY)
        .map_err(|e| format!("无法写入注册表：{e}"))?;

    let _ = pref.set_value("InputMode", &opts.input_mode);
    let _ = pref.set_value("DesktopShortcut", &(u32::from(opts.desktop_shortcut)));
    let _ = pref.set_value("InstallLocation", &dir_s);

    // 文件关联：注册一个属于本应用的 ProgID，并把它挂到 .pptx / .ppsx 的候选里。
    // 不去抢 UserChoice（Windows 不允许程序自己改默认打开方式），
    // 但「打开方式 → 更多应用」里会出现 OpenPPTView。
    if let Ok(classes) = root.open_subkey_with_flags("Software\\Classes", KEY_WRITE) {
        for ext in [".pptx", ".ppsx"] {
            let prog_id = format!("{PRODUCT}{ext}");
            if let Ok((pid_key, _)) = classes.create_subkey(&prog_id) {
                let _ = pid_key.set_value("", &format!("{PRODUCT} 课件"));
                let _ = pid_key.set_value("FriendlyTypeName", &format!("{PRODUCT} 课件"));
            }
            if let Ok((icon_key, _)) = classes.create_subkey(format!("{prog_id}\\DefaultIcon")) {
                let _ = icon_key.set_value("", &format!("\"{exe_s}\",0"));
            }
            if let Ok((cmd_key, _)) = classes.create_subkey(format!("{prog_id}\\shell\\open\\command")) {
                let _ = cmd_key.set_value("", &format!("\"{exe_s}\" \"%1\""));
            }
            // 挂到「打开方式」的候选列表
            if let Ok((ow, _)) = classes.create_subkey(format!("{ext}\\OpenWithProgids")) {
                let _ = ow.set_value(&prog_id, &"");
            }
        }
        // 「打开方式」列表里显示的名字
        let app_key = format!("Applications\\{EXE_NAME}");
        if let Ok((k, _)) = classes.create_subkey(format!("{app_key}\\shell\\open\\command")) {
            let _ = k.set_value("", &format!("\"{exe_s}\" \"%1\""));
        }
        if let Ok((k, _)) = classes.create_subkey(format!("{app_key}\\SupportedTypes")) {
            let _ = k.set_value(".pptx", &"");
            let _ = k.set_value(".ppsx", &"");
        }
        if let Ok((k, _)) = classes.create_subkey(&app_key) {
            let _ = k.set_value("FriendlyAppName", &PRODUCT);
        }
    }
    Ok(())
}

fn remove_registry(scope: Scope) {
    use winreg::RegKey;
    let root = RegKey::predef(match scope {
        Scope::Machine => winreg::enums::HKEY_LOCAL_MACHINE,
        Scope::User => winreg::enums::HKEY_CURRENT_USER,
    });
    let _ = root.delete_subkey_all(UNINST_KEY);
    let _ = root.delete_subkey_all(PREF_KEY);
    if let Ok(classes) =
        root.open_subkey_with_flags("Software\\Classes", winreg::enums::KEY_WRITE)
    {
        for ext in [".pptx", ".ppsx"] {
            let prog_id = format!("{PRODUCT}{ext}");

            // ① 「打开方式」候选列表里去掉自己的名字。
            //
            // 这里必须用 `delete_value`：`OpenWithProgids` 下面挂的是**值**，
            // 不是子键。以前写成 `delete_subkey`，等于什么都没删 ——
            // 卸载之后右键「打开方式」里还留着一条指向已删程序的条目。
            if let Ok(ow) =
                classes.open_subkey_with_flags(format!("{ext}\\OpenWithProgids"), winreg::enums::KEY_WRITE)
            {
                let _ = ow.delete_value(&prog_id);
            }

            // ② 要是这个格式的默认程序就是我们，先把默认值撤掉，再删 ProgID。
            //
            // 顺序反了会留下一句「默认打开方式指向一个已经不存在的程序」，
            // 老师双击课件直接报错 —— 卸载比不装还糟糕。
            //
            // 只管 `Classes\.ext` 这一层。`FileExts\...\UserChoice` 是 Windows
            // 保护起来的键（值里带哈希，程序写不进去），只能请老师到
            // 「默认应用」里重新挑一个，这里如实说明、不假装能改。
            if let Ok(k) = classes
                .open_subkey_with_flags(format!(".{ext}"), winreg::enums::KEY_WRITE)
            {
                let current: Option<String> = k.get_value("").ok();
                if current.as_deref() == Some(prog_id.as_str()) {
                    let _ = k.delete_value("");
                }
            }

            let _ = classes.delete_subkey_all(&prog_id);
        }
        let _ = classes.delete_subkey_all(format!("Applications\\{EXE_NAME}"));
    }
}

// ---------------------------------------------------------------------------
// 运行环境：WebView2
// ---------------------------------------------------------------------------

/// 系统里有没有 WebView2 运行时（机器级或用户级）。
pub fn webview2_installed() -> bool {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;
    const GUID: &str = "{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}";
    let paths = [
        format!("SOFTWARE\\WOW6432Node\\Microsoft\\EdgeUpdate\\Clients\\{GUID}"),
        format!("SOFTWARE\\Microsoft\\EdgeUpdate\\Clients\\{GUID}"),
    ];
    for root in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        let key = RegKey::predef(root);
        for p in &paths {
            if let Ok(k) = key.open_subkey(p) {
                if let Ok(v) = k.get_value::<String, _>("pv") {
                    if !v.trim().is_empty() {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// 装运行环境：优先用安装包里带的引导器；没有就现下（教室断网时靠前者）。
fn ensure_webview2(bootstrapper: Option<&[u8]>, p: &mut dyn Progress) -> Result<(), String> {
    let temp = std::env::temp_dir().join("OpenPPTView.WebView2Setup.exe");
    match bootstrapper {
        Some(bytes) => {
            write_file(&temp, bytes)?;
        }
        None => {
            if !p.step("正在下载运行环境…", 88.0) {
                return Err("已取消".into());
            }
            if let Err(e) = download_webview2(&temp) {
                // 下不下来不算致命：装完第一次启动时应用自己还会再要一次
                log::warn!("下载 WebView2 失败：{e}");
                return Ok(());
            }
        }
    }
    if !p.step("正在安装运行环境，请稍候…", 92.0) {
        return Err("已取消".into());
    }
    let status = std::process::Command::new(&temp)
        .args(["/silent", "/install"])
        .status();
    let _ = fs::remove_file(&temp);
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!(
            "运行环境安装未完成（引导程序返回 {}）。请联网后重新运行安装程序。",
            s.code().unwrap_or(-1)
        )),
        Err(e) => Err(format!("无法启动运行环境安装程序：{e}")),
    }
}

/// 下载 WebView2 引导器。用系统自带的 `urlmon`，不引入 HTTP 客户端依赖。
fn download_webview2(dest: &Path) -> Result<(), String> {
    let url = wide_str("https://go.microsoft.com/fwlink/p/?LinkId=2124703");
    let dst = wide(dest.as_os_str());
    let hr = unsafe {
        URLDownloadToFileW(None, PCWSTR(url.as_ptr()), PCWSTR(dst.as_ptr()), 0, None)
    };
    if hr.is_err() {
        return Err(format!("下载运行环境失败（{hr:?}）"));
    }
    if dest.exists() {
        Ok(())
    } else {
        Err("下载运行环境失败".into())
    }
}

// ---------------------------------------------------------------------------
// 卸载
// ---------------------------------------------------------------------------

/// 卸。删文件、删快捷方式、删注册表。
pub fn uninstall(dir: &Path, p: &mut dyn Progress) -> Result<(), String> {
    if !p.step("正在删除快捷方式…", 10.0) {
        return Err("已取消".into());
    }
    remove_shortcut(&shortcut_path(true));
    remove_shortcut(&shortcut_path(false));

    if !p.step("正在清理注册表…", 35.0) {
        return Err("已取消".into());
    }
    remove_registry(Scope::for_dir(dir));

    if !p.step("正在关闭 OpenPPTView…", 48.0) {
        return Err("已取消".into());
    }
    stop_running_app(dir);

    if !p.step("正在删除程序文件…", 72.0) {
        return Err("已取消".into());
    }
    remove_program_files(dir);

    let uninst = dir.join(UNINST_EXE);
    let _ = fs::remove_file(&uninst);
    let _ = fs::remove_dir(dir);
    // 上面那句删自己**必然失败**：正在运行的 exe 被 Windows 锁着。
    // 不补这一下，安装目录里就会永远留着一个十几 MB 的 uninstall.exe，
    // 文件夹也跟着留在那儿 —— 看起来就像「没卸载干净」。
    schedule_delete_on_reboot(&uninst);

    p.step("卸载完成", 100.0);
    Ok(())
}

/// 把装在 `dir` 里的程序关掉。
///
/// # 为什么非关不可
///
/// 正在运行的 exe 被 Windows 锁着，**删不掉**。不关它，卸载就只会删掉
/// 几个无关紧要的文件，主程序和文件夹原样留着 —— 老师看到的就是
/// 「点了卸载，程序还在，还能双击打开」，等于没卸。
///
/// 顺序是「先好好说，再动手」：
/// 1. 用 `--quit` 请它自己退。走的是应用里「先把标注存盘再退」那条路，
///    老师写在课件上的笔迹不会白丢；
/// 2. 等最多 5 秒；
/// 3. 还不走就强制结束。卸载完还留着一个删不掉的文件，比丢一次标注更糟。
fn stop_running_app(dir: &Path) {
    let exe = dir.join(EXE_NAME);
    if !exe.exists() {
        return;
    }

    if let Err(e) = std::process::Command::new(&exe).arg("--quit").spawn() {
        log::warn!("请应用退出时没能启动它（继续走强制流程）：{e}");
    }
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(250));
        if !process_running(EXE_NAME) {
            log::info!("应用已退出，可以删文件了");
            return;
        }
    }
    log::warn!("应用没有在 5 秒内退出，强制结束");
    kill_process(EXE_NAME);
    std::thread::sleep(std::time::Duration::from_millis(400));
}

/// 删安装目录里的文件（正在跑的 `uninstall.exe` 除外）。
///
/// 分几轮重试：应用刚退出的那几百毫秒里，文件可能还被系统攥着一会儿。
/// 实在删不掉的记一条「下次重启时删」，别把整个卸载判成失败。
fn remove_program_files(dir: &Path) {
    for attempt in 0..4 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(400));
        }
        let Ok(entries) = fs::read_dir(dir) else { return };
        let mut left = 0usize;
        for e in entries.flatten() {
            let path = e.path();
            if path.file_name().map(|n| n == UNINST_EXE).unwrap_or(false) {
                continue; // 正在运行的就是它自己
            }
            let done = if path.is_dir() {
                fs::remove_dir_all(&path).is_ok()
            } else {
                fs::remove_file(&path).is_ok()
            };
            if !done {
                left += 1;
                schedule_delete_on_reboot(&path);
            }
        }
        if left == 0 {
            return;
        }
        log::warn!("第 {} 轮还剩 {left} 个文件删不掉，稍后重试", attempt + 1);
    }
}

/// 按可执行文件名看有没有同名进程在跑。
fn process_running(name: &str) -> bool {
    let mut found = false;
    for_each_process(|_pid, exe| {
        if exe.eq_ignore_ascii_case(name) {
            found = true;
        }
    });
    found
}

/// 强制结束同名进程（卸载时兜底用）。
fn kill_process(name: &str) {
    use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    for_each_process(|pid, exe| {
        if !exe.eq_ignore_ascii_case(name) || pid == std::process::id() {
            return;
        }
        unsafe {
            if let Ok(handle) = OpenProcess(PROCESS_TERMINATE, false, pid) {
                let _ = TerminateProcess(handle, 1);
                let _ = windows::Win32::Foundation::CloseHandle(handle);
            }
        }
    });
}

/// 枚举当前可见的进程，把「进程名 + pid」逐个交给 `f`。
fn for_each_process(mut f: impl FnMut(u32, String)) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(entry.szExeFile.len());
                f(
                    entry.th32ProcessID,
                    String::from_utf16_lossy(&entry.szExeFile[..len]),
                );
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
}

/// 安排「下次重启时删除这个文件」。
///
/// 这是 Windows 给的标准出路：写一条待删记录，重启时由系统删掉。
/// 它写的是 HKLM，需要管理员权限 —— 正式安装包本来就提权，所以正常路径有效；
/// 用户级安装下会静默失败，那也只是维持现状，不会更糟。
fn schedule_delete_on_reboot(path: &Path) {
    use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT};
    let p = wide(path.as_os_str());
    let _ = unsafe { MoveFileExW(PCWSTR(p.as_ptr()), None, MOVEFILE_DELAY_UNTIL_REBOOT) };
}

/// 装完把应用拉起来（以当前登录用户身份，不要以管理员身份跑界面）。
pub fn launch_app(dir: &Path) {
    let exe = dir.join(EXE_NAME);
    let file = wide(exe.as_os_str());
    let verb = wide_str("open");
    unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_round_trips() {
        let setup = build_setup(b"INSTALLER", b"APP-EXE", Some(b"BOOT"));
        let path = std::env::temp_dir().join("oppv-payload-test.exe");
        fs::write(&path, &setup).unwrap();
        let (app, boot) = read_payload(&path).unwrap();
        assert_eq!(app, b"APP-EXE");
        assert_eq!(boot.as_deref(), Some(&b"BOOT"[..]));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn payload_without_bootstrapper() {
        let setup = build_setup(b"INSTALLER", b"APP", None);
        let path = std::env::temp_dir().join("oppv-payload-test2.exe");
        fs::write(&path, &setup).unwrap();
        let (app, boot) = read_payload(&path).unwrap();
        assert_eq!(app, b"APP");
        assert!(boot.is_none());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn rejects_file_without_payload() {
        let path = std::env::temp_dir().join("oppv-payload-test3.exe");
        fs::write(&path, b"just a normal executable, nothing appended").unwrap();
        assert!(read_payload(&path).is_err());
        let _ = fs::remove_file(&path);
    }
}
