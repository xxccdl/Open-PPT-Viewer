//! OpenPPTView 安装程序。
//!
//! # 这是一个「自己的程序」，不是打包器的壳
//!
//! 界面、安装动作、卸载动作都在这份代码里：界面自己画（`ppt-installer-ui`），
//! 文件复制、注册表、快捷方式、运行环境检测都在 `install` 模块。
//! 不依赖 NSIS / Inno，也不依赖 WebView2 —— 后者很关键：
//! 安装程序的任务之一就是把 WebView2 装上，它自己不能先要一个 WebView2。
//!
//! # 命令行
//!
//! ```text
//! OpenPPTView-Setup.exe                图形界面安装（默认）
//! OpenPPTView-Setup.exe --silent       静默安装到默认位置
//! OpenPPTView-Setup.exe --uninstall    图形界面卸载
//! OpenPPTView-Setup.exe --uninstall --silent
//! ```
//!
//! # 别拿 `cargo build` 的产物去装
//!
//! 这个 crate 的产物叫 `ppt-installer.exe`，是安装器的**壳** ——
//! 尾部还没追加载荷，单独运行只会得到「安装包中缺少程序数据」。
//! 交付给用户的成品由 `packaging/windows/make-setup.ps1` 合成，
//! 名字是 `OpenPPTView-Setup.exe`，两者故意不同名，免得拿错。
//!
//! # 界面为什么能离线预览
//!
//! 绘制代码在 `ppt-installer-ui` 里，与窗口无关；`cargo run -p ppt-installer-ui
//! --example preview` 能把每一屏渲染成 PNG。安装程序的界面是最难改的东西
//! （改一次要装一次），有了它就能先把样子调好再打包。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod install;
mod win32;

use std::path::{Path, PathBuf};

use ppt_installer_ui::ui::{Action, Phase, Ui};
use ppt_installer_ui::TextCtx;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    WM_CLOSE, WM_DESTROY, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT, WM_SETCURSOR,
    WM_TIMER,
};

use install::{Options, Progress};

/// 定时器：刷新界面（hover 之类）。安装本身跑在点击处理里，不需要它推进。
const TIMER_ID: usize = 1;

/// 程序在跑哪一种活。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Job {
    Install,
    Uninstall,
}

struct App {
    win: win32::Win,
    ui: Ui,
    text: TextCtx,
    /// 安装/卸载的载荷（延迟到要用的时候再读，省得开界面还要等一次全文件读）
    payload: Option<(Vec<u8>, Option<Vec<u8>>)>,
    /// 正在忙（安装中）：此时不接受任何点击，避免重入
    busy: bool,
    /// 用户点了取消
    cancelled: bool,
    exit_code: i32,
}

impl App {
    fn new() -> App {
        let default_dir = default_install_dir();
        App {
            // 窗口是在 App 建好之后才创建的（窗口过程需要 App 的地址），
            // 所以这里先占位，建好窗口后立刻填上
            win: win32::Win::new(HWND(std::ptr::null_mut()), 1.0),
            ui: Ui::new(install::VERSION, default_dir.to_string_lossy().to_string()),
            text: TextCtx::new(),
            payload: None,
            busy: false,
            cancelled: false,
            exit_code: 0,
        }
    }

    fn self_exe(&self) -> PathBuf {
        std::env::current_exe().unwrap_or_else(|_| PathBuf::from("OpenPPTView-Setup.exe"))
    }

    /// 取载荷：卸载时不需要主程序，装的时候才读。
    fn load_payload(&mut self) -> Result<(Vec<u8>, Option<Vec<u8>>), String> {
        if let Some(p) = &self.payload {
            return Ok(p.clone());
        }
        let (app, boot) = install::read_payload(&self.self_exe())?;
        self.payload = Some((app.clone(), boot.clone()));
        Ok((app, boot))
    }

    fn repaint(&mut self) {
        let scale = self.win.scale;
        let pixmap = self.win.canvas();
        self.ui.draw(pixmap, &mut self.text, scale);
        self.win.present();
    }

