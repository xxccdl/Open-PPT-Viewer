//! 远程协助。
//!
//! # 这个功能在解决什么
//!
//! 老师的机器坏了，最难受的是「说不清楚」：远程电话里教他点哪儿，
//! 说十遍也不如自己看一眼屏幕。这个模块让那台机器**主动每隔几秒连一次服务器**，
//! 管理员在 [crash.xxccdl.cn](https://crash.xxccdl.cn) 后台就能看到它在不在线、
//! 看它的屏幕、在它上面跑一条命令。
//!
//! # 谁在控制
//!
//! 方向的全部主动权在这台机器手上：它出去拉（poll），服务器**不可能主动连进来**
//! （学校没有公网 IP，也没开任何端口）。拉回来的东西只有两种：
//!
//! - 一条要执行的命令（`cmd.exe` 或鼠标键盘动作），管理员在后台发的；
//! - 「把屏幕推上来」的开关。
//!
//! 没有管理员账号就发不出任何命令；设置页把开关关掉，这个循环连心跳都不发，
//! 服务器那边只剩一条过期的记录。
//!
//! # 边界
//!
//! - 命令只以**当前进程的权限**执行：应用自己是普通用户，就只能做普通用户的事。
//!   要「以管理员身份重装某个服务」，得把那台机器上的应用以管理员身份运行。
//! - 每条命令都有超时，输出截断到 60 KB，结果随下一次心跳回传；
//! - 屏幕只在后台**明确打开**时才推，且缩到 1920 宽、JPEG 质量 72 ——
//!   机房上行通常只有几 Mbps，不能再宽了；
//! - 断网、服务器不可达时不重试轰炸：睡一会儿再试一次，一切失败都只是下次重来。

use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// 后台没在看屏幕时，多久问一次服务器「有活儿吗」。
///
/// 3 秒是权衡：报到得够勤，管理员点开后台就能看到机器在线；
/// 又不至于让上百台机器把一条上行占满。
const POLL_IDLE: Duration = Duration::from_secs(3);
/// 屏幕打开时：抓图 + 上传一轮，再问一次。
const POLL_SCREEN: Duration = Duration::from_millis(900);
/// 连不上服务器之后歇多久。别在断网时每秒撞一次。
const RETRY_OFFLINE: Duration = Duration::from_secs(12);
/// 单次 HTTP 的超时。
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// 一条 `cmd.exe` 命令最多跑多久。
const SHELL_TIMEOUT: Duration = Duration::from_secs(30);
/// 命令输出最多带回去多少字节。
const MAX_OUT: usize = 60 * 1024;
/// 截图最宽多少像素（超出的按整数倍抽样缩小）。
const MAX_FRAME_W: i32 = 1920;

/* ------------------------------------------------------------------ */
/* 协议                                                                */
/* ------------------------------------------------------------------ */

#[derive(Debug, Deserialize)]
struct PollReply {
    #[serde(default)]
    commands: Vec<CommandItem>,
    #[serde(default)]
    screen: bool,
}

#[derive(Debug, Deserialize)]
struct CommandItem {
    id: String,
    kind: String,
    #[serde(default)]
    arg: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ResultItem {
    id: String,
    ok: bool,
    out: String,
    ms: u64,
}

/// 设置里那个上报地址（`…/api/v1/crash`）→ 远程接口的根（`…/api/v1/remote`）。
///
/// 两个接口挂在同一个服务上，路径只差最后一段；不写死域名是因为
/// 学校完全可以把整套服务搬到自己的服务器上（见设置页的「上报服务器」）。
pub(crate) fn api_base(endpoint: &str) -> String {
    let e = endpoint.trim().trim_end_matches('/');
    e.strip_suffix("/crash").unwrap_or(e).to_string()
}

fn remote_base(endpoint: &str) -> String {
    format!("{}/remote", api_base(endpoint))
}

/* ------------------------------------------------------------------ */
/* 主循环                                                              */
/* ------------------------------------------------------------------ */
/// 应用正常启动后调一次。没开远程协助时，这条线程自己睡过去，不联网。
pub fn spawn() {
    std::thread::spawn(run);
}

fn run() {
    // 开机那几秒系统还在忙（杀毒、索引），晚一点再出门
    std::thread::sleep(Duration::from_secs(3));

    let client = match reqwest::blocking::Client::builder()
        .user_agent(format!("OpenPPTView/{}", env!("CARGO_PKG_VERSION")))
        .timeout(HTTP_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            log::warn!("远程协助：建不出 HTTP 连接（{e}），这一轮不启用");
            return;
        }
    };

