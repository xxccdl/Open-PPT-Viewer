//! 检查更新与一键升级。
//!
//! # 版本从哪来
//!
//! GitHub Releases（[`REPO`]）。取的是 `releases/latest`，比对版本号，
//! 认发布里那个安装包。
//!
//! # 为什么要测速挑线路
//!
//! 在国内直连 `github.com` 基本不可用，得走加速代理。而这类服务**生灭无常**：
//! 今天好用的明天 502，同一份文件在不同线路上的速度能差十倍以上，
//! 而且**我们事先不知道哪条能用**。
//!
//! 所以这里不写死一条线路，也不按名单顺序试：**并发测一遍，谁快用谁**。
//! 测一次 256KB 只要一两秒，换来的是一次下完 15MB 而不是中途卡死。
//!
//! # 下载完必须校验
//!
//! 接下来要跑的是一个**要求管理员权限**的安装程序，而它很可能是从第三方
//! 加速线路下来的。大小对不上、SHA-256 对不上就一律丢弃重来 ——
//! 这一步几十毫秒，不能省。

use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// 发布版本所在的仓库。
pub const REPO: &str = "xxccdl/Open-PPT-Viewer";

/// 发布里那个安装包的名字（打包脚本产出的就是它）。
const ASSET_NAME: &str = "OpenPPTView-Setup.exe";

/// 测一条线路探多少字节。
const PROBE_BYTES: u64 = 256 * 1024;
/// 单条线路的测速上限。慢到这个份上就不必再等了。
const PROBE_TIMEOUT: Duration = Duration::from_secs(6);
/// 测速至少要拿到这么多字节，才认为这条线路「能用」。
///
/// 拿到几百字节就说「通了」是不作数的：那种速度下完 15MB 要等到下课。
const PROBE_MIN_BYTES: u64 = 48 * 1024;

/// 国内常用的 GitHub 加速前缀。
///
/// 这些服务**生灭无常**，所以逻辑里不写死任何一条：测速会把不通的、慢的
/// 自然筛掉，剩下谁快用谁。这里只管多列几个 —— 它是可以随时改的数据。
///
/// 加一条的规矩只有一条：**必须是「前缀 + 原始 URL」这种拼法**。
pub const PROXIES: &[&str] = &[
    "https://gh-proxy.com/",
    "https://ghfast.top/",
    "https://ghproxy.net/",
    "https://github.moeyy.xyz/",
    "https://gh.llkk.cc/",
    "https://ghproxy.cc/",
    "https://gh-proxy.net/",
    "https://hub.gitmirror.com/",
    "https://gh.ddlc.top/",
    "https://gh.space/",
];

/// 本程序版本。
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// 一条可用的发布信息。
#[derive(Debug, Clone)]
pub struct Release {
    /// 版本号（已去掉 `v` 前缀）。
    pub version: String,
    /// 更新说明（发布时写的正文）。
    pub notes: String,
    /// 安装包在 github.com 上的原始地址。
    ///
    /// 注意是**原始地址**：加速前缀由 [`fastest_source`] 在下载前拼上去。
    pub asset_url: String,
    /// 安装包字节数（发布信息里带的，用来判断下载是否完整）。
    pub asset_size: u64,
    /// `sha256:...` 里的那串；发布信息里没带就是 `None`。
    pub sha256: Option<String>,
}

/// 取最新发布。
///
/// - `Ok(Some(_))`：拿到了
/// - `Ok(None)`：仓库里还没有发布过任何版本（不是错误）
/// - `Err(_)`：网络不通等
///
/// 先直连 API，不通再借加速前缀走一遍 —— 不少加速服务同时也转发
/// `api.github.com`，先试一遍不亏。
pub fn check_latest() -> Result<Option<Release>, String> {
    let api = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let mut tries: Vec<(String, String)> = vec![("直连".to_string(), api.clone())];
    for prefix in PROXIES {
        tries.push(((*prefix).to_string(), format!("{prefix}{api}")));
    }

    let mut last = String::from("没有可用的线路");
    for (label, url) in tries {
        match fetch_text(&url, Duration::from_secs(10)) {
            Ok(Some(body)) => {
                let release = parse_release(&body)?;
                log::info!(
                    "检查更新：最新版本 {}（走 {label}）",
                    release.version
                );
                return Ok(Some(release));
            }
            Ok(None) => {
                log::info!("检查更新：{label} 回报「还没有发布版本」");
                return Ok(None);
            }
            Err(e) => {
                log::debug!("检查更新：{label} 不可用（{e}）");
                last = e;
            }
        }
    }
    Err(format!("连不上 GitHub：{last}"))
}

