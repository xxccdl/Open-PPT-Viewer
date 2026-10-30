//! 「这台电脑属于哪个班」——浏览器授权，本机回传。
//!
//! # 为什么不是邀请码
//!
//! 邀请码难在两头：贴在墙上会被人抄走，一次性码在机房几十台机器上发到手软。
//! 改成「在浏览器登录 → 点一下授权」，权限天然落在「你已经是这个班的人」上，
//! 而且不用在教室里传任何秘密。
//!
//! # 流程
//!
//! 1. 在设置页点「登录」，这里起一个 `127.0.0.1:随机端口` 的临时服务；
//! 2. 用系统浏览器打开 `…/crash/authorize?port=…&state=…`；
//! 3. 老师在网页里选班级、点「授权这台电脑」，网页 302 回 `http://127.0.0.1:<port>/callback`；
//! 4. 本机拿到一次性 `code`，换成长期机器令牌，写进 `settings.json`。
//!
//! # 安全
//!
//! 本地端口是对本机所有进程开放的：别的网页可以往 `/callback` 塞任意 `code`。
//! 所以 `state` 必须逐字比对 —— 不知道 `state` 的页面塞不进来。
//! 授权码本身 60 秒过期、只能用一次，就算被截获也换不了第二次。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::crash::MachineLink;

/// 等浏览器回传最多这么久。老师可能还要先注册账号，给足时间。
const WAIT: Duration = Duration::from_secs(180);
/// 回传地址的路径。
const CALLBACK_PATH: &str = "/callback";

/// 正在等浏览器？（前端的「登录」按钮据此显示进度，也据此变成「取消」）
static PENDING: AtomicBool = AtomicBool::new(false);
/// 第几次授权。
///
/// 取消、或者再点一次「登录」，都只是把这个数加一：等浏览器的那条线程
/// 发现自己的编号过期了就自己收摊。这样不用去关它的端口 ——
/// 端口在别人手里时（浏览器正连着）关掉反而容易卡住。
static GEN: AtomicU64 = AtomicU64::new(0);
/// 最近一次授权失败的原因（成功 / 取消时清空）。
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub linked: bool,
    pub pending: bool,
    pub school: String,
    pub class: String,
    pub linked_at: String,
    pub error: String,
}

pub fn status() -> Status {
    let link = crate::crash::machine();
    let error = LAST_ERROR
        .lock()
        .ok()
        .and_then(|e| e.clone())
        .unwrap_or_default();
    match link {
        Some(l) => Status {
            linked: true,
            pending: PENDING.load(Ordering::SeqCst),
            school: l.school_name,
            class: l.class_name,
            linked_at: l.linked_at,
            error,
        },
        None => Status {
            linked: false,
            pending: PENDING.load(Ordering::SeqCst),
            school: String::new(),
            class: String::new(),
            linked_at: String::new(),
            error,
        },
    }
}