    let mut seq: u64 = 0;
    loop {
        // 每一轮都重新读设置：老师在设置页点「关闭」，几秒内就真的断开
        let settings = crate::crash::load_settings();
        if !settings.remote {
            std::thread::sleep(Duration::from_secs(8));
            continue;
        }
        // 没登录班级的机器不该出现在任何人的远程列表里：连心跳都不发
        if settings.machine.is_none() {
            std::thread::sleep(Duration::from_secs(8));
            continue;
        }
        let base = remote_base(&settings.endpoint);
        let me = identity();

        let screen = match poll_once(&client, &base, &me) {
            Ok(reply) => {
                let started = Instant::now();
                let results: Vec<ResultItem> = reply
                    .commands
                    .iter()
                    .map(|c| {
                        let t = Instant::now();
                        let out = run_command(c);
                        ResultItem {
                            id: c.id.clone(),
                            ok: out.0,
                            out: out.1,
                            ms: t.elapsed().as_millis() as u64,
                        }
                    })
                    .collect();
                if !results.is_empty() {
                    // 回传失败也不要紧：服务器下次会重新发（它自己会过期），
                    // 反正管理员看得到「发出去但没回音」，比悄悄吞掉强
                    let _ = post_results(&client, &base, &me.hwid, &results);
                }
                if reply.screen {
                    if let Some((jpeg, w, h)) = capture_jpeg() {
                        seq += 1;
                        let _ = post_frame(&client, &base, &me.hwid, seq, w, h, jpeg);
                    }
                }
                log::debug!(
                    "远程协助：{} 条命令，{} 毫秒；屏幕{}",
                    results.len(),
                    started.elapsed().as_millis(),
                    if reply.screen { "开着" } else { "关着" }
                );
                reply.screen
            }
            Err(e) => {
                log::debug!("远程协助：连不上服务器（{e}），稍后再试");
                std::thread::sleep(RETRY_OFFLINE);
                false
            }
        };

        std::thread::sleep(if screen { POLL_SCREEN } else { POLL_IDLE });
    }
}

/* ------------------------------------------------------------------ */
/* 这台机器是谁                                                        */
/* ------------------------------------------------------------------ */

struct Identity {
    hwid: String,
    name: String,
    user: String,
    admin: bool,
}

fn identity() -> Identity {
    Identity {
        hwid: crate::crash::hwid(),
        name: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "未命名".into()),
        user: std::env::var("USERNAME").unwrap_or_else(|_| "未知用户".into()),
        admin: crate::crash_repair::is_admin(),
    }
}

/* ------------------------------------------------------------------ */
/* HTTP                                                                */
/* ------------------------------------------------------------------ */

fn with_token(req: reqwest::blocking::RequestBuilder) -> reqwest::blocking::RequestBuilder {
    // 优先用这台机器登录班级后拿到的那枚令牌（服务端靠它认出「这台机器归哪个班」）；
    // 没有才退回运维用的环境变量令牌。
    if let Some(link) = crate::crash::machine() {
        if !link.token.trim().is_empty() {
            return req.bearer_auth(link.token.trim());
        }
    }
    match std::env::var("OPENPPTVIEW_CRASH_TOKEN") {
        Ok(t) if !t.trim().is_empty() => req.bearer_auth(t.trim()),
        _ => req,
    }
}

