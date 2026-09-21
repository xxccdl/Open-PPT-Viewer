//! 后台常驻：关窗不退出，下次打开课件直接复用这个进程。
//!
//! # 为什么值得留一个进程在后台
//!
//! 一次冷启动里最贵的几件事，没有一件是「打开课件」本身：
//! 建 WebView2、扫系统字体、把渲染管线拉起来。而老师上课时
//! 「讲完一个课件，接着开下一个」是最高频的动作 ——
//! 每次都重付这几百毫秒到一秒，观感就是「这软件慢」。
//!
//! 所以：关窗只把窗口藏起来，进程留着；再双击课件时，
//! 第二个进程把路径转交给已经开着的那个（`tauri-plugin-single-instance`），
//! 自己随即退出。老师看到的是「点下去就已经在眼前了」。
//!
//! # 留在后台的东西必须有出口
//!
//! 常驻进程没有出口是最招人烦的做法（只能去任务管理器杀）。
//! 所以配一个托盘图标：「显示主界面」和「退出 OpenPPTView」。
//! 另外闲置 [`IDLE_EXIT`] 之后自动退出 —— 老师放学忘了关，
//! 不该让一个 WebView2 在后台常驻一整夜。
//!
//! # 标注不能因为「关窗」丢
//!
//! 关窗只是隐藏，文档和笔迹都还在内存里，所以那一步不需要提醒保存。
//! 但**真退出**（托盘「退出」、闲置超时）会把没落盘的标注一起带走，
//! 所以退出前先让界面存一次：发 `app-quitting`，等界面回话
//! [`mark_ready_to_quit`]，最多等 [`QUIT_GRACE`]。

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, Window};

/// 窗口藏起来多久之后自动退出进程。
///
/// 60 分钟：够覆盖「连着上两节课、课间翻翻别的课件」，
/// 又不至于放学后白占一晚内存（常驻一个 WebView2 约 100MB）。
const IDLE_EXIT: Duration = Duration::from_secs(60 * 60);

/// 真退出前，最多等界面多久把标注落盘。
const QUIT_GRACE: Duration = Duration::from_millis(1500);

/// 看门狗醒来的间隔。半分钟一次，够及时又不折腾。
const WATCHDOG_TICK: Duration = Duration::from_secs(30);

/// 后台常驻状态。
pub struct Resident {
    /// 窗口藏起来的时刻；`None` 表示窗口正开着（或还没开过）。
    hidden_since: Mutex<Option<Instant>>,
    /// 已经在退出流程里 —— 此时 `CloseRequested` 不再拦截，放行。
    quitting: AtomicBool,
    /// 界面回话「标注已经存好了」。
    ready: Mutex<bool>,
    ready_cv: Condvar,
    /// 另一个进程转交过来、还没被界面接走的文件路径。
    ///
    /// 为什么不能只发事件：界面刚起来的那一两百毫秒里还没注册监听，
    /// 这时候转来的路径会丢。存在这里，界面自己来取（`take_pending_open`），
    /// 于是「事件」和「来取」两条路必有一条接住。
    pending_open: Mutex<Option<String>>,
}

impl Resident {
    pub fn new() -> Resident {
        Resident {
            hidden_since: Mutex::new(None),
            quitting: AtomicBool::new(false),
            ready: Mutex::new(false),
            ready_cv: Condvar::new(),
            pending_open: Mutex::new(None),
        }
    }

    /// 取走待打开的文件（取过就没了，避免同一次转交被处理两遍）。
    pub fn take_pending_open(&self) -> Option<String> {
        self.pending_open.lock().unwrap().take()
    }
}

impl Default for Resident {
    fn default() -> Self {
        Resident::new()
    }
}

/// 把主窗口叫出来并聚焦。
pub fn show_main_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
    // 窗口回来了，闲置计时重新开始
    *app.state::<Resident>().hidden_since.lock().unwrap() = None;
}

/// 点了 × / Alt+F4：不退出，只把窗口藏起来。
pub fn on_close_requested(window: &Window, api: &tauri::CloseRequestApi) {
    let app = window.app_handle();
    let resident = app.state::<Resident>();
    if resident.quitting.load(Ordering::SeqCst) {
        return; // 真的在退出，放行
    }
    api.prevent_close();
    let _ = window.hide();
    *resident.hidden_since.lock().unwrap() = Some(Instant::now());
    log::info!("窗口已隐藏，进程留在后台（下次打开课件可直接复用）");
}

