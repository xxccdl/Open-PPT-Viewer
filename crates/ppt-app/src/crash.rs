//! 蓝屏（BSOD）自动诊断。
//!
//! # 这个功能在解决什么
//!
//! 学校机房里「上一节课还好好的，下一节课开机就蓝屏」是最难查的一类问题：
//! 老师只会说「它坏了」，而我们既不在现场，也拿不到那台机器的任何东西。
//! 蓝屏的现场（错误码、四个参数、内核转储、当时的系统版本与驱动）
//! 全部躺在 Windows 自己的记录里，只要**下一次开机时把它捡起来**就行。
//!
//! # 为什么在开机时做，而不是出问题时
//!
//! 蓝屏的一瞬间进程已经没了，应用不可能「当场」分析。能做的只有：
//! 下次开机时回头看 Windows 留下的痕迹。所以这里靠登录时启动一次
//! （见安装程序登记的计划任务），静默检查 —— 没有蓝屏记录就当场退出，
//! 连窗口都不建（WebView2 那几百毫秒都省了）。
//!
//! # 它的边界
//!
//! - 分析阶段**只读**系统记录：不碰引导配置、不改系统设置；
//! - 修复阶段只用 Windows 自带的系统文件/组件存储/磁盘只读扫描，
//!   以及停用「Windows 已判定无法启动」的非关键设备（见 [`crate::crash_repair`]）；
//! - 采集的内容固定且可解释（见 [`collect`]），可在设置里关掉；
//! - 上传走一条 POST，服务器地址由使用者自己填（见 [`load_settings`]）；
//! - 传不出去就把包留在本地，下一次开机静默重试，不打扰上课。
//!
//! # 数据去哪了
//!
//! 本地：`%LOCALAPPDATA%\OpenPPTView\crash\`（`reports\` 是分析结果，
//! `outbox\` 是还没送出去的包）。设置页有「打开报告文件夹」。

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

/// 登录时用的开关。安装程序会把它写进 `Run`。
pub const FLAG: &str = "--crash-report";
/// 演示开关：不真的蓝屏也能把整套流程走一遍（见 `mode`）。
pub const DEMO_FLAG: &str = "--crash-report-demo";

/// 「正在分析」至少停留这么久，不然一闪而过，老师看不见发生了什么。
const INTRO_HOLD: Duration = Duration::from_millis(1500);
/// 缩到右下角之后，成功/失败的结果再停留多久。
const OUTRO_HOLD: Duration = Duration::from_millis(2000);
/// 单次采集最多带多少个转储、总共多少字节。
const MAX_DUMPS: usize = 3;
const MAX_DUMP_BYTES: u64 = 64 * 1024 * 1024;
/// 上传超时。机房上行慢，给足时间；补送旧包时用更短的一档，
/// 免得开机被一条连不上的地址卡住。
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(45);

/// 进程只跑一次分析（第二个实例转交过来的那一次除外）。
static STARTED: AtomicBool = AtomicBool::new(false);
/// 主进程启动前已经探测到的报告，交给 `setup` 用。
static PENDING: std::sync::Mutex<Option<Report>> = std::sync::Mutex::new(None);

/* ------------------------------------------------------------------ */
/* 设置                                                                */
/* ------------------------------------------------------------------ */

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// 关掉之后：登录时仍然会启动一次，但**立刻退出**，不分析也不上传。
    pub enabled: bool,
    /// 蓝屏后要不要自动尝试修复。关掉就只分析、只上报。
    ///
    /// `serde(default)`：这个字段是后加的，老版本留下的 `settings.json`
    /// 里没有它 —— 缺省按「开」读，不能让老师升级完发现修复悄悄没了。
    #[serde(default = "default_true")]
    pub repair: bool,
    /// 上报地址。留空 = 只分析、只存在本地。
    pub endpoint: String,
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            enabled: true,
            repair: true,
            endpoint: String::new(),
        }
    }
}

/// `%LOCALAPPDATA%\OpenPPTView\crash`。
pub fn root_dir() -> PathBuf {
    crate::data_dir().join("crash")
}

fn settings_path() -> PathBuf {
    root_dir().join("settings.json")
}

fn state_path() -> PathBuf {
    root_dir().join("state.json")
}

pub fn load_settings() -> Settings {
    let mut s = std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Settings>(&t).ok())
        .unwrap_or_default();
    // 环境变量优先：部署时（或我们排查时）不必去改用户目录里的文件
    if let Ok(v) = std::env::var("OPENPPTVIEW_CRASH_ENDPOINT") {
        if !v.trim().is_empty() {
            s.endpoint = v;
        }
    }
    s
}

pub fn save_settings(s: &Settings) -> Result<(), String> {
    std::fs::create_dir_all(root_dir()).map_err(|e| format!("建不出目录：{e}"))?;
    let text = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    std::fs::write(settings_path(), text).map_err(|e| format!("写设置失败：{e}"))
}

/// 现在要不要做这件事。
///
/// 两道闸门都要过：设置里没关、而且没有 `OPENPPTVIEW_CRASH_DISABLE=1`。
/// 后者是给机房批量部署留的「一键关掉」，不必逐台点界面。
pub fn enabled() -> bool {
    if std::env::var("OPENPPTVIEW_CRASH_DISABLE").map(|v| v == "1").unwrap_or(false) {
        return false;
    }
    load_settings().enabled
}