fn poll_once(
    client: &reqwest::blocking::Client,
    base: &str,
    me: &Identity,
) -> Result<PollReply, String> {
    let body = serde_json::json!({
        "hwid": me.hwid,
        "name": me.name,
        "user": me.user,
        "admin": me.admin,
        "version": env!("CARGO_PKG_VERSION"),
    });
    let req = with_token(client.post(format!("{base}/poll"))).json(&body);
    let resp = req.send().map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("服务器返回 {}", resp.status()));
    }
    resp.json::<PollReply>().map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct ResultsBody<'a> {
    hwid: &'a str,
    results: &'a [ResultItem],
}

fn post_results(
    client: &reqwest::blocking::Client,
    base: &str,
    hwid: &str,
    results: &[ResultItem],
) -> Result<(), String> {
    let body = ResultsBody { hwid, results };
    let req = with_token(client.post(format!("{base}/result"))).json(&body);
    let resp = req.send().map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("服务器返回 {}", resp.status()));
    }
    Ok(())
}

fn post_frame(
    client: &reqwest::blocking::Client,
    base: &str,
    hwid: &str,
    seq: u64,
    w: u32,
    h: u32,
    jpeg: Vec<u8>,
) -> Result<(), String> {
    let req = with_token(client.post(format!("{base}/frame")))
        .header("Content-Type", "image/jpeg")
        .header("X-Hwid", hwid)
        .header("X-Frame-Seq", seq.to_string())
        .header("X-Frame-W", w.to_string())
        .header("X-Frame-H", h.to_string())
        .body(jpeg);
    let resp = req.send().map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("服务器返回 {}", resp.status()));
    }
    Ok(())
}

/* ------------------------------------------------------------------ */
/* 执行一条命令                                                        */
/* ------------------------------------------------------------------ */

/// 返回 `(是否成功, 输出)`。
fn run_command(c: &CommandItem) -> (bool, String) {
    match c.kind.as_str() {
        "shell" => run_shell(&c.arg),
        "mouse" => apply_mouse(&c.arg),
        "key" => apply_key(&c.arg),
        other => (false, format!("不认识的命令类型：{other}")),
    }
}

/// 跑一条 `cmd.exe` 命令。
///
/// 刻意不用 `powershell`：它启动要一两秒、还会被组策略拦；`cmd` 必定在。
fn run_shell(text: &str) -> (bool, String) {
    let text = text.trim();
    if text.is_empty() {
        return (false, "命令是空的".into());
    }
    let mut cmd = Command::new("cmd.exe");
    cmd.args(["/C", text])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // 不给老师看到一闪而过的黑窗口
        .creation_flags(0x0800_0000);

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (false, format!("起不来 cmd.exe：{e}")),
    };
    let pid = child.id();

    // `wait_with_output` 会把 stdout 读干净再返回，但它不接受超时，
    // 所以放到一条线程里跑，这边用通道等 —— 超时就把整棵进程树杀掉。
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });

    let output = match rx.recv_timeout(SHELL_TIMEOUT) {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return (false, format!("读不到输出：{e}")),
        Err(_) => {
            kill_tree(pid);
            match rx.recv_timeout(Duration::from_secs(3)) {
                Ok(Ok(o)) => {
                    return (false, clip(&format!("{}\n（超过 30 秒，已强制结束）", join(&o))));
                }
                _ => return (false, "超过 30 秒没有输出，已强制结束".into()),
            }
        }
    };
    let mut all = String::new();
    all.push_str(&String::from_utf8_lossy(&output.stdout));
    let err = String::from_utf8_lossy(&output.stderr);
    if !err.trim().is_empty() {
        all.push_str(&err);
    }
    (output.status.success(), clip(&all))
}

fn join(o: &std::process::Output) -> String {
    let mut s = String::from_utf8_lossy(&o.stdout).to_string();
    s.push_str(&String::from_utf8_lossy(&o.stderr));
    s
}

/// 输出太长只留前面一段 —— 管理员要的是「命令怎么报错的」，通常在最前几行。
fn clip(s: &str) -> String {
    if s.len() <= MAX_OUT {
        return s.to_string();
    }
    let cut = s
        .char_indices()
        .take_while(|(i, _)| *i < MAX_OUT)
        .last()
        .map(|(i, _)| i)
        .unwrap_or(0);
    format!("{}\n…（还有更多输出，已截断）", &s[..cut])
}

