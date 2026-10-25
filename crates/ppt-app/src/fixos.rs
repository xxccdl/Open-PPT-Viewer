//! openFixOS：蓝屏/引导损坏后的离线修复环境（**实验性**）。
//!
//! # 它是什么
//!
//! 系统盘还在、但引导扇区或系统文件被破坏时，Windows 本身已经起不来了，
//! 「在 Windows 里跑 sfc」也就无从谈起。这一模块的做是：在还能进系统的时候，
//! 把 Windows 自带的恢复环境（WinRE）**复制一份**、往里塞一个自动修复脚本，
//! 再把下一次启动指向这份镜像。下次重启就会自动进 openFixOS：
//! 扫描并修复系统盘 → 修引导记录 → 结果写回系统盘 → 自己重启回 Windows。
//!
//! # 为什么不是「自己写引导程序」
//!
//! 手写 MBR/VBR 一旦有偏差，就是连恢复环境都进不去的真变砖；而且要在里面
//! 跑网卡/文件系统/上报，等于重写一个操作系统。这里走的是 OEM 恢复分区的
//! 同一条路：**只改 Windows 自己的恢复环境配置**。具体是：原镜像先原样
//! 复制一份留作「干净的底」，改造版装进恢复分区（`/disable` → `/setreimage`
//! → `/enable`），正常回到 Windows 后立刻用干净的底换回原样。
//!
//! # 安全边界（刻意做窄）
//!
//! - 默认关闭（`settings.json` 的 `bootFix`），且只在管理员权限下生效；
//! - 只在**引导类蓝屏**（0x7B / 0xED / 0xEF / 0x109 …）之后才挂一次；
//! - 改的是一份**副本**，原镜像另有完整备份，任何一步失败都自动还原；
//! - `reagentc /boottore` 是「下次启动」的一次性开关，不会形成启动循环；
//! - 修复脚本无论如何都会重启回 Windows（`wpeutil reboot`），不会停在黑屏；
//! - 恢复正常启动后自动把恢复环境换回原镜像，并删掉我们那份副本。
//!
//! # 结果去哪了
//!
//! 恢复环境里没有把握连上 WiFi（要机型相关的无线驱动），所以脚本只把
//! `result.json` / `result.log` 写回 `C:\OpenPPTView\fixos\`。
//! 下次启动本应用时会把结果读出来展示，并随下一份蓝屏报告一起上报。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 写进恢复环境的脚本版本。
///
/// 改了 `openfixos.cmd` / `winpeshl.ini` 就 +1：已经装过的机器下次准备时会自动
/// 重装一遍，否则它们会一直跑着旧脚本（镜像里的是副本，不会自己更新）。
const SCRIPT_REV: u32 = 3;

/// 恢复环境里用的目录（系统盘根目录，WinRE 里也能写）。
///
/// 不写死 `C:`：Windows 不一定装在 C 盘，恢复环境里盘符也可能变。
fn result_dir() -> PathBuf {
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
    PathBuf::from(format!(r"{drive}\OpenPPTView\fixos"))
}

/// 我们自己那份镜像与挂载点所在的目录。
fn work_dir() -> PathBuf {
    result_dir().join("WinRE")
}

fn installed_path() -> PathBuf {
    result_dir().join("installed.json")
}

/// 「下次启动进 openFixOS」的一次性标记（正常启动后被清掉）。
fn boot_pending_path() -> PathBuf {
    result_dir().join("boottore.json")
}

/// 本机标记：openFixOS 已经装进恢复环境、原来的镜像在哪。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Installed {
    /// 装好的时刻（UTC，ISO 8601）。
    #[serde(alias = "armedAt")]
    installed_at: String,
    /// 触发它的错误码（手动准备时是一句说明）。
    reason: String,
    /// 原来的 WinRE 位置（`reagentc /info` 里那一行）。
    original: String,
    /// 当时装进去的脚本版本，见 [`SCRIPT_REV`]。
    #[serde(default)]
    script_rev: u32,
}

/// 「已经叫了 `/boottore`」的痕迹，只用来在设置页显示「下次重启会进修复」。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BootPending {
    #[serde(default)]
    armed_at: String,
}

/// 修复结果（由恢复环境里的脚本写回）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootResult {
    /// 修复开始时刻。
    #[serde(default)]
    pub started_at: String,
    /// 修复结束时刻。
    #[serde(default)]
    pub finished_at: String,
    /// 判定的系统盘（`C:`）。
    #[serde(default)]
    pub system_drive: String,
    /// 各步骤的退出码。
    #[serde(default)]
    pub steps: Vec<BootStep>,
    /// 最后一行日志（给人看）。
    #[serde(default)]
    pub summary: String,
}

/// 一步修复的退出码。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BootStep {
    #[serde(default)]
    pub step: String,
    #[serde(default)]
    pub exit: i32,
}