/* ------------------------------------------------------------------ */
/* 启动模式                                                            */
/* ------------------------------------------------------------------ */

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 真的去查 Windows 记录的蓝屏。
    Real,
    /// 造一份（标着 `demo`）走完整流程，用来验证界面与上传。
    Demo,
}

/// 命令行是不是要求做蓝屏诊断（以及是不是演示模式）。
pub fn requested(args: &[String]) -> Option<Mode> {
    if args.iter().skip(1).any(|a| a == DEMO_FLAG) {
        return Some(Mode::Demo);
    }
    if args.iter().skip(1).any(|a| a == FLAG) {
        return Some(Mode::Real);
    }
    None
}

/// 主进程启动前已经把报告放这儿了。
pub fn stash(r: Report) {
    if let Ok(mut slot) = PENDING.lock() {
        *slot = Some(r);
    }
}

pub fn has_pending() -> bool {
    PENDING.lock().map(|s| s.is_some()).unwrap_or(false)
}

fn take_pending() -> Option<Report> {
    PENDING.lock().ok().and_then(|mut s| s.take())
}

/* ------------------------------------------------------------------ */
/* 报告                                                                */
/* ------------------------------------------------------------------ */

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub id: String,
    /// 蓝屏错误码（形如 `0x000000EF`）；没读到就写「未知」。
    pub bugcheck: String,
    /// 发生时刻（UTC，ISO 8601）。
    pub occurred_at: String,
    /// 采集时是否找到了内核转储。
    pub has_dump: bool,
    /// 演示模式产生的：服务器一眼能认出来，别当真问题去查。
    pub demo: bool,
}

/// 转储文件路径（只在本次进程里用，不写进报告）。
#[derive(Debug, Clone)]
struct DumpFile {
    path: PathBuf,
    size: u64,
    modified: u64,
}

/* ------------------------------------------------------------------ */
/* 探测                                                                */
/* ------------------------------------------------------------------ */

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Marker {
    /// 上次检查到的时刻（Unix 秒）。只看这之后发生的事。
    last_check: u64,
}

fn read_marker() -> Option<Marker> {
    std::fs::read_to_string(state_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Marker>(&t).ok())
}

/// 记下「已经查到这一刻」，下次不再重复报同一次蓝屏。
///
/// 只在**包已经生成之后**才推进：万一分析中途进程没了，
/// 下次开机还能把这次蓝屏重新捡起来。
fn write_marker(now: u64) {
    let _ = std::fs::create_dir_all(root_dir());
    let m = Marker { last_check: now };
    if let Ok(t) = serde_json::to_string(&m) {
        let _ = std::fs::write(state_path(), t);
    }
}

/// 看看自上次检查以来有没有蓝屏。
///
/// 两条证据互相补位：事件日志是「系统自己说它蓝屏了」（通常最准），
/// 转储文件是「现场确实留下了东西」。有的机器事件日志被清理工具
/// 清空了，但 `Minidump` 还在；有的机器转储目录读不了（非管理员），
/// 但事件日志可读。只要有一条成立就认。
pub fn detect(mode: Mode) -> Option<Report> {
    let now = now_epoch();

    if mode == Mode::Demo {
        return Some(Report {
            id: format!("demo-{}", stamp(now)),
            bugcheck: "0x000000EF".into(),
            occurred_at: iso_utc(now),
            has_dump: false,
            demo: true,
        });
    }

    // 第一次跑：只回看最近 24 小时，别把几个月前的旧账翻出来
    let since = read_marker()
        .map(|m| m.last_check)
        .filter(|v| *v > 0)
        .unwrap_or_else(|| now.saturating_sub(24 * 3600));

    let events = bugcheck_events().unwrap_or_default();
    let hit = events
        .iter()
        .filter(|e| e.epoch >= since)
        .max_by_key(|e| e.epoch);

    let dumps = recent_dumps(since);
    if hit.is_none() && dumps.is_empty() {
        // 没有新东西：把游标推到此刻，下次只看更新的
        write_marker(now);
        return None;
    }

    let (bugcheck, occurred_at) = match hit {
        Some(e) => (e.code.clone(), e.time.clone()),
        None => (
            "未知（未读到蓝屏事件）".to_string(),
            iso_utc(dumps.first().map(|d| d.modified).unwrap_or(now)),
        ),
    };

    Some(Report {
        id: stamp(now),
        bugcheck,
        occurred_at,
        has_dump: !dumps.is_empty(),
        demo: false,
    })
}

#[derive(Debug, Clone)]
struct BugCheckEvent {
    time: String,
    epoch: u64,
    code: String,
    provider: String,
    params: Vec<String>,
}

impl BugCheckEvent {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "time": self.time,
            "provider": self.provider,
            "code": self.code,
            "params": self.params,
        })
    }
}