/// `remote` 比 `local` 新吗。
///
/// 按点分的数字比，`-` 后面的预发布后缀忽略 —— 只做「要不要提示升级」这个判断，
/// 不需要完整的语义化版本规则。
pub fn is_newer(remote: &str, local: &str) -> bool {
    fn parts(s: &str) -> Vec<u64> {
        s.split('-')
            .next()
            .unwrap_or(s)
            .split('.')
            .map(|p| p.trim().parse::<u64>().unwrap_or(0))
            .collect()
    }
    let (r, l) = (parts(remote), parts(local));
    for i in 0..r.len().max(l.len()) {
        let a = r.get(i).copied().unwrap_or(0);
        let b = l.get(i).copied().unwrap_or(0);
        if a != b {
            return a > b;
        }
    }
    false
}

/// 并发测一遍所有线路，返回（最快的完整下载地址, 字节/秒）。
pub fn fastest_source(asset_url: &str) -> Result<(String, f64), String> {
    let (tx, rx) = mpsc::channel::<(String, Result<f64, String>, String)>();
    let mut expected = 0usize;

    for prefix in PROXIES {
        let url = format!("{prefix}{asset_url}");
        if spawn_probe(tx.clone(), url, (*prefix).to_string()) {
            expected += 1;
        }
    }
    // 直连也测一次：装了 VPN 或者学校出口没被限制时它最快
    if spawn_probe(tx.clone(), asset_url.to_string(), "直连".to_string()) {
        expected += 1;
    }
    // 自己这一份要丢掉，否则下面的 recv 永远等不到「所有发送方都结束」
    drop(tx);

    let deadline = Instant::now() + PROBE_TIMEOUT + Duration::from_millis(500);
    let mut best: Option<(f64, String, String)> = None;
    let mut failures: Vec<String> = Vec::new();
    let mut got = 0usize;
    while got < expected {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match rx.recv_timeout(left) {
            Ok((url, result, label)) => {
                got += 1;
                match result {
                    Ok(speed) => {
                        log::debug!("测速：{label} {:.1} MB/s", speed / 1048576.0);
                        let better = match &best {
                            Some((s, _, _)) => speed > *s,
                            None => true,
                        };
                        if better {
                            best = Some((speed, url, label));
                        }
                    }
                    Err(why) => {
                        log::debug!("测速：{label} 不可用（{why}）");
                        failures.push(format!("{label} {why}"));
                    }
                }
            }
            // 超时/通道关了：不再等剩下的
            Err(_) => break,
        }
    }

    match best {
        Some((speed, url, label)) => {
            log::info!(
                "更新线路测速：最快是 {label}（{:.1} MB/s）",
                speed / 1048576.0
            );
            Ok((url, speed))
        }
        None => {
            // 把原因带出去：只说「不通」，老师和我们都无从下手
            let detail = failures
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join("；");
            if detail.is_empty() {
                Err("下载超时，请检查网络后重试".to_string())
            } else {
                Err(format!("所有下载线路都不通（{detail}）"))
            }
        }
    }
}