/// 开始一次登录：起本地监听、打开浏览器，返回要打开的那个地址。
///
/// 立刻返回（不能在这里等浏览器）：设置页点完按钮要马上看到反应。
pub fn login() -> Result<String, String> {
    // 再点一次「登录」= 重新来一次：先把上一次的等待作废（不需要先去取消）
    let my_gen = GEN.fetch_add(1, Ordering::SeqCst) + 1;
    if let Ok(mut e) = LAST_ERROR.lock() {
        *e = None;
    }

    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|e| format!("开不了本地端口：{e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("端口设置失败：{e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("读不到本地端口：{e}"))?
        .port();
    let state = random_hex(16);
    let endpoint = crate::crash::load_settings().endpoint;
    let url = authorize_url(&endpoint, port, &state);

    PENDING.store(true, Ordering::SeqCst);
    let for_thread = url.clone();
    std::thread::spawn(move || {
        let outcome = wait_callback(&listener, &state, WAIT, my_gen);
        // 被取消、或被更新的那一次顶替了：这一轮什么也不算，别去改任何状态
        if GEN.load(Ordering::SeqCst) != my_gen {
            log::info!("班级授权已取消（{for_thread}）");
            return;
        }
        match outcome {
            Ok(code) => match exchange(&endpoint, &code) {
                Ok(link) => {
                    if let Err(e) = crate::crash::set_machine(Some(link)) {
                        set_error(format!("保存授权失败：{e}"));
                    }
                }
                Err(e) => set_error(e),
            },
            Err(e) => set_error(e),
        }
        PENDING.store(false, Ordering::SeqCst);
        log::info!("班级授权结束（{for_thread}）");
    });

    // 交给系统浏览器：Tauri 自己的 webview 不适合承载第三方登录页
    if let Err(e) = crate::shell_execute(&url, None) {
        // 浏览器都没打开，这次授权已经没戏了：把等待收掉，
        // 否则「正在等」会一直挂着，下一次点「登录」又会被挡回去
        cancel();
        return Err(format!("打不开浏览器：{e}"));
    }
    Ok(url)
}

/// 取消正在等待的那次授权。返回是否真的取消了一个在等的。
pub fn cancel() -> bool {
    if !PENDING.load(Ordering::SeqCst) {
        return false;
    }
    // 编号一变，等浏览器的那条线程下一轮就自己退出（80ms 内）
    GEN.fetch_add(1, Ordering::SeqCst);
    PENDING.store(false, Ordering::SeqCst);
    if let Ok(mut e) = LAST_ERROR.lock() {
        *e = None;
    }
    log::info!("已取消班级授权");
    true
}

/// 退出登录：先在服务器撤销这枚令牌，再清本地。
///
/// 撤销失败也照样清本地 —— 老师要的是「这台机器现在不属于任何班」，
/// 服务器上那条记录留着也不会让它重新变成别人的机器。
pub fn logout() -> Result<(), String> {
    let Some(link) = crate::crash::machine() else {
        return Ok(());
    };
    let settings = crate::crash::load_settings();
    let base = crate::remote::api_base(&settings.endpoint);
    let client = reqwest::blocking::Client::builder()
        .user_agent(format!("OpenPPTView/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(12))
        .build()
        .map_err(|e| format!("建不出连接：{e}"))?;
    let resp = client
        .post(format!("{base}/machine/logout"))
        .bearer_auth(link.token.trim())
        .send();
    if let Err(e) = resp {
        log::warn!("服务器没有确认退出（{e}），本地照样清掉");
    }
    crate::crash::set_machine(None)
}

fn set_error(msg: String) {
    log::warn!("班级授权失败：{msg}");
    if let Ok(mut slot) = LAST_ERROR.lock() {
        *slot = Some(msg);
    }
}

/* ------------------------------------------------------------------ */
/* 本地监听                                                            */
/* ------------------------------------------------------------------ */

/// 等浏览器那条回调。超时 / state 不对 / 被取消都算失败。
fn wait_callback(
    listener: &TcpListener,
    state: &str,
    timeout: Duration,
    gen: u64,
) -> Result<String, String> {
    let deadline = Instant::now() + timeout;
    loop {
        // 被取消、或者又点了一次「登录」：这一轮立刻收摊
        if GEN.load(Ordering::SeqCst) != gen {
            return Err("已取消".into());
        }
        if Instant::now() >= deadline {
            return Err("等浏览器授权超时（3 分钟）。可以再点一次「登录」".into());
        }
        match listener.accept() {
            Ok((stream, _)) => {
                // 一次授权只处理一个连接：别的进程撞上来也不会把真正的回调吃掉 ——
                // 拿不到合法 state 的连接直接回 403，然后继续等下一个
                match handle_stream(stream, state) {
                    Ok(Some(code)) => return Ok(code),
                    Ok(None) => continue,
                    Err(e) => return Err(e),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(80));
            }
            Err(e) => return Err(format!("本地端口出错：{e}")),
        }
    }
}

/// 处理一条回调请求：校验 state，回一页「可以关掉了」，返回 code。
fn handle_stream(mut stream: TcpStream, state: &str) -> Result<Option<String>, String> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf).unwrap_or(0);
    let request = String::from_utf8_lossy(&buf[..n]).to_string();
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("");

    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    let pairs = parse_query(query);

    if !target.starts_with(CALLBACK_PATH) {
        respond(&mut stream, "404 Not Found", "<p>没有这个地址。</p>");
        return Ok(None);
    }
    let got_state = pairs.iter().find(|(k, _)| *k == "state").map(|(_, v)| v.clone()).unwrap_or_default();
    if !state_eq(&got_state, state) {
        respond(&mut stream, "403 Forbidden", "<p>授权状态不匹配，已拒绝。</p>");
        return Ok(None);
    }
    let Some((_, code)) = pairs.into_iter().find(|(k, _)| k == "code") else {
        respond(&mut stream, "400 Bad Request", "<p>缺少授权码。</p>");
        return Err("回调里没有授权码".into());
    };

    respond(
        &mut stream,
        "200 OK",
        "<h1>授权成功</h1><p>可以关闭这个页面了，OpenPPTView 会自动连上。</p>",
    );
    Ok(Some(code))
}

fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let html = format!(
        "<!DOCTYPE html><html lang=\"zh-CN\"><meta charset=\"utf-8\">\
         <body style=\"font-family:system-ui;padding:40px;background:#0a0c0f;color:#e9edf3\">{body}</body></html>"
    );
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        html.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(html.as_bytes());
    let _ = stream.flush();
}