/// 设置页看到的状态。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    /// 开关（`settings.json` 的 `bootFix`）。
    pub enabled: bool,
    /// 当前进程有没有管理员权限 —— 没有就什么都不做。
    pub admin: bool,
    /// 恢复环境是否可用（`reagentc /info` 的结论，或失败原因）。
    pub winre: String,
    /// openFixOS 已经装进恢复环境（常驻待命）。
    pub installed: bool,
    /// 装好的时刻（没装就是空）。
    pub installed_at: String,
    /// 已经叫过 `reagentc /boottore`：下次重启会进 openFixOS。
    pub boot_pending: bool,
    /// 还能「撤销」：装过，或者至少还留着干净的原始镜像。
    pub can_restore: bool,
    /// 上次修复的结果（没有就是 null）。
    pub result: Option<BootResult>,
    /// 正在准备（复制/挂载镜像要一两分钟）。
    pub busy: bool,
    /// 当前做到哪一步（给人看的）。
    pub step: String,
    /// 上一次准备失败的原因。
    pub error: String,
    /// 给老师看的一句话。
    pub detail: String,
}

/// 后台准备任务的状态。
///
/// 为什么要这个：「准备」要复制几百 MB 的恢复镜像、再挂载/卸载，
/// 一分钟往上。**绝不能放在命令里同步跑** —— Tauri 的同步命令跑在主线程上，
/// 那样窗口会直接变成「未响应」（老师点了「立即准备」就以为卡死了）。
#[derive(Default)]
struct Job {
    busy: bool,
    step: String,
    error: String,
}

static JOB: std::sync::Mutex<Option<Job>> = std::sync::Mutex::new(None);
/// 准备期间不要每个轮询都去问一次 `reagentc`（那是子进程，会拖慢界面）。
static WINRE_CACHE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn job_snapshot() -> (bool, String, String) {
    JOB.lock()
        .ok()
        .and_then(|j| j.as_ref().map(|j| (j.busy, j.step.clone(), j.error.clone())))
        .unwrap_or((false, String::new(), String::new()))
}

fn set_step(step: &str) {
    if let Ok(mut slot) = JOB.lock() {
        let job = slot.get_or_insert_with(Job::default);
        job.step = step.to_string();
    }
}

fn finish_job(error: Option<String>) {
    if let Ok(mut slot) = JOB.lock() {
        let job = slot.get_or_insert_with(Job::default);
        job.busy = false;
        job.step.clear();
        job.error = error.unwrap_or_default();
    }
}

/* ------------------------------------------------------------------ */
/* 判定                                                                */
/* ------------------------------------------------------------------ */