/// 下载到 `dest`，边下边回报进度（已下字节, 总字节；总字节未知时为 0）。
pub fn download(
    url: &str,
    dest: &Path,
    on_progress: &mut dyn FnMut(u64, u64),
) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // 下载不设总超时（十几 MB 慢慢下也得下完），只设连接超时 + 读超时
    let client = builder(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("无法初始化网络：{e}"))?;

    let mut resp = client
        .get(url)
        .send()
        .map_err(|e| format!("下载失败：{e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("下载失败：服务器返回 {status}"));
    }
    let total = resp.content_length().unwrap_or(0);

    // 先写临时文件再改名：中途断了不会在缓存里留半个安装包
    let tmp = dest.with_extension("part");
    let _ = std::fs::remove_file(&tmp);
    let mut file = std::fs::File::create(&tmp).map_err(|e| format!("无法写入临时文件：{e}"))?;

    let mut buf = vec![0u8; 64 * 1024];
    let mut done = 0u64;
    loop {
        match resp.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                file.write_all(&buf[..n])
                    .map_err(|e| format!("写入下载文件失败：{e}"))?;
                done += n as u64;
                on_progress(done, total);
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!("下载中断（已收到 {} 字节）：{e}", done));
            }
        }
    }
    file.flush().map_err(|e| format!("写入下载文件失败：{e}"))?;
    // 必须先关句柄再改名：Windows 上句柄没关时**改名会失败**
    drop(file);

    std::fs::rename(&tmp, dest).map_err(|e| format!("无法保存下载文件：{e}"))
}

/// 校验下载到的安装包：先看大小，再看发布信息里带的 SHA-256。
///
/// 不敢省这一步：接下来要跑的是一个要求管理员权限的安装程序。
pub fn verify(path: &Path, release: &Release) -> Result<(), String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("下载的文件不见了：{e}"))?;
    if release.asset_size > 0 && meta.len() != release.asset_size {
        return Err(format!(
            "下载不完整（{} 字节，应为 {} 字节），请重试",
            meta.len(),
            release.asset_size
        ));
    }
    if let Some(want) = &release.sha256 {
        let got = sha256_of(path)?;
        if &got != want {
            let _ = std::fs::remove_file(path);
            return Err("安装包校验没通过（内容与发布信息对不上），已丢弃，请重试".to_string());
        }
    }
    Ok(())
}

/// 静默安装新版本。
///
/// # 为什么走 `ShellExecuteW` 而不是 `Command::new`
///
/// 安装程序带「需要管理员权限」的清单，用 `CreateProcess` 直接起会
/// 以 `ERROR_ELEVATION_REQUIRED(740)` 失败。交给系统外壳才会弹出
/// 「是否允许此应用对你的设备进行更改」那个系统提示。
///
/// 安装程序自己会请正在运行的我们让开位置（`--quit`，先落盘标注再退），
/// 所以这里只管把它叫起来。
pub fn launch_installer(setup: &Path) -> Result<(), String> {
    crate::shell_execute(&setup.to_string_lossy(), Some("--silent"))
}

// ---------------------------------------------------------------------------
// 内部
// ---------------------------------------------------------------------------

/// 统一的 HTTP 客户端配置。
///
/// GitHub 的 API **不带 User-Agent 一律 403**，所以这个头不是可选项。
fn builder(timeout: Duration) -> reqwest::blocking::ClientBuilder {
    reqwest::blocking::Client::builder()
        .user_agent(format!("OpenPPTView/{}", current_version()))
        .timeout(timeout)
}