fn kill_tree(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x0800_0000)
        .status();
}

/* ------------------------------------------------------------------ */
/* 鼠标与键盘                                                          */
/* ------------------------------------------------------------------ */

/// `{"x":0.5,"y":0.3,"a":"move|left|right|double|wheel","dy":-3}`
///
/// 坐标是 0～1 的比例而不是像素：管理员那边看到的画面是缩放过的，
/// 用比例两端才对得上，也不怕两台机器的分辨率不同。
#[derive(Deserialize)]
struct MouseArg {
    x: f64,
    y: f64,
    #[serde(default = "default_action")]
    a: String,
    #[serde(default)]
    dy: i32,
}

fn default_action() -> String {
    "move".into()
}

fn apply_mouse(raw: &str) -> (bool, String) {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            mouse_event, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_RIGHTDOWN,
            MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL,
        };
        use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetSystemMetrics, SetCursorPos, SM_CXSCREEN, SM_CYSCREEN};

        let m: MouseArg = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(e) => return (false, format!("鼠标参数不是合法 JSON：{e}")),
        };
        unsafe {
            let w = GetSystemMetrics(SM_CXSCREEN).max(1);
            let h = GetSystemMetrics(SM_CYSCREEN).max(1);
            let x = ((m.x.clamp(0.0, 1.0)) * f64::from(w)).round() as i32;
            let y = ((m.y.clamp(0.0, 1.0)) * f64::from(h)).round() as i32;
            if SetCursorPos(x, y).is_err() {
                return (false, "移动鼠标失败".into());
            }
            std::thread::sleep(Duration::from_millis(40));
            match m.a.as_str() {
                "move" => {}
                "left" => {
                    mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0);
                    std::thread::sleep(Duration::from_millis(60));
                    mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0);
                }
                "double" => {
                    for _ in 0..2 {
                        mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0);
                        std::thread::sleep(Duration::from_millis(60));
                        mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0);
                        std::thread::sleep(Duration::from_millis(90));
                    }
                }
                "right" => {
                    mouse_event(MOUSEEVENTF_RIGHTDOWN, 0, 0, 0, 0);
                    std::thread::sleep(Duration::from_millis(60));
                    mouse_event(MOUSEEVENTF_RIGHTUP, 0, 0, 0, 0);
                }
                "wheel" => {
                    let dy = if m.dy.abs() > 0 { m.dy } else { 1 };
                    mouse_event(MOUSEEVENTF_WHEEL, 0, 0, dy * 120, 0);
                }
                other => return (false, format!("不认识的鼠标动作：{other}")),
            }
            // 顺手把当前位置带回去：管理员能看出这一下点到了哪儿
            let mut p = POINT::default();
            let at = match GetCursorPos(&mut p) {
                Ok(()) => format!("{}x{}", p.x, p.y),
                Err(_) => format!("{x}x{y}"),
            };
            (true, format!("{} @ {at}", m.a))
        }
    }
    #[cfg(not(windows))]
    {
        let _ = raw;
        (false, "只有 Windows 支持远程鼠标".into())
    }
}

/// `{"keys":["ctrl","alt","del"]}` 或 `{"text":"dir"}`。
///
/// 两种都支持：组合键用来操作界面，`text` 用来往输入框里打字
/// （走 `KEYEVENTF_UNICODE`，中文也能直接打进活动窗口）。
#[derive(Deserialize)]
struct KeyArg {
    #[serde(default)]
    keys: Vec<String>,
    #[serde(default)]
    text: String,
}

