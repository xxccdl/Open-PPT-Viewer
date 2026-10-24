//! 蓝屏后的「自动尝试修复」。
//!
//! # 它做什么
//!
//! 蓝屏本身是结果，不是原因。绝大多数课堂机器上的蓝屏来自三类可修的问题：
//! 系统文件/组件存储损坏、磁盘上出现坏扇区、某个第三方驱动反复把内核带崩。
//! 这里就按这个顺序跑 Windows 自带的三个工具，再做一次驱动体检：
//!
//! 1. `sfc /scannow` —— 用组件存储里的正本替换被改坏的系统文件；
//! 2. `DISM /Online /Cleanup-Image /ScanHealth`，必要时 `/RestoreHealth` —— 修组件存储本身；
//! 3. `chkdsk <系统盘> /scan` —— **只读**扫描，发现坏扇区只报告、不动数据；
//! 4. 驱动体检 —— 找出 Windows 已经判为「无法启动」的设备，停用其中
//!    非微软、非关键类别的那几个（这类设备本来就用不了，停用能避免再次蓝屏）。
//!
//! # 它刻意不做什么
//!
//! - 不改引导配置（`bcdedit`）、不重装驱动、不删任何用户数据；
//! - 不自动重启：老师可能正在用这台机器，重启的决定留给人；
//! - 不碰磁盘、显示、键盘鼠标、USB 控制器这类一旦停用就没法用的设备；
//! - 修复动作**只在演示模式之外、且确实拿到管理员权限时**才执行。
//!
//! # 权限
//!
//! 上面这些命令都要管理员。安装程序登记了一个「登录时、以最高权限运行」的
//! 计划任务（见 `ppt-installer` 的 `install_crash_task`），所以正常安装的机器上
//! 这个进程本身就有管理员令牌，不需要老师点 UAC。拿不到管理员时如实跳过，
//! 分析照常进行 —— 报告里会写清楚「本次没有修复，原因：没有管理员权限」。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;

/// 单条命令的进度回调（给右下角小条用）。
pub type Progress<'a> = &'a mut dyn FnMut(&str);

/// 一条修复命令跑完的样子。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    /// 稳定的英文键：日志与服务器都按它归类。
    pub key: String,
    /// 给老师看的中文名。
    pub name: String,
    pub command: String,
    /// 跑完了并且退出码为 0。
    pub ok: bool,
    pub skipped: bool,
    pub timed_out: bool,
    pub exit_code: Option<i32>,
    pub seconds: u64,
    /// 输出里最值得看的那一句（整份输出在同目录的 `.log` 里）。
    pub summary: String,
    /// 日志文件名（相对 `repair/`）。
    pub log: String,
}