/// `a=1&b=2` → `[("a","1"), ("b","2")]`（不做百分号解码：state 和 code 都是十六进制）。
fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (kv.to_string(), String::new()),
        })
        .collect()
}

/// 授权页地址：`…/api/v1/crash` → `…/authorize`。
pub fn authorize_url(endpoint: &str, port: u16, state: &str) -> String {
    let api = crate::remote::api_base(endpoint);
    let root = api.strip_suffix("/api/v1").unwrap_or(&api);
    format!("{root}/authorize?port={port}&state={state}")
}

/* ------------------------------------------------------------------ */
/* 换令牌                                                              */
/* ------------------------------------------------------------------ */

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExchangeReply {
    token: String,
    #[serde(default)]
    school_name: String,
    #[serde(default)]
    class_name: String,
}

fn exchange(endpoint: &str, code: &str) -> Result<MachineLink, String> {
    let base = crate::remote::api_base(endpoint);
    let client = reqwest::blocking::Client::builder()
        .user_agent(format!("OpenPPTView/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| format!("建不出连接：{e}"))?;
    let name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "未命名".into());
    let body = serde_json::json!({
        "code": code,
        "hwid": crate::crash::hwid(),
        "name": name,
        "version": env!("CARGO_PKG_VERSION"),
    });
    let resp = client
        .post(format!("{base}/machine/exchange"))
        .json(&body)
        .send()
        .map_err(|e| format!("连不上服务器：{e}"))?;
    if !resp.status().is_success() {
        let text = resp.text().unwrap_or_default();
        let short = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str().map(|s| s.to_string())))
            .unwrap_or_else(|| "服务器拒绝了这次授权".into());
        return Err(short);
    }
    let reply: ExchangeReply = resp.json().map_err(|e| format!("服务器返回看不懂：{e}"))?;
    Ok(MachineLink {
        token: reply.token,
        school_name: reply.school_name,
        class_name: reply.class_name,
        linked_at: crate::crash::iso_utc(crate::crash::now_epoch()),
    })
}

/* ------------------------------------------------------------------ */
/* 随机与比较                                                          */
/* ------------------------------------------------------------------ */

/// 32 位十六进制随机串。用系统 CSPRNG：`state` 是这整条流程唯一的防线。
fn random_hex(bytes: usize) -> String {
    #[cfg(windows)]
    {
        use windows::Win32::Security::Cryptography::{
            BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        };
        let mut buf = vec![0u8; bytes];
        let status = unsafe {
            BCryptGenRandom(None, &mut buf, BCRYPT_USE_SYSTEM_PREFERRED_RNG)
        };
        if status.is_ok() {
            return buf.iter().map(|b| format!("{b:02x}")).collect();
        }
    }
    // 兜底（或非 Windows）：时间 + 进程号。只在 CSPRNG 不可用时才走到这里
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seed = format!("{now}:{}:{}", std::process::id(), bytes);
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(seed.as_bytes());
    let digest = hasher.finalize();
    digest.iter().take(bytes).map(|b| format!("{b:02x}")).collect()
}

/// 逐字比较，不许省略 —— `state` 少一个字符也得拒绝。
fn state_eq(got: &str, want: &str) -> bool {
    !want.is_empty() && got.len() == want.len() && got.as_bytes() == want.as_bytes()
}

/* ------------------------------------------------------------------ */
/* 测试                                                                */
/* ------------------------------------------------------------------ */

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_points_at_the_page_not_the_api() {
        assert_eq!(
            authorize_url("https://xxccdl.cn/crash/api/v1/crash", 40001, "abc"),
            "https://xxccdl.cn/crash/authorize?port=40001&state=abc"
        );
        assert_eq!(
            authorize_url("https://school.local/api/v1", 40002, "def"),
            "https://school.local/authorize?port=40002&state=def"
        );
    }

    #[test]
    fn state_must_match_exactly() {
        assert!(state_eq("abc", "abc"));
        assert!(!state_eq("abc", "abcd"));
        assert!(!state_eq("", "abc"));
        assert!(!state_eq("abcdef", "ABCdef"));
    }

    #[test]
    fn query_is_parsed_without_decoding() {
        let pairs = parse_query("code=deadbeef&state=0123");
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0, "code");
        assert_eq!(pairs[0].1, "deadbeef");
        assert_eq!(pairs[1].1, "0123");
    }

    /// 没有在等的授权时点「取消」应当什么都不做（按钮偶尔点快了不该报错）。
    #[test]
    fn cancel_is_a_noop_when_nothing_is_pending() {
        assert!(!cancel());
    }

    #[test]
    fn random_hex_is_hex_and_long_enough() {
        let a = random_hex(16);
        let b = random_hex(16);
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "两次取值不应相同");
    }
}