/// 这个错误码像不像「引导/系统文件被破坏」。
///
/// 只认这些：真正的驱动小毛病（0x116 显卡、0x9F 电源）不该把机器拖进恢复环境，
/// 那是十几分钟的离线扫描，用在能正常进系统的机器上是打扰。
pub fn is_boot_threat(code: &str) -> bool {
    let t = code.to_uppercase();
    let Some(pos) = t.find("0X") else {
        return false;
    };
    let hex: String = t[pos + 2..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    let hex = hex.trim_start_matches('0');
    matches!(
        hex,
        "7B"      // INACCESSIBLE_BOOT_DEVICE
            | "ED"    // UNMOUNTABLE_BOOT_VOLUME
            | "24"    // NTFS_FILE_SYSTEM
            | "EF"    // CRITICAL_PROCESS_DIED
            | "109"   // CRITICAL_STRUCTURE_CORRUPTION（病毒/驱动篡改内核）
            | "67"    // CONFIG_INITIALIZATION_FAILED
            | "6B"    // PROCESS1_INITIALIZATION_FAILED
            | "71"    // SESSION5_INITIALIZATION_FAILED
            | "C0000218" // STATUS_CANNOT_LOAD_REGISTRY_FILE
            | "C0000221" // STATUS_IMAGE_CHECKSUM_MISMATCH（系统文件被替换）
    ) || t.contains("BOOT") // 没读到码、只写了「引导」之类的情况
}

/* ------------------------------------------------------------------ */
/* 状态                                                                */
/* ------------------------------------------------------------------ */

fn read_installed() -> Option<Installed> {
    let text = std::fs::read_to_string(installed_path()).ok()?;
    serde_json::from_str(&text).ok()
}

fn boot_pending() -> bool {
    boot_pending_path().exists()
}

/// 上次修复的结果（没有就是 None）。
pub fn boot_result() -> Option<BootResult> {
    let text = std::fs::read_to_string(result_dir().join("result.json")).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn status() -> Status {
    let settings = crate::crash::load_settings();
    let admin = crate::crash_repair::is_admin();
    let (busy, step, error) = job_snapshot();

    // 准备期间不重复起 `reagentc` 子进程：轮询只读缓存
    let winre = if busy {
        let cached = WINRE_CACHE
            .lock()
            .ok()
            .and_then(|c| c.clone())
            .unwrap_or_else(|| "Windows 恢复环境可用".to_string());
        (true, cached)
    } else if admin {
        let state = winre_state();
        if let Ok(mut c) = WINRE_CACHE.lock() {
            *c = Some(state.1.clone());
        }
        state
    } else {
        (false, "需要管理员权限才能读取".to_string())
    };
    let installed = read_installed();
    let pending = boot_pending();
    let can_restore = installed.is_some() || work_dir().join("original").join("winre.wim").exists();
    let result = boot_result();

    let detail = if busy {
        format!("正在准备：{step}（复制恢复镜像要一两分钟，别关窗口）")
    } else if !error.is_empty() {
        // 失败原因比「开关没开」更该被看到：这是老师刚点过的那一下的结果
        format!("上次准备失败：{error}")
    } else if !settings.boot_fix {
        "未开启。开启后会先把 openFixOS 装进恢复环境待命，蓝屏或系统起不来时自动修复。"
            .to_string()
    } else if !admin {
        "当前不是管理员：装不了恢复环境，请用安装时登记的开机自检（自带最高权限）。"
            .to_string()
    } else if !winre.0 && installed.is_none() {
        format!("不可用：{}", winre.1)
    } else if installed.is_some() && !winre.0 {
        // 装过但恢复环境现在不可用：多半是系统更新把它还原/关掉了，重装一次即可
        "装过，但 Windows 恢复环境现在不可用（可能被系统更新还原了）：关掉开关再开启即可重装。"
            .to_string()
    } else if installed.as_ref().map(|m| m.script_rev < SCRIPT_REV) == Some(true) {
        "openFixOS 需要更新（程序里的修复脚本改过了）：点一次「立即准备」即可。".to_string()
    } else if installed.is_some() && pending {
        "已武装：下次重启会进入 openFixOS 自动修复，修完自己重启回 Windows。".to_string()
    } else if installed.is_some() {
        "已待命：恢复环境已换成 openFixOS，蓝屏或系统起不来时会自动修复。".to_string()
    } else if can_restore {
        // 标记丢了但镜像还在：多半是安装被打断（比如装到一半重启）
        "恢复环境里可能还留着上一版 openFixOS：点「立即准备」重装，或点「撤销」还原。"
            .to_string()
    } else {
        "已开启，但还没装好 —— 点「立即准备」装一次（几分钟，只需一次）。".to_string()
    };

    Status {
        enabled: settings.boot_fix,
        admin,
        winre: winre.1,
        installed: installed.is_some(),
        installed_at: installed.map(|m| m.installed_at).unwrap_or_default(),
        boot_pending: pending,
        can_restore,
        result,
        busy,
        step,
        error,
        detail,
    }
}

/* ------------------------------------------------------------------ */
/* 准备：复制恢复镜像、塞脚本、指向它                                   */
/* ------------------------------------------------------------------ */

/// 把 openFixOS 装进恢复环境（常驻待命，**不**触发下次启动）。
///
/// 为什么不再「蓝屏之后才装」：病毒/蓝屏把引导或系统文件弄坏时，Windows
/// 根本起不来，也就没有机会再装 —— 这个功能必须**提前装好**。所以开关一打开
/// 就装一次，之后一直待命；蓝屏之后只剩一个毫秒级的 `reagentc /boottore`。
///
/// 安装顺序：暂停原恢复环境 → 指向副本 → 重新启用。
/// `/setreimage` 在恢复环境**已启用**时会直接报「Windows RE 已经启用」，
/// 所以必须先 `/disable`；`/disable` 顺手会把原镜像暂存到
/// `C:\Windows\System32\Recovery`，真要回滚，`/enable` 还能把系统恢复原样。
/// 我们自己另外留一份**未经修改的原镜像**（`WinRE\original\winre.wim`），
/// 撤销时用它换回原样，不依赖 reagentc 的暂存还剩什么。
///
/// **这是阻塞的**：复制/挂载/搬镜像要几分钟。从界面调起时走
/// [`install_background`]，别直接在命令里调用（会卡住主线程）。
pub fn install(reason: &str) -> Result<(), String> {
    // 已经待命、且镜像里是当前脚本：不重复搬镜像（那要几分钟，而且会白搬一次）
    if let Some(done) = read_installed() {
        if done.script_rev >= SCRIPT_REV {
            return Ok(());
        }
        log::info!(
            "openFixOS 脚本版本更新（{} → {}），重新装一遍",
            done.script_rev,
            SCRIPT_REV
        );
        let _ = std::fs::remove_file(installed_path());
    }
    set_step("读取恢复环境信息");
    if !crate::crash_repair::is_admin() {
        return Err("需要管理员权限".into());
    }
    let info = run("reagentc", &["/info"]).map_err(|e| format!("读不到恢复环境状态：{e}"))?;
    // 后台准备期间 `status()` 会走缓存，别让它每次轮询都再起一个 reagentc。
    if let Ok(mut c) = WINRE_CACHE.lock() {
        *c = Some("Windows 恢复环境可用".to_string());
    }
    let was_enabled = winre_enabled(&info);

    let original = original_winre_path(&info).ok_or("读不到原恢复镜像的位置")?;
    let work = work_dir();
    let pristine_dir = work.join("original");
    let pristine = pristine_dir.join("winre.wim");
    let image_dir = work.join("WindowsRE");
    let image = image_dir.join("winre.wim");
    let mount = work.join("mnt");

    std::fs::create_dir_all(&pristine_dir).map_err(|e| format!("建不出目录：{e}"))?;
    std::fs::create_dir_all(&image_dir).map_err(|e| format!("建不出目录：{e}"))?;
    let _ = std::fs::remove_dir_all(&mount);
    std::fs::create_dir_all(&mount).map_err(|e| format!("建不出挂载点：{e}"))?;

    // 1) 原镜像留一份干净的底（撤销时靠它换回原样）
    let source = Path::new(&original).join("winre.wim");
    if !pristine.exists() {
        set_step("复制恢复镜像（几百 MB，需要一会儿）");
        copy_wim(&source, &pristine)?;
    }

    // 2) 再复制一份用来改（改坏了删掉重来，干净的底还在）
    if !image.exists() {
        set_step("复制恢复镜像（几百 MB，需要一会儿）");
        copy_wim(&pristine, &image)?;
    }

    // 3) 挂载副本，写入 winpeshl.ini 与修复脚本
    set_step("挂载镜像并写入修复脚本");
    if let Err(e) = inject(&image, &mount) {
        let _ = run_ok("dism", &["/Unmount-Image", &mount_arg(&mount), "/Discard"]);
        return Err(e);
    }

    // 4) 暂停原来的恢复环境（已启用时必须先停，否则 /setreimage 会被拒）
    if was_enabled {
        set_step("暂停原来的恢复环境");
        if let Err(e) = run_ok("reagentc", &["/disable"]) {
            return Err(format!("暂停原恢复环境失败：{e}"));
        }
    }

    // 5) 把恢复环境指向副本；失败就整段还原
    set_step("把恢复环境指向副本");
    if let Err(e) = run_ok(
        "reagentc",
        &["/setreimage", "/path", &image_dir.to_string_lossy()],
    ) {
        restore_original(&original);
        return Err(format!("把恢复镜像指向副本失败：{e}"));
    }

    // 6) 启用（把副本装进恢复分区）
    set_step("启用 openFixOS 恢复环境");
    if let Err(e) = run_ok("reagentc", &["/enable"]) {
        restore_original(&original);
        return Err(format!("启用恢复环境失败：{e}"));
    }

    // 副本已经装进恢复分区，工作目录里那份可以扔了；只留干净的底用于撤销
    let _ = std::fs::remove_dir_all(&image_dir);
    let _ = std::fs::remove_dir_all(&mount);

    let marker = Installed {
        installed_at: crate::crash::iso_utc(crate::crash::now_epoch()),
        reason: reason.to_string(),
        original,
        script_rev: SCRIPT_REV,
    };
    std::fs::write(
        installed_path(),
        serde_json::to_string_pretty(&marker).unwrap_or_default(),
    )
    .map_err(|e| format!("写标记失败：{e}"))?;

    log::info!("openFixOS 已装好待命（触发：{reason}）");
    Ok(())
}

/// 武装：保证装好，然后让**下次启动**进 openFixOS。
///
/// 这是蓝屏之后唯一要做的事 —— 装镜像的慢活已经在开关打开时做完了，
/// 正常情况下这里只是 `reagentc /boottore`，毫秒级。
pub fn arm_boot(reason: &str) -> Result<(), String> {
    if !crate::crash_repair::is_admin() {
        return Err("需要管理员权限".into());
    }
    // 系统更新之类的操作会把恢复环境换回原样或直接关掉：旧的「已装好」标记不算数
    if read_installed().is_some() {
        let still_there = run("reagentc", &["/info"])
            .map(|t| winre_enabled(&t))
            .unwrap_or(false);
        if !still_there {
            let _ = std::fs::remove_file(installed_path());
        }
    }
    install(reason)?;

    set_step("设置下次启动进入 openFixOS");
    run_ok("reagentc", &["/boottore"]).map_err(|e| format!("设置下次启动失败：{e}"))?;

    let pending = BootPending {
        armed_at: crate::crash::iso_utc(crate::crash::now_epoch()),
    };
    let _ = std::fs::create_dir_all(result_dir());
    std::fs::write(
        boot_pending_path(),
        serde_json::to_string_pretty(&pending).unwrap_or_default(),
    )
    .map_err(|e| format!("写标记失败：{e}"))?;

    log::info!("openFixOS 已武装：下次启动进入恢复环境（触发：{reason}）");
    Ok(())
}

/// 起一条后台线程干活，状态通过 `busy` / `step` / `error` 给前端轮询。
///
/// 这些活要复制几百 MB 镜像、挂载/卸载，一分钟往上。**绝不能放在命令里同步跑**
/// —— Tauri 的同步命令跑在主线程上，窗口会直接变成「未响应」。
fn start_job<F>(step: &str, f: F) -> Result<(), String>
where
    F: FnOnce() -> Result<(), String> + Send + 'static,
{
    if let Ok(mut slot) = JOB.lock() {
        let job = slot.get_or_insert_with(Job::default);
        if job.busy {
            return Err("已经在准备了，请稍候".into());
        }
        job.busy = true;
        job.step = step.to_string();
        job.error.clear();
    }
    std::thread::spawn(move || {
        let result = f();
        finish_job(result.err());
    });
    Ok(())
}

/// 后台安装（开关打开时调）：装好待命，不触发下次启动。
pub fn install_background(reason: &str) -> Result<(), String> {
    let reason = reason.to_string();
    start_job("准备中", move || install(&reason))
}

/// 后台武装（设置页「立即准备」/ 蓝屏之后调）：装好 + 下次启动进修复。
pub fn arm_boot_background(reason: &str) -> Result<(), String> {
    let reason = reason.to_string();
    start_job("准备中", move || arm_boot(&reason))
}

/// 撤销：把恢复环境还原成原来的镜像，删掉我们的副本。
///
/// 不依赖 `installed.json`：那个文件可能被清理软件删掉、也可能某次安装刚好在
/// 写标记前被打断 —— 只要还留着干净的底（`WinRE\original\winre.wim`）就照样能还原。
/// 否则「装进去了却撤不回来」是最糟的状态。
pub fn cancel() -> Result<(), String> {
    if !crate::crash_repair::is_admin() {
        return Err("需要管理员权限".into());
    }
    let marker = read_installed();
    let pristine = work_dir().join("original").join("winre.wim");
    if marker.is_none() && !pristine.exists() {
        // 没装过、也没有镜像可还原：什么也不用做
        return Ok(());
    }
    let original = marker.map(|m| m.original).unwrap_or_default();
    if !restore_original(&original) {
        return Err(
            "没能完全还原恢复环境：镜像先留在 C:\\OpenPPTView\\fixos\\，\
             请在管理员命令行执行一次 reagentc /enable 后重试撤销"
                .into(),
        );
    }
    let _ = std::fs::remove_dir_all(work_dir());
    let _ = std::fs::remove_file(installed_path());
    let _ = std::fs::remove_file(boot_pending_path());
    log::info!("openFixOS 已撤销，恢复环境已还原为原镜像");
    Ok(())
}

/// 应用正常启动时调一次。
///
/// **不再卸甲**：openFixOS 要常驻，否则「病毒把引导弄坏」时又得从头装一遍、
/// 甚至根本没机会装。这里只做两件轻事：清掉一次性的「下次重启进修复」标记，
/// 以及把上一次的修复结果记进日志。
pub fn after_normal_boot() {
    if boot_pending() {
        let _ = std::fs::remove_file(boot_pending_path());
    }
    if let Some(r) = boot_result() {
        log::info!("openFixOS 修复结果：{}", r.summary);
    }
}

/// 把上次的修复结果并进这一份报告（随报告一起上报）。
///
/// 复制成功后才删原件：万一这次报告没发出去，zip 还在 outbox 里，
/// 结果也跟着 zip 一起等着下次补发，不会两处都没有。
pub fn take_result_into(report_dir: &Path) {
    let src = result_dir();
    let dst = report_dir.join("fixos");
    let mut copied = false;
    for name in ["result.json", "result.log"] {
        let from = src.join(name);
        if !from.exists() {
            continue;
        }
        if std::fs::create_dir_all(&dst).is_err() {
            continue;
        }
        if std::fs::copy(&from, dst.join(name)).is_ok() {
            copied = true;
            let _ = std::fs::remove_file(&from);
        }
    }
    if copied {
        log::info!("上次 openFixOS 的修复结果已并入本次报告");
    }
}

/* ------------------------------------------------------------------ */
/* 内部实现                                                            */
/* ------------------------------------------------------------------ */

/// 还原：把干净的原始镜像换回去。
///
/// 顺序同样是「停 → 指回原镜像 → 启用」：`/setreimage` 在已启用时会被拒。
/// 成功返回 true（调用方据此决定要不要删工作目录）。
fn restore_original(original: &str) -> bool {
    let work = work_dir();
    let pristine_dir = work.join("original");
    let pristine = pristine_dir.join("winre.wim");

    // 先把当前这份（我们的）镜像从恢复分区撤下来
    let _ = run_ok("reagentc", &["/disable"]);

    // `/disable` 会把它暂存到 System32\Recovery。手里有干净的底时，把这份
    // 我方镜像删掉 —— 否则以后 Windows 自己 `/enable` 会把它又装回去。
    let staged = PathBuf::from(system_recovery_dir()).join("winre.wim");
    if pristine.exists() {
        let _ = std::fs::remove_file(&staged);
    }

    let mut pointed = false;
    if pristine.exists() {
        pointed = run_ok(
            "reagentc",
            &["/setreimage", "/path", &pristine_dir.to_string_lossy()],
        )
        .is_ok();
    }
    if !pointed && !original.trim().is_empty() {
        // 没有干净的底：试试点回原来的位置
        pointed = run_ok("reagentc", &["/setreimage", "/path", original]).is_ok();
    }
    let enabled = run_ok("reagentc", &["/enable"]).is_ok();
    if !enabled {
        log::warn!("还原恢复环境失败：reagentc /enable 没有成功，需要手动执行一次");
    }
    pointed && enabled
}

/// 系统盘上的 Windows 默认暂存目录（`reagentc /disable` 把镜像放这儿）。
fn system_recovery_dir() -> String {
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
    format!(r"{drive}\Windows\System32\Recovery")
}

/// `reagentc /info` → (可用?, 给人看的一句话)。
fn winre_state() -> (bool, String) {
    match run("reagentc", &["/info"]) {
        Ok(text) => {
            if winre_enabled(&text) {
                (true, "Windows 恢复环境可用".to_string())
            } else {
                (false, "Windows 恢复环境没有启用（reagentc /info）".to_string())
            }
        }
        Err(e) => (false, format!("读不到恢复环境状态：{e}")),
    }
}

/// `/info` 里那行「Windows RE 状态」是不是已启用。
///
/// 中文是「已启用 / 未启用」，英文是 Enabled / Disabled；
/// 都读不到时按「可用」处理 —— 后面每一步失败都会自己回滚，不必在这里拦。
fn winre_enabled(info: &str) -> bool {
    for line in info.lines() {
        let lower = line.to_lowercase();
        if !(lower.contains("status") || line.contains("状态")) {
            continue;
        }
        if lower.contains("disabled") || line.contains("未启用") {
            return false;
        }
        if lower.contains("enabled") || line.contains("已启用") {
            return true;
        }
    }
    true
}

/// 从 `reagentc /info` 里取出原恢复镜像所在目录。
///
/// 中文输出是「Windows 恢复环境 (WinRE) 位置: …」，英文是
/// 「Windows RE location: …」，两种都认；「恢复映像位置」那几行要排除。
fn original_winre_path(info: &str) -> Option<String> {
    for line in info.lines() {
        let lower = line.to_lowercase();
        let is_winre = lower.contains("windows re") || line.contains("恢复环境");
        let is_location = lower.contains("location") || line.contains("位置");
        if !(is_winre && is_location) {
            continue;
        }
        if line.contains("恢复映像")
            || lower.contains("recovery image")
            || lower.contains("custom image")
        {
            continue;
        }
        if let Some(idx) = line.find(':') {
            let value = line[idx + 1..].trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// 复制恢复镜像，并核对字节数。
///
/// 半截的镜像比没有更糟：它会被当成「干净的底」留着，撤销时换回一个坏镜像。
fn copy_wim(from: &Path, to: &Path) -> Result<(), String> {
    let copied = std::fs::copy(from, to)
        .map_err(|e| format!("复制恢复镜像失败（{}）：{e}", from.display()))?;
    let src = std::fs::metadata(from).map(|m| m.len()).unwrap_or(0);
    let dst = std::fs::metadata(to).map(|m| m.len()).unwrap_or(0);
    if src == 0 || src != dst {
        let _ = std::fs::remove_file(to);
        return Err(format!(
            "恢复镜像只复制了 {dst}/{src} 字节，可能是磁盘空间不够"
        ));
    }
    log::info!("已复制恢复镜像：{copied} 字节");
    Ok(())
}

/// 挂载副本 → 写两个文件 → 卸载提交。
fn inject(image: &Path, mount: &Path) -> Result<(), String> {
    run_ok(
        "dism",
        &[
            "/Mount-Image",
            &format!("/ImageFile:{}", image.display()),
            "/Index:1",
            &mount_arg(mount),
        ],
    )
    .map_err(|e| format!("挂载恢复镜像失败：{e}"))?;

    let sys32 = mount.join("Windows").join("System32");
    std::fs::create_dir_all(&sys32).map_err(|e| format!("镜像里没有 System32：{e}"))?;

    // 原入口先留一份：万一老师想手动进原来的恢复界面
    let ini = sys32.join("winpeshl.ini");
    if ini.exists() {
        let _ = std::fs::copy(&ini, sys32.join("winpeshl.ini.orig"));
    }
    std::fs::write(&ini, WINPESHL_INI).map_err(|e| format!("写 winpeshl.ini 失败：{e}"))?;
    // cmd.exe 只认 CRLF；源码里的裸 LF 会让 `if` / `goto` / 重定向解析错位
    std::fs::write(
        sys32.join("openfixos.cmd"),
        OPENFIXOS_CMD.replace('\n', "\r\n"),
    )
    .map_err(|e| format!("写修复脚本失败：{e}"))?;

    run_ok(
        "dism",
        &["/Unmount-Image", &mount_arg(mount), "/Commit"],
    )
    .map_err(|e| format!("提交恢复镜像失败：{e}"))?;
    Ok(())
}

fn mount_arg(dir: &Path) -> String {
    format!("/MountDir:{}", dir.display())
}

fn run(program: &str, args: &[&str]) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let out = std::process::Command::new(program)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("调不起 {program}：{e}"))?;
    let text = crate::crash_repair::decode_console(&out.stdout);
    if out.status.success() {
        Ok(text)
    } else {
        let err = crate::crash_repair::decode_console(&out.stderr);
        Err(format!(
            "{program} {} 失败：{}",
            args.join(" "),
            err.trim()
        ))
    }
}

fn run_ok(program: &str, args: &[&str]) -> Result<(), String> {
    run(program, args).map(|_| ())
}

/* ------------------------------------------------------------------ */
/* 恢复环境里跑的东西                                                  */
/* ------------------------------------------------------------------ */

/// 恢复环境的入口：用我们的脚本替掉默认的恢复界面。
const WINPESHL_INI: &str = "[LaunchApps]\r\n%SYSTEMROOT%\\System32\\openfixos.cmd\r\n";

/// 自动修复脚本。
///
/// 原则：
/// - **一定要重启回去**：任何一步失败都走到 `wpeutil reboot`，不会停在半截；
/// - **只能是纯 ASCII**：cmd.exe 按控制台代码页（中文系统是 GBK）读批处理，
///   UTF-8 的中文会被读成乱码，还会把下一行吃掉 —— 真机上就是
///   `'谁针凝?N' 不是内部或外部命令`、`X:\result.log 试图引用不存在的令牌`；
/// - **盘符不写死**：恢复环境里盘符可能变，先找 `\Windows\System32\config\SYSTEM`
///   定位系统盘，日志也写到那个盘上（否则应用读不到 `result.json`）。
///
/// 版式上刻意做成一块「屏幕」：自己的标题栏、上下双线、左边留白、
/// 每步一条会往前走的进度条、每步跑完标出起止时间与退出码，
/// 结尾署名 xxccdl。黑窗口在恢复环境里是唯一能用的界面，
/// 那就把它排得像样一点 —— 老师看到的第一屏不该是一堆裸 echo。
const OPENFIXOS_CMD: &str = r#"@echo off
setlocal enabledelayedexpansion
title openFixOS
mode con: cols=100 lines=34 >nul 2>nul
cls

set RULE=  ----------------------------------------------------------------------
set BAR=  ======================================================================

rem brand blue (same as #0071e3 in the app); body text goes back to grey
color 0B
echo.
echo %BAR%
echo.
echo       o p e n F i x O S
echo.
echo       offline repair for Windows startup and system files
echo.
echo                                            by  xxccdl
echo.
echo %BAR%
color 07
echo.

rem ---- locate the Windows volume ----
set WIN=
for %%d in (C D E F G H) do if exist "%%d:\Windows\System32\config\SYSTEM" set WIN=%%d:
if "%WIN%"=="" set WIN=C:
set OUT=%WIN%\OpenPPTView\fixos
if not exist "%OUT%" mkdir "%OUT%" >nul 2>nul
set LOG=%OUT%\result.log
del /q "%OUT%\result.json" >nul 2>nul

echo    Windows volume : %WIN%
echo    Log file       : %LOG%
echo    Started        : %DATE% %TIME%
echo.
echo %RULE%
echo.

set S_START=%DATE% %TIME%
set STEPS=

rem ---- 0) we took over the recovery entry: let the user opt out ----
if not exist "%SYSTEMROOT%\System32\choice.exe" goto countdown_done
echo    Repair starts in 20 seconds.
echo    Press N to skip and boot Windows.
echo.
choice /c yn /n /t 20 /d y >nul
if errorlevel 2 if not errorlevel 3 goto skip
:countdown_done
echo.
echo %RULE%
echo.

rem ---- 1) disk: bad sectors and file system ----
echo    [ 25%%] [####------------]  [1/4] disk check - chkdsk /f
set T=%TIME%
chkdsk %WIN% /f >> "%LOG%" 2>&1
set E=!ERRORLEVEL!
echo            started %T%   finished %TIME%   exit !E!
set STEPS=!STEPS!{"step":"chkdsk","exit":!E!},
echo.

rem ---- 2) system files, offline ----
echo    [ 50%%] [########--------]  [2/4] system files - sfc /scannow
set T=%TIME%
sfc /scannow /offbootdir=%WIN%\ /offwindir=%WIN%\Windows >> "%LOG%" 2>&1
set E=!ERRORLEVEL!
echo            started %T%   finished %TIME%   exit !E!
set STEPS=!STEPS!{"step":"sfc","exit":!E!},
echo.

rem ---- 3) component store ----
echo    [ 75%%] [############----]  [3/4] component store - DISM /RestoreHealth
set T=%TIME%
dism /Image:%WIN%\ /Cleanup-Image /RestoreHealth >> "%LOG%" 2>&1
set E=!ERRORLEVEL!
echo            started %T%   finished %TIME%   exit !E!
set STEPS=!STEPS!{"step":"dism","exit":!E!},
echo.

rem ---- 4) boot records: MBR / boot sector / BCD ----
echo    [100%%] [################]  [4/4] boot records - bootrec
set T=%TIME%
bootrec /fixmbr >> "%LOG%" 2>&1
bootrec /fixboot >> "%LOG%" 2>&1
bootrec /rebuildbcd >> "%LOG%" 2>&1
set E=!ERRORLEVEL!
echo            started %T%   finished %TIME%   exit !E!
set STEPS=!STEPS!{"step":"bootrec","exit":!E!}
echo.

set S_END=%DATE% %TIME%
> "%OUT%\result.json" echo {"startedAt":"%S_START%","finishedAt":"%S_END%","systemDrive":"%WIN%","steps":[!STEPS!],"summary":"chkdsk / sfc / dism / bootrec finished, see result.log"}
echo ==== openFixOS done %S_END% ==== >> "%LOG%"
color 0B
echo %RULE%
echo.
echo    Finished %S_END% - rebooting to Windows...
echo.
echo                                            openFixOS - crafted by xxccdl
echo %BAR%
color 07
goto done

:skip
echo ==== openFixOS skipped by user ==== >> "%LOG%"
color 0B
echo.
echo %RULE%
echo.
echo    Skipped - no changes were made.
echo    Rebooting to Windows...
echo.
echo                                            openFixOS - crafted by xxccdl
echo %BAR%
color 07

:done
rem hold 3s so the result screen above can be read, then reboot
choice /c y /n /t 3 /d y >nul
wpeutil reboot
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_related_codes_are_recognised() {
        for code in [
            "0x0000007B",
            "0x000000ED",
            "0x000000EF",
            "0x00000109",
            "0xC0000218",
            "0x0000007b (0xffff...)",
        ] {
            assert!(is_boot_threat(code), "{code} 该算引导类");
        }
        for code in ["0x00000116", "0x0000009F", "0x000000D1", "未知"] {
            assert!(!is_boot_threat(code), "{code} 不该拖进恢复环境");
        }
    }

    #[test]
    fn openfixos_script_always_reboots() {
        // 这条是安全底线：脚本最后必须重启，不能把机器留在恢复环境
        assert!(OPENFIXOS_CMD.contains("wpeutil reboot"));
        // 结果写进找到的那个系统盘的固定目录（盘符不写死）
        assert!(OPENFIXOS_CMD.contains(r"%WIN%\OpenPPTView\fixos"));
    }

    /// 真机上踩过的坑：cmd 按 GBK 读批处理，UTF-8 的中文会变成
    /// `'谁针凝?N' 不是内部或外部命令`，还会把 `>` 重定向搞坏
    /// （`X:\result.log` 试图引用不存在的令牌）。脚本必须是纯 ASCII。
    #[test]
    fn openfixos_script_is_pure_ascii() {
        assert!(OPENFIXOS_CMD.is_ascii());
        assert!(WINPESHL_INI.is_ascii());
    }

    /// 恢复环境那一屏是唯一会露脸的品牌位置：开屏与结尾都要带上署名。
    #[test]
    fn openfixos_screen_credits_the_author() {
        assert!(OPENFIXOS_CMD.matches("xxccdl").count() >= 3);
        assert!(OPENFIXOS_CMD.contains("o p e n F i x O S"));
    }

    /// 脚本写进镜像时要把 LF 换成 CRLF，否则 `if` / `goto` 会错位。
    #[test]
    fn openfixos_script_is_written_with_crlf() {
        let text = OPENFIXOS_CMD.replace('\n', "\r\n");
        assert!(!text.contains("\r\r\n"));
        assert!(text.starts_with("@echo off\r\n"));
    }

    #[test]
    fn openfixos_script_can_be_skipped() {
        // 常驻待命意味着「手动进恢复环境」也会先跑这个脚本：
        // 必须留一个不用干等十几分钟的出口
        assert!(OPENFIXOS_CMD.contains("choice /c yn"));
        assert!(OPENFIXOS_CMD.contains("goto skip"));
        assert!(OPENFIXOS_CMD.contains(":skip"));
    }

    #[test]
    fn boot_result_parses_what_the_script_writes() {
        let r: BootResult = serde_json::from_str(
            r#"{"startedAt":"2026/10/25 13:00:00.12","finishedAt":"2026/10/25 13:20:00.10",
                "systemDrive":"C:","steps":[{"step":"chkdsk","exit":0}],
                "summary":"磁盘、系统文件与引导记录已检查"}"#,
        )
        .expect("脚本写出来的形状必须能读");
        assert_eq!(r.system_drive, "C:");
        assert_eq!(r.steps.len(), 1);
        assert_eq!(r.steps[0].step, "chkdsk");
        assert!(r.summary.contains("引导记录"));
    }

    /// 真机上的两种 /info 输出（中英文各一份）。
    const INFO_ZH: &str = "Windows 恢复环境 (WinRE) 状态: Enabled\r\n\
Windows 恢复环境 (WinRE) 位置: \\\\?\\GLOBALROOT\\device\\harddisk0\\partition4\\Recovery\\WindowsRE\r\n\
启动配置数据 (BCD) 标识符: f23ce3b9-61a9-11f1-b5b2-401a589fdfcf\r\n\
恢复映像位置:\r\n\
恢复映像索引: 0\r\n";

    const INFO_EN: &str = "Windows Recovery Environment (Windows RE) and system reset configuration\r\n\
    Information:\r\n\
    Windows RE status:         Enabled\r\n\
    Windows RE location:       \\\\?\\GLOBALROOT\\device\\harddisk0\\partition4\\Recovery\\WindowsRE\r\n\
    Boot Configuration Data (BCD) identifier: f23ce3b9\r\n\
    Recovery image location:\r\n";

    #[test]
    fn reagentc_info_is_read_in_both_languages() {
        assert!(winre_enabled(INFO_ZH));
        assert!(winre_enabled(INFO_EN));
        assert!(original_winre_path(INFO_ZH)
            .unwrap()
            .ends_with(r"Recovery\WindowsRE"));
        assert!(original_winre_path(INFO_EN)
            .unwrap()
            .ends_with(r"Recovery\WindowsRE"));
    }

    #[test]
    fn a_disabled_recovery_environment_is_not_mistaken_for_enabled() {
        let disabled = INFO_EN.replace("status:         Enabled", "status:         Disabled");
        assert!(!winre_enabled(&disabled));
        let disabled_zh = INFO_ZH.replace("状态: Enabled", "状态: Disabled");
        assert!(!winre_enabled(&disabled_zh));
        // 未启用时位置是空的：不该把「恢复映像位置」当成恢复环境位置
        let empty = INFO_ZH
            .replace(r"\\?\GLOBALROOT\device\harddisk0\partition4\Recovery\WindowsRE", "")
            .replace("状态: Enabled", "状态: Disabled");
        assert_eq!(original_winre_path(&empty), None);
    }
}