/// 读「计算机已从检测到的错误中重新启动」这类事件（WER 1001）。
fn bugcheck_events() -> Result<Vec<BugCheckEvent>, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    // 这个 provider 是 Windows 自己记「上一次启动是因为 bugcheck」的地方。
    // 用 XML 而不是文本输出：文本里的错误码随系统语言变，XML 的字段名不变。
    let query = "*[System[Provider[@Name='Microsoft-Windows-WER-SystemErrorReporting'] \
                 and (EventID=1001)]]";
    let out = std::process::Command::new("wevtutil")
        .args(["qe", "System", &format!("/q:{query}"), "/f:xml", "/rd:true", "/c:8"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("读不了事件日志：{e}"))?;

    if out.stdout.is_empty() {
        return Ok(Vec::new());
    }
    let xml = String::from_utf8_lossy(&out.stdout);
    Ok(parse_bugcheck_xml(&xml))
}

#[derive(Default)]
struct PendingEvent {
    time: String,
    provider: String,
    fields: Vec<(String, String)>,
}

/// 从 wevtutil 吐出的（可能连着好几份）`<Event>` XML 里取出事件。
///
/// 不引入完整的事件日志 API：这里的形状非常固定，一个几十行的小解析器
/// 比多背一层 COM/EvtQuery 更划算，也不会因为系统语言不同而失灵。
fn parse_bugcheck_xml(xml: &str) -> Vec<BugCheckEvent> {
    use quick_xml::events::Event;

    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut out = Vec::new();
    let mut cur: Option<PendingEvent> = None;
    let mut in_data = false;
    let mut data_name = String::new();
    let mut data_text = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.name().local_name().as_ref() {
                "Event" => cur = Some(PendingEvent::default()),
                "Provider" => {
                    if let Some(c) = cur.as_mut() {
                        c.provider = attr_of(&e, "Name");
                    }
                }
                "TimeCreated" => {
                    if let Some(c) = cur.as_mut() {
                        c.time = attr_of(&e, "SystemTime");
                    }
                }
                "Data" => {
                    in_data = true;
                    data_name = attr_of(&e, "Name");
                    data_text.clear();
                }
                _ => {}
            },
            Ok(Event::Empty(e)) => match e.name().local_name().as_ref() {
                "Provider" => {
                    if let Some(c) = cur.as_mut() {
                        c.provider = attr_of(&e, "Name");
                    }
                }
                "TimeCreated" => {
                    if let Some(c) = cur.as_mut() {
                        c.time = attr_of(&e, "SystemTime");
                    }
                }
                _ => {}
            },
            Ok(Event::Text(t)) => {
                if in_data {
                    let s: &str = t.as_ref();
                    data_text.push_str(s);
                }
            }
            Ok(Event::End(e)) => match e.name().local_name().as_ref() {
                "Data" => {
                    if let Some(c) = cur.as_mut() {
                        c.fields.push((data_name.clone(), data_text.clone()));
                    }
                    in_data = false;
                }
                "Event" => {
                    if let Some(p) = cur.take() {
                        out.push(finish_event(p));
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    out
}

fn finish_event(p: PendingEvent) -> BugCheckEvent {
    let mut code = String::new();
    let mut params = Vec::new();
    for (name, value) in &p.fields {
        match name.as_str() {
            // param1 是 bugcheck code，param2..4 是它的三个参数
            "param1" => code = value.clone(),
            "param2" | "param3" | "param4" => params.push(value.clone()),
            _ => {}
        }
    }
    if code.is_empty() {
        code = "未知".into();
    }
    BugCheckEvent {
        epoch: iso_to_epoch(&p.time).unwrap_or(0),
        time: p.time,
        code,
        provider: p.provider,
        params,
    }
}

fn attr_of(e: &quick_xml::events::BytesStart<'_>, name: &str) -> String {
    for a in e.attributes().flatten() {
        if a.key.local_name().as_ref() == name {
            // 时间与错误码都是纯 ASCII，直接取原值即可
            return a.value.to_string();
        }
    }
    String::new()
}

/// 自 `since` 之后写出来的内核转储（按时间从新到旧）。
fn recent_dumps(since: u64) -> Vec<DumpFile> {
    let root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\Windows"))
        .join("Minidump");
    let Ok(entries) = std::fs::read_dir(&root) else {
        // 常见：标准用户读不了 `C:\Windows\Minidump`。
        // 这不是错误 —— 事件日志那条证据还在
        return Vec::new();
    };
    let mut list: Vec<DumpFile> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("dmp") {
                return None;
            }
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            Some(DumpFile {
                path,
                size: meta.len(),
                modified,
            })
        })
        .filter(|d| d.modified >= since)
        .collect();

    list.sort_by_key(|d| std::cmp::Reverse(d.modified));
    list.truncate(MAX_DUMPS);
    list
}

/* ------------------------------------------------------------------ */
/* 分析：把现场收拢成一个包裹                                          */
/* ------------------------------------------------------------------ */

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Receipt {
    pub id: String,
    pub zip: PathBuf,
    pub bytes: u64,
}

/// 采集现场。产物落在 `crash\reports\<id>\`，返回这个目录。
///
/// 打包（[`finish`]）故意放到修复之后：修复日志要一起进包，
/// 所以先「把现场收下来」，修完再「封包」。
pub fn collect(report: &Report) -> Result<PathBuf, String> {
    let dir = root_dir().join("reports").join(&report.id);
    if dir.exists() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("建不出目录：{e}"))?;

    // 1. 事件记录（原始形状，含参数）：排查时往往靠这三个参数定位到具体驱动
    let events = bugcheck_events().unwrap_or_default();
    let events_json = serde_json::json!({
        "collectedAt": iso_utc(now_epoch()),
        "events": events.iter().map(|e| e.to_json()).collect::<Vec<_>>(),
    });
    write_json(&dir.join("bugcheck-events.json"), &events_json)?;

    // 2. 系统信息：版本、补丁号、CPU、内存、显卡
    let mut info = vec![
        ("应用版本".to_string(), env!("CARGO_PKG_VERSION").to_string()),
        ("采集时间".to_string(), iso_utc(now_epoch())),
        ("蓝屏代码".to_string(), report.bugcheck.clone()),
        ("发生时间".to_string(), report.occurred_at.clone()),
        ("演示数据".to_string(), if report.demo { "是" } else { "否" }.to_string()),
    ];
    info.extend(system_info());
    let info_text = info
        .iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect::<Vec<_>>()
        .join("\r\n");
    std::fs::write(dir.join("system.txt"), info_text).map_err(|e| e.to_string())?;

    // 3. 应用日志尾部（出事前老师做过什么，这里最清楚）
    let log_dir = dir.join("logs");
    std::fs::create_dir_all(&log_dir).map_err(|e| e.to_string())?;
    for (name, tail) in [
        ("openpptview.log", 512 * 1024),
        ("openpptview.log.1", 128 * 1024),
    ] {
        let src = crate::log_dir().join(name);
        if let Ok(bytes) = read_tail(&src, tail) {
            let _ = std::fs::write(log_dir.join(name), bytes);
        }
    }

    // 4. 内核转储：只带最近几个，总量封顶。
    //    说明一下为什么敢传：这是内核内存镜像，可能含少量用户态残留；
    //    设置页写明了「会上传内存转储」，关掉开关就什么都不传。
    let dumps = recent_dumps(0);
    let mut budget = MAX_DUMP_BYTES;
    let mut copied = Vec::new();
    if !dumps.is_empty() {
        let dump_dir = dir.join("minidumps");
        std::fs::create_dir_all(&dump_dir).map_err(|e| e.to_string())?;
        for d in &dumps {
            if d.size > budget {
                continue;
            }
            let name = d
                .path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "dump.dmp".into());
            let dst = dump_dir.join(&name);
            match std::fs::copy(&d.path, &dst) {
                Ok(n) => {
                    budget = budget.saturating_sub(n);
                    copied.push(name);
                }
                Err(e) => log::warn!("转储复制不了（不影响报告）：{e}"),
            }
        }
    }

    let summary = serde_json::json!({
        "version": 1,
        "report": report,
        "hasDumpFiles": copied,
        "system": info.iter().map(|(k, v)| serde_json::json!({"name": k, "value": v}))
            .collect::<Vec<_>>(),
    });
    write_json(&dir.join("summary.json"), &summary)?;

    Ok(dir)
}