/// 运行修复。产物全部落在 `dir`（也就是 `crash/reports/<id>/`）下的 `repair/`。
///
/// 返回的 JSON 会被合并进报告的 `summary.json`，随整包一起上传。
pub fn run(dir: &Path, demo: bool, progress: Progress<'_>) -> serde_json::Value {
    let started = crate::crash::now_epoch();
    let rdir = dir.join("repair");
    let _ = std::fs::create_dir_all(&rdir);

    let admin = is_admin();
    let mut steps: Vec<Step> = Vec::new();

    let mut out = serde_json::json!({
        "version": 1,
        "demo": demo,
        "admin": admin,
        "startedAt": crate::crash::iso_utc(started),
        "steps": [],
        "drivers": serde_json::Value::Null,
        "verdict": "",
    });

    // 演示模式绝不执行真正的修复：一次演示测试不该去改一台老师的机器。
    // 只有做「修复链路本身」的联调时才用 `OPENPPTVIEW_CRASH_FORCE_REPAIR=1`
    // 显式打开（和 `_DISABLE` / `_ENDPOINT` 一样是给我们自己留的开关）。
    let force = std::env::var("OPENPPTVIEW_CRASH_FORCE_REPAIR")
        .map(|v| v == "1")
        .unwrap_or(false);
    if demo && !force {
        progress("演示模式：跳过实际修复");
        std::thread::sleep(Duration::from_millis(1600));
        out["verdict"] = serde_json::json!("演示模式，未执行修复");
        write_summary(&rdir.join("summary.json"), &out);
        return out;
    }

    if !admin {
        progress("没有管理员权限，跳过修复");
        out["verdict"] = serde_json::json!("未执行修复：需要管理员权限");
        write_summary(&rdir.join("summary.json"), &out);
        return out;
    }

    let system_drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
    let sys32 = PathBuf::from(&system_root).join("System32");

    // 1) 系统文件
    progress("正在修复系统文件…");
    steps.push(run_step(
        &rdir,
        "sfc",
        "系统文件检查",
        &sys32.join("sfc.exe"),
        &["/scannow".to_string()],
        Duration::from_secs(45 * 60),
    ));

    // 2) 组件存储：先扫，扫出问题才修（RestoreHealth 要联网，离线机房上跑不动）
    progress("正在检查组件存储…");
    let scan = run_step(
        &rdir,
        "dism-scan",
        "组件存储检查",
        &sys32.join("Dism.exe"),
        &[
            "/Online".to_string(),
            "/Cleanup-Image".to_string(),
            "/ScanHealth".to_string(),
        ],
        Duration::from_secs(20 * 60),
    );
    let need_restore = store_needs_repair(&rdir, &scan);
    steps.push(scan);

    if need_restore {
        progress("正在修复组件存储…");
        steps.push(run_step(
            &rdir,
            "dism-restore",
            "组件存储修复",
            &sys32.join("Dism.exe"),
            &[
                "/Online".to_string(),
                "/Cleanup-Image".to_string(),
                "/RestoreHealth".to_string(),
            ],
            Duration::from_secs(30 * 60),
        ));
    }

    // 3) 磁盘：只读扫描，不修、不卸载卷
    progress("正在检查磁盘…");
    steps.push(run_step(
        &rdir,
        "chkdsk",
        "磁盘只读扫描",
        &sys32.join("chkdsk.exe"),
        &[system_drive.clone(), "/scan".to_string()],
        Duration::from_secs(25 * 60),
    ));

    // 4) 驱动体检
    progress("正在检查驱动…");
    let drivers = driver_check(&rdir);

    out["steps"] = serde_json::json!(steps);
    out["drivers"] = drivers;
    out["finishedAt"] = serde_json::json!(crate::crash::iso_utc(crate::crash::now_epoch()));
    out["verdict"] = serde_json::json!(verdict(&steps));

    write_summary(&rdir.join("summary.json"), &out);
    out
}

/// 组件存储扫描的结果要不要接着做 `RestoreHealth`。
///
/// 判据刻意保守：扫描本身失败（离线、存储不可用）就不修 —— 否则会白等
/// 半小时还修不动；只有日志里明确说「有东西可修」才动手。
///
/// 看的是**整份日志**而不是摘要那一句：DISM 的结论句往往被
/// 「操作成功完成」盖住，只读摘要会漏掉「检测到损坏、可以修复」。
fn store_needs_repair(rdir: &Path, scan: &Step) -> bool {
    if scan.timed_out || scan.exit_code != Some(0) {
        return false;
    }
    let text = std::fs::read_to_string(rdir.join(&scan.log)).unwrap_or_default();
    store_output_needs_repair(&text)
}

/// 判据本身（与文件解耦，便于直接测）。
fn store_output_needs_repair(text: &str) -> bool {
    let text = text.to_lowercase();
    // 健康的输出里也有「损坏」两个字（「未检测到组件存储损坏」），先排掉
    if text.contains("未检测到")
        || text.contains("没有发现")
        || text.contains("沒有發現")
        || text.contains("no component store corruption")
    {
        return false;
    }
    text.contains("repair")
        || text.contains("corrupt")
        || text.contains("可修复")
        || text.contains("需要修复")
        || text.contains("0x800f081f")
}

/// 一句话结论，给服务器和老师看。
fn verdict(steps: &[Step]) -> String {
    let broken: Vec<&Step> = steps.iter().filter(|s| !s.ok).collect();
    if broken.is_empty() {
        "修复已经跑完，没有发现需要处理的问题".to_string()
    } else {
        format!(
            "修复已经跑完，其中 {} 项没能完成：{}",
            broken.len(),
            broken
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join("、")
        )
    }
}

fn write_summary(path: &Path, value: &serde_json::Value) {
    if let Ok(text) = serde_json::to_string_pretty(value) {
        let _ = std::fs::write(path, text);
    }
}

/* ------------------------------------------------------------------ */
/* 跑一条命令                                                          */
/* ------------------------------------------------------------------ */

/// `CREATE_NO_WINDOW`：不要弹出黑框。
/// `BELOW_NORMAL_PRIORITY_CLASS`：修复是后台活，不能跟老师正在放的课件抢 CPU。
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;