    /// 安装/卸载时由 `Progress` 回调驱动：更新文案与进度条，并让窗口活下去。
    fn show_progress(&mut self, text: &str, percent: f32) {
        self.ui.step = text.to_string();
        self.ui.progress = percent / 100.0;
        self.repaint();
        // 安装是同步跑的，这里得替 Windows 把消息泵一下，
        // 否则窗口在这几秒里会是「未响应」的死样子
        pump_messages();
    }

    fn on_action(&mut self, action: Action) {
        match action {
            Action::Close => self.finish(0),
            Action::Browse => {
                if let Some(dir) = pick_folder(&self.ui.path) {
                    self.ui.path = dir;
                }
                self.win.invalidate();
            }
            Action::Cancel => {
                self.cancelled = true;
            }
            Action::Install => self.start_install(),
            Action::None => {}
        }
    }

    fn start_install(&mut self) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.ui.phase = Phase::Installing;
        self.ui.progress = 0.0;
        self.ui.step = "正在准备…".to_string();
        self.win.invalidate();
        self.repaint();

        let opts = Options {
            dir: PathBuf::from(&self.ui.path),
            desktop_shortcut: self.ui.desktop_shortcut,
            input_mode: self.ui.mode.as_str().to_string(),
        };

        let result = match self.load_payload() {
            Ok((app, boot)) => {
                let self_exe = self.self_exe();
                let mut progress = UiProgress { app: self };
                install::install(&app, boot.as_deref(), &opts, &self_exe, &mut progress)
            }
            Err(e) => Err(e),
        };

        // 借用在这里结束，才能继续用 self
        self.busy = false;
        match result {
            Ok(()) => {
                self.ui.phase = Phase::Done {
                    launch: self.ui.launch_after,
                };
                self.ui.progress = 1.0;
                self.ui.step.clear();
            }
            Err(e) if e == "已取消" => {
                self.ui.phase = Phase::Setup;
                self.ui.progress = 0.0;
            }
            Err(e) => {
                // 界面上给老师的是短句，完整原因（含具体路径）留在日志里
                log::error!("安装失败：{e}");
                self.ui.phase = Phase::Failed(e);
            }
        }
        self.win.invalidate();
        self.repaint();

        // 完成页的「完成」按钮按下后才算真正结束；这里不自动关窗
    }

    fn finish(&mut self, code: i32) {
        self.exit_code = code;
        win32::close_window(self.win.hwnd);
    }

    /// 卸载：驱动界面走一遍进度，最后停在「已经卸载了」。
    fn run_uninstall(&mut self) {
        self.busy = true;
        self.ui.phase = Phase::Installing;
        self.ui.progress = 0.0;
        self.win.invalidate();
        self.repaint();

        let dir = dir_of_self(&self.self_exe());
        let result = {
            let mut progress = UiProgress { app: self };
            install::uninstall(&dir, &mut progress)
        };
        self.busy = false;
        match result {
            Ok(()) => self.ui.phase = Phase::Uninstalled,
            Err(e) if e == "已取消" => self.ui.phase = Phase::Uninstalled,
            Err(e) => self.ui.phase = Phase::Failed(e),
        }
        self.win.invalidate();
        self.repaint();
    }
}

/// 把安装进度接到界面上。
struct UiProgress<'a> {
    app: &'a mut App,
}

impl Progress for UiProgress<'_> {
    fn step(&mut self, text: &str, percent: f32) -> bool {
        self.app.show_progress(text, percent);
        !self.app.cancelled
    }
}

/// 默认安装位置：`%ProgramFiles%\OpenPPTView`。
///
/// 装到所有用户可见的位置（与学校机房的用法一致：老师装一次，全班机器都能用）。
/// `--dir <路径>` 可以改，本机自测时用它装到用户目录，不必过 UAC。
fn default_install_dir() -> PathBuf {
    if let Some(p) = DIR_OVERRIDE.get() {
        return p.clone();
    }

    // 正式安装包带 `requireAdministrator` 清单，一启动就提权，
    // 装到 `Program Files` 是学校机房的标准用法（老师装一次，全班机器都能用）。
    let system = Path::new(
        &std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".to_string()),
    )
    .join(install::PRODUCT);
    if writable(&system) {
        return system;
    }

    // 写不进去就退到「当前用户」目录。
    //
    // 什么时候会走到这儿：装的是**不提权的那份构建**（本机自测用的开发包）。
    // 以前这种情况要等老师填完表单、点到「正在把程序复制进去」才报
    // 「拒绝访问 C:\Program Files\...」—— 表单白填一遍，看着像程序坏了。
    // 现在一进界面就给一个装得成功的位置，行为上也仍然是正规的
    // per-user 安装（注册表、快捷方式都在当前用户下，`Scope::for_dir` 已经这么分）。
    per_user_dir()
}