/// 把「修复日志」并进摘要，再封包。
///
/// 修复是在 [`collect`] 之后跑的，它把结果写在 `repair/summary.json`；
/// 这里合进顶层摘要，服务器只读一个文件就能看到「分析了什么、修了什么」。
pub fn finish(report: &Report, dir: &Path) -> Result<Receipt, String> {
    merge_repair(dir);

    // 打包
    let outbox = root_dir().join("outbox");
    std::fs::create_dir_all(&outbox).map_err(|e| format!("建不出目录：{e}"))?;
    let zip_path = outbox.join(format!("{}.zip", report.id));
    write_zip(dir, &zip_path)?;
    let bytes = std::fs::metadata(&zip_path).map(|m| m.len()).unwrap_or(0);

    // 包裹已经落地，这一页可以翻过去了。
    // 演示模式不推进游标 —— 否则真蓝屏的记录会被一次演示测试「吃掉」
    if !report.demo {
        write_marker(now_epoch());
    }

    // 上传时要靠它填请求头，所以一起放在 outbox 里
    let sidecar = outbox.join(format!("{}.json", report.id));
    let summary_text = std::fs::read_to_string(dir.join("summary.json")).map_err(|e| e.to_string())?;
    std::fs::write(&sidecar, summary_text).map_err(|e| format!("写 {sidecar:?} 失败：{e}"))?;

    Ok(Receipt {
        id: report.id.clone(),
        zip: zip_path,
        bytes,
    })
}

fn merge_repair(dir: &Path) {
    let repair_path = dir.join("repair").join("summary.json");
    let Ok(text) = std::fs::read_to_string(&repair_path) else {
        return;
    };
    let Ok(repair) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    let summary_path = dir.join("summary.json");
    let Ok(text) = std::fs::read_to_string(&summary_path) else {
        return;
    };
    let Ok(mut summary) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    if let Some(obj) = summary.as_object_mut() {
        obj.insert("repair".into(), repair);
    }
    let _ = write_json(&summary_path, &summary);
}

fn write_json(path: &Path, value: &serde_json::Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| format!("写 {path:?} 失败：{e}"))
}