fn run_step(
    rdir: &Path,
    key: &str,
    name: &str,
    program: &Path,
    args: &[String],
    timeout: Duration,
) -> Step {
    let command = format!("{} {}", program.display(), args.join(" "));
    let log_name = format!("{key}.log");
    let started = Instant::now();

    let result = if program.exists() {
        run_tool(program, args, timeout)
    } else {
        ToolResult {
            code: None,
            timed_out: false,
            output: format!("找不到 {}", program.display()),
            spawn_error: Some("找不到程序".to_string()),
        }
    };

    let seconds = started.elapsed().as_secs();
    let ok = !result.timed_out && result.code == Some(0);
    let summary = if result.timed_out {
        format!("超过 {} 分钟没有结束，已经中止", timeout.as_secs() / 60)
    } else if let Some(e) = &result.spawn_error {
        e.clone()
    } else {
        pick_summary(&result.output)
    };

    // 完整输出留档：排查时往往要看那几行「发现损坏并已修复」
    let log_text = format!(
        "# {name}\n# {command}\n# 退出码 {code:?}　耗时 {seconds}s\n\n{output}",
        code = result.code,
        output = result.output
    );
    let _ = std::fs::write(rdir.join(&log_name), log_text);

    Step {
        key: key.to_string(),
        name: name.to_string(),
        command,
        ok,
        skipped: false,
        timed_out: result.timed_out,
        exit_code: result.code,
        seconds,
        summary,
        log: log_name,
    }
}

struct ToolResult {
    code: Option<i32>,
    timed_out: bool,
    output: String,
    spawn_error: Option<String>,
}

fn run_tool(program: &Path, args: &[String], timeout: Duration) -> ToolResult {
    use std::os::windows::process::CommandExt;

    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS)
        .spawn();

    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            return ToolResult {
                code: None,
                timed_out: false,
                output: String::new(),
                spawn_error: Some(format!("启动失败：{e}")),
            }
        }
    };

    // stdout / stderr 各用一条线程读到尾：不读的话管道写满，子进程就卡住了
    let out = child.stdout.take();
    let err = child.stderr.take();
    let h1 = std::thread::spawn(move || drain(out));
    let h2 = std::thread::spawn(move || drain(err));

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) => {}
            Err(_) => break None,
        }
        if Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(400));
    };

    let stdout = h1.join().unwrap_or_default();
    let stderr = h2.join().unwrap_or_default();
    let mut text = decode_console(&stdout);
    let err_text = decode_console(&stderr);
    if !err_text.trim().is_empty() {
        if !text.is_empty() {
            text.push_str("\r\n");
        }
        text.push_str(&err_text);
    }

    ToolResult {
        code,
        timed_out,
        output: text,
        spawn_error: None,
    }
}

fn drain(pipe: Option<impl Read>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(mut p) = pipe {
        let _ = p.read_to_end(&mut buf);
    }
    buf
}

/// 从整段输出里挑一句最像结论的。
///
/// `sfc` / `chkdsk` 的结论在最后几行、但中间混着大量进度条；`DISM` 的关键句
/// 通常带「损坏 / 可修复」。按这个优先级挑，挑不到就退回最后一行非空文本。
fn pick_summary(text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();

    let interesting = [
        "已修复",
        "修复成功",
        "成功修复",
        "没有发现",
        "未发现",
        "修复了",
        "无法修复",
        "损坏",
        "無損壞",
        "repair",
        "corrupt",
        "found",
    ];
    for line in lines.iter().rev() {
        let low = line.to_lowercase();
        if interesting.iter().any(|k| low.contains(&k.to_lowercase())) {
            return truncate(line, 220);
        }
    }
    lines
        .iter()
        .rev()
        .find(|l| !l.contains('%'))
        .map(|l| truncate(l, 220))
        .unwrap_or_default()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// 控制台程序被重定向时，中文 Windows 上的输出一般是 OEM 代码页（936），
/// 但 `chkdsk` / `sfc` 有时直接吐 UTF-16LE。两种都得认。
fn decode_console(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return utf16(&bytes[2..]);
    }
    let pairs = bytes.len() / 2;
    if pairs >= 4 {
        let zeros = bytes.iter().skip(1).step_by(2).filter(|b| **b == 0).count();
        if zeros * 10 > pairs * 6 {
            return utf16(bytes);
        }
    }
    oem_to_string(bytes)
}