/// 真退出：先请界面把标注落盘，再退。
///
/// # 为什么不能在这里就地等
///
/// 这个函数会从托盘菜单调用，也就是跑在**主线程**的事件循环上；
/// 而界面的回话 `ready_to_quit` 同样要走主线程才能被处理。
/// 就地等就是死锁 —— 只能另开一条线程等。
pub fn quit(app: &AppHandle) {
    let resident = app.state::<Resident>();
    if resident.quitting.swap(true, Ordering::SeqCst) {
        return; // 已经在退了，别重复走一遍
    }
    if let Err(e) = app.emit("app-quitting", ()) {
        log::warn!("通知界面落盘失败（直接退出）：{e}");
    }

    let app = app.clone();
    std::thread::spawn(move || {
        let resident = app.state::<Resident>();
        let deadline = Instant::now() + QUIT_GRACE;
        let mut ready = resident.ready.lock().unwrap();
        while !*ready {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                log::warn!("界面没有在 {QUIT_GRACE:?} 内回话，直接退出");
                break;
            }
            let (guard, _) = resident.ready_cv.wait_timeout(ready, left).unwrap();
            ready = guard;
        }
        drop(ready);
        app.exit(0);
    });
}

/// 界面回话：标注已经落盘，可以退了。
pub fn mark_ready_to_quit(app: &AppHandle) {
    let resident = app.state::<Resident>();
    *resident.ready.lock().unwrap() = true;
    resident.ready_cv.notify_all();
}

/// 闲置看门狗：窗口藏得太久就把进程收掉。
pub fn spawn_idle_watchdog(app: AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(WATCHDOG_TICK);
        let resident = app.state::<Resident>();
        if resident.quitting.load(Ordering::SeqCst) {
            return;
        }
        let hidden_since = *resident.hidden_since.lock().unwrap();
        if let Some(t) = hidden_since {
            if t.elapsed() >= IDLE_EXIT {
                log::info!(
                    "窗口已隐藏 {} 分钟，自动退出后台进程",
                    t.elapsed().as_secs() / 60
                );
                quit(&app);
                return;
            }
        }
    });
}

/// 第二个进程被挡下时走这里：把窗口叫出来，有文件就转给界面。
pub fn on_second_instance(app: &AppHandle, argv: &[String]) {
    // `--quit`：有人请我们让开位置（卸载程序删文件之前就是这么做的）。
    //
    // 必须在「把窗口叫出来」之前判断 —— 否则卸载的时候窗口会闪一下，
    // 而且这时把窗口显示出来正好卡在卸载流程中间。
    if argv.iter().skip(1).any(|a| a == "--quit") {
        log::info!("收到 --quit，准备退出（会先把标注落盘）");
        quit(app);
        return;
    }

    show_main_window(app);

    let Some(file) = file_arg(argv) else { return };
    log::info!("另一个实例转来文件：{file}");

    // 先记下再发事件：界面可能还没注册好监听，那就靠它来取
    *app.state::<Resident>().pending_open.lock().unwrap() = Some(file.clone());
    if let Err(e) = app.emit("open-file", file) {
        log::warn!("把文件转给界面失败：{e}");
    }
}

/// 从命令行里挑出「要打开的课件」。
///
/// 与 [`crate::startup_info`] 同一套判断：第一个不以 `-` 开头、
/// 且确实存在的参数（双击课件时系统传的就是它）。
fn file_arg(argv: &[String]) -> Option<String> {
    argv.iter()
        .skip(1) // argv[0] 是 exe 自己
        .find(|a| !a.starts_with('-') && Path::new(a).exists())
        .cloned()
}

/// 建托盘图标 —— 常驻进程唯一的正经出口。
pub fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "显示主界面", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "退出 OpenPPTView", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit_item])?;

    let mut builder = TrayIconBuilder::new()
        .tooltip("OpenPPTView 课件讲演器")
        .menu(&menu)
        // 左键直接开窗（老师的第一直觉），右键才出菜单
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => show_main_window(app),
            "quit" => quit(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        });

    // 复用窗口图标，不为托盘单独特制一份
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_arg_skips_flags_and_the_exe_itself() {
        let exe = std::env::current_exe().unwrap();
        let exe = exe.to_string_lossy().to_string();
        let argv = vec![exe.clone(), "--foo".to_string()];
        assert_eq!(file_arg(&argv), None, "只有开关时不该挑出文件");

        // 用一个确实存在的路径当「课件」
        let real = std::env::temp_dir().join("oppv-file-arg-test.pptx");
        std::fs::write(&real, b"x").unwrap();
        let argv = vec![exe, "--foo".to_string(), real.to_string_lossy().to_string()];
        assert_eq!(file_arg(&argv), Some(real.to_string_lossy().to_string()));
        let _ = std::fs::remove_file(&real);
    }

    #[test]
    fn pending_open_is_taken_only_once() {
        let r = Resident::new();
        assert_eq!(r.take_pending_open(), None);
        *r.pending_open.lock().unwrap() = Some("a.pptx".into());
        assert_eq!(r.take_pending_open(), Some("a.pptx".into()));
        assert_eq!(r.take_pending_open(), None, "取过就不该再来一遍");
    }
}