/// 读取文件末尾 `max` 字节（日志可能已经很大，没必要整份装进来）。
fn read_tail(path: &Path, max: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(max);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

fn write_zip(dir: &Path, zip_path: &Path) -> Result<(), String> {
    use zip::write::SimpleFileOptions;

    let file = std::fs::File::create(zip_path).map_err(|e| format!("建不出包：{e}"))?;
    let mut zw = zip::ZipWriter::new(file);

    let mut files = Vec::new();
    collect_files(dir, dir, &mut files).map_err(|e| format!("遍历报告目录失败：{e}"))?;
    files.sort();

    for (rel, abs) in files {
        // 转储本身几乎压不动，还费时间；文本才值得压
        let stored = abs.extension().and_then(|s| s.to_str()) == Some("dmp");
        let opts = SimpleFileOptions::default().compression_method(if stored {
            zip::CompressionMethod::Stored
        } else {
            zip::CompressionMethod::Deflated
        });
        zw.start_file::<_, ()>(rel.replace('\\', "/"), opts)
            .map_err(|e| format!("写入包失败：{e}"))?;
        let bytes = std::fs::read(&abs).map_err(|e| format!("读不了 {}：{e}", abs.display()))?;
        zw.write_all(&bytes).map_err(|e| format!("写入包失败：{e}"))?;
    }
    zw.finish().map_err(|e| format!("收尾包失败：{e}"))?;
    Ok(())
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, out)?;
        } else if path.is_file() {
            let rel = path
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| path.to_string_lossy().to_string());
            out.push((rel, path));
        }
    }
    Ok(())
}

/// 系统版本、补丁、CPU、内存、显卡。
///
/// 都从注册表/系统 API 读，不跑 `systeminfo`：那个命令要好几秒，
/// 而这段时间老师就盯着「正在分析…」。
fn system_info() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (label, root, key, value) in [
        (
            "系统",
            "HKLM",
            "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion",
            "ProductName",
        ),
        (
            "版本",
            "HKLM",
            "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion",
            "DisplayVersion",
        ),
        (
            "内部版本",
            "HKLM",
            "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion",
            "CurrentBuild",
        ),
        (
            "补丁号",
            "HKLM",
            "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion",
            "UBR",
        ),
        (
            "CPU",
            "HKLM",
            "HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0",
            "ProcessorNameString",
        ),
        (
            "BIOS",
            "HKLM",
            "HARDWARE\\DESCRIPTION\\System\\BIOS",
            "BIOSVersion",
        ),
        (
            "主板厂商",
            "HKLM",
            "HARDWARE\\DESCRIPTION\\System\\BIOS",
            "BaseBoardManufacturer",
        ),
        (
            "主板型号",
            "HKLM",
            "HARDWARE\\DESCRIPTION\\System\\BIOS",
            "BaseBoardProduct",
        ),
    ] {
        if let Some(v) = reg_string(root, key, value) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                out.push((label.to_string(), v));
            }
        }
    }

    if let Some(mb) = total_ram_mb() {
        out.push(("内存".into(), format!("{mb} MB")));
    }

    // 显卡：设备类下一串子键，挑出有 DriverDesc 的
    let gpus = reg_subkey_values(
        "HKLM",
        "SYSTEM\\CurrentControlSet\\Control\\Class\\{4d36e968-e325-11ce-bfc1-08002be10318}",
        "DriverDesc",
    );
    if !gpus.is_empty() {
        out.push(("显卡".into(), gpus.join(" / ")));
    }
    out
}

fn reg_string(root: &str, path: &str, value: &str) -> Option<String> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;

    let root = match root {
        "HKCU" => RegKey::predef(HKEY_CURRENT_USER),
        _ => RegKey::predef(HKEY_LOCAL_MACHINE),
    };
    root.open_subkey(path).ok()?.get_value(value).ok()
}

fn reg_subkey_values(root: &str, path: &str, value: &str) -> Vec<String> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;

    let _ = root;
    let Ok(key) = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(path) else {
        return Vec::new();
    };
    key.enum_keys()
        .flatten()
        .filter_map(|name| {
            key.open_subkey(&name)
                .ok()
                .and_then(|k| k.get_value::<String, _>(value).ok())
        })
        .filter(|s| !s.trim().is_empty())
        .collect()
}

fn total_ram_mb() -> Option<u64> {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    let mut m = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    if unsafe { GlobalMemoryStatusEx(&mut m) }.is_ok() {
        Some(m.ullTotalPhys / (1024 * 1024))
    } else {
        None
    }
}

/* ------------------------------------------------------------------ */
/* 上传                                                                */
/* ------------------------------------------------------------------ */

/// 把包送到服务器。
///
/// 协议刻意做得很朴素：整个 zip 当请求体，元数据放请求头。
/// 服务端用 Node 写（见 `server/crash/server.mjs`），不解析 multipart，
/// 也就不必在客户端和服务端各背一套 multipart 实现。
pub fn upload(receipt: &Receipt, report: &Report) -> Result<(), String> {
    let settings = load_settings();
    let endpoint = settings.endpoint.trim();
    if endpoint.is_empty() {
        return Err("没有配置上报服务器".into());
    }

    let client = reqwest::blocking::Client::builder()
        .user_agent(format!("OpenPPTView/{}", env!("CARGO_PKG_VERSION")))
        .timeout(UPLOAD_TIMEOUT)
        .build()
        .map_err(|e| format!("建不出连接：{e}"))?;

    let file = std::fs::File::open(&receipt.zip).map_err(|e| format!("打不开报告包：{e}"))?;
    let mut req = client
        .post(endpoint)
        .header("Content-Type", "application/zip")
        .header("Content-Length", receipt.bytes.to_string())
        .header("X-Crash-Id", &report.id)
        .header("X-App-Version", env!("CARGO_PKG_VERSION"))
        .header("X-Bugcheck", &report.bugcheck)
        .header("X-Occurred-At", &report.occurred_at)
        .header("X-Report-Mode", if report.demo { "demo" } else { "real" })
        .body(file);

    if let Ok(token) = std::env::var("OPENPPTVIEW_CRASH_TOKEN") {
        if !token.trim().is_empty() {
            req = req.bearer_auth(token);
        }
    }

    let resp = req.send().map_err(|e| format!("连不上服务器：{e}"))?;
    if !resp.status().is_success() {
        return Err(format!("服务器返回 {}", resp.status()));
    }
    Ok(())
}