fn utf16(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

fn oem_to_string(bytes: &[u8]) -> String {
    use windows::Win32::Globalization::{
        GetOEMCP, MultiByteToWideChar, MULTI_BYTE_TO_WIDE_CHAR_FLAGS,
    };

    let cp = unsafe { GetOEMCP() };
    let flags = MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0);
    let need = unsafe { MultiByteToWideChar(cp, flags, bytes, None) };
    if need <= 0 {
        return String::from_utf8_lossy(bytes).to_string();
    }
    let mut buf = vec![0u16; need as usize];
    let written = unsafe { MultiByteToWideChar(cp, flags, bytes, Some(&mut buf)) };
    if written <= 0 {
        return String::from_utf8_lossy(bytes).to_string();
    }
    String::from_utf16_lossy(&buf[..written as usize])
}

/* ------------------------------------------------------------------ */
/* 驱动体检                                                            */
/* ------------------------------------------------------------------ */

/// 这类设备停用之后机器就用不了了，永远不碰。
const CRITICAL_CLASSES: &[&str] = &[
    "System",
    "DiskDrive",
    "SCSIAdapter",
    "HDC",
    "Volume",
    "Keyboard",
    "Mouse",
    "Display",
    "Monitor",
    "USB",
    "Processor",
    "Firmware",
    "Computer",
];

/// 这些「无法启动」的错误码才考虑停用：
/// 10（无法启动）、18（需重装驱动）、28（未安装驱动）、31、37、43（驱动报错）。
const DISABLE_CODES: &[i64] = &[10, 18, 28, 31, 37, 43];

/// 一次最多停用几个。批量停一堆设备比蓝屏更难收场。
const MAX_DISABLE: usize = 2;

fn driver_check(rdir: &Path) -> serde_json::Value {
    let script = r#"
$ErrorActionPreference = 'SilentlyContinue'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$list = @(Get-CimInstance Win32_PnPEntity |
  Where-Object { $_.ConfigManagerErrorCode -ne 0 } |
  Select-Object Name, PNPClass, DeviceID, Manufacturer, Service, ConfigManagerErrorCode)
ConvertTo-Json -InputObject $list -Depth 4 -Compress
"#;

    let mut value = serde_json::json!({
        "problemDevices": [],
        "disabled": [],
        "note": "",
    });

    let Some(json) = ps_json(script, Duration::from_secs(45)) else {
        value["note"] = serde_json::json!("读不到设备信息（PowerShell 查询失败或超时）");
        return value;
    };

    let devices = normalize_array(json);
    let mut disabled = Vec::new();

    for d in &devices {
        let code = d
            .get("ConfigManagerErrorCode")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        if !DISABLE_CODES.contains(&code) {
            continue;
        }
        let class = d.get("PNPClass").and_then(|v| v.as_str()).unwrap_or("");
        let maker = d.get("Manufacturer").and_then(|v| v.as_str()).unwrap_or("");
        let service = d.get("Service").and_then(|v| v.as_str()).unwrap_or("");
        let id = d.get("DeviceID").and_then(|v| v.as_str()).unwrap_or("");
        if id.is_empty() || class.is_empty() {
            continue;
        }
        if CRITICAL_CLASSES.contains(&class) {
            continue;
        }
        if maker.to_lowercase().contains("microsoft") {
            continue;
        }
        if service.is_empty() {
            // 没有对应驱动服务的设备（多半是固件/虚拟设备），停用没有意义
            continue;
        }
        if disabled.len() >= MAX_DISABLE {
            break;
        }

        let safe_id = id.replace('\'', "''");
        let cmd = format!(
            "$ErrorActionPreference='Stop'; Disable-PnpDevice -InstanceId '{safe_id}' -Confirm:$false"
        );
        match run_powershell(&cmd, Duration::from_secs(30)) {
            Ok(text) if text.trim().is_empty() => {
                log::info!("已停用反复报错的设备：{id}（{class}）");
                disabled.push(serde_json::json!({
                    "deviceId": id,
                    "name": d.get("Name"),
                    "class": class,
                    "manufacturer": maker,
                    "service": service,
                    "errorCode": code,
                    "ok": true,
                    // 想恢复的话，管理员执行这一句即可
                    "enableCommand": format!("Enable-PnpDevice -InstanceId '{id}' -Confirm:$false"),
                }));
            }
            Ok(text) => {
                disabled.push(serde_json::json!({
                    "deviceId": id,
                    "ok": false,
                    "output": truncate(&text, 300),
                }));
            }
            Err(e) => {
                disabled.push(serde_json::json!({
                    "deviceId": id,
                    "ok": false,
                    "output": e,
                }));
            }
        }
    }

    value["problemDevices"] = serde_json::json!(devices);
    value["disabled"] = serde_json::json!(disabled);
    value["note"] = serde_json::json!(if devices.is_empty() {
        "没有发现 Windows 标记为异常的设备"
    } else if disabled.is_empty() {
        "有异常设备，但不属于可以安全停用的范围，已记录待人工处理"
    } else {
        "已停用反复报错的设备，恢复命令写在报告里"
    });

    let _ = std::fs::write(
        rdir.join("drivers.json"),
        serde_json::to_string_pretty(&value).unwrap_or_default(),
    );
    value
}