fn apply_key(raw: &str) -> (bool, String) {
    #[cfg(windows)]
    {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            keybd_event, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
            KEYEVENTF_UNICODE, VIRTUAL_KEY,
        };

        let k: KeyArg = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(e) => return (false, format!("按键参数不是合法 JSON：{e}")),
        };
        let mut notes = Vec::new();

        if !k.keys.is_empty() {
            let mut codes = Vec::new();
            for name in &k.keys {
                match vk_of(name) {
                    Some(v) => codes.push(v),
                    None => return (false, format!("不认识的键：{name}")),
                }
            }
            unsafe {
                for &vk in &codes {
                    keybd_event(vk.0 as u8, 0, Default::default(), 0);
                }
                std::thread::sleep(Duration::from_millis(40));
                for &vk in codes.iter().rev() {
                    keybd_event(vk.0 as u8, 0, KEYEVENTF_KEYUP, 0);
                }
            }
            notes.push(format!("按了 {}", k.keys.join("+")));
        }

        if !k.text.is_empty() {
            // 一次 SendInput 带一整串字符，比逐字符快得多，打字机效果也更顺
            let mut inputs = Vec::new();
            for ch in k.text.chars().take(2000) {
                let code = ch as u32;
                // 基本多文种平面以外的字符（emoji）要拆成两半，这里不处理：
                // 管理员往输入框里打的字，几乎不会超出 BMP
                if code > 0xFFFF {
                    continue;
                }
                let scan = code as u16;
                for up in [false, true] {
                    let flags = if up {
                        KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
                    } else {
                        KEYEVENTF_UNICODE
                    };
                    inputs.push(INPUT {
                        r#type: INPUT_KEYBOARD,
                        Anonymous: INPUT_0 {
                            ki: KEYBDINPUT {
                                wVk: VIRTUAL_KEY(0),
                                wScan: scan,
                                dwFlags: flags,
                                time: 0,
                                dwExtraInfo: 0,
                            },
                        },
                    });
                }
            }
            if !inputs.is_empty() {
                unsafe {
                    SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
                }
            }
            notes.push(format!("输入了 {} 个字", k.text.chars().count()));
        }

        if notes.is_empty() {
            return (false, "按键参数是空的".into());
        }
        (true, notes.join("；"))
    }
    #[cfg(not(windows))]
    {
        let _ = raw;
        (false, "只有 Windows 支持远程按键".into())
    }
}

#[cfg(windows)]
fn vk_of(name: &str) -> Option<windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY> {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    let n = name.trim().to_lowercase();
    let vk = match n.as_str() {
        "enter" | "return" => VK_RETURN,
        "esc" | "escape" => VK_ESCAPE,
        "tab" => VK_TAB,
        "space" => VK_SPACE,
        "backspace" | "back" => VK_BACK,
        "delete" | "del" => VK_DELETE,
        "up" => VK_UP,
        "down" => VK_DOWN,
        "left" => VK_LEFT,
        "right" => VK_RIGHT,
        "home" => VK_HOME,
        "end" => VK_END,
        "ctrl" => VK_CONTROL,
        "shift" => VK_SHIFT,
        "alt" => VK_MENU,
        "win" | "windows" => VK_LWIN,
        "f1" => VK_F1,
        "f2" => VK_F2,
        "f3" => VK_F3,
        "f4" => VK_F4,
        "f5" => VK_F5,
        "f6" => VK_F6,
        "f7" => VK_F7,
        "f8" => VK_F8,
        "f9" => VK_F9,
        "f10" => VK_F10,
        "f11" => VK_F11,
        "f12" => VK_F12,
        other => {
            let mut chars = other.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            match c {
                'a'..='z' => VIRTUAL_KEY(c.to_ascii_uppercase() as u16),
                '0'..='9' => VIRTUAL_KEY(c as u16),
                _ => return None,
            }
        }
    };
    Some(vk)
}

/* ------------------------------------------------------------------ */
/* 截屏                                                                */
/* ------------------------------------------------------------------ */