/// 上次没送出去的包，这次开机再试一遍。
///
/// 成功就删掉；失败就留着 —— 但**不弹界面**，老师是来上课的，
/// 不该因为上次的崩溃记录没传上去被打断。
pub fn flush_outbox() {
    let outbox = root_dir().join("outbox");
    let Ok(entries) = std::fs::read_dir(&outbox) else {
        return;
    };
    let endpoint = load_settings().endpoint.trim().to_string();
    if endpoint.is_empty() {
        return;
    }

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("zip") {
            continue;
        }
        let Some(id) = path.file_stem().map(|s| s.to_string_lossy().to_string()) else {
            continue;
        };
        let sidecar = outbox.join(format!("{id}.json"));
        let Ok(text) = std::fs::read_to_string(&sidecar) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let report = v
            .get("report")
            .and_then(|r| serde_json::from_value::<Report>(r.clone()).ok());
        let Some(report) = report else { continue };

        let receipt = Receipt {
            id: id.clone(),
            bytes: std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
            zip: path.clone(),
        };
        match upload(&receipt, &report) {
            Ok(()) => {
                log::info!("上一次的蓝屏报告已补送：{id}");
                let _ = std::fs::remove_file(&path);
                let _ = std::fs::remove_file(&sidecar);
            }
            Err(e) => log::warn!("补送蓝屏报告失败（留着下次再试）：{e}"),
        }
    }
}

/* ------------------------------------------------------------------ */
/* 界面与动画                                                          */
/* ------------------------------------------------------------------ */

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Phase {
    phase: String,
    message: String,
}

fn emit(app: &AppHandle, phase: &str, message: &str) {
    let _ = app.emit(
        "crash-phase",
        Phase {
            phase: phase.to_string(),
            message: message.to_string(),
        },
    );
}

/// 把主窗口藏起来、把诊断窗口叫出来。
///
/// 只做「叫出来」这一件事：真正开始分析要等界面回 `crash_ready`
/// （见 [`begin`]）。界面先挂好事件监听再说「可以了」，
/// 否则分析进度会在没人听的时候发出去，窗口就永远停在「正在分析…」。
pub fn show_window(app: &AppHandle) {
    if let Some(main) = app.get_webview_window("main") {
        let _ = main.hide();
    }
    let Some(win) = app.get_webview_window("crash") else {
        log::warn!("找不到 crash 窗口（配置里漏了？）");
        return;
    };
    let _ = win.show();
    let _ = win.set_focus();
}

/// 展示窗口，并设一个「界面没来叫我也得开始」的兜底。
///
/// 为什么需要兜底：万一是前端脚本没加载起来（改坏了 CSP、资源缺失），
/// 这个窗口就会永远挂着 —— 开机时挂着一块「正在分析…」比不显示更糟。
/// 8 秒还等不到 `crash_ready` 就直接开始，最坏也只是看不到进度文字。
pub fn prepare(app: AppHandle) {
    if !has_pending() {
        app.exit(0);
        return;
    }
    show_window(&app);
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(8));
        if !STARTED.load(Ordering::SeqCst) {
            log::warn!("界面没有回 crash_ready，按兜底直接开始分析");
            begin(app);
        }
    });
}

/// 第二个实例带着 `--crash-report` 进来时，由正在跑的那一份接住。
///
/// 不显示主界面：老师并不是要打开课件，而是刚蓝屏重启。
pub fn raise_if_new(app: &AppHandle) {
    if STARTED.load(Ordering::SeqCst) || !enabled() {
        return;
    }
    let mode = if std::env::args().skip(1).any(|a| a == DEMO_FLAG) {
        Mode::Demo
    } else {
        Mode::Real
    };
    let Some(report) = detect(mode) else {
        return;
    };
    stash(report);
    prepare(app.clone());
}

/// 界面已经准备好接进度了 —— 现在才开始。
pub fn begin(app: AppHandle) {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(report) = take_pending() else {
        app.exit(0);
        return;
    };
    std::thread::spawn(move || run(app, report));
}