fn ps_json(script: &str, timeout: Duration) -> Option<serde_json::Value> {
    let text = run_powershell(script, timeout).ok()?;
    serde_json::from_str(text.trim()).ok()
}

fn run_powershell(script: &str, timeout: Duration) -> Result<String, String> {
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
    let ps = PathBuf::from(system_root)
        .join("System32\\WindowsPowerShell\\v1.0\\powershell.exe");
    if !ps.exists() {
        return Err("找不到 PowerShell".into());
    }
    let result = run_tool(
        &ps,
        &[
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-ExecutionPolicy".to_string(),
            "Bypass".to_string(),
            "-Command".to_string(),
            script.to_string(),
        ],
        timeout,
    );
    if result.timed_out {
        return Err("PowerShell 超时".into());
    }
    if let Some(e) = result.spawn_error {
        return Err(e);
    }
    Ok(result.output)
}

fn normalize_array(v: serde_json::Value) -> Vec<serde_json::Value> {
    match v {
        serde_json::Value::Array(a) => a,
        serde_json::Value::Null => Vec::new(),
        other => vec![other],
    }
}

/* ------------------------------------------------------------------ */
/* 权限                                                                */
/* ------------------------------------------------------------------ */

/// 当前进程是不是拿到了管理员令牌。
///
/// 用 `IsUserAnAdmin`：它看的是**本进程的令牌**，而不是「这个账号在不在
/// 管理员组」—— 后者在学校机器上经常是「在组里但以标准权限运行」，
/// 按它判断会导致修复命令一条条失败，还都以为是系统坏了。
pub fn is_admin() -> bool {
    use windows::Win32::UI::Shell::IsUserAnAdmin;
    unsafe { IsUserAnAdmin().as_bool() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthy_store_output_does_not_trigger_restore() {
        // 这句里含「损坏」，但它说的是「没坏」—— 误判会白跑半小时的 RestoreHealth
        assert!(!store_output_needs_repair(
            "部署映像服务和管理工具\r\n操作成功完成。\r\n未检测到组件存储损坏。"
        ));
        assert!(!store_output_needs_repair(
            "No component store corruption detected.\r\nThe operation completed successfully."
        ));
        assert!(!store_output_needs_repair("操作成功完成。"));
    }

    #[test]
    fn damaged_store_output_triggers_restore() {
        assert!(store_output_needs_repair(
            "组件存储损坏。\r\n修复操作可以修复此问题。\r\nThe component store is repairable."
        ));
        assert!(store_output_needs_repair("错误: 0x800f081f"));
    }

    #[test]
    fn summary_prefers_the_conclusion_line() {
        let sfc = "正在开始系统扫描。此过程将需要一些时间。\r\n\
                   开始系统扫描的验证阶段。\r\n\
                   验证 100% 已完成。\r\n\
                   Windows 资源保护找到了损坏文件并成功修复了它们。\r\n\
                   详细信息参见 CBS.Log。\r\n";
        assert!(pick_summary(sfc).contains("修复"));
    }

    #[test]
    fn summary_falls_back_to_last_line_without_percent() {
        assert_eq!(
            pick_summary("进度 10%\r\n进度 90%\r\n阶段完成\r\n\r\n"),
            "阶段完成"
        );
    }

    #[test]
    fn console_output_is_decoded_from_oem_codepage() {
        // cmd 的 echo 走控制台 OEM 代码页（中文机器上是 936）——
        // 这正是 `sfc` / `chkdsk` 输出中文时的编码。用 UTF-8 硬解会全是问号。
        use std::os::windows::process::CommandExt;
        let out = Command::new("cmd.exe")
            .args(["/c", "echo 已修复损坏的文件"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .expect("跑不了 cmd");
        let text = decode_console(&out.stdout);
        assert!(text.contains("已修复"), "解出来是：{text:?}");
    }

    #[test]
    fn utf16_output_is_recognised() {
        let mut bytes = vec![0xFF, 0xFE];
        for u in "需要修复".encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(decode_console(&bytes), "需要修复");
    }
}
