//! 借用本机已装的 **WPS / Microsoft Office 内核**，把 `.pptx` 出成画面。
//!
//! 主路径是 [`Converter::warm_pages`]：**逐页导出 PNG**。
//! 另有 [`Converter::convert`] 导出整本矢量 PDF，留给诊断与将来的备用。
//!
//! # 为什么不再自己画
//!
//! 自研渲染内核面对的是「无穷尽的排版长尾」：符号字体 cmap、文本行高模型、
//! 组合坐标系的局部单位、预设几何的调整值单位、阴影层级……每修好一批，
//! 下一份课件又冒出一批。而 WPS / PowerPoint 本身就是那份课件的作者工具，
//! 让它自己出图，**保真度是 100%，且不需要我们维护**。
//!
//! # 为什么主路径是逐页 PNG，而不是一次导出整本
//!
//! 用一份 174MB / 39 页的真实课件实测（release）：
//!
//! | 操作 | 耗时 | 产物 |
//! |---|---|---|
//! | `Presentations.Open` | 1784ms | — |
//! | `Slides(1).Export(PNG, 1920×1080)` | **404ms** | 1.6MB |
//! | 逐页 `Slide.Export(PNG)` 全本 | 22.3s | 41MB |
//! | 一次 `SaveAs(PDF)` | 6.2~8.3s | 6.4MB 矢量 |
//!
//! 看总耗时，整本 PDF 明显更快；但**老师打开课件时只看得到第一页**。
//!
//! 整本导出那 7 秒里，能被看见的只有第一页，却把「首屏就绪」推迟到 9 秒以上
//! —— 这就是「翻页一翻就得半天」的来源。逐页出图把首屏压到
//! `Open + 一页` ≈ 2.2 秒（典型小课件约 1 秒），剩下的页在后台排队
//! （每页约 0.4 秒，比翻页快），老师翻到哪页基本都已经好了。
//!
//! 代价是位图会糊——因此**出图分辨率按屏幕自适应**（见
//! `openpptview` 的 `raster_bucket`）：1080p 屏出 1920 正好 1:1，
//! 4K 大屏出 3840，小屏笔记本不必白出大图。
//!
//! # 线程模型
//!
//! COM 自动化要求**单线程套间（STA）**，而且 `Application` 对象不能跨线程，
//! 因此整条转换流水线跑在一条专用线程上：
//!
//! ```text
//! 线程启动 → CoInitializeEx(STA) → 建 Application（约 2s）→ 收任务循环
//! ```
//!
//! `Application` 的创建要 2 秒左右，所以调用方应当在 App 启动时就
//! [`Converter::spawn`]（预热），用户真正打开课件时它已经是热的。
//!
//! # 实测的进程行为
//!
//! 三个阶段（创建 / 打开 / 导出）**都没有任何可见窗口**，全程 headless ——
//! 靠的是 `Presentations.Open(..., WithWindow = false)`，
//! 老师在讲台上不会看到任何东西闪一下。

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[cfg(windows)]
mod com;
pub mod source;

/// `ppSaveAsPDF`：`Presentation.SaveAs` 的文件格式常量。
///
/// 与 Office 的 `ppSaveAsPDF` 同值，WPS 亦兼容（已实测）。
const SAVE_AS_PDF: i32 = 32;

/// `ppAlertsNone`：把所有模态提示关掉。
///
/// 后台线程里一旦弹出模态框，转换就会**永久挂死**（没人去点确定），
/// 所以这个属性必须在打开文档之前设好。
const ALERTS_NONE: i32 = 1;

/// 单次转换的等待上限。
///
/// 超时不代表引擎坏了，更可能是踩到了某个模态框；调用方应当放弃这次
/// 转换并回退到自研渲染，而不是把用户卡在那里。
const CONVERT_TIMEOUT: Duration = Duration::from_secs(180);

/// 逐页出图的等待上限。
///
/// 比整本导出宽松得多：页数多的课件按每页 0.4~0.6s 算，几百页也要几分钟。
/// 这是纯粹的**后台**任务，超时只意味着「后面那些页还没好」，
/// 已经出好的页照样能用，所以宁可给足时间也不要误杀。
const WARM_TIMEOUT: Duration = Duration::from_secs(900);

/// 可用的转换引擎。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Wps,
    Office,
}

impl Engine {
    /// COM ProgID。
    pub fn progid(self) -> &'static str {
        match self {
            Engine::Wps => "KWPP.Application",
            Engine::Office => "PowerPoint.Application",
        }
    }

    /// 给用户看的中文名。
    pub fn display_name(self) -> &'static str {
        match self {
            Engine::Wps => "WPS 演示",
            Engine::Office => "Microsoft PowerPoint",
        }
    }
}