fn run(app: AppHandle, report: Report) {
    log::info!(
        "开始分析蓝屏报告 {}（{}，演示={}）",
        report.id,
        report.bugcheck,
        report.demo
    );
    let settings = load_settings();
    emit(&app, "analyzing", "");

    // 采集放到另一条线程：界面要在这段时间里先亮着「正在分析…」，
    // 然后在 1.5 秒后缩到角上 —— 打包大转储可能要好几秒，不能卡住动画。
    let (tx, rx) = mpsc::channel::<Result<PathBuf, String>>();
    let for_worker = report.clone();
    std::thread::spawn(move || {
        let _ = tx.send(collect(&for_worker));
    });

    std::thread::sleep(INTRO_HOLD);

    // 「启动分析后，页面自动退出，优雅缩小到系统右下角」
    //
    // 缩角放在**修复之前**：修复（尤其 sfc）可能跑十几分钟，
    // 大卡片挡在教室屏幕中间是不能接受的；缩到角上就不碍事了。
    //
    // 修复关掉时直接进「正在发送」：这时候不该让界面写「正在尝试修复」——
    // 老师在设置里明明关了，看到修复字样只会以为关了个寂寞。
    if settings.repair {
        emit(&app, "repairing", "正在准备修复…");
    } else {
        emit(&app, "sending", "");
    }
    // 「正在发送」是什么时候亮起来的：修复关掉时是现在（打包+上传都在这个状态下），
    // 修复开着时是修完之后（下面那处 emit）。
    let mut sending_at = if settings.repair {
        None
    } else {
        Some(std::time::Instant::now())
    };
    shrink_to_corner(&app);

    let dir = match rx.recv_timeout(Duration::from_secs(120)) {
        Ok(Ok(dir)) => dir,
        Ok(Err(e)) => {
            log::warn!("蓝屏分析没完成：{e}");
            emit(&app, "failed", &format!("分析没能完成：{e}"));
            std::thread::sleep(OUTRO_HOLD + Duration::from_millis(1200));
            app.exit(0);
            return;
        }
        Err(_) => {
            log::warn!("蓝屏分析超时");
            emit(&app, "failed", "分析没能完成：超时");
            std::thread::sleep(OUTRO_HOLD + Duration::from_millis(1200));
            app.exit(0);
            return;
        }
    };

    // 修复。演示模式在 repair 内部直接跳过 —— 一次演示测试
    // 不该去动一台老师的机器。
    if settings.repair {
        let a = app.clone();
        let outcome =
            crate::crash_repair::run(&dir, report.demo, &mut |msg| emit(&a, "repairing", msg));
        log::info!("修复阶段结束：{}", outcome["verdict"]);
    }

    emit(&app, "sending", "");
    // 上传成功时「正在发送」可能只闪几十毫秒（局域网服务器）——
    // 那就等于老师根本没看见这句话。给它留一段最短可读时间。
    if sending_at.is_none() {
        sending_at = Some(std::time::Instant::now());
    }
    let sending_at = sending_at.unwrap();

    let receipt = match finish(&report, &dir) {
        Ok(r) => r,
        Err(e) => {
            log::warn!("蓝屏报告没能打包：{e}");
            emit(&app, "failed", &format!("报告没能打包：{e}"));
            std::thread::sleep(OUTRO_HOLD + Duration::from_millis(1200));
            app.exit(0);
            return;
        }
    };

    let endpoint = load_settings().endpoint.trim().to_string();
    if endpoint.is_empty() {
        hold_phase(sending_at, 1200);
        emit(&app, "local", "没有配置上报服务器，报告已保存在本机");
        log::info!("报告已保存在本地：{}", receipt.zip.display());
        std::thread::sleep(OUTRO_HOLD + Duration::from_millis(600));
    } else {
        match upload(&receipt, &report) {
            Ok(()) => {
                log::info!("蓝屏报告已发送：{}", receipt.id);
                hold_phase(sending_at, 1200);
                emit(&app, "sent", "");
                let _ = std::fs::remove_file(&receipt.zip);
                let _ = std::fs::remove_file(receipt.zip.with_extension("json"));
                std::thread::sleep(OUTRO_HOLD);
            }
            Err(e) => {
                log::warn!("蓝屏报告发送失败（留着下次再试）：{e}");
                emit(&app, "failed", &e);
                std::thread::sleep(OUTRO_HOLD + Duration::from_millis(1500));
            }
        }
    }

    app.exit(0);
}

/// 「正在发送」至少亮这么久，免得局域网服务器上它一闪而过。
fn hold_phase(started: std::time::Instant, min_ms: u64) {
    let min = Duration::from_millis(min_ms);
    let spent = started.elapsed();
    if spent < min {
        std::thread::sleep(min - spent);
    }
}

/// 从当前大小/位置，优雅地收到工作区右下角。
///
/// 用三次缓出（ease-out cubic）：起步快、收尾慢，观感上像是「滑过去停住」，
/// 而不是线性平移那种机械感。
fn shrink_to_corner(app: &AppHandle) {
    let Some(win) = app.get_webview_window("crash") else {
        return;
    };
    let Ok(Some(monitor)) = win.current_monitor() else {
        return;
    };
    let scale = monitor.scale_factor().max(1.0);
    let area = monitor.work_area();

    let target_w = (360.0 * scale).round() as u32;
    let target_h = (116.0 * scale).round() as u32;
    let margin = (18.0 * scale).round() as i32;
    let target_x = area.position.x + area.size.width as i32 - target_w as i32 - margin;
    let target_y = area.position.y + area.size.height as i32 - target_h as i32 - margin;

    let start_pos = win.outer_position().unwrap_or(tauri::PhysicalPosition::new(0, 0));
    let start_size = win
        .outer_size()
        .unwrap_or(tauri::PhysicalSize::new(target_w, target_h));

    const FRAMES: u32 = 26; // 26 × 16ms ≈ 420ms
    for i in 1..=FRAMES {
        let t = i as f64 / FRAMES as f64;
        let e = 1.0 - (1.0 - t).powi(3);
        let w = lerp_u32(start_size.width, target_w, e);
        let h = lerp_u32(start_size.height, target_h, e);
        let x = lerp_i32(start_pos.x, target_x, e);
        let y = lerp_i32(start_pos.y, target_y, e);
        let _ = win.set_size(tauri::PhysicalSize::new(w, h));
        let _ = win.set_position(tauri::PhysicalPosition::new(x, y));
        std::thread::sleep(Duration::from_millis(16));
    }
    log::info!(
        "诊断窗口已收到右下角：{}×{} @ ({target_x}, {target_y})",
        target_w,
        target_h
    );
}