/// 抓一张当前桌面，缩到最多 1920 宽，编成 JPEG。
fn capture_jpeg() -> Option<(Vec<u8>, u32, u32)> {
    #[cfg(windows)]
    {
        use windows::Win32::Graphics::Gdi::{
            BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
            GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BI_RGB, DIB_RGB_COLORS, SRCCOPY,
        };
        use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

        let (w, h) = unsafe {
            (
                GetSystemMetrics(SM_CXSCREEN),
                GetSystemMetrics(SM_CYSCREEN),
            )
        };
        if w <= 0 || h <= 0 {
            return None;
        }

        // 缩放：只按整数倍抽样，不引入模糊 —— 管理员要看清界面上的字
        let step = ((w + MAX_FRAME_W - 1) / MAX_FRAME_W).max(1);
        let ow = (w / step) as u32;
        let oh = (h / step) as u32;

        let mut raw: Vec<u8> = Vec::new();
        unsafe {
            let screen = GetDC(None);
            if screen.is_invalid() {
                return None;
            }
            let mem = CreateCompatibleDC(Some(screen));
            if mem.is_invalid() {
                return None;
            }
            let bmp = CreateCompatibleBitmap(screen, w, h);
            if bmp.is_invalid() {
                let _ = DeleteDC(mem);
                return None;
            }
            let old = SelectObject(mem, bmp.into());
            let copied = BitBlt(mem, 0, 0, w, h, Some(screen), 0, 0, SRCCOPY).is_ok();

            let mut bi: BITMAPINFO = std::mem::zeroed();
            bi.bmiHeader.biSize = std::mem::size_of_val(&bi.bmiHeader) as u32;
            bi.bmiHeader.biWidth = w;
            // 负高度 = 自上而下，这样第 0 行就是屏幕顶部，省得再翻一遍
            bi.bmiHeader.biHeight = -h;
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            bi.bmiHeader.biCompression = BI_RGB.0;

            let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
            let lines = GetDIBits(
                mem,
                bmp,
                0,
                h as u32,
                Some(buf.as_mut_ptr().cast()),
                &mut bi,
                DIB_RGB_COLORS,
            );

            SelectObject(mem, old);
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);

            if !copied || lines <= 0 {
                return None;
            }

            // BGRA → RGB，同时按 step 抽样
            raw.reserve((ow as usize) * (oh as usize) * 3);
            for oy in 0..oh as usize {
                let sy = oy * step as usize;
                let row = sy * w as usize * 4;
                for ox in 0..ow as usize {
                    let sx = ox * step as usize;
                    let i = row + sx * 4;
                    raw.push(buf[i + 2]);
                    raw.push(buf[i + 1]);
                    raw.push(buf[i]);
                }
            }
        }

        let mut jpeg = Vec::new();
        use image::ImageEncoder;
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 72);
        if enc
            .write_image(&raw, ow, oh, image::ExtendedColorType::Rgb8)
            .is_err()
        {
            return None;
        }
        Some((jpeg, ow, oh))
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/* ------------------------------------------------------------------ */
/* 测试                                                                */
/* ------------------------------------------------------------------ */

#[cfg(test)]
mod tests {
    use super::*;

    /// 默认地址要能推出远程接口的根：两个接口挂在同一个服务上。
    #[test]
    fn base_of_default_endpoint() {
        assert_eq!(
            remote_base("https://xxccdl.cn/crash/api/v1/crash"),
            "https://xxccdl.cn/crash/api/v1/remote"
        );
    }

    /// 自己搭的服务通常只填到 `…/api/v1/crash`，尾斜杠不该影响结果。
    #[test]
    fn trims_and_keeps_custom_host() {
        assert_eq!(
            remote_base("https://school.local/oppv/api/v1/crash/"),
            "https://school.local/oppv/api/v1/remote"
        );
        assert_eq!(
            remote_base("http://10.0.0.5:8787/api/v1"),
            "http://10.0.0.5:8787/api/v1/remote"
        );
    }

    /// 输出截断不按字节切：中文被切一半会变成乱码。
    #[test]
    fn clip_keeps_utf8_whole() {
        let long = "蓝屏".repeat(40_000);
        let cut = clip(&long);
        assert!(cut.len() <= MAX_OUT + 64);
        assert!(cut.is_char_boundary(cut.len()));
        assert!(cut.ends_with("（还有更多输出，已截断）"));
    }

    #[test]
    fn mouse_arg_parses() {
        let m: MouseArg = serde_json::from_str(r#"{"x":0.5,"y":0.25}"#).unwrap();
        assert_eq!(m.a, "move");
        assert_eq!(m.x, 0.5);
    }
}