/// 当前用户目录下的安装位置（`%LOCALAPPDATA%\Programs\OpenPPTView`）。
fn per_user_dir() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".to_string());
    Path::new(&base).join("Programs").join(install::PRODUCT)
}

/// 这个位置写得进去吗？
///
/// 必须**真写一个文件**试，不能只看目录建不建得出来：
/// `C:\Program Files\OpenPPTView` 在装过一次的机器上本来就在，
/// `create_dir_all` 对已存在的目录一律返回成功 —— 拿它当判据就会
/// 认为「写得进去」，然后一路走到复制文件那一步才报拒绝访问。
fn writable(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(".oppv-write-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

static DIR_OVERRIDE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// 取 `--x value` 这种形式的参数。
fn arg_value(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// 弹系统原生的「选文件夹」对话框。
fn pick_folder(current: &str) -> Option<String> {
    use windows::Win32::UI::Shell::{
        SHBrowseForFolderW, SHGetPathFromIDListW, BROWSEINFOW, BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS,
    };
    let title = "选一个文件夹".encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let mut display = vec![0u16; 260];
    let bi = BROWSEINFOW {
        lpszTitle: windows::core::PCWSTR(title.as_ptr()),
        ulFlags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE,
        pszDisplayName: windows::core::PWSTR(display.as_mut_ptr()),
        ..Default::default()
    };
    let _ = current;
    unsafe {
        let idl = SHBrowseForFolderW(&bi);
        if idl.is_null() {
            return None;
        }
        let mut buf = [0u16; 260];
        let ok = SHGetPathFromIDListW(idl, &mut buf).as_bool();
        windows::Win32::System::Com::CoTaskMemFree(Some(idl as *const _));
        if !ok {
            return None;
        }
        let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }
}

// ---------------------------------------------------------------------------
// 窗口过程
// ---------------------------------------------------------------------------

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let app = win32::user_data::<App>(hwnd);
    match msg {
        WM_PAINT => {
            if let Some(app) = app {
                app.repaint();
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if let Some(app) = app {
                let (x, y) = mouse_xy(lparam);
                let scale = app.win.scale;
                let hit = app.ui.hit(x, y, scale);
                let changed = app.ui.set_hover(hit);
                let hand = app.ui.wants_hand();
                app.win.set_hand_cursor(hand);
                if changed {
                    app.win.invalidate();
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            if let Some(app) = app {
                let (x, y) = mouse_xy(lparam);
                let scale = app.win.scale;
                // 没有系统标题栏：空白处按住就当标题栏拖（控件上不算）
                let hit = app.ui.hit(x, y, scale);
                match hit {
                    None => win32::begin_drag(hwnd),
                    Some(h) => {
                        app.ui.set_pressed(Some(h));
                        app.win.invalidate();
                    }
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if let Some(app) = app {
                let (x, y) = mouse_xy(lparam);
                let scale = app.win.scale;
                let hit = app.ui.hit(x, y, scale);
                let pressed = app.ui.pressed();
                app.ui.set_pressed(None);
                if let (Some(p), Some(h)) = (pressed, hit) {
                    if p == h {
                        let action = app.ui.click(h);
                        app.on_action(action);
                    }
                }
                app.win.invalidate();
            }
            LRESULT(0)
        }
        WM_SETCURSOR => {
            if let Some(app) = app {
                let hand = app.ui.wants_hand();
                app.win.set_hand_cursor(hand);
            }
            LRESULT(1)
        }
        WM_TIMER => {
            if let Some(app) = app {
                app.win.refresh_scale();
                if app.win.dirty {
                    app.win.dirty = false;
                    app.win.invalidate();
                }
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            if let Some(app) = app {
                if app.busy {
                    // 正装着的时候别让人误关；想停就点界面上的取消
                    return LRESULT(0);
                }
                app.finish(0);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            win32::post_quit();
            LRESULT(0)
        }
        _ => win32::default_proc(hwnd, msg, wparam, lparam),
    }
}

fn mouse_xy(lparam: LPARAM) -> (f32, f32) {
    let v = lparam.0 as u32;
    let x = (v & 0xFFFF) as i16 as f32;
    let y = ((v >> 16) & 0xFFFF) as i16 as f32;
    (x, y)
}

/// 把消息泵干净（在同步的安装循环里调用，让窗口不假死）。
fn pump_messages() {
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

// ---------------------------------------------------------------------------
// 命令行模式
// ---------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let silent = args.iter().any(|a| a == "--silent" || a == "/S");
    let job = if args.iter().any(|a| a == "--uninstall" || a == "/U") {
        Job::Uninstall
    } else if is_uninstaller_copy() {
        Job::Uninstall
    } else {
        Job::Install
    };
    if let Some(dir) = arg_value(&args, "--dir") {
        let _ = DIR_OVERRIDE.set(PathBuf::from(dir));
    }

    // 日志：装不上时唯一的线索，写到 %TEMP%
    let log_path = std::env::temp_dir().join("OpenPPTView-Setup.log");
    if let Ok(file) = std::fs::File::create(&log_path) {
        simple_logger(file);
    }
    log::info!("安装程序启动：job={job:?} silent={silent}");

    if silent {
        std::process::exit(run_silent(job));
    }

    // 自检：建窗口 → 画一帧 → 从窗口上抓一张图存盘。
    // 验证的是「建窗口 → 画布 → DIB → 贴到屏幕 → 再读回来」这条链路，
    // 界面本身长什么样由 ppt-installer-ui 的预览负责。
    if let Some(out) = arg_value(&args, "--selftest") {
        std::process::exit(run_selftest(&out));
    }

    let Some((hwnd, scale)) = win32::create_window(
        "OpenPPTView 安装程序",
        ppt_installer_ui::theme::WINDOW_W,
        ppt_installer_ui::theme::WINDOW_H,
        Some(wnd_proc),
    ) else {
        eprintln!("创建窗口失败");
        std::process::exit(1);
    };

    let mut app = App::new();
    app.win = win32::Win::new(hwnd, scale);
    win32::set_user_data(hwnd, &mut app);
    win32::set_timer(hwnd, TIMER_ID, 33);

    if job == Job::Uninstall {
        app.repaint();
        app.run_uninstall();
    } else {
        app.repaint();
    }

    let code = win32::run_message_loop();
    win32::kill_timer(hwnd, TIMER_ID);

    // 完成页：老师说「装好以后马上就打开」，关窗口时把应用拉起来
    if job == Job::Install {
        if let Phase::Done { launch } = app.ui.phase {
            if launch && code == 0 {
                install::launch_app(Path::new(&app.ui.path));
            }
        }
    }
    std::process::exit(code);
}

/// 自检：把首屏画出来并抓图存盘。
fn run_selftest(out: &str) -> i32 {
    let Some((hwnd, scale)) = win32::create_window(
        "OpenPPTView 安装程序",
        ppt_installer_ui::theme::WINDOW_W,
        ppt_installer_ui::theme::WINDOW_H,
        Some(wnd_proc),
    ) else {
        eprintln!("创建窗口失败");
        return 1;
    };
    let mut app = App::new();
    app.win = win32::Win::new(hwnd, scale);
    win32::set_user_data(hwnd, &mut app);
    app.repaint();
    // 让窗口先真正画到屏幕上
    std::thread::sleep(std::time::Duration::from_millis(250));
    pump_messages();
    let Some(pix) = win32::capture_client(hwnd) else {
        eprintln!("抓图失败");
        win32::close_window(hwnd);
        return 1;
    };
    match pix.encode_png() {
        Ok(png) => match std::fs::write(out, png) {
            Ok(()) => {
                println!("自检截图：{out}（{}x{}）", pix.width(), pix.height());
                win32::close_window(hwnd);
                0
            }
            Err(e) => {
                eprintln!("写 {out} 失败：{e}");
                win32::close_window(hwnd);
                1
            }
        },
        Err(e) => {
            eprintln!("编码 PNG 失败：{e}");
            win32::close_window(hwnd);
            1
        }
    }
}

/// 静默模式：没有窗口，把活干完，用退出码说话（0 成功）。
fn run_silent(job: Job) -> i32 {
    struct Log;
    impl Progress for Log {
        fn step(&mut self, text: &str, percent: f32) -> bool {
            log::info!("{percent:5.1}% {text}");
            true
        }
    }
    let mut p = Log;
    let self_exe = std::env::current_exe().unwrap_or_default();
    match job {
        Job::Install => match install::read_payload(&self_exe) {
            Ok((app, boot)) => {
                // 已经装过就**原地升级**，并沿用老师当初的选择
                // （安装位置、操作方式、要不要桌面快捷方式）。
                //
                // 少了这一条，一次自动升级就会把装在 D 盘的变成两份、
                // 把「触摸屏」重置成「自动识别」、还给已经删掉快捷方式的
                // 桌面又塞一个回去。
                let opts = match install::existing_install() {
                    Some(prev) => {
                        log::info!(
                            "检测到已安装：原地升级到 {}（操作方式 {}）",
                            prev.dir.display(),
                            prev.input_mode
                        );
                        prev
                    }
                    None => Options {
                        dir: default_install_dir(),
                        desktop_shortcut: true,
                        input_mode: "auto".to_string(),
                    },
                };
                match install::install(&app, boot.as_deref(), &opts, &self_exe, &mut p) {
                    Ok(()) => {
                        // 自动升级这条路，应用是自己退出把位置让出来的，
                        // 装完必须把它拉回来 —— 老师点的是「立即更新」，
                        // 不是「更新完请自己再找一次图标」。界面上也这么写着。
                        //
                        // 拉起来的这一份是管理员权限（安装程序本身就提了权）。
                        // 实测出图链路不受影响（仍是办公软件出的图），
                        // 但拖拽进窗口会被系统挡住 —— 只影响升级后这一次运行，
                        // 下次从快捷方式打开就恢复了。
                        install::launch_app(&opts.dir);
                        0
                    }
                    Err(e) => {
                        log::error!("安装失败：{e}");
                        1
                    }
                }
            }
            Err(e) => {
                log::error!("{e}");
                1
            }
        },
        Job::Uninstall => match install::uninstall(&dir_of_self(&self_exe), &mut p) {
            Ok(()) => 0,
            Err(e) => {
                log::error!("卸载失败：{e}");
                1
            }
        },
    }
}

/// 自己是不是「安装目录里那份卸载程序」。
///
/// # 为什么不能只看命令行
///
/// 卸载程序不是单独的程序，而是安装包的一份拷贝
/// （见 [`install::UNINST_EXE`]）—— 双击它时**一个参数都没有**，
/// 只认 `--uninstall` 的话它会把自己当成安装程序弹出来：
/// 用户点「卸载」，看到的却是安装界面。这是实打实发生过的 bug。
///
/// 所以入口有两个，都要认：
/// - 命令行 `--uninstall`：程序化调用（系统「应用」列表、脚本）；
/// - 文件名 `uninstall.exe`：老师双击。
fn is_uninstaller_copy() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .map(|n| n.eq_ignore_ascii_case(install::UNINST_EXE))
        .unwrap_or(false)
}

/// 卸载时，自己在哪个目录就是装在哪个目录。
fn dir_of_self(exe: &Path) -> PathBuf {
    exe.parent().map(|p| p.to_path_buf()).unwrap_or_default()
}

/// 极简日志：没有额外依赖，够把出错原因写下来就行。
///
/// 写到 `%TEMP%\OpenPPTView-Setup.log` —— 装不上时，这是唯一的线索。
fn simple_logger(file: std::fs::File) {
    use std::io::Write;
    use std::sync::{Mutex, OnceLock};
    struct F(Mutex<std::fs::File>);
    impl log::Log for F {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }
        fn log(&self, r: &log::Record) {
            if let Ok(mut f) = self.0.lock() {
                let _ = writeln!(f, "[{}] {}", r.level(), r.args());
            }
        }
        fn flush(&self) {}
    }
    static LOGGER: OnceLock<F> = OnceLock::new();
    let logger = LOGGER.get_or_init(|| F(Mutex::new(file)));
    if log::set_logger(logger).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
}