/// GET 一段文本。`Ok(None)` 表示 404（这里用来区分「还没有发布版本」）。
pub(crate) fn fetch_text(url: &str, timeout: Duration) -> Result<Option<String>, String> {
    let client = builder(timeout)
        .build()
        .map_err(|e| format!("无法初始化网络：{e}"))?;
    let resp = client.get(url).send().map_err(|e| e.to_string())?;
    let status = resp.status();
    if status.as_u16() == 404 {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    resp.text().map(Some).map_err(|e| e.to_string())
}

/// 解析发布信息。
fn parse_release(body: &str) -> Result<Release, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("发布信息看不懂：{e}"))?;

    let tag = v.get("tag_name").and_then(|t| t.as_str()).unwrap_or_default();
    if tag.is_empty() {
        return Err("发布信息里没有版本号".to_string());
    }
    let notes = v.get("body").and_then(|t| t.as_str()).unwrap_or_default();

    let empty = Vec::new();
    let assets = v
        .get("assets")
        .and_then(|a| a.as_array())
        .unwrap_or(&empty);
    /// 取资产名。写成 `fn` 而不是闭包：闭包推不出这种「借用输入」的生命周期。
    fn name_of(a: &serde_json::Value) -> &str {
        a.get("name").and_then(|n| n.as_str()).unwrap_or_default()
    }

    // 优先认我们自己的名字；改了名/传了别的包时退一步，找一个 exe
    let asset = assets
        .iter()
        .find(|a| name_of(a) == ASSET_NAME)
        .or_else(|| {
            assets
                .iter()
                .find(|a| name_of(a).to_ascii_lowercase().ends_with(".exe"))
        })
        .ok_or_else(|| format!("这个发布里没有可安装的程序（没找到 {ASSET_NAME}）"))?;

    let asset_url = asset
        .get("browser_download_url")
        .and_then(|u| u.as_str())
        .unwrap_or_default()
        .to_string();
    if asset_url.is_empty() {
        return Err("发布里的下载地址是空的".to_string());
    }

    Ok(Release {
        version: tag.trim_start_matches(['v', 'V']).to_string(),
        notes: notes.trim().to_string(),
        asset_url,
        asset_size: asset.get("size").and_then(|s| s.as_u64()).unwrap_or(0),
        sha256: asset
            .get("digest")
            .and_then(|d| d.as_str())
            .and_then(|d| d.strip_prefix("sha256:"))
            .map(|s| s.to_ascii_lowercase()),
    })
}

/// 起一条测速线程。返回是否真的起了。
fn spawn_probe(
    tx: mpsc::Sender<(String, Result<f64, String>, String)>,
    url: String,
    label: String,
) -> bool {
    let spawned = std::thread::Builder::new()
        .name("oppv-update-probe".to_string())
        .spawn(move || {
            let _ = tx.send((url.clone(), measure(&url), label));
        });
    match spawned {
        Ok(_) => true,
        Err(e) => {
            log::warn!("测速线程起不来，跳过这条线路：{e}");
            false
        }
    }
}