fn lerp_u32(a: u32, b: u32, t: f64) -> u32 {
    (a as f64 + (b as f64 - a as f64) * t).round() as u32
}

fn lerp_i32(a: i32, b: i32, t: f64) -> i32 {
    (a as f64 + (b as f64 - a as f64) * t).round() as i32
}

/* ------------------------------------------------------------------ */
/* 设置页用的接口                                                      */
/* ------------------------------------------------------------------ */

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub enabled: bool,
    /// 蓝屏后自动尝试修复。
    pub repair: bool,
    pub endpoint: String,
    pub dir: String,
    /// 还没送出去的包有几个。
    pub pending: usize,
    /// 当前进程有没有管理员权限。设置页据此解释「为什么这次没修」。
    pub admin: bool,
}

pub fn view() -> View {
    let s = load_settings();
    let pending = std::fs::read_dir(root_dir().join("outbox"))
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("zip"))
                .count()
        })
        .unwrap_or(0);
    View {
        enabled: s.enabled,
        repair: s.repair,
        endpoint: s.endpoint,
        dir: root_dir().to_string_lossy().to_string(),
        pending,
        admin: crate::crash_repair::is_admin(),
    }
}

/* ------------------------------------------------------------------ */
/* 时间                                                                */
/* ------------------------------------------------------------------ */

pub fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `20261024-083012`：按时间排序、又能一眼看出是什么时候。
fn stamp(epoch: u64) -> String {
    let (y, mo, d, h, mi, s) = civil(epoch);
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// `2026-10-24T08:30:12Z`。`crash_repair` 写日志时间戳也用它。
pub(crate) fn iso_utc(epoch: u64) -> String {
    let (y, mo, d, h, mi, s) = civil(epoch);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// 把 Unix 秒拆成 UTC 年月日时分秒（不引 chrono：只需要这一个方向）。
fn civil(epoch: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (epoch / 86400) as i64;
    let secs = (epoch % 86400) as u32;
    let (y, m, d) = civil_from_days(days);
    (y, m, d, secs / 3600, (secs % 3600) / 60, secs % 60)
}

/// Howard Hinnant 的 `civil_from_days`。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 解析 `2026-10-24T07:12:33.1234567Z`（wevtutil 的 SystemTime）。
fn iso_to_epoch(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |from: usize, to: usize| -> i64 {
        s.get(from..to)
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0)
    };
    let y = num(0, 4);
    let mo = num(5, 7);
    let d = num(8, 10);
    let h = num(11, 13);
    let mi = num(14, 16);
    let sec = num(17, 19);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let days = days_from_civil(y, mo as u32, d as u32);
    Some((days * 86400 + h * 3600 + mi * 60 + sec).max(0) as u64)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe as i64 - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_and_epoch_round_trip() {
        for epoch in [0u64, 1_700_000_000, 1_790_000_000, now_epoch()] {
            let iso = iso_utc(epoch);
            assert_eq!(iso_to_epoch(&iso), Some(epoch), "{iso}");
        }
    }

    #[test]
    fn wevtutil_xml_is_parsed() {
        let xml = r#"
        <Event xmlns="http://schemas.microsoft.com/win/2004/08/events/event">
          <System>
            <Provider Name="Microsoft-Windows-WER-SystemErrorReporting" Guid="{x}"/>
            <EventID>1001</EventID>
            <TimeCreated SystemTime="2026-10-24T07:12:33.1234567Z"/>
          </System>
          <EventData>
            <Data Name="param1">0x000000ef</Data>
            <Data Name="param2">0xffffc000</Data>
            <Data Name="param3">0x0</Data>
            <Data Name="param4">0x0</Data>
          </EventData>
        </Event>"#;
        let events = parse_bugcheck_xml(xml);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code, "0x000000ef");
        assert_eq!(events[0].params.len(), 3);
        assert_eq!(
            events[0].epoch,
            iso_to_epoch("2026-10-24T07:12:33.1234567Z").unwrap()
        );
    }

    #[test]
    fn empty_output_yields_no_events() {
        assert!(parse_bugcheck_xml("").is_empty());
    }

    #[test]
    fn demo_report_is_marked_as_demo() {
        let r = detect(Mode::Demo).expect("演示模式总该有报告");
        assert!(r.demo);
        assert!(r.id.starts_with("demo-"));
    }
}