/// 探测本机可用的引擎，优先 WPS。
///
/// 只查注册表（`CLSIDFromProgID`），**不会启动任何进程**，
/// 因此可以在 App 启动路径上放心调用。
pub fn detect_engine() -> Option<Engine> {
    #[cfg(windows)]
    {
        // 国内教师机 WPS 覆盖率远高于 Office，故 WPS 优先
        for engine in [Engine::Wps, Engine::Office] {
            if com::clsid_of_progid(engine.progid()).is_some() {
                return Some(engine);
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// 转换任务的返回值。
type Reply = Sender<Result<(), String>>;

/// 一步动画里要**藏起来**的东西。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hide {
    /// 整个形状：`Shape.Visible = msoFalse`。
    Shape(u32),
    /// 形状里的某几段文字，段落索引从 0 起。
    ///
    /// # 为什么段落要单独一条路
    ///
    /// 「项目符号逐条弹出」是课件里最常见的动画形态，而它动的是**段落**
    /// 不是形状 —— 形状从头到尾都在（框和底色开局就该看得见）。
    /// 拿不出段落级的手段，这类页面就会「一开局全部文字都在，
    /// 点一下再把已经显示的段落往上面砸一遍」。
    ///
    /// 手段只有一条：Office 2007+ 的 `TextFrame2` 能把**文字填充**
    /// 设成全透明。WPS 的 `Font` 上既没有 `Visible` 也没有 `Hidden`
    /// （实测两个都不存在），所以走这条。
    Paragraphs(u32, Vec<u32>),
}

/// 一页的动画分步计划：第 k 项 = 「已播 k 步」时该藏起来的东西。
pub type StepPlan = Vec<Vec<Hide>>;

/// 「这一页每一步该藏什么」的算账人。
///
/// 传进来的是一份**由调用方提供**的闭包，而不是让本 crate 自己去解析 OOXML：
/// 解析器在 `ppt-format-pptx` 里，而这里只该负责「按清单让办公软件出图」。
/// 调用方手上同时握有画面来源与语义来源，只有它能算这笔账。
///
/// 参数是页码（从 0 起）。返回空表示这一页没有动画。
pub type StepPlanner = Arc<dyn Fn(usize) -> StepPlan + Send + Sync>;

/// 逐页出图任务。
///
/// # 为什么是「逐页」而不是「整本」
///
/// 实测同一份 174MB / 39 页课件：
///
/// | 操作 | 耗时 |
/// |---|---|
/// | `Presentations.Open` | 1784ms |
/// | `Slides(1).Export` 导出首页位图 | **404ms** |
/// | `SaveAs` 导出整本矢量 PDF | 7178ms |
///
/// 老师要的是「打开就看到第一页」。整本导出那 7 秒里，能被看见的只有第一页，
/// 却把「首屏就绪」推迟到了 9 秒以上 —— 这就是「翻页一翻就得半天」的来源。
///
/// 逐页出图把首屏压到 `Open + 一页` ≈ 2.2 秒（典型小课件约 1 秒），
/// 剩下的页在后台排队，老师翻到哪页基本都已经好了。
pub struct WarmRequest {
    /// 源课件。
    pub pptx: PathBuf,
    /// 每页 PNG 的落盘目录；文件名就是 `<页码>.png`（页码从 0 起）。
    ///
    /// 由 [`crate::source::WpsRasterSource`] 按同一约定读取，
    /// 所以两边的路径规则必须写在一起，不能各写一份。
    pub dir: PathBuf,
    /// 出图像素宽度（按老师屏幕自适应，见 `ppt-app` 的档位选择）。
    pub width: u32,
    /// 出图像素高度。
    pub height: u32,
    /// 页数。
    pub page_count: usize,
    /// 每出一页就报一次（页码从 0 起）。
    ///
    /// 第一页也走这里 —— 调用方据此判断「可以换成办公软件出的画面了」。
    pub on_page: Option<Sender<usize>>,
    /// 老师当前想看的那一页，插队用。
    ///
    /// 顺序出图时老师若是往后跳（比如从第 1 页跳到第 30 页），
    /// 干等 12 秒是不能接受的；把这个请求塞进来，下一张就出它。
    pub jump: Arc<Mutex<Option<usize>>>,
    /// 动画分步的算账人，见 [`StepPlanner`]。`None` 表示不做按步出图。
    pub step_plans: Option<StepPlanner>,
}

enum Job {
    Convert {
        pptx: PathBuf,
        out: PathBuf,
        reply: Reply,
    },
    WarmPages {
        req: Box<WarmRequest>,
        reply: Reply,
    },
    Stop,
}

/// 一条待执行的工作（不含回信通道 —— 那是任务信封的一部分）。
#[cfg(windows)]
enum Task {
    /// 整本导出成矢量 PDF。产品路径不用它，留着当「WPS 到底能多快」的
    /// 基准工具，也方便将来需要矢量时直接拿来（见 `examples/convert.rs`）。
    Pdf { pptx: PathBuf, out: PathBuf },
    /// 逐页出位图。
    Warm(Box<WarmRequest>),
}

/// Pptx → PDF 的转换器。
///
/// 内部只有一条工作线程；多次调用 [`Converter::convert`] 会排队，
/// 不会并发唤起多个 WPS 实例（并发实例会争抢用户配置目录而失败）。
///
/// `Sender` 不是 `Sync`，而调用方（Tauri 的 `State`）要求共享状态是 `Sync`，
/// 所以用 `Mutex` 包一层 —— 锁只在「投递任务」这一瞬间持有，
/// 不会把漫长的转换过程锁在里面。
pub struct Converter {
    engine: Engine,
    tx: Mutex<Sender<Job>>,
    join: Option<JoinHandle<()>>,
}

impl Converter {
    /// 起一条转换线程并立即返回；`Application` 在后台预热。
    ///
    /// 返回后可以马上调用 [`Converter::convert`]，请求会排队等预热完成。
    pub fn spawn(engine: Engine) -> Converter {
        let (tx, rx) = mpsc::channel();
        let join = thread::Builder::new()
            .name("ppt-convert".to_string())
            .spawn(move || worker(engine, rx))
            .map_err(|e| log::error!("无法启动转换线程：{e}"))
            .ok();

        Converter {
            engine,
            tx: Mutex::new(tx),
            join,
        }
    }

    /// 使用的引擎。
    pub fn engine(&self) -> Engine {
        self.engine
    }

    /// 把 `pptx` 导出成矢量 PDF 到 `out`。
    ///
    /// 阻塞当前线程直到出结果（上限 [`CONVERT_TIMEOUT`]）。
    /// 成功时保证 `out` 已是一份完整、合法的 PDF。
    pub fn convert(&self, pptx: &Path, out: &Path) -> Result<(), String> {
        let (reply_tx, reply_rx) = mpsc::channel();
        {
            let tx = self.tx.lock().map_err(|_| "转换器状态损坏".to_string())?;
            tx.send(Job::Convert {
                pptx: pptx.to_path_buf(),
                out: out.to_path_buf(),
                reply: reply_tx,
            })
            .map_err(|_| "转换线程已经退出".to_string())?;
        }

        match reply_rx.recv_timeout(CONVERT_TIMEOUT) {
            Ok(r) => r,
            Err(_) => Err(format!(
                "转换超时（超过 {} 秒）。可能被 {} 的某个弹窗挡住，请手动打开一次该课件确认。",
                CONVERT_TIMEOUT.as_secs(),
                self.engine.display_name()
            )),
        }
    }

    /// 逐页出图到 `req.dir`，阻塞直到**整本排完**。
    ///
    /// 单页就绪会通过 `req.on_page` 实时回报，所以调用方不必等这个函数返回 ——
    /// 第一页几百毫秒后就能拿去显示（见 [`WarmRequest`] 的说明）。
    ///
    /// 整本排完可能要十几秒（每页约 0.4s），因此调用方**必须放在后台线程**里，
    /// 不能挡在打开课件那条路上。
    pub fn warm_pages(&self, req: WarmRequest) -> Result<(), String> {
        let (reply_tx, reply_rx) = mpsc::channel();
        {
            let tx = self.tx.lock().map_err(|_| "转换器状态损坏".to_string())?;
            tx.send(Job::WarmPages {
                req: Box::new(req),
                reply: reply_tx,
            })
            .map_err(|_| "转换线程已经退出".to_string())?;
        }

        match reply_rx.recv_timeout(WARM_TIMEOUT) {
            Ok(r) => r,
            Err(_) => Err(format!(
                "逐页出图超时（超过 {} 秒）。可能被 {} 的某个弹窗挡住。",
                WARM_TIMEOUT.as_secs(),
                self.engine.display_name()
            )),
        }
    }
}

impl Drop for Converter {
    fn drop(&mut self) {
        if let Ok(tx) = self.tx.lock() {
            let _ = tx.send(Job::Stop);
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// 工作线程主循环。
#[cfg(windows)]
fn worker(engine: Engine, rx: Receiver<Job>) {
    // STA 是本线程一切 COM 调用的前提
    let ours = com::init_sta();

    // 先拍一张进程快照，创建之后做差集就能认出「我们刚拉起来的那一个」
    let before = com::snapshot_processes();

    let app = match com::create(engine.progid()) {
        Ok(app) => com::Obj(app),
        Err(e) => {
            log::warn!("转换引擎不可用：{e}");
            // 逐条把失败回给调用方（而不是静默丢弃，否则调用方要等满超时）
            while let Ok(job) = rx.recv() {
                match job {
                    Job::Stop => break,
                    Job::Convert { reply, .. } | Job::WarmPages { reply, .. } => {
                        let _ = reply.send(Err(e.clone()));
                    }
                }
            }
            if ours {
                com::uninit();
            }
            return;
        }
    };

    // 把导出进程降到「低于正常」优先级：老师此刻已经在翻课件了，
    // 不能让它和前台渲染抢 CPU（见 `lower_office_priority` 的说明）
    let lowered = com::lower_office_priority(&before);
    if lowered > 0 {
        log::info!("已把 {lowered} 个导出进程降到低于正常优先级，避免与翻页抢 CPU");
    }

    // 关弹窗必须在任何 Open 之前
    if let Err(e) = app.put("DisplayAlerts", com::v_i32(ALERTS_NONE)) {
        log::debug!("设置 DisplayAlerts 失败（不致命）：{e}");
    }

    // 用 `Option` 装：失败时整个换掉实例（见下面的重建逻辑）
    let mut current: Option<com::Obj> = Some(app);

    while let Ok(job) = rx.recv() {
        let (task, reply): (Task, Reply) = match job {
            Job::Stop => break,
            Job::Convert { pptx, out, reply } => (Task::Pdf { pptx, out }, reply),
            Job::WarmPages { req, reply } => (Task::Warm(req), reply),
        };

        let Some(app) = current.as_ref() else {
            let _ = reply.send(Err("导出内核已不可用（重建失败）".to_string()));
            continue;
        };

        let mut result = run_task(app, &task);

        // 失败时**重建 `Application` 再试一次**。
        //
        // # 为什么必须换实例，而不是原地重试
        //
        // 这个 `Application` 是全程复用的，而实测 `Quit` 之后 WPS 仍会残留
        // 进程 —— 一旦它进了坏状态，之后每一次 `Open` 都会抛
        // `0x80020009 发生意外`。此时**所有课件都会失败并静默回落到
        // 自研渲染**，老师看到的就是「明明装了 WPS，画面还是自研的、还有错」。
        //
        // 那种情况下对同一个坏对象重试毫无意义，重建是唯一能自救的动作。
        //
        // 对逐页任务还有额外好处：已经出好的页会被跳过（循环里查文件在不在），
        // 所以重试是**接着干**，不是从头再来。
        if let Err(detail) = &result {
            log::warn!("导出失败（{detail}），重建导出实例后重试一次");
            rebuild(&mut current, engine);

            if let Some(app) = current.as_ref() {
                result = run_task(app, &task);
            }
        }

        if let Err(e) = &result {
            log::warn!("仍未成功：{e}");
        }
        let _ = reply.send(result);
    }

    if let Some(app) = current.take() {
        let _ = app.call("Quit", Vec::new());
    }
    if ours {
        com::uninit();
    }
}

/// 换一个全新的 `Application` 实例。
#[cfg(windows)]
fn rebuild(current: &mut Option<com::Obj>, engine: Engine) {
    if let Some(old) = current.take() {
        let _ = old.call("Quit", Vec::new());
        drop(old);
    }

    let before = com::snapshot_processes();
    match com::create(engine.progid()) {
        Ok(fresh) => {
            let fresh = com::Obj(fresh);
            com::lower_office_priority(&before);
            if let Err(e) = fresh.put("DisplayAlerts", com::v_i32(ALERTS_NONE)) {
                log::debug!("重建后设置 DisplayAlerts 失败（不致命）：{e}");
            }
            *current = Some(fresh);
        }
        Err(e) => log::warn!("重建导出实例失败：{e}"),
    }
}

/// 执行一条任务。
#[cfg(windows)]
fn run_task(app: &com::Obj, task: &Task) -> Result<(), String> {
    match task {
        Task::Pdf { pptx, out } => export_pdf(app, pptx, out),
        Task::Warm(req) => warm_pages_impl(app, req),
    }
}

#[cfg(not(windows))]
fn worker(_engine: Engine, rx: Receiver<Job>) {
    while let Ok(job) = rx.recv() {
        match job {
            Job::Stop => break,
            Job::Convert { reply, .. } | Job::WarmPages { reply, .. } => {
                let _ = reply.send(Err("当前平台不支持借用办公软件内核".to_string()));
            }
        }
    }
}

/// 逐页出图：`Open` 一次，然后一页一页导出 PNG。
///
/// 详见 [`WarmRequest`] 里对「为什么不是整本」的说明。
#[cfg(windows)]
fn warm_pages_impl(app: &com::Obj, req: &WarmRequest) -> Result<(), String> {
    std::fs::create_dir_all(&req.dir)
        .map_err(|e| format!("无法创建出图目录 {}：{e}", req.dir.display()))?;

    let started = std::time::Instant::now();
    let presentations = app.get_obj("Presentations")?;
    let doc = presentations.call_obj(
        "Open",
        vec![
            com::v_bstr(&req.pptx.to_string_lossy()),
            com::v_bool(true),  // ReadOnly：绝不修改老师的原文件
            com::v_bool(false), // Untitled
            com::v_bool(false), // WithWindow = false —— 全程不弹窗的关键
        ],
    )?;
    log::info!("逐页出图：Open 耗时 {:.0}ms", started.elapsed().as_secs_f64() * 1000.0);

    // 这一页搞不出来也不要让整个任务失败 —— 剩下的页仍然值得出。
    // 缺的那页由显示层如实报「本机办公软件导出这一页失败」，**不**降级渲染
    // （见 `WpsRasterSource::rasterize_page`：宁可说清楚，也不亮出画错的画面）。
    let mut failed = 0usize;
    let mut steps_failed = 0usize;
    let mut step_frames = 0usize;
    let mut done = vec![false; req.page_count];
    let mut cursor = 0usize;
    let mut first_page_ms = 0.0f64;

    while let Some(page) = next_page(&done, &mut cursor, &req.jump) {
        done[page] = true;

        let out = crate::source::page_path(&req.dir, page);
        // 这一页有几步动画、每一步该藏谁。解析只在 app 侧发生，
        // 这里拿到的是一份现成的清单。
        let hide = req.step_plans.as_ref().map(|planner| planner(page));
        let hide = hide.filter(|h| !h.is_empty());

        let need_full = !out.is_file();
        // 分帧也要一起判：老缓存里只有整页图，光看整页图会把它整个跳过，
        // 于是「动画按步出图」这版永远生效不了
        let need_steps = hide
            .as_ref()
            .is_some_and(|h| crate::source::steps_missing(&req.dir, page, h.len()));
        if !need_full && !need_steps {
            continue;
        }

        let slide = match slide_of(&doc, page) {
            Ok(s) => s,
            Err(e) => {
                failed += 1;
                log::warn!("第 {} 页取不到：{e}", page + 1);
                continue;
            }
        };

        if need_full {
            let page_started = std::time::Instant::now();
            match export_slide(&slide, &out, req.width, req.height) {
                Ok(()) => {
                    if page == 0 {
                        first_page_ms = started.elapsed().as_secs_f64() * 1000.0;
                    }
                    log::debug!(
                        "第 {} 页出图完成：{:.0}ms",
                        page + 1,
                        page_started.elapsed().as_secs_f64() * 1000.0
                    );
                    if let Some(tx) = &req.on_page {
                        let _ = tx.send(page);
                    }
                }
                Err(e) => {
                    failed += 1;
                    log::warn!("第 {} 页出图失败：{e}", page + 1);
                    continue;
                }
            }
        }

        // 这一页有动画的话，把每一步的样子也出成图（见 `StepPlanner`）。
        // 没有这一步，老师点下去只会看到「弹出的都是已经显示的内容」。
        if let Some(hide) = &hide {
            let (ok, bad) = export_step_frames(&slide, page, req, hide);
            step_frames += ok;
            steps_failed += bad;
        }
    }

    // 无论成败都要关掉，否则这个文档会一直占着文件锁
    let _ = doc.call("Close", Vec::new());

    // 全部页的分帧都成功才写下标记：下次打开就不必再跑这一趟了。
    //
    // 只要有失败就不写 —— 那样下次打开还会补做一遍（已出好的各自跳过），
    // 而不是把一份缺帧的缓存当成完整的。
    if req.step_plans.is_some() && steps_failed == 0 {
        if let Err(e) = crate::source::mark_steps_done(&req.dir) {
            log::warn!("写动画分帧标记失败（下次打开会重做一遍）：{e}");
        }
    }

    log::info!(
        "逐页出图完成：{} 页任务、{} 页失败、{} 张动画分帧（{} 张失败），\
         总耗时 {:.1}s（首屏 {:.1}s）",
        req.page_count,
        failed,
        step_frames,
        steps_failed,
        started.elapsed().as_secs_f64(),
        first_page_ms / 1000.0
    );

    // 一页都没出来才算失败 —— 那说明这条路整个不通，调用方该回退到整本 PDF
    if failed == req.page_count && req.page_count > 0 {
        return Err("所有页都未能出图，本机办公软件可能无法处理这份课件".to_string());
    }
    Ok(())
}

/// 下一张该出哪一页：老师的插队请求优先，否则顺序推进。
#[cfg(windows)]
fn next_page(done: &[bool], cursor: &mut usize, jump: &Mutex<Option<usize>>) -> Option<usize> {
    if let Ok(mut g) = jump.lock() {
        // 一次只让一页插队：老师连点几页时，最后点的那页最要紧，
        // 但每次取走一个就已经够了 —— 循环下一轮会再问一次
        if let Some(p) = g.take() {
            if p < done.len() && !done[p] {
                return Some(p);
            }
        }
    }
    while *cursor < done.len() {
        let p = *cursor;
        *cursor += 1;
        if !done[p] {
            return Some(p);
        }
    }
    None
}

/// 取一页 `Slide` 对象。
#[cfg(windows)]
fn slide_of(doc: &com::Obj, page: usize) -> Result<com::Obj, String> {
    let slides = doc.get_obj("Slides")?;
    // COM 的 `Slides` 是 1-based，页码对外是 0-based —— 这一处换算错了会整本错位
    slides.call_obj("Item", vec![com::v_i32(page as i32 + 1)])
}

/// 把一页导出成 PNG。
#[cfg(windows)]
fn export_slide(
    slide: &com::Obj,
    out: &Path,
    width: u32,
    height: u32,
) -> Result<(), String> {
    slide.call(
        "Export",
        vec![
            com::v_bstr(&out.to_string_lossy()),
            com::v_bstr("PNG"),
            com::v_i32(width as i32),
            com::v_i32(height as i32),
        ],
    )?;

    // 个别版本会「调用成功但没写出文件」，所以还是看落盘结果
    if !out.is_file() {
        return Err(format!("导出后没有生成图片 {}", out.display()));
    }
    Ok(())
}

/// 出这一页的「按步」各帧，返回 `(成功张数, 失败张数)`。
///
/// 第 k 张 = 「已播 k 步」的样子：把 `hide[k]` 里那些**这一步还不该露面**的
/// 东西藏掉，再让办公软件导出。老师点一下看一张 —— 逐元素弹出就成立了，
/// 而且每一帧都是作者工具自己画的，不是我们猜的。
#[cfg(windows)]
fn export_step_frames(
    slide: &com::Obj,
    page: usize,
    req: &WarmRequest,
    hide: &[Vec<Hide>],
) -> (usize, usize) {
    // 一次把这一页的形状（含组合内部）连同「它原本可不可见」都拿下来。
    //
    // 记下原本的可见性是为了**只动我们确实该动的那些**：课件里本来就
    // `hidden="1"` 的对象不能被我们顺手点亮。
    let mut shapes = Vec::new();
    if let Ok(collection) = slide.get_obj("Shapes") {
        com::collect_shapes(&collection, &mut shapes, 0);
    }
    if shapes.is_empty() {
        log::warn!("第 {} 页有动画却一个形状都取不到，这一步的帧出不了", page + 1);
        return (0, hide.len());
    }

    // 形状 id → `TextFrame2.TextRange`。取一次就留着：
    // 一页里同一个文本框可能在好几步里都要动段落。
    let mut text_ranges: std::collections::HashMap<u32, Option<com::Obj>> =
        std::collections::HashMap::new();

    let mut ok = 0usize;
    let mut bad = 0usize;
    let mut previous: Option<&Vec<Hide>> = None;

    for (k, hidden) in hide.iter().enumerate() {
        let out = crate::source::step_path(&req.dir, page, k);
        if out.is_file() {
            ok += 1;
            previous = Some(hidden);
            continue;
        }

        // 上一步和这一步「谁藏谁露」完全一样时不必让办公软件再画一遍 ——
        // 拷一份就行。典型课件里强调动画不改变可见性，这种步很常见，
        // 省下的是实实在在的出图时间。
        if previous == Some(hidden) {
            let prev = crate::source::step_path(&req.dir, page, k - 1);
            if std::fs::copy(&prev, &out).is_ok() {
                ok += 1;
                previous = Some(hidden);
                continue;
            }
        }

        let undo = apply_hidden(&shapes, hidden, &mut text_ranges);
        let result = export_slide(slide, &out, req.width, req.height);
        undo.apply();

        match result {
            Ok(()) => {
                ok += 1;
                previous = Some(hidden);
            }
            Err(e) => {
                bad += 1;
                log::warn!("第 {} 页第 {} 步出图失败：{e}", page + 1, k);
            }
        }
    }
    (ok, bad)
}

/// 这一步临时改动过什么，用来原样放回去。
///
/// **必须原样放回**：后面每一步、以及整页图，都建立在「文档还是原样」上。
#[cfg(windows)]
#[derive(Default)]
struct Undo {
    /// (形状, 原本是否可见)。只有原本可见的我们才动过。
    shapes: Vec<com::Obj>,
    /// (文字填充对象, 原本的透明度)。
    paragraphs: Vec<(com::Obj, f32)>,
}

#[cfg(windows)]
impl Undo {
    fn apply(self) {
        for (fill, original) in self.paragraphs {
            let _ = fill.put("Transparency", com::v_f32(original));
        }
        for shape in self.shapes {
            let _ = shape.put("Visible", com::v_bool(true));
        }
    }
}

/// 把 `hidden` 里的东西藏起来，返回「怎么放回去」。
///
/// **只动「本来可见」的形状**：`hidden="1"` 的对象读出来就是不可见，
/// 我们绝不把它点亮 —— 那会让课件里本不该出现的东西冒出来。
#[cfg(windows)]
fn apply_hidden(
    shapes: &[(i32, com::Obj, bool)],
    hidden: &[Hide],
    text_ranges: &mut std::collections::HashMap<u32, Option<com::Obj>>,
) -> Undo {
    let mut undo = Undo::default();

    for item in hidden {
        match item {
            Hide::Shape(id) => {
                let found = shapes
                    .iter()
                    .find(|(sid, _, visible)| *sid == *id as i32 && *visible);
                let Some((sid, shape, _)) = found else {
                    continue;
                };
                if let Err(e) = shape.put("Visible", com::v_bool(false)) {
                    // 设不上就这一帧少了隐藏效果，不影响其它帧 —— 记一笔即可
                    log::warn!("隐藏形状 {sid} 失败：{e}");
                    continue;
                }
                undo.shapes.push(shape.clone());
            }
            Hide::Paragraphs(id, paragraphs) => {
                let range = text_ranges
                    .entry(*id)
                    .or_insert_with(|| text_range2_of(shapes, *id));
                let Some(range) = range else {
                    log::warn!("形状 {id} 取不到 TextFrame2，这一步的段落藏不了");
                    continue;
                };
                if hide_paragraphs(range, *id, paragraphs, &mut undo) == 0 {
                    // 一段都没藏成：这一帧会退化成「全部都显示」，
                    // 必须出声 —— 否则看起来就像「藏了但没效果」
                    log::warn!(
                        "形状 {id} 的第 {paragraphs:?} 段一段都没能藏住，\
                         这一帧会和整页图一样"
                    );
                }
            }
        }
    }
    undo
}

/// 取某个形状的 `TextFrame2.TextRange`。
///
/// 段落级的隐藏只能走这条 Office 2007+ 的对象模型：WPS 的 `Font` 上
/// 既没有 `Visible` 也没有 `Hidden`（实测都不存在）。
#[cfg(windows)]
fn text_range2_of(shapes: &[(i32, com::Obj, bool)], id: u32) -> Option<com::Obj> {
    let (_, shape, _) = shapes.iter().find(|(sid, _, _)| *sid == id as i32)?;
    shape.get_obj("TextFrame2").ok()?.get_obj("TextRange").ok()
}

/// 把指定段落设成全透明（并记下原值）。
///
/// # 段落序号要对齐
///
/// 我们的段落索引从 0 起，办公软件的 `Paragraphs()` 从 1 起。两边对同一段
/// 文字的计数可能差一个（解析器会保留末尾的空段落），所以这里先比一下两侧
/// 的段数：对不上就记一条警告 —— 否则「藏错了段落」是**看不出来**的。
///
/// # 一段都没藏成也要出声
///
/// 返回藏成功的段数。调用方拿它判断「这一步是不是白跑了」——
/// 静默地什么都不做是最难查的一类故障。
#[cfg(windows)]
fn hide_paragraphs(
    range: &com::Obj,
    shape_id: u32,
    paragraphs: &[u32],
    undo: &mut Undo,
) -> usize {
    // `Paragraphs()` 不带参数时返回整段文字，它的 `Count` 就是办公软件认到的段数
    let count = range
        .get_obj_with("Paragraphs", Vec::new())
        .ok()
        .and_then(|all| all.get("Count").ok())
        .and_then(|v| com::as_i32(&v))
        .unwrap_or(0);
    if count > 0 && paragraphs.iter().any(|p| *p as i32 >= count) {
        log::warn!(
            "形状 {shape_id}：要藏的段落序号超出了办公软件认到的段数（{count}），\
             这一步可能藏错了段落"
        );
    }

    let mut done = 0usize;
    for p in paragraphs {
        // COM 的段落是 1-based
        let Ok(para) = range.get_obj_with("Paragraphs", vec![com::v_i32(*p as i32 + 1)]) else {
            continue;
        };
        let Some(fill) = para
            .get_obj("Font")
            .ok()
            .and_then(|f| f.get_obj("Fill").ok())
        else {
            continue;
        };
        let original = fill
            .get("Transparency")
            .ok()
            .and_then(|v| com::as_f32(&v))
            .unwrap_or(0.0);
        if let Err(e) = fill.put("Transparency", com::v_f32(1.0)) {
            log::warn!("隐藏形状 {shape_id} 第 {} 段失败：{e}", p + 1);
            continue;
        }
        undo.paragraphs.push((fill, original));
        done += 1;
    }
    done
}

/// 一次完整的「打开 → 另存为 PDF → 关闭」。
#[cfg(windows)]
fn export_pdf(app: &com::Obj, pptx: &Path, out: &Path) -> Result<(), String> {
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("无法创建目录 {}：{e}", parent.display()))?;
    }

    // 先导出到同目录下的临时名字，成功后再改名。
    // 直接写目标路径的话，中途失败会留下一份半截 PDF，
    // 而缓存层只看「文件在不在」—— 半个文件会被当成有效缓存一直用下去。
    let tmp = partial_path(out);
    let _ = std::fs::remove_file(&tmp);

    let started = std::time::Instant::now();
    let presentations = app.get_obj("Presentations")?;
    let doc = presentations.call_obj(
        "Open",
        vec![
            com::v_bstr(&pptx.to_string_lossy()),
            com::v_bool(true),  // ReadOnly：绝不修改老师的原文件
            com::v_bool(false), // Untitled
            com::v_bool(false), // WithWindow = false —— 全程不弹窗的关键
        ],
    )?;

    // 分阶段计时。
    //
    // 这两个数决定了产品能做到多快：`Open` 是「老师等多久才能看到第一页」
    // 的下限，`SaveAs` 是「整本矢量图备好」的下限。
    // 之前只记总耗时，没法判断优化该往哪边使劲。
    let open_ms = started.elapsed().as_secs_f64() * 1000.0;

    let save_started = std::time::Instant::now();
    let saved = doc.call(
        "SaveAs",
        vec![com::v_bstr(&tmp.to_string_lossy()), com::v_i32(SAVE_AS_PDF)],
    );
    let save_ms = save_started.elapsed().as_secs_f64() * 1000.0;

    // 无论成败都要关掉，否则这个文档会一直占着文件锁
    let _ = doc.call("Close", Vec::new());

    log::info!("导出阶段耗时：Open {open_ms:.0}ms + SaveAs {save_ms:.0}ms");

    saved?;

    let check = validate_pdf(&tmp)
        .and_then(|_| {
            std::fs::rename(&tmp, out)
                .map_err(|e| format!("无法写入 {}：{e}", out.display()))
        });
    if check.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    check
}

/// 同目录下的临时文件名。
///
/// 故意保留 `.pdf` 后缀：`SaveAs` 的格式参数虽然指定了 PDF，
/// 但个别版本仍会按目标扩展名做一次判断。
fn partial_path(out: &Path) -> PathBuf {
    let stem = out
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "deck".to_string());
    out.with_file_name(format!(".{stem}.partial.pdf"))
}

/// 确认产物真的是 PDF。
///
/// 引擎可能「成功返回但写出空文件」（例如源文件损坏时某些版本的行为），
/// 这里把这种情况挡在缓存之外，让调用方回退到自研渲染。
fn validate_pdf(path: &Path) -> Result<(), String> {
    let meta = std::fs::metadata(path)
        .map_err(|e| format!("引擎没有产出文件 {}：{e}", path.display()))?;
    // 一份正常课件的 PDF 不可能只有几百字节；阈值取小一点以免误杀单页课件
    if meta.len() < 512 {
        return Err(format!("导出的 PDF 过小（{} 字节），判定为失败", meta.len()));
    }
    let head = std::fs::read(path)
        .map_err(|e| format!("无法读取导出的 PDF：{e}"))?;
    if !head.starts_with(b"%PDF-") {
        return Err("导出的文件不是 PDF".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progids_match_the_documented_ones() {
        assert_eq!(Engine::Wps.progid(), "KWPP.Application");
        assert_eq!(Engine::Office.progid(), "PowerPoint.Application");
    }

    #[test]
    fn partial_path_stays_next_to_the_target() {
        // 临时文件必须与目标同目录：跨卷 rename 会失败
        let out = Path::new(r"C:\cache\v9\abc123\deck.pdf");
        let partial = partial_path(out);
        assert_eq!(partial.parent(), out.parent());
        assert_ne!(partial, out);
        assert!(partial.to_string_lossy().ends_with(".pdf"));
    }

    #[test]
    fn partial_path_survives_a_name_without_extension() {
        let partial = partial_path(Path::new(r"C:\cache\deck"));
        assert_eq!(partial.parent(), Some(Path::new(r"C:\cache")));
    }

    #[test]
    fn validate_rejects_a_non_pdf() {
        let dir = std::env::temp_dir().join("oppv-convert-test-notpdf");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.pdf");
        std::fs::write(&path, vec![b'x'; 4096]).unwrap();
        assert!(validate_pdf(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn validate_rejects_a_tiny_file() {
        let dir = std::env::temp_dir().join("oppv-convert-test-tiny");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tiny.pdf");
        // 前缀对，但太小 —— 引擎「成功返回空产物」就长这样
        std::fs::write(&path, b"%PDF-1.4\n").unwrap();
        assert!(validate_pdf(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn validate_accepts_a_plausible_pdf() {
        let dir = std::env::temp_dir().join("oppv-convert-test-ok");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ok.pdf");
        let mut body = b"%PDF-1.4\n".to_vec();
        body.resize(2048, b' ');
        std::fs::write(&path, &body).unwrap();
        assert!(validate_pdf(&path).is_ok());
        let _ = std::fs::remove_file(&path);
    }
}