/// 试探一条线路：最多下 [`PROBE_BYTES`]，返回字节/秒；不可用返回**原因**。
///
/// 带原因是为了排查：只回一个「不通」，老师和我们都没法判断
/// 是线路挂了、还是这台机器的网络本来就出不去。
fn measure(url: &str) -> Result<f64, String> {
    let client = builder(PROBE_TIMEOUT)
        .build()
        .map_err(|e| format!("客户端建不起来：{e}"))?;
    let started = Instant::now();

    let mut resp = client
        .get(url)
        // 只取开头一段。加速服务若忽略 Range 也无妨 —— 我们读够就断开
        .header("Range", format!("bytes=0-{}", PROBE_BYTES - 1))
        .send()
        .map_err(|e| format!("连不上：{e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }

    let mut got = 0u64;
    let mut buf = vec![0u8; 32 * 1024];
    while got < PROBE_BYTES {
        match resp.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => got += n as u64,
            // 读中断：拿到的够多就还能算速度（有些线路不给 Range，读完就断）
            Err(_) => break,
        }
    }
    if got < PROBE_MIN_BYTES {
        return Err(format!("只收到 {got} 字节"));
    }
    let secs = started.elapsed().as_secs_f64().max(0.001);
    Ok(got as f64 / secs)
}

/// 算文件的 SHA-256（小写十六进制）。
fn sha256_of(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};

    let mut file = std::fs::File::open(path).map_err(|e| format!("无法读取安装包：{e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("读取安装包失败：{e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let sum = hasher.finalize();
    let mut out = String::with_capacity(sum.len() * 2);
    for b in sum.iter() {
        let _ = std::fmt::Write::write_fmt(&mut out, format_args!("{b:02x}"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison_is_numeric_not_lexicographic() {
        // 字符串比较会把 "0.10.0" 判成比 "0.9.0" 小 —— 这正是要避开的坑
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(is_newer("0.0.2", "0.0.1"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
    }

    #[test]
    fn version_comparison_tolerates_prefixes_and_suffixes() {
        // 发布标签常带 v；预发布后缀不该让比较失真
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("0.2.0-beta.1", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
    }

    #[test]
    fn release_picks_our_installer() {
        let body = r#"{
            "tag_name": "v0.2.0",
            "body": "修了几个渲染问题",
            "assets": [
                {"name": "source.zip", "browser_download_url": "https://example.com/s.zip", "size": 10},
                {"name": "OpenPPTView-Setup.exe",
                 "browser_download_url": "https://github.com/o/r/releases/download/v0.2.0/OpenPPTView-Setup.exe",
                 "size": 16000000,
                 "digest": "sha256:AB12"}
            ]
        }"#;
        let r = parse_release(body).unwrap();
        assert_eq!(r.version, "0.2.0", "标签里的 v 要去掉");
        assert!(r.asset_url.ends_with("OpenPPTView-Setup.exe"));
        assert_eq!(r.asset_size, 16_000_000);
        assert_eq!(r.sha256.as_deref(), Some("ab12"), "digest 要小写归一");
        assert_eq!(r.notes, "修了几个渲染问题");
    }

    #[test]
    fn release_falls_back_to_any_exe_and_reports_when_there_is_none() {
        let body = r#"{
            "tag_name": "v0.2.0",
            "assets": [{"name": "Setup.exe", "browser_download_url": "https://example.com/a.exe", "size": 1}]
        }"#;
        assert!(parse_release(body).is_ok(), "认不到固定名字时应退而求其次");

        let body = r#"{"tag_name": "v0.2.0", "assets": [{"name": "notes.txt", "browser_download_url": "https://example.com/a", "size": 1}]}"#;
        let err = parse_release(body).unwrap_err();
        assert!(err.contains("没有可安装的程序"), "要说清为什么装不了：{err}");
    }

    #[test]
    fn release_without_assets_is_an_error_not_a_panic() {
        let err = parse_release(r#"{"tag_name": "v0.1.0"}"#).unwrap_err();
        assert!(err.contains("没有可安装的程序"), "{err}");
    }

    #[test]
    fn proxy_list_is_made_of_usable_prefixes() {
        for p in PROXIES {
            assert!(p.starts_with("https://"), "加速前缀必须是 https：{p}");
            assert!(p.ends_with('/'), "前缀要以 / 结尾才好拼：{p}");
        }
    }

    /// 真联网的验证：能走通 API、能测出最快线路、能真把文件下下来。
    ///
    /// 默认 `#[ignore]` —— 单元测试不该依赖网络，也不该在别人的机器上
    /// 拉十几 MB。手动跑：
    ///
    /// ```text
    /// cargo test -p ppt-app -- --ignored --nocapture proxy_round_trip
    /// ```
    ///
    /// 拿别人的公开发布做样本：本仓库现在还没有发布过版本，
    /// 而这条测试要验的是**下载链路**，不是本仓库有没有发版。
    #[test]
    #[ignore]
    fn proxy_round_trip_works() {
        let api = "https://api.github.com/repos/BurntSushi/ripgrep/releases/latest";
        let body = fetch_text(api, Duration::from_secs(15))
            .expect("API 请求失败")
            .expect("这个仓库应当有发布");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let url = v["assets"][0]["browser_download_url"]
            .as_str()
            .expect("应能找到资源")
            .to_string();
        let want = v["assets"][0]["size"].as_u64().unwrap_or(0);
        println!("样本资源：{url}（{want} 字节）");

        let (best, speed) = fastest_source(&url).expect("至少应有一条线路可用");
        println!("最快线路（{:.1} MB/s）：{best}", speed / 1048576.0);

        let dest = std::env::temp_dir().join("oppv-update-round-trip.bin");
        let mut last = 0u64;
        download(&best, &dest, &mut |done, _| last = done).expect("下载应成功");
        let got = std::fs::metadata(&dest).unwrap().len();
        println!("下载完成：{got} 字节");
        let _ = std::fs::remove_file(&dest);
        assert!(got > 100_000, "下到的文件太小，链路可能没走完：{got}");
        assert_eq!(got, last, "进度回调的最后一次应等于文件大小");
    }
}
