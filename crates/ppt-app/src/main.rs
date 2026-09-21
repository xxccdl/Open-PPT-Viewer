//! OpenPPTView 桌面应用。
//!
//! # 前后端分工
//!
//! ```text
//! Rust（本文件 + 各 crate）           WebView（web/）
//! ─────────────────────────           ──────────────
//! 打开课件、解析 OOXML              窗口 UI、缩略图侧栏
//! 惰性解析 + 两级缓存                标注画布（Pointer Events）
//! 光栅化 → PNG                      放映模式、辅助工具
//! 预渲染调度（优先级 + 取消）
//! ```
//!
//! 幻灯片**像素**由 Rust 产出，WebView 只负责显示与标注 ——
//! 这是本项目与「网页套壳 PPT 预览器」的本质区别：
//! 排版不经过浏览器的 DOM/CSS 引擎，因此不受其性能与保真度限制。
//!
//! # 为什么用 `tauri::ipc::Response` 回传位图
//!
//! Tauri 的普通命令返回值走 JSON 序列化。一张 1920×1080 的 PNG
//! 若被序列化成 JSON 数字数组，体积会膨胀数倍且解析极慢。
//! `Response::new(Vec<u8>)` 直接走二进制通道，前端拿到的是 `ArrayBuffer`。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod associations;
mod install_pref;
mod resident;
mod update;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
// `Emitter` 提供 `AppHandle::emit`，用来把「办公软件出的第一页已就绪」推给前端
use tauri::Emitter;

use ppt_convert::source::{ready_count, WpsRasterSource};
use ppt_convert::{detect_engine, Converter, StepPlan, StepPlanner};
use ppt_core::scene::Size;
use ppt_core::{detect_format, detect_format_by_extension, DocFormat, DocumentSource, SharedSource};
use ppt_format_pdf::PdfSource;
use ppt_format_pptx::PptxSource;
use ppt_pipeline::{Pipeline, PipelineConfig, Priority};
use ppt_text::FontContext;

/// 应用状态。
#[derive(Default)]
struct AppState {
    /// 当前打开的文档（同一时刻只打开一份，切换课件时替换）。
    doc: Mutex<Option<OpenDoc>>,
    /// 系统字体索引。
    ///
    /// 惰性构建 + 全局复用：扫描系统字体要几十毫秒，
    /// 若每次打开课件都重建，老师连续换课件时会有明显卡顿。
    fonts: std::sync::OnceLock<Arc<FontContext>>,
    /// 最近打开列表。
    recent: Mutex<Vec<String>>,
    /// Pptx → PDF 的转换器（借用本机 WPS / Office 内核）。
    ///
    /// 惰性创建但**只创建一次**：`Application` 的启动要 2 秒左右，
    /// 每次转换都新建的话老师会明确感到「打开很慢」。调用方应当在
    /// 应用启动时就摸一次 [`AppState::converter`] 把它预热起来。
    converter: std::sync::OnceLock<Option<Arc<Converter>>>,
    /// 正在出图中的课件（键是出图目录）。
    ///
    /// 同一份课件只排一次队：老师在窗口模式下来回翻页会反复触发
    /// 「需要出图」，没有这道闸门就会排出一长串重复任务。
    warming: Mutex<HashSet<String>>,
    /// 各出图目录的「任务已排完」标志。
    ///
    /// 显示层靠它把「还没轮到这一页」和「这一页出不来」分开：
    /// 前者该转圈等，后者该说实话。见 `WpsRasterSource::rasterize_page`。
    warm_done: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// 正在跑的那个出图任务的「插队」通道。
    ///
    /// 老师直接跳到第 30 页时，顺序出图还在第 5 页 —— 把请求塞进来，
    /// 下一张就出第 30 页，不必干等 12 秒。
    jump: Mutex<Option<Arc<Mutex<Option<usize>>>>>,
}

impl AppState {
    /// 取（或首次构建）字体索引。
    fn fonts(&self) -> Arc<FontContext> {
        Arc::clone(self.fonts.get_or_init(|| {
            let start = std::time::Instant::now();
            let ctx = FontContext::new();
            log::info!(
                "字体索引构建完成：{} 个字体面，耗时 {:?}",
                ctx.font_count(),
                start.elapsed()
            );
            Arc::new(ctx)
        }))
    }

    /// 取（或首次创建）转换器；本机没有可用的办公软件时返回 `None`。
    fn converter(&self) -> Option<Arc<Converter>> {
        self.converter
            .get_or_init(|| {
                let engine = detect_engine()?;
                log::info!("已启用 {} 作为课件导出内核", engine.display_name());
                // `spawn` 立刻返回：`Application` 在后台线程里预热，
                // 不占用主线程，也不拖慢窗口出现
                Some(Arc::new(Converter::spawn(engine)))
            })
            .clone()
    }

    /// 取（或新建）某个出图目录的「任务已排完」标志。
    ///
    /// 显示层靠它把「还没轮到这一页」和「这一页出不来」分开。
    ///
    /// # 默认必须是「已排完」
    ///
    /// 默认值代表的是「**没有任何出图任务在跑**」这个状态 —— 那一刻缺帧就是
    /// 真的缺，该说实话。把它默认成「还没排完」，就会在没有任务的情况下
    /// 让显示层一直等下去（实测症状：一页原地转圈转到天荒地老）。
    /// 只有 [`AppState::begin_warm`]（任务真的开跑）有资格把它清零。
    fn warm_flag(&self, dir: &str) -> Arc<AtomicBool> {
        let mut map = match self.warm_done.lock() {
            Ok(m) => m,
            Err(_) => return Arc::new(AtomicBool::new(true)),
        };
        Arc::clone(
            map.entry(dir.to_string())
                .or_insert_with(|| Arc::new(AtomicBool::new(true))),
        )
    }

    /// 出图任务开跑：标志清零。
    ///
    /// 必须在**任务开始**时清，而不是第一页出好时：任务一旦开始，
    /// 缺的页就是「还在排队」，前端该转圈等而不是报错。
    fn begin_warm(&self, dir: &str) -> Arc<AtomicBool> {
        let flag = self.warm_flag(dir);
        flag.store(false, Ordering::Relaxed);
        flag
    }

    /// 出图任务收尾（无论成败）：此后缺的页就是真的没有。
    fn mark_warm_done(&self, dir: &str) {
        if let Ok(map) = self.warm_done.lock() {
            if let Some(flag) = map.get(dir) {
                flag.store(true, Ordering::Relaxed);
            }
        }
    }

    /// 请正在跑的出图任务**优先出这一页**。
    ///
    /// 老师直接跳到第 30 页、而顺序出图还在第 5 页时，这一步能把等待
    /// 从十几秒压到一张图的时间（约 0.4 秒）。
    fn request_page(&self, page: usize) {
        if let Ok(slot) = self.jump.lock() {
            if let Some(jump) = slot.as_ref() {
                if let Ok(mut g) = jump.lock() {
                    *g = Some(page);
                }
            }
        }
    }
}

/// 一份已打开的文档及其管线。
struct OpenDoc {
    pipeline: Arc<Pipeline>,
    /// 标注旁挂文件路径（`<课件>.oppv-annot.json`）。
    annotation_path: PathBuf,
}

/// 返回给前端的文档信息。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DocInfo {
    path: String,
    file_name: String,
    title: Option<String>,
    page_count: usize,
    /// 幻灯片尺寸（pt）。
    width_pt: f32,
    height_pt: f32,
    /// 宽高比，供前端布局占位。
    aspect_ratio: f32,
    format: String,
    fingerprint: String,
    /// 是否存在已保存的标注。
    has_annotations: bool,
    /// 当前画面由谁出：`wps`（本机办公软件导出的逐页位图）/ `self`（自研渲染）/ `pdf`。
    ///
    /// 前端据此决定要不要提示老师「当前用的是内置渲染」——
    /// 老师有权知道画面是不是「作者工具的原样」。
    display_source: String,
    /// 是否正在后台等办公软件出**第一页**（本机装了 WPS/Office 且还没出过图）。
    ///
    /// 为真时前端应当**等它出图**，而不是先把自研画面放出来 ——
    /// 自研画面在复杂课件上会出错，先给一份错的再换，老师只会记得那份错的。
    upgrading: bool,
}

/// 启动参数。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct StartupInfo {
    /// 命令行传入的待打开文件。
    file: Option<String>,
    /// 运行环境信息（供设置页展示与问题排查）。
    cpu_threads: usize,
    render_threads: usize,
}

/// 错误转换：统一成前端可直接显示的中文。
fn err_msg(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// 把用户可见的错误写进日志，便于课后排查。
fn log_error(context: &str, e: &str) {
    log::error!("{context}：{e}");
}

/// 「逐页出图」缓存的格式版本。
///
/// 与位图缓存（`ppt-pipeline` 的 `DISK_VERSION`）**分开版本化**：
/// 这些 PNG 是**办公软件**的产物，只有换出图方式（换引擎、改导出参数、
/// 改「按步出图」藏什么）才需要作废，跟渲染内核改了多少行毫无关系。
///
/// # 改动「一步长什么样」必须抬这个号
///
/// 出图任务干完会在目录里留一个 `steps.done` 标记，下次打开就**不再重做**。
/// 所以只改代码不抬版本，旧的分帧会被当成已完成的留在原地 ——
/// 看起来就是「改了没效果」。v2 就是这么白跑一轮的。
///
/// - v2：动画分帧从「只藏形状」扩到「形状 + 段落」
/// - v3：修好段落隐藏的调用方式（带参数的属性必须按属性调，v2 全部静默失败）
const RASTER_CACHE_VERSION: u32 = 3;

/// 出图宽度的下限：再小就糊了（投影仪也不低于这个数）。
const RASTER_MIN_WIDTH: u32 = 1280;
/// 出图宽度的上限：4K 大屏也就到这里，再往上出图时间和体积都不划算。
const RASTER_MAX_WIDTH: u32 = 3840;
/// 档位粒度。屏幕宽度按它向上取整。
///
/// 为什么要有粒度：老师拖窗口大小时宽度是连续变化的，若按每 1 像素都建一套
/// 缓存，同一份课件会散出几十套图，白占磁盘还白出图。
const RASTER_WIDTH_STEP: u32 = 160;

/// 按屏幕宽度定出图档位。
///
/// 「按屏幕自适应」而不是写死 1920：1080p 屏出 1920 正好 1:1 清晰，
/// 4K 大屏出 3840 才不会糊；小屏笔记本也不必白出大图、白等时间。
fn raster_bucket(viewport_width: Option<u32>) -> u32 {
    let w = viewport_width
        .unwrap_or(1920)
        .clamp(RASTER_MIN_WIDTH, RASTER_MAX_WIDTH);
    w.div_ceil(RASTER_WIDTH_STEP) * RASTER_WIDTH_STEP
}

/// 一份课件在某个档位下的出图目录。
///
/// 键是课件的**内容指纹**（`DocumentSource::fingerprint`）而不是路径 ——
/// 老师把同一份课件改一版存在别处，或者原地改内容，指纹都会变，
/// 因此不会拿到旧画面。
fn raster_dir(fingerprint: &str, bucket: u32) -> PathBuf {
    cache_dir()
        .join("wps")
        .join(format!("v{RASTER_CACHE_VERSION}"))
        .join(fingerprint)
        .join(bucket.to_string())
}

/// 待后台逐页出图的课件。
struct PendingWarm {
    /// 拿来出图的文件（旧版 `.ppt` 是缓存里那份转换产物）。
    pptx: PathBuf,
    /// 老师打开的那个文件。出图就绪事件里带的是它 ——
    /// 前端拿它跟界面上显示的文件比对，两边必须是同一个字符串。
    original: PathBuf,
    dir: PathBuf,
    /// 出图像素尺寸（按屏幕自适应算出）。
    width: u32,
    height: u32,
    page_count: usize,
    /// 动画分步的算账人，见 [`StepPlanner`]。
    step_plans: StepPlanner,
}

/// 给一页算出「每一步该藏起什么」。
///
/// 返回第 k 项 = 「已播 k 步」时**不该显示**的东西。形状 id 就是 OOXML 的
/// `p:cNvPr/@id`，也正是办公软件 `Shape.Id` 的值，因此能直接对上。
/// 返回空表示这一页没有动画。
///
/// # 为什么这件事只能在这里做
///
/// 出图在 `ppt-convert` 里（它只管让办公软件画图），解析在 `ppt-format-pptx` 里。
/// 只有 `ppt-app` 同时握着两边，所以由它把「哪一步该藏谁」算出来交给出图方。
///
/// # 为什么用 `visible_in` 而不是自己判读动画类型
///
/// 「出现 / 消失 / 强调」三类的可见性规则已经写在 [`Build::visible_in`] 里，
/// 自研渲染内核走的就是它。这里复用同一份规则，画面与动画才不会各说各话。
///
/// # 形状级和段落级都要管
///
/// 「项目符号逐条弹出」动的是**段落**：形状从头到尾都在（框和底色开局就该
/// 看得见），只有文字一段段出现。只按形状判可见性的话，这类页面会
/// 「一开局全部文字都在，点一下再把已经显示的段落往上面砸一遍」——
/// 这正是老师报的那个现象。
fn step_hide_plans(src: &dyn DocumentSource, page: usize) -> StepPlan {
    let Ok(ppt_core::PageContent::Scene(scene)) = src.page_content(page) else {
        return Vec::new();
    };
    let total = scene.anim.steps.len();
    if total == 0 {
        return Vec::new();
    }

    // 一次收集，之后每步只做过滤 —— 别在循环里反复遍历场景
    let mut nodes: Vec<(u32, ppt_core::scene::Build, Vec<(u32, ppt_core::scene::Build)>)> =
        Vec::new();
    for node in scene.walk() {
        let Some(id) = node.shape_id else {
            continue;
        };
        let paragraphs: Vec<(u32, ppt_core::scene::Build)> = node
            .text
            .as_ref()
            .map(|t| {
                t.paragraphs
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| p.build.is_animated())
                    .map(|(i, p)| (i as u32, p.build))
                    .collect()
            })
            .unwrap_or_default();
        if !node.build.is_animated() && paragraphs.is_empty() {
            continue;
        }
        nodes.push((id, node.build, paragraphs));
    }

    (0..total)
        .map(|k| {
            let state = ppt_core::scene::linear_state(k);
            let mut out = Vec::new();
            for (id, build, paragraphs) in &nodes {
                // 整个形状都不该在：直接藏形状，比逐段藏更彻底
                if !build.visible_in(state) {
                    out.push(ppt_convert::Hide::Shape(*id));
                    continue;
                }
                let hidden: Vec<u32> = paragraphs
                    .iter()
                    .filter(|(_, b)| !b.visible_in(state))
                    .map(|(i, _)| *i)
                    .collect();
                if !hidden.is_empty() {
                    out.push(ppt_convert::Hide::Paragraphs(*id, hidden));
                }
            }
            out
        })
        .collect()
}

/// `build_pipeline` 的结果。
struct Opened {
    pipeline: Arc<Pipeline>,
    info: DocInfo,
    /// 非空表示「这次还没能让办公软件出图，需要后台逐页补上」。
    ///
    /// 画面此刻由自研渲染内核撑着，第一页出好（约 1~2 秒）后前端会收到
    /// `raster-first-page` 并重载，届时就换成办公软件出的画面。
    pending_warm: Option<PendingWarm>,
}

/// 打开一份 pptx：优先用「办公软件已出好的逐页位图」出画面。
///
/// 返回值里的三个元素依次是：
/// (画面来源, 语义来源, 待出图任务)。
///
/// 动画分步的算账人（[`StepPlanner`]）不对外返回：它已经分别装进了
/// 画面来源（读取方）与待出图任务（写入方），两边共用同一份。
///
/// # 为什么画面和语义必须分开
///
/// 逐页位图（以及之前的矢量 PDF）画得 100% 准，但它把超链接、媒体
/// **全都拍平成了像素**。一旦把整条管线都换成它，课件就退化成
/// 「不能点的幻灯片图片」—— 按钮点不动、视频播不了。
///
/// **这两条是这个软件的底线**，所以只要画面不是原始 OOXML，
/// 就必须把 OOXML 一并挂上当作语义来源（见 `Pipeline::with_interaction`）：
///
/// | 问题 | 谁来回答 |
/// |---|---|
/// | 这一页长什么样 | 办公软件出的位图 |
/// | 哪里能点（超链接、动作按钮） | OOXML |
/// | 哪里有视频、字节在哪 | OOXML |
/// | 这一页怎么一步步弹出来 | OOXML 的 `p:timing`，交给办公软件**按步出图** |
/// | 备注写了什么 | OOXML |
///
/// # 为什么首次打开要等一次
///
/// 出图要跑本机办公软件（`Open` 约 1.8s + 每页约 0.4s），这段时间是
/// **物理下限**，绕不过去。但等的是「第一页」而不是「整本」：
/// 第一页好了就先显示，剩下的页在后台排队（见 [`PendingWarm`]）。

/// 提供「某个出图目录的任务是否已排完」标志。
///
/// 解析层需要按出图目录取标志，而目录要等打开课件、算出内容指纹之后才知道 ——
/// 所以这里传回调，而不是把整个 `AppState` 塞进解析层。
type WarmFlagFn<'a> = &'a dyn Fn(&str) -> Arc<AtomicBool>;

#[allow(clippy::type_complexity)]
fn open_pptx(
    path: &Path,
    original: &Path,
    bucket: u32,
    warm_flag: WarmFlagFn<'_>,
) -> Result<(SharedSource, Option<SharedSource>, Option<PendingWarm>), String> {
    // 无论最终用哪条画面路径，OOXML 都要打开：它负责回答所有语义问题
    let src: Arc<PptxSource> = Arc::new(PptxSource::open(path).map_err(err_msg)?);
    let fingerprint = src.fingerprint().to_string();
    let pages = src.page_count();
    let size = src.default_page_size_pt();
    let title = src.title();
    let dir = raster_dir(&fingerprint, bucket);
    let dir_key = dir.to_string_lossy().to_string();
    let ready = ready_count(&dir, pages);
    // 动画分帧也要判：整页图全在了不等于分帧也在了。
    //
    // 少了这一条，老缓存（只有整页图的那一版）会让出图任务**一次都不启动**，
    // 于是分帧永远生不出来，而显示层又一直在等它 —— 表现就是「一直转圈」。
    let step_frames_done = ppt_convert::source::steps_done(&dir);

    // 「哪一步该藏谁」的算账人。
    //
    // 出图方（逐页导出时隐掉还不该出现的形状）与读取方（翻到某一步该取哪张图）
    // **共用同一份** —— 两边步数对不上，帧就对不上。
    //
    // 它只是把 OOXML 的解析器借出去：真正解析发生在老师翻到动画那一步、
    // 或者后台出到那一页时，**不在打开课件这条路上**。
    let step_plans: StepPlanner = {
        // 这里靠的是 `Arc<PptxSource>` → `Arc<dyn DocumentSource>` 的隐式退化，
        // 所以不能写成 `Arc::clone(&src)`（那要求两边类型完全相同）
        let semantic: SharedSource = src.clone();
        Arc::new(move |page: usize| step_hide_plans(semantic.as_ref(), page))
    };

    // 页数不够时**接着把剩下的补上**：
    // 上一次可能是被中断的（老师关了应用），也可能某几页出图失败过。
    // 出图任务会跳过已有的页，所以补做不会从头再来一遍。
    let pending = (ready < pages || !step_frames_done).then(|| PendingWarm {
        pptx: path.to_path_buf(),
        original: original.to_path_buf(),
        dir: dir.clone(),
        width: bucket,
        height: ((bucket as f32 * size.h / size.w.max(1.0)).round() as u32).max(1),
        page_count: pages,
        step_plans: Arc::clone(&step_plans),
    });

    if ready > 0 {
        // 已经有图了：**立刻**用办公软件出的画面，不再让老师看自研渲染。
        // 还缺的那几页会返回「正在生成中」，前端转圈等（见 `WpsRasterSource`）。
        log::info!("画面来源：办公软件出的图（已就绪 {ready}/{pages} 页）");
        let display: SharedSource = Arc::new(WpsRasterSource::new(
            dir,
            bucket as f32 / size.w.max(1.0),
            pages,
            size,
            fingerprint,
            title,
            warm_flag(&dir_key),
            Arc::clone(&step_plans),
        ));
        return Ok((display, Some(src), pending));
    }

    // 一张图都没有：先用自研渲染顶着。
    // 这一行是排查「为什么画面还是自研渲染」的第一手证据。
    log::info!("画面来源：自研渲染内核（办公软件出图尚未就绪，已在后台开始）");
    let display: SharedSource = src;
    Ok((display, None, pending))
}

/// 本机没有办公软件时，打开旧版 `.ppt` 的说明。
const LEGACY_NO_OFFICE_HINT: &str = "这是 PowerPoint 97-2003 的旧版 .ppt 文件。\
     把它转成能放的格式需要本机装有 WPS 或 PowerPoint，这台机器上没找到；\
     请装一个，或先用别的机器把它另存为 .pptx 再拿过来";

/// 打开之前的准备：把**旧版 `.ppt`** 转成能解析的 `.pptx`。
///
/// 97-2003 的二进制格式我们没有自己的解析器。而本机那个办公软件
/// **就是这份课件的作者工具** —— 让它另存为一次，比要求老师自己去
/// 「另存为」靠谱得多：老师双击一份 `.ppt`，期待的是「看到里面的内容」，
/// 而不是「先去学一个转换步骤」。
///
/// 返回**拿来解析**的路径；不是旧版格式时原样返回。
///
/// # 转不成也不能把老师堵死
///
/// 本机没装办公软件、或转换失败（文件损坏、加密、正被别的程序占用）时，
/// 退回原来的行为：交给系统默认程序打开，并说清楚为什么。
fn prepare_source(path: &Path, state: &tauri::State<'_, AppState>) -> Result<PathBuf, String> {
    let mut probe = [0u8; 8];
    let n = std::fs::File::open(path)
        .and_then(|mut f| {
            use std::io::Read;
            f.read(&mut probe)
        })
        .map_err(|e| describe_open_failure(path, &e))?;

    // 两个条件都要满足才动手：**容器是 OLE2** 且**扩展名是演示文稿**。
    // 只看魔数不行 —— `.doc` / `.xls` 与 `.ppt` 共用同一个 OLE2 容器，
    // 只按魔数判断会把一份 Word 文档送去当演示文稿转换。
    let legacy_presentation = detect_format(&probe[..n]) == Some(DocFormat::PptLegacy)
        && detect_format_by_extension(&path.to_string_lossy()) == Some(DocFormat::PptLegacy);
    if !legacy_presentation {
        return Ok(path.to_path_buf());
    }

    let out = legacy_converted_path(path);
    if out.is_file() {
        // 已经转过（键里含大小与修改时间，源文件一改就换一个键）
        log::info!("旧版 .ppt 已有转换产物，直接使用：{}", out.display());
        return Ok(out);
    }

    let Some(converter) = state.converter() else {
        return Err(handoff_to_default_app(path, LEGACY_NO_OFFICE_HINT));
    };

    log::info!("旧版 .ppt 开始转换：{}", path.display());
    let started = std::time::Instant::now();
    if let Err(e) = converter.save_as(path, &out, ppt_convert::SaveAs::Pptx) {
        log::warn!("旧版 .ppt 转换失败：{e}");
        let hint = format!(
            "打不开「{}」：它是 PowerPoint 97-2003 的旧版 .ppt，\
             自动转成 .pptx 没有成功（{e}）",
            file_name_of(path)
        );
        return Err(handoff_to_default_app(path, &hint));
    }
    log::info!(
        "旧版 .ppt 转换完成：耗时 {:.1}s → {}",
        started.elapsed().as_secs_f32(),
        out.display()
    );

    // 同一份课件的旧产物不必留着：键变了就说明源文件变过，旧的再没人用
    drop_stale_legacy(path, &out);
    Ok(out)
}

/// 转换产物的缓存路径。
///
/// 键是「文件名 + 大小 + 修改时间」：老师换了一份同名课件、或在别处改过它，
/// 键都会变，不会拿到上一次的旧产物。这里刻意**不算文件哈希** ——
/// 那要多读一遍整份课件（几十 MB），而我们要的只是「变了就换一个键」。
fn legacy_converted_path(src: &Path) -> PathBuf {
    let meta = std::fs::metadata(src).ok();
    let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let mtime = meta
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    cache_dir()
        .join("legacy")
        .join(format!("{}-{size}-{mtime}.pptx", file_stem_of(src)))
}

/// 删掉同一份课件的旧转换产物（`keep` 之外、同名的那些）。
fn drop_stale_legacy(src: &Path, keep: &Path) {
    let prefix = format!("{}-", file_stem_of(src));
    let Ok(entries) = std::fs::read_dir(cache_dir().join("legacy")) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == keep {
            continue;
        }
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

fn file_stem_of(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "deck".to_string())
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

/// 解析课件路径并建立管线。
///
/// `path` 是**拿来解析**的文件，`original` 是**老师打开的那个文件**。
/// 两者平时是同一个；只有旧版 `.ppt` 不同 —— 它要先转成 `.pptx` 才能解析，
/// 而标题、备注、标注、界面上的文件名都必须用老师的原件
/// （不然标注会写进缓存目录，老师把课件拷走什么也没带走）。
fn build_pipeline(
    path: &Path,
    original: &Path,
    fonts: Arc<FontContext>,
    bucket: u32,
    warm_flag: WarmFlagFn<'_>,
) -> Result<Opened, String> {
    if !path.exists() {
        return Err(format!("文件不存在：{}", path.display()));
    }

    // 用魔数判断真实格式，而不是只看扩展名 ——
    // 老师手里的课件常被改过扩展名
    //
    // **读失败不能装作没看见。** 以前这里 `.ok()` 一吞，转而按扩展名当成正常
    // pptx 往下走，最后抛出来的是解析器里的原始系统错误 ——
    // 老师看到的是「云文件提供程序未运行」这种天书。
    // 而「文件根本没读出来」本身就是一类独立的、可解释的故障。
    let mut probe = [0u8; 8];
    let format = match std::fs::File::open(path).and_then(|mut f| {
        use std::io::Read;
        f.read(&mut probe)
    }) {
        Ok(n) => detect_format(&probe[..n])
            .or_else(|| detect_format_by_extension(&path.to_string_lossy()))
            .ok_or_else(|| {
                "无法识别的文件格式。当前版本支持 .pptx / .pptm / .ppsx / .ppsm / .potx / .potm \
                 课件与 .pdf 文档"
                    .to_string()
            })?,
        Err(e) => return Err(describe_open_failure(path, &e)),
    };

    // OLE2 容器是 .ppt / .doc / .xls 三家共用的，光看魔数分不出是哪一种。
    // 走到这里说明它**不是**演示文稿（是的话在 `prepare_source` 里就转成
    // .pptx 了），那就按扩展名说清楚它到底是什么 ——
    // 别让老师对着一份 Word 文档被告知「你的课件坏了」。
    let format = if format == DocFormat::PptLegacy {
        detect_format_by_extension(&path.to_string_lossy()).unwrap_or(format)
    } else {
        format
    };

    if let Some(hint) = format.unsupported_hint() {
        return Err(handoff_to_default_app(path, hint));
    }

    let (source, interactive, pending_warm): (
        SharedSource,
        Option<SharedSource>,
        Option<PendingWarm>,
    ) = match format {
        DocFormat::Pptx => open_pptx(path, original, bucket, warm_flag)?,
        DocFormat::Pdf => {
            let src = PdfSource::open(path).map_err(err_msg)?;
            (Arc::new(src), None, None)
        }
        other => {
            return Err(format!(
                "暂不支持 {}，请先另存为 .pptx",
                other.display_name()
            ));
        }
    };

    let page_count = source.page_count();
    let size: Size = source.default_page_size_pt();
    // 标题优先取**语义来源**的：画面是办公软件出的位图，它给不出标题
    let title = interactive
        .as_ref()
        .and_then(|s| s.title())
        .or_else(|| source.title());
    let fingerprint = source.fingerprint().to_string();

    let config = PipelineConfig {
        disk_cache_dir: Some(cache_dir()),
        ..PipelineConfig::default()
    };

    // 画面与语义分开：画面是本机办公软件出的位图，
    // 但链接/媒体/动画/备注必须由原始 OOXML 回答（见 `Pipeline::with_interaction`）。
    //
    // 这一条是**底线**：少了它，按钮点不动、视频播不了、逐元素弹出全没了。
    let display_from_office = interactive.is_some();
    let pipeline = Arc::new(
        Pipeline::with_interaction(source, interactive, fonts, config).map_err(err_msg)?,
    );

    // 标注旁挂在**老师的原件**旁边：转出来的 `.pptx` 住在缓存目录里，
    // 标注跟着它走等于没存（缓存会被清理，老师也不会把缓存拷走）
    let annotation_path = annotation_path_for(original);

    let info = DocInfo {
        path: original.to_string_lossy().to_string(),
        file_name: original
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
        title,
        page_count,
        width_pt: size.w,
        height_pt: size.h,
        aspect_ratio: size.aspect_ratio(),
        format: match format {
            DocFormat::Pptx => "pptx".to_string(),
            DocFormat::Pdf => "pdf".to_string(),
            other => other.display_name().to_string(),
        },
        fingerprint,
        has_annotations: annotation_path.exists(),
        // `upgrading` 要问 `AppState`（本机有没有导出内核），
        // `build_pipeline` 拿不到，由 `open_document` 补上
        display_source: if display_from_office {
            "wps"
        } else if matches!(format, DocFormat::Pdf) {
            // 打开的就是 PDF：它本身就是权威画面，不存在「自研」一说
            "pdf"
        } else {
            "self"
        }
        .to_string(),
        // 本机没有出图内核时谈不上「正在出图」，由 `open_document` 决定
        upgrading: false,
    };

    Ok(Opened {
        pipeline,
        info,
        pending_warm,
    })
}

/// 应用缓存目录。
fn cache_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("OpenPPTView").join("cache")
}

/// 导出的媒体放在应用数据目录下的 `media/`。
///
/// 这个位置**必须**与 `tauri.conf.json` 里
/// `assetProtocol.scope` 的 `$APPLOCALDATA/media/**` 一致 ——
/// 因此这里不手写 `%LOCALAPPDATA%`，而是走 Tauri 自己的路径解析，
/// 由它保证两者指向同一个目录。
fn media_root(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    use tauri::Manager;
    app.path()
        .app_local_data_dir()
        .map(|d| d.join("media"))
        .map_err(|e| format!("无法定位应用数据目录：{e}"))
}

/// 标注旁挂文件路径。
///
/// 与课件同目录、以固定后缀命名 —— 这样老师拷贝课件时
/// 标注会跟着走，且**不修改原课件**。
fn annotation_path_for(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "deck".to_string());
    name.push_str(".oppv-annot.json");
    path.with_file_name(name)
}

// ---------- Tauri 命令 ----------

/// 启动信息（前端首帧即需要，因此是最轻量的调用）。
#[tauri::command]
fn startup_info(state: tauri::State<'_, AppState>) -> StartupInfo {
    let _ = &state;
    let file = std::env::args()
        .skip(1)
        .find(|a| !a.starts_with('-') && Path::new(a).exists());

    StartupInfo {
        file,
        cpu_threads: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
        render_threads: ppt_pipeline::recommended_threads(),
    }
}

/// 取走「另一个进程转交过来的课件」。
///
/// 关窗不退出之后，双击课件是**第二个进程**把路径转给已经开着的这个
/// （见 `resident`）。转交同时走两条路：一条 `open-file` 事件，
/// 一条存在这里等着被取 —— 界面注册监听之前发的事件会丢，
/// 所以两条都要走，靠「取走就没了」保证只会打开一次。
#[tauri::command]
fn take_pending_open(resident: tauri::State<'_, resident::Resident>) -> Option<String> {
    resident.take_pending_open()
}

/// 界面回话：标注已经落盘，可以退出进程了。
#[tauri::command]
fn ready_to_quit(app: tauri::AppHandle) {
    resident::mark_ready_to_quit(&app);
}

/// 安装向导里老师选的「操作方式」。
///
/// 前端首启时优先用它，而不是靠 `(pointer: coarse)` 猜 —— 一体机、
/// 带触摸屏的笔记本上那个探测结果并不可靠（插上鼠标就说自己是鼠标）。
/// 绿色版 / 开发时直接跑 exe 读不到，返回 `None`，前端退回自动识别。
#[tauri::command]
fn install_preference() -> Option<install_pref::InstallPreference> {
    install_pref::read()
}

/// 通知前端「办公软件的第一页已经出好了」。
#[derive(Serialize, Clone)]
struct RasterReady {
    path: String,
    /// 已就绪的页数（前端可用来显示进度）。
    ready_pages: usize,
    page_count: usize,
}

/// 后台让本机办公软件**逐页出图**，第一页好了就通知前端重载。
///
/// # 为什么等的是第一页而不是整本
///
/// 实测（174MB / 39 页）：`Open` 1784ms，出首页 404ms，整本导出 7178ms。
/// 「打开就能看到第一页」只需要前两项 ≈ 2.2 秒，而整本要 9 秒以上 ——
/// 老师感受到的「翻页一翻就得半天」正是后者造成的。
///
/// 所以这里只把「第一页好了」当作触发点，剩下的页在后台慢慢排队；
/// 老师翻到哪页基本都已经好了（每页约 0.4s，比翻页快）。
///
/// # 为什么走事件通知而不是让打开请求等
///
/// 打开请求已经返回了一份能看的画面（自研渲染），出图是**锦上添花**：
/// 出好之前不该打断任何人。前端收到事件后重载一次即可。
fn kick_off_warm(app: tauri::AppHandle, state: &AppState, job: PendingWarm) {
    let Some(converter) = state.converter() else {
        // 本机没装 WPS / Office 是**正常情况**，不是故障：
        // 这份课件继续用自研内核渲染即可，不需要打扰老师
        log::info!("未检测到 WPS / Office，本份课件使用自研渲染内核");
        return;
    };

    let key = job.dir.to_string_lossy().to_string();

    // 同一份课件只排一次队：来回翻页会反复走到这里
    {
        let Ok(mut set) = state.warming.lock() else {
            return;
        };
        if !set.insert(key.clone()) {
            return;
        }
    }

    // 插队通道交给 AppState，前端「翻到还没出的页」时能立刻把它提到最前
    let jump = Arc::new(Mutex::new(None));
    if let Ok(mut slot) = state.jump.lock() {
        *slot = Some(Arc::clone(&jump));
    }

    // 通知前端的那条通道。转换线程每出一页就发一个页码过来。
    let (page_tx, page_rx) = std::sync::mpsc::channel::<usize>();
    // 事件里带的是**老师打开的那个文件**：前端拿它跟界面上显示的文件比对，
    // 旧版 `.ppt` 的转换产物住在缓存里，报那个路径前端一个都认不出来
    let path = job.original.to_string_lossy().to_string();
    let dir_for_log = key.clone();
    let page_count = job.page_count;
    let started = std::time::Instant::now();

    // 事件转发：从通道读到页码就推给前端。
    //
    // 单独起一条线程是为了**不阻塞转换线程** —— 转换线程要连轴转地出图，
    // 不能被 `emit` 或前端处理拖慢。
    {
        let app = app.clone();
        let path = path.clone();
        let dir = key.clone();
        std::thread::Builder::new()
            .name("oppv-warm-events".to_string())
            .spawn(move || {
                while let Ok(page) = page_rx.recv() {
                    // 第一页就绪 = 画面可以换成办公软件出的了
                    if page == 0 {
                        let ready = ppt_convert::source::ready_count(Path::new(&dir), page_count);
                        log::info!("办公软件出的第 1 页已就绪（{ready}/{page_count}）");
                        let _ = app.emit(
                            "raster-first-page",
                            RasterReady {
                                path: path.clone(),
                                ready_pages: ready,
                                page_count,
                            },
                        );
                    }
                }
            })
            .ok();
    }

    let started_dir = key.clone();
    let request = ppt_convert::WarmRequest {
        pptx: job.pptx.clone(),
        dir: job.dir.clone(),
        width: job.width,
        height: job.height,
        page_count: job.page_count,
        on_page: Some(page_tx),
        jump: Arc::clone(&jump),
        step_plans: Some(Arc::clone(&job.step_plans)),
    };

    let spawned = std::thread::Builder::new()
        .name("oppv-warm-job".to_string())
        .spawn(move || {
            use tauri::Manager;
            let result = converter.warm_pages(request);

            // 整本排完了（无论成败）：
            // 1. 置位「已排完」，让显示层把「还没轮到」和「出不来」分开
            // 2. 放掉闸门，否则这份课件再也不会被重试
            if let Some(state) = app.try_state::<AppState>() {
                state.mark_warm_done(&started_dir);
                if let Ok(mut set) = state.warming.lock() {
                    set.remove(&started_dir);
                }
                if let Ok(mut slot) = state.jump.lock() {
                    // 只清自己那一份，期间老师可能已经开了另一份课件
                    if slot.as_ref().is_some_and(|j| Arc::ptr_eq(j, &jump)) {
                        *slot = None;
                    }
                }
            }

            match result {
                Ok(()) => log::info!(
                    "逐页出图全部完成：耗时 {:.1}s（{}）",
                    started.elapsed().as_secs_f32(),
                    started_dir
                ),
                Err(e) => log::warn!("逐页出图失败，继续使用自研渲染：{e}"),
            }
            let _ = dir_for_log;
        });

    if spawned.is_err() {
        if let Ok(mut set) = state.warming.lock() {
            set.remove(&key);
        }
        state.mark_warm_done(&key);
        log::warn!("无法启动出图任务线程");
    }
}

/// 打开课件。
#[tauri::command(async)]
fn open_document(
    path: String,
    viewport_width: Option<u32>,
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<DocInfo, String> {
    let p = PathBuf::from(&path);
    // 出图档位按老师屏幕宽度定（前端把物理像素宽传上来，见 `web/app.js`）——
    // 1080p 屏出 1920 正好 1:1，4K 大屏出 3840 才不糊
    let bucket = raster_bucket(viewport_width);

    // 旧版 `.ppt` 先转成 `.pptx` 才能解析（见 `prepare_source`）。
    // `content` 是拿去解析的文件，`p` 始终是老师打开的那个文件。
    let content = prepare_source(&p, &state).map_err(|e| {
        log_error("打开课件失败", &e);
        e
    })?;

    let Opened {
        pipeline,
        info,
        pending_warm,
    } = build_pipeline(&content, &p, state.fonts(), bucket, &|dir| state.warm_flag(dir))
        .map_err(|e| {
            log_error("打开课件失败", &e);
            e
        })?;

    // 替换旧文档。
    //
    // **旧管线的关停必须放到锁外**：`shutdown()` 要等工作线程把手上的
    // 渲染做完再退出（PDF 一页 50~360ms，排队时更久）。如果关停时还握着
    // `state.doc`，这期间所有 `render_page` 都会卡在等锁上 ——
    // 前端看到的就是「一直转圈加载」。锁里只做一次替换，代价恒定。
    let old = {
        let mut guard = state.doc.lock().map_err(err_msg)?;
        let old = guard.take();
        *guard = Some(OpenDoc {
            pipeline: Arc::clone(&pipeline),
            annotation_path: annotation_path_for(&p),
        });
        old
    };
    if let Some(old) = old {
        old.pipeline.shutdown();
    }

    if let Ok(mut recent) = state.recent.lock() {
        recent.retain(|r| r != &path);
        recent.insert(0, path.clone());
        recent.truncate(15);
    }

    log::info!(
        "已打开 {}（{} 页，{:.0}×{:.0}pt）",
        info.file_name,
        info.page_count,
        info.width_pt,
        info.height_pt
    );

    // 出图任务在后台慢慢跑。
    // 放在最后：绝不能让「出图内核的准备工作」挡在打开课件这条路上。
    let mut info = info;
    if let Some(job) = pending_warm {
        // 一张图都还没有 = 老师看到的还是自研渲染，前端该显示「正在生成画面」；
        // 已经有图了（只是没出全）= 画面已经是办公软件出的，**不必**再等，
        // 缺的页由显示层逐页报「正在生成」即可
        info.upgrading = info.display_source == "self" && state.converter().is_some();
        // 任务开跑：把「已排完」清零，此后缺的页都是「还在排队」
        state.begin_warm(&job.dir.to_string_lossy());
        kick_off_warm(app, &state, job);
    } else if info.display_source == "self" {
        // 走不到（自研画面必然伴随待出图任务），留个保险：
        // 万一状态不一致，也别让前端误以为在等
        log::warn!("画面来源是自研但没有任何出图任务，请检查 open_pptx 的分支");
    }

    Ok(info)
}

/// 取当前文档的管线句柄。
fn with_pipeline<T>(
    state: &tauri::State<'_, AppState>,
    f: impl FnOnce(&Pipeline) -> Result<T, String>,
) -> Result<T, String> {
    let guard = state.doc.lock().map_err(err_msg)?;
    let doc = guard.as_ref().ok_or_else(|| "尚未打开课件".to_string())?;
    f(&doc.pipeline)
}

/// 与 [`with_pipeline`] 相同，但**保留结构化错误**。
///
/// 用于区分「这一页还在生成中」（正常的等待）和「这一页渲染不出来」（真故障）——
/// 前者要转圈等，后者要报错，混在一起就会把等待显示成红叉。
fn with_pipeline_typed<T>(
    state: &tauri::State<'_, AppState>,
    f: impl FnOnce(&Pipeline) -> ppt_core::Result<T>,
) -> Result<T, String> {
    let guard = state.doc.lock().map_err(err_msg)?;
    let doc = guard.as_ref().ok_or_else(|| "尚未打开课件".to_string())?;
    f(&doc.pipeline).map_err(|e| {
        // 「正在生成」用约定前缀传出去，前端据此显示进度而不是错误
        if e.is_not_ready() {
            format!("{NOT_READY_PREFIX}{e}")
        } else {
            e.to_string()
        }
    })
}

/// 「这一页正在生成」的错误前缀。
///
/// Tauri 的命令错误只能是一个字符串，所以用约定的前缀携带这个语义。
/// 前缀取一个正常错误消息里绝不会出现的形状，避免误判。
const NOT_READY_PREFIX: &str = "\u{1}not-ready\u{1}";

/// 「这一页正在生成」时，最多在原地等多久。
///
/// # 为什么只有这么短
///
/// 曾经是 3 秒，想把「刚好还没轮到这一页」的等待藏起来。但这段时间是
/// **死等**：一个 worker 线程被按住 3 秒，而这期间缩略图那一批请求
/// 也在各自死等 —— 几个缩略图就能把工作线程占满，老师扭头翻页时
/// 请求排在它们后面，手感就是「翻页反应慢」。
///
/// 现在只等一小会儿（够 absorb 一次很快的出图），剩下的等待交给前端轮询：
/// 轮询不占后端线程，等待期间后端该干嘛干嘛。
const NOT_READY_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

/// 轮询间隔。短到不拖慢响应，长到不占满 CPU。
const NOT_READY_POLL: std::time::Duration = std::time::Duration::from_millis(60);

/// 取「画面来源已经写好的那张 PNG」的字节。
///
/// 有就返回它 —— 这条快路省掉整条「解码 → 缩放 → 转预乘 → 传原始像素」的链，
/// 见 [`ppt_core::DocumentSource::raster_png_path`]。
///
/// 返回 `None` 表示这一步还没有现成的图（或来源不出这种图），
/// 调用方走正常渲染路径 —— 那条路会如实报「正在生成」。
fn raster_png(
    state: &tauri::State<'_, AppState>,
    page: usize,
    play_state: ppt_core::scene::PlayState,
    scale: f32,
) -> Option<Vec<u8>> {
    let guard = state.doc.lock().ok()?;
    let doc = guard.as_ref()?;
    // 缩略图这类「只要原图零头」的请求别走快路：传一整页 PNG 过去、
    // 解码后按整页分辨率占内存，39 张就能吃掉几百 MB。
    // 它们该走原来那条「后端缩好再传小的」的路。
    if !doc.pipeline.wants_native_raster(scale) {
        return None;
    }
    let path = doc.pipeline.display_png_path(page, play_state)?;
    match std::fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(e) => {
            // 文件刚好被删/被占：不值得报错，走正常路径即可
            log::debug!("读现成 PNG 失败（{}）：{e}", path.display());
            None
        }
    }
}

/// 渲染一页并返回**帧字节**。
///
/// 两种载荷（前端靠头 4 字节分辨，见 `web/app.js` 的 `decodeFrame`）：
///
/// - 办公软件已经写好的 **PNG 文件原样**：后端零解码、传输量小一个数量级
/// - 自家渲染的**原始像素帧**：`[宽 u32le][高 u32le][直通 RGBA8…]`
///
/// `played` 是「已经播过的步号」（从 1 起）；不传（`None`）表示全部显示，
/// 静态导出、缩略图与首屏都走这条路径。
///
/// 为什么传的是一个**列表**而不是「推进到第几步」：触发器动画允许跳着播
/// （先点按钮播第 3 步、再点空白播第 1 步），单个数表达不了这种状态。
///
/// `hide` 是要在底帧里**摘掉**的形状：强调动画（放大、旋转）的前端合成
/// 需要它 —— 否则「静止的原件 + 动起来的图层」会叠成重影。
#[tauri::command(async)]
fn render_page(
    page: usize,
    scale: f32,
    played: Option<Vec<u32>>,
    hide: Option<Vec<u32>>,
    state: tauri::State<'_, AppState>,
) -> Result<tauri::ipc::Response, String> {
    let started = std::time::Instant::now();
    let play_state = play_state(played);
    let hide = hide.unwrap_or_default();

    // 快路：这一步已经有现成的 PNG 就直接把文件传过去。
    //
    // 「读 → 解码 → 缩放 → 转预乘 → 传 8MB 原始像素」整条链都省了，
    // 解码与缩放交给浏览器（有 GPU）。老师点的每一下、翻的每一页
    // 基本都走这条路 —— 这是手感从「有点慢」变「跟手」的关键。
    //
    // 有 `hide` 时必须走下面那条：那一帧要「摘掉某些形状」，
    // 只有认识形状的来源做得到。
    if hide.is_empty() {
        if let Some(png) = raster_png(&state, page, play_state, scale) {
            return Ok(tauri::ipc::Response::new(png));
        }
    }

    // 真正画一帧。返回类型保留结构化错误，好让调用方区分
    // 「还在生成」与「画不出来」。
    let draw = |state: &tauri::State<'_, AppState>| -> Result<Arc<ppt_core::Bitmap>, String> {
        with_pipeline_typed(state, |p| {
            if hide.is_empty() {
                let (bmp, _) = p.render_now_at(page, scale, play_state)?;
                Ok(bmp)
            } else {
                // 有隐藏列表时不走缓存（见 `render_frame_hiding`）
                Ok(Arc::new(p.render_frame_hiding(
                    page, scale, play_state, &hide,
                )?))
            }
        })
    };

    let bitmap = match draw(&state) {
        Ok(b) => b,
        Err(e) if e.starts_with(NOT_READY_PREFIX) => {
            // 这一页办公软件还没出到：请出图任务**插队**先出它，
            // 然后在原地等一小会儿 —— 通常 0.4 秒就好了，
            // 老师连「等待」都不该察觉。
            state.request_page(page);
            let deadline = std::time::Instant::now() + NOT_READY_WAIT;
            loop {
                std::thread::sleep(NOT_READY_POLL);
                match draw(&state) {
                    Ok(b) => break b,
                    Err(e2) if e2.starts_with(NOT_READY_PREFIX) => {
                        if std::time::Instant::now() >= deadline {
                            // 等不到了：交给前端显示「正在生成第 N 页」，
                            // **绝不**在这里换成自研渲染 —— 那会亮出画错的画面
                            return Err(e2);
                        }
                    }
                    Err(e2) => return Err(e2),
                }
            }
        }
        Err(e) => return Err(e),
    };

    // 原始帧而非 PNG：这些像素的唯一去处是前端的 <canvas>，
    // 走 PNG 要多付一次 66ms/页 的压缩（见 `encode_frame` 的说明）
    let frame = ppt_render::encode_frame(&bitmap).map_err(err_msg)?;

    // 只记录「明显偏慢」的那些次。
    //
    // 老师反馈「翻页卡」时，日志是唯一能区分「渲染慢」「编码慢」
    // 「IPC 慢」的证据；而每一次都记又会被正常情况（个位数毫秒）淹没。
    // 60ms 的门槛大约是「肉眼开始觉得顿一下」的位置。
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    if elapsed_ms > 60.0 {
        log::info!(
            "第 {} 页渲染偏慢：{:.0}ms（{}×{}，缩放 {:.2}，载荷 {:.1}MB）",
            page + 1,
            elapsed_ms,
            bitmap.width,
            bitmap.height,
            scale,
            frame.len() as f64 / 1048576.0
        );
    }

    Ok(tauri::ipc::Response::new(frame))
}

/// 把「已播步号列表」折成渲染用的位掩码。
fn play_state(played: Option<Vec<u32>>) -> ppt_core::scene::PlayState {
    match played {
        None => ppt_core::scene::PLAY_ALL,
        Some(list) => list
            .iter()
            .fold(0u64, |acc, &s| acc | ppt_core::scene::step_bit(s)),
    }
}

/// 同步渲染并返回尺寸信息（供前端做布局占位）。
#[tauri::command(async)]
fn render_page_meta(
    page: usize,
    scale: f32,
    state: tauri::State<'_, AppState>,
) -> Result<(u32, u32), String> {
    with_pipeline(&state, |p| {
        let (bmp, _) = p.render_now(page, scale).map_err(err_msg)?;
        Ok((bmp.width, bmp.height))
    })
}

/// 本页的动画播放序列与转场。
///
/// 前端据此决定「这一下点击是推进动画、触发某一步，还是翻页」，
/// 以及翻到本页时该播什么转场。
#[tauri::command(async)]
fn page_anim(page: usize, state: tauri::State<'_, AppState>) -> Result<PageAnim, String> {
    with_pipeline(&state, |p| {
        let scene = p.scene(page).map_err(err_msg)?;
        let steps = scene
            .anim
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let (trigger, shape_id) = match s.trigger {
                    ppt_core::scene::StepTrigger::OnClick => ("click", None),
                    ppt_core::scene::StepTrigger::AfterPrevious => ("auto", None),
                    ppt_core::scene::StepTrigger::OnShape(id) => ("shape", Some(id)),
                };
                PageAnimStep {
                    index: i as u32 + 1,
                    trigger,
                    // 触发器形状的包围盒（pt 空间）：前端拿它做命中测试
                    rect: shape_id.and_then(|id| scene.node_bounds(id)),
                    shape_id,
                    targets: s.targets.len(),
                    exit: s.is_exit(),
                    dur_ms: s.dur_ms,
                    // 有「过程」的步才需要去取图层；纯出现/消失只要两帧淡入
                    animated: s.targets.iter().any(|t| !t.is_static()),
                    is_emphasis: !s.changes_visibility(),
                }
            })
            .collect();
        Ok(PageAnim {
            steps,
            transition: scene.transition.clone(),
        })
    })
}

/// 一步动画的前端视图。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PageAnimStep {
    /// 步号（从 1 起），与 `render_page` 的 `played` 对应。
    index: u32,
    /// `click`（点空白）| `auto`（上一动画之后）| `shape`（点指定形状）。
    trigger: &'static str,
    /// 触发形状的包围盒（pt）。
    rect: Option<ppt_core::scene::Rect>,
    /// 触发形状的课件内 id。
    shape_id: Option<u32>,
    /// 本步涉及的对象个数（0 表示这一步只占一次点击、没有可见变化）。
    targets: usize,
    /// 是否是「消失」。
    exit: bool,
    /// 本步动画时长（毫秒）。
    dur_ms: u32,
    /// 本步有没有**过程**（位移/缩放/旋转/透明度/擦除）。
    ///
    /// 有过程时前端会去取「图层」，按 60fps 自己合成；
    /// 没有过程（纯出现/消失）只要两帧淡入就够了。
    animated: bool,
    /// 是否是强调动画（不改变可见性）。
    is_emphasis: bool,
}

/// 一页的动画与转场。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PageAnim {
    steps: Vec<PageAnimStep>,
    transition: ppt_core::scene::Transition,
}

/// 某一步动画的图层清单（不含像素）。
///
/// 像素单独取：一次 IPC 把十来张 PNG 塞进 JSON 不划算 ——
/// 要么 base64 膨胀三分之一，要么变成几万个数字的数组。
///
/// `played` 是**当前**已播的步号（含这一步）；底帧状态由后端把它摘掉，
/// 免得前端漏算 —— 图层要画的是「这一步开始之前」的样子。
#[tauri::command(async)]
fn anim_layers(
    page: usize,
    step: u32,
    played: Option<Vec<u32>>,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<AnimLayer>, String> {
    let before = play_state(played) & !ppt_core::scene::step_bit(step);
    with_pipeline(&state, |p| {
        let scene = p.scene(page).map_err(err_msg)?;
        let Some(s) = scene.anim.steps.get(step.saturating_sub(1) as usize) else {
            return Ok(Vec::new());
        };
        Ok(s.targets
            .iter()
            // 「没有任何变化」的目标没有图层可给（只占了这一步的点击）
            .filter(|t| !t.is_static())
            .filter_map(|t| {
                Some(AnimLayer {
                    shape_id: t.shape_id,
                    // 位置用渲染端同一套算法算，避免两边对不上而错位
                    rect: ppt_render::layer_bounds(&scene, t.shape_id, before, &[t.from, t.to])?,
                    from: t.from,
                    to: t.to,
                    mask: t.mask,
                    para_range: t.para_range,
                })
            })
            .collect())
    })
}

/// 取一个图层的像素（PNG，带透明通道）。
#[tauri::command(async)]
fn anim_layer_png(
    page: usize,
    step: u32,
    shape_id: u32,
    first_para: Option<u32>,
    last_para: Option<u32>,
    played: Option<Vec<u32>>,
    scale: f32,
    state: tauri::State<'_, AppState>,
) -> Result<tauri::ipc::Response, String> {
    let before = play_state(played) & !ppt_core::scene::step_bit(step);
    let paragraphs = first_para.map(|st| (st, last_para.unwrap_or(u32::MAX)));
    let layer = with_pipeline(&state, |p| {
        // 形变范围从这一步的目标上取：放大过程会超出静止时的包围盒
        let extra = p
            .scene(page)
            .ok()
            .and_then(|scene| {
                scene
                    .anim
                    .steps
                    .get(step.saturating_sub(1) as usize)?
                    .targets
                    .iter()
                    .find(|t| t.shape_id == shape_id)
                    .map(|t| vec![t.from, t.to])
            })
            .unwrap_or_default();
        p.render_layer(page, shape_id, paragraphs, before, &extra, scale)
            .map_err(err_msg)
    })?;
    let Some((bmp, _)) = layer else {
        // 图层渲染不出来时给一个 1×1 的透明帧，
        // 前端拿到空图会跳过这一层，不必为「没有图层」再设计一种错误
        return Ok(tauri::ipc::Response::new(ppt_render::empty_frame()));
    };
    let frame = ppt_render::encode_frame(&bmp).map_err(err_msg)?;
    Ok(tauri::ipc::Response::new(frame))
}

/// 一个动画图层的前端视图。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AnimLayer {
    /// 课件里的形状 id（`p:cNvPr/@id`），取像素时要用。
    shape_id: u32,
    /// 图层在画布 pt 空间占的矩形。
    rect: ppt_core::scene::Rect,
    /// 起点形变状态。
    from: ppt_core::scene::EffectState,
    /// 终点形变状态。
    to: ppt_core::scene::EffectState,
    /// 遮罩式揭示（擦除一类）。
    mask: Option<ppt_core::scene::MaskKind>,
    /// 只保留文本框的第 `st..=end` 段（`p:txEl/p:pRg`）；`None` 表示整框。
    para_range: Option<(u32, u32)>,
}

/// 异步预取一页。
#[tauri::command]
fn prefetch_page(page: usize, state: tauri::State<'_, AppState>) -> Result<(), String> {
    with_pipeline(&state, |p| {
        p.prefetch(page, Priority::Adjacent);
        Ok(())
    })
}

/// 预取当前页周围（前后各 2 页）。
#[tauri::command]
fn prefetch_around(page: usize, state: tauri::State<'_, AppState>) -> Result<(), String> {
    with_pipeline(&state, |p| {
        p.prefetch_around(page, 2, 2);
        Ok(())
    })
}

/// 批量生成缩略图。
#[tauri::command]
fn prefetch_thumbnails(scale: f32, state: tauri::State<'_, AppState>) -> Result<(), String> {
    with_pipeline(&state, |p| {
        p.prefetch_thumbnails(scale);
        Ok(())
    })
}

/// 后台把整本课件按原生档预热一遍。
///
/// 只对刚换成矢量来源（PDF）的课件有意义 —— 那类来源每页都要重新解释
/// 内容流，不预热就会「翻到哪页卡哪页」。详见 `Pipeline::prewarm_all`。
#[tauri::command]
fn prewarm_all(state: tauri::State<'_, AppState>) -> Result<(), String> {
    with_pipeline(&state, |p| {
        p.prewarm_all();
        Ok(())
    })
}

/// 切换缩放档（作废旧档位的预取任务）。
#[tauri::command]
fn set_scale(scale: f32, state: tauri::State<'_, AppState>) -> Result<(), String> {
    with_pipeline(&state, |p| {
        p.set_scale(scale);
        Ok(())
    })
}

/// 取演讲者备注。
#[tauri::command(async)]
fn page_notes(page: usize, state: tauri::State<'_, AppState>) -> Result<Option<String>, String> {
    let guard = state.doc.lock().map_err(err_msg)?;
    let doc = guard.as_ref().ok_or_else(|| "尚未打开课件".to_string())?;
    // 备注读取失败不应影响放映，降级为「无备注」
    Ok(doc.pipeline.notes(page).unwrap_or(None))
}

/// 读取标注（返回原始 JSON 字符串，结构由前端定义）。
#[tauri::command(async)]
fn load_annotations(state: tauri::State<'_, AppState>) -> Result<Option<String>, String> {
    let guard = state.doc.lock().map_err(err_msg)?;
    let doc = guard.as_ref().ok_or_else(|| "尚未打开课件".to_string())?;
    if !doc.annotation_path.exists() {
        return Ok(None);
    }
    match std::fs::read_to_string(&doc.annotation_path) {
        Ok(s) => Ok(Some(s)),
        Err(e) => {
            // 标注文件损坏不应阻止课件打开
            log::warn!("读取标注失败（已忽略）：{e}");
            Ok(None)
        }
    }
}

/// 保存标注。
#[tauri::command]
fn save_annotations(json: String, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let guard = state.doc.lock().map_err(err_msg)?;
    let doc = guard.as_ref().ok_or_else(|| "尚未打开课件".to_string())?;
    std::fs::write(&doc.annotation_path, json)
        .map_err(|e| format!("保存标注失败：{e}"))?;
    Ok(())
}

/// 当前课件是否有未保存的标注（前端关闭窗口时询问）。
#[tauri::command]
fn annotations_exist(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    let guard = state.doc.lock().map_err(err_msg)?;
    Ok(guard
        .as_ref()
        .map(|d| d.annotation_path.exists())
        .unwrap_or(false))
}

/// 最近打开列表。
#[tauri::command]
fn recent_files(state: tauri::State<'_, AppState>) -> Vec<String> {
    state
        .recent
        .lock()
        .map(|r| r.clone())
        .unwrap_or_default()
}

/// 关闭当前课件。
#[tauri::command]
fn close_document(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let mut guard = state.doc.lock().map_err(err_msg)?;
    if let Some(doc) = guard.take() {
        doc.pipeline.shutdown();
    }
    Ok(())
}

/// 进入 / 退出放映模式。
///
/// # 为什么由 Rust 侧控制窗口而不是前端 CSS 全屏
///
/// 老师讲课时的实际需求（对标 WPS 放映）：
/// - **真正的全屏**：连任务栏一起盖住，投影上不能露出桌面；
/// - **禁止误退出**：不能因为不小心点到别的窗口就退出放映；
/// - **投影仪友好**：退出后要精确恢复到放映前的窗口位置与大小。
///
/// 这些都需要操作原生窗口，浏览器的 Fullscreen API 做不到
/// （它受浏览器安全策略限制，且无法置顶、无法禁止休眠）。
#[tauri::command]
fn set_presentation_mode(
    on: bool,
    window: tauri::Window,
) -> Result<(), String> {
    if on {
        // 记住退出时要恢复的尺寸，避免退出后窗口变成奇怪的形状
        if let Ok(size) = window.inner_size() {
            if let Ok(pos) = window.outer_position() {
                PRESENT_RESTORE.with(|r| {
                    *r.borrow_mut() = Some((pos.x, pos.y, size.width, size.height));
                });
            }
        }

        window.set_fullscreen(true).map_err(err_msg)?;
        window
            .set_always_on_top(true)
            .map_err(err_msg)?;
        // 光标的显隐交给前端 CSS 控制，这里**不要**动它。
        //
        // `set_cursor_visible(false)` 是窗口级的隐藏（系统层不画光标），
        // 前端再用 `cursor: crosshair` 也换不回来 —— 结果是「鼠标」模式下
        // 全屏后完全看不到光标，老师既点不准按钮也不知道自己在哪。
        // WPS 的做法是「鼠标动就显示、停一会儿再隐藏」，那必须由 CSS 做。
        log::info!("进入放映模式");
    } else {
        window.set_fullscreen(false).map_err(err_msg)?;
        window.set_always_on_top(false).map_err(err_msg)?;

        // 恢复放映前的窗口几何（有些窗口管理器在全屏退出后不还原）
        let saved = PRESENT_RESTORE.with(|r| r.borrow_mut().take());
        if let Some((x, y, w, h)) = saved {
            let _ = window.set_size(tauri::PhysicalSize::new(w, h));
            let _ = window.set_position(tauri::PhysicalPosition::new(x, y));
        }
        log::info!("退出放映模式");
    }

    // 让窗口获得焦点，否则全屏后键盘事件可能仍落在其它窗口
    let _ = window.set_focus();
    Ok(())
}

/// 当前是否处于全屏（用于同步状态）。
#[tauri::command]
fn is_presentation_mode(window: tauri::Window) -> Result<bool, String> {
    window.is_fullscreen().map_err(err_msg)
}

thread_local! {
    /// 放映前的窗口几何：`(x, y, width, height)`。
    ///
    /// 用 `thread_local` 而非全局静态：Tauri 的窗口操作都在主线程，
    /// 避免为这个简单状态引入锁与跨线程语义。
    static PRESENT_RESTORE: std::cell::RefCell<Option<(i32, i32, u32, u32)>> =
        const { std::cell::RefCell::new(None) };
}

/// 真的退出应用（不是关窗）。
///
/// 关窗现在只是隐藏窗口、进程留在后台（见 `resident`），
/// 所以这里不能再用 `window.close()` —— 那只会把窗口藏起来。
#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    resident::quit(&app);
}

/// 运行时统计（供设置页与问题排查展示）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeStats {
    cached_bitmaps: usize,
    cached_scenes: usize,
    cached_bytes: usize,
    hits: u64,
    misses: u64,
    dropped_tasks: u64,
    pending_tasks: usize,
}

#[tauri::command]
fn runtime_stats(state: tauri::State<'_, AppState>) -> Result<RuntimeStats, String> {
    with_pipeline(&state, |p| {
        Ok(RuntimeStats {
            cached_bitmaps: p.cached_bitmap_count(),
            cached_scenes: p.cached_scene_count(),
            cached_bytes: p.cached_bitmap_bytes(),
            hits: p.hit_count(),
            misses: p.miss_count(),
            dropped_tasks: p.dropped_count(),
            pending_tasks: p.pending_tasks(),
        })
    })
}

/// 未使用导入守卫：`Deserialize` 供后续命令的参数结构使用。
#[allow(dead_code)]
fn _assert_serde_imports<T: for<'de> Deserialize<'de>>() {}

/* ---------------- 设置：缓存与日志 ---------------- */

/// 应用数据目录（`%LOCALAPPDATA%\OpenPPTView`）。
fn data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("OpenPPTView")
}

/// 日志目录。
///
/// 与 [`env_logger_init`] 共用这一处定义：两边各写一份，改目录时必会漏掉一处，
/// 而症状是「设置页点『打开日志』打开的是个空文件夹」，最难查。
fn log_dir() -> PathBuf {
    data_dir().join("logs")
}

/// 递归算一个目录的字节数（目录不存在算 0）。
fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => dir_size(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

/// 各类缓存的占用（设置页「存储」用）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CacheUsage {
    total: u64,
    /// 办公软件出的逐页图与动画分帧（`cache/wps`）。
    raster: u64,
    /// 自研渲染的位图缓存（`cache/v<N>`）。
    bitmap: u64,
    /// 旧版 .ppt 的转换产物（`cache/legacy`）。
    legacy: u64,
}

fn measure_cache() -> CacheUsage {
    let cache = cache_dir();
    let raster = dir_size(&cache.join("wps"));
    let legacy = dir_size(&cache.join("legacy"));
    // 位图缓存是 `cache/v<数字>/…`；命名规则由那边的 `is_version_dir` 定
    let bitmap = std::fs::read_dir(&cache)
        .ok()
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| {
                    ppt_pipeline::cache::is_version_dir(&e.file_name().to_string_lossy())
                })
                .map(|e| dir_size(&e.path()))
                .sum()
        })
        .unwrap_or(0);
    CacheUsage {
        total: raster + bitmap + legacy,
        raster,
        bitmap,
        legacy,
    }
}

#[tauri::command]
fn cache_usage() -> CacheUsage {
    measure_cache()
}

/// 清理可以重新生成的缓存。
///
/// `kind` 取 `bitmap` / `raster` / `legacy` / `all`，返回清理后的占用。
///
/// # 为什么办公软件出的图要「留着当前这份」
///
/// 那些图**就是画面来源**，不是副产物。把正在讲的这份也删了，老师下一页
/// 就要等它重新出图。清理缓存不该让正在上的课变卡，所以当前课件的那份跳过，
/// 只清别的课件留下的。
#[tauri::command(async)]
fn clear_cache(kind: String, state: tauri::State<'_, AppState>) -> Result<CacheUsage, String> {
    let cache = cache_dir();
    let all = kind == "all";

    if all || kind == "bitmap" {
        let has_doc = {
            let guard = state.doc.lock().map_err(err_msg)?;
            match guard.as_ref() {
                Some(doc) => {
                    // 交给管线清：它连内存缓存与渲染器内部缓存一起清，
                    // 只删磁盘文件会留下一堆指向旧像素的内存副本
                    doc.pipeline.clear_caches();
                    true
                }
                None => false,
            }
        };
        if !has_doc {
            // 没有打开课件时没有管线可用，直接按命名规则删版本目录
            if let Ok(entries) = std::fs::read_dir(&cache) {
                for entry in entries.flatten() {
                    if ppt_pipeline::cache::is_version_dir(&entry.file_name().to_string_lossy()) {
                        let _ = std::fs::remove_dir_all(entry.path());
                    }
                }
            }
        }
    }

    if all || kind == "raster" {
        remove_raster_cache(&state)?;
    }

    if all || kind == "legacy" {
        let _ = std::fs::remove_dir_all(cache.join("legacy"));
    }

    log::info!("已清理缓存：{kind}");
    Ok(measure_cache())
}

/// 删掉「办公软件出的逐页图」，**当前打开的那份留着**（见 [`clear_cache`]）。
fn remove_raster_cache(state: &tauri::State<'_, AppState>) -> Result<(), String> {
    let keep = {
        let guard = state.doc.lock().map_err(err_msg)?;
        guard
            .as_ref()
            .map(|doc| doc.pipeline.fingerprint().to_string())
    };
    let Ok(versions) = std::fs::read_dir(cache_dir().join("wps")) else {
        return Ok(());
    };
    for version in versions.flatten() {
        let Ok(items) = std::fs::read_dir(version.path()) else {
            continue;
        };
        for item in items.flatten() {
            let name = item.file_name().to_string_lossy().to_string();
            if keep.as_deref() == Some(name.as_str()) {
                continue;
            }
            let _ = std::fs::remove_dir_all(item.path());
        }
    }
    Ok(())
}

/// 打开日志所在的文件夹（设置页与问题排查用）。
#[tauri::command]
fn open_log_dir() -> Result<(), String> {
    let dir = log_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("无法创建日志目录：{e}"))?;
    shell_execute(&dir.to_string_lossy(), None)
}

/* ---------------- 检查更新与一键升级 ---------------- */

/// 「关于与更新」要显示的版本情况。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateStatus {
    /// 当前版本。
    current: String,
    /// 最新版本；`None` 表示仓库里**还没有发布过任何版本**。
    latest: Option<String>,
    has_update: bool,
    /// 安装包大小（MB）：下载前先让老师知道要下多少。
    size_mb: f64,
    /// 更新说明。
    notes: Option<String>,
}

/// 检查有没有新版本。设置页打开时调一次，启动时也静默调一次。
#[tauri::command(async)]
fn check_update() -> Result<UpdateStatus, String> {
    let current = update::current_version().to_string();
    let Some(release) = update::check_latest()? else {
        return Ok(UpdateStatus {
            current,
            latest: None,
            has_update: false,
            size_mb: 0.0,
            notes: None,
        });
    };
    Ok(UpdateStatus {
        has_update: update::is_newer(&release.version, &current),
        size_mb: release.asset_size as f64 / 1048576.0,
        notes: (!release.notes.is_empty()).then(|| release.notes.clone()),
        latest: Some(release.version.clone()),
        current,
    })
}

/// 更新进度（前端画进度条用）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateProgress {
    /// `probing` / `downloading` / `verifying` / `installing` / `failed`。
    phase: String,
    percent: f32,
    message: String,
    /// 选中的线路速度（MB/s）。
    speed_mbps: f64,
}

fn emit_update(app: &tauri::AppHandle, phase: &str, percent: f32, message: &str, speed_mbps: f64) {
    let _ = app.emit(
        "update-progress",
        UpdateProgress {
            phase: phase.to_string(),
            percent,
            message: message.to_string(),
            speed_mbps,
        },
    );
}

/// 下载并安装新版本。
///
/// 立刻返回，进度通过 `update-progress` 事件推给界面 —— 下载十几 MB 要几十秒，
/// 不能让这条命令一直挂着。
#[tauri::command(async)]
fn start_update(app: tauri::AppHandle) -> Result<(), String> {
    let Some(release) = update::check_latest()? else {
        return Err("仓库里还没有发布版本，暂时无法更新".to_string());
    };
    if !update::is_newer(&release.version, update::current_version()) {
        return Err("已经是最新版本了".to_string());
    }

    let handle = app.clone();
    std::thread::Builder::new()
        .name("oppv-update".to_string())
        .spawn(move || {
            if let Err(e) = run_update(&handle, release) {
                log::error!("更新失败：{e}");
                emit_update(&handle, "failed", 0.0, &e, 0.0);
            }
        })
        .map_err(|e| format!("无法启动更新线程：{e}"))?;
    Ok(())
}

/// 更新全流程：挑线路 → 下载 → 校验 → 静默安装 → 退出自己。
fn run_update(app: &tauri::AppHandle, release: update::Release) -> Result<(), String> {
    log::info!("开始更新到 {}（{}）", release.version, release.asset_url);

    emit_update(app, "probing", 0.0, "正在挑选最快的下载线路…", 0.0);
    let (url, speed) = update::fastest_source(&release.asset_url)?;
    let mbps = speed / 1048576.0;
    log::info!("更新线路已选定（{mbps:.1} MB/s）");

    let dest = std::env::temp_dir().join(format!("OpenPPTView-{}.exe", release.version));
    let mut last = std::time::Instant::now();
    let started = std::time::Instant::now();
    emit_update(app, "downloading", 0.0, "正在下载新版本…", mbps);
    update::download(&url, &dest, &mut |done, total| {
        // 事件别发太密：两百毫秒一次足够把进度条画顺
        if last.elapsed() < std::time::Duration::from_millis(200) {
            return;
        }
        last = std::time::Instant::now();
        // 速度按**这次下载的实际平均值**算，不用测速阶段的数：
        // 那个数含着代理回源的等待，比真实带宽低得多，显示出来只会吓人
        let secs = started.elapsed().as_secs_f64().max(0.001);
        let rate = done as f64 / 1048576.0 / secs;
        let (percent, text) = if total > 0 {
            (
                done as f32 * 100.0 / total as f32,
                format!(
                    "正在下载新版本… {:.1} / {:.1} MB（{rate:.1} MB/s）",
                    done as f64 / 1048576.0,
                    total as f64 / 1048576.0
                ),
            )
        } else {
            (
                0.0,
                format!(
                    "正在下载新版本… {:.1} MB（{rate:.1} MB/s）",
                    done as f64 / 1048576.0
                ),
            )
        };
        emit_update(app, "downloading", percent, &text, rate);
    })?;

    emit_update(app, "verifying", 100.0, "正在校验安装包…", mbps);
    update::verify(&dest, &release)?;

    emit_update(
        app,
        "installing",
        100.0,
        "正在安装，请在弹出的系统提示里点「是」",
        mbps,
    );
    update::launch_installer(&dest)?;
    log::info!("升级程序已启动，本程序即将退出");

    // 给界面一点时间把这句话显示出来，再请自己退出。
    // 退出走「先把标注落盘」那条路（见 `resident::quit`）。
    std::thread::sleep(std::time::Duration::from_millis(1500));
    resident::quit(app);
    Ok(())
}

/* ---------------- 默认打开方式（文件关联） ---------------- */

/// 查询各扩展名的关联状态，供设置界面展示。
#[tauri::command]
fn associations_status() -> Vec<associations::AssocStatus> {
    associations::status()
}

/// 把本应用注册为这些格式的「打开方式」。
///
/// 返回注册后的真实状态；`allDefault` 为假时前端应引导老师
/// 去系统「默认应用」页点一次确认 —— Windows 不允许程序自己改这个值。
#[tauri::command]
fn register_associations() -> Result<associations::AssocOutcome, String> {
    let exe = std::env::current_exe().map_err(|e| format!("无法定位程序路径：{e}"))?;
    let outcome = associations::register_all(&exe)?;
    log::info!(
        "文件关联注册完成：{} 项，全部为默认={}",
        outcome.items.len(),
        outcome.all_default
    );
    Ok(outcome)
}

/// 调起系统「默认应用」设置页。
#[tauri::command]
fn open_default_apps_settings() -> Result<(), String> {
    associations::open_default_apps_page()
}

/* ---------------- 交互元素（超链接 / 动作按钮） ---------------- */

/// 一个可点击热区，坐标在**幻灯片 pt 空间**。
///
/// 前端按当前缩放换算成屏幕坐标，因此缩放、平移、全屏切换都不会错位。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PageLink {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    /// `slide` / `next` / `prev` / `first` / `last` / `endShow` / `url` / `other`
    kind: String,
    /// 目标页（0 起），仅 `kind == "slide"` 时有值。
    slide: Option<usize>,
    /// 外部链接，仅 `kind == "url"` 时有值。
    url: Option<String>,
    tooltip: Option<String>,
}

/// 取一页的可点击热区。
#[tauri::command(async)]
fn page_links(page: usize, state: tauri::State<'_, AppState>) -> Result<Vec<PageLink>, String> {
    with_pipeline(&state, |p| {
        let spots = p.links(page).map_err(err_msg)?;
        Ok(spots
            .into_iter()
            .map(|spot| {
                use ppt_core::scene::HyperlinkTarget as T;
                let (kind, slide, url) = match &spot.link.target {
                    T::Slide(i) => ("slide", Some(*i), None),
                    T::NextSlide => ("next", None, None),
                    T::PreviousSlide => ("prev", None, None),
                    T::FirstSlide => ("first", None, None),
                    T::LastSlide => ("last", None, None),
                    T::EndShow => ("endShow", None, None),
                    T::Url(u) => ("url", None, Some(u.clone())),
                    T::OtherFile(_) => ("other", None, None),
                };
                PageLink {
                    x: spot.rect.x,
                    y: spot.rect.y,
                    w: spot.rect.w,
                    h: spot.rect.h,
                    kind: kind.to_string(),
                    slide,
                    url,
                    tooltip: spot.link.tooltip,
                }
            })
            .collect())
    })
}

/* ---------------- 内嵌视频/音频 ---------------- */

/// 一个可播放的媒体热区。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PageMedia {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    /// `video` / `audio`
    kind: String,
    /// 包内部件名，取字节时回传这个值。
    part: String,
    loop_play: bool,
    /// 「裁剪」后的播放起点（毫秒），没裁剪过为 0。
    trim_start_ms: u32,
    /// 播放终点（毫秒）；`null` 表示一直到结尾。
    trim_end_ms: Option<u32>,
}

/// 取一页里可播放的媒体。
#[tauri::command(async)]
fn page_media(page: usize, state: tauri::State<'_, AppState>) -> Result<Vec<PageMedia>, String> {
    with_pipeline(&state, |p| {
        let spots = p.media_items(page).map_err(err_msg)?;
        Ok(spots
            .into_iter()
            .map(|spot| PageMedia {
                x: spot.rect.x,
                y: spot.rect.y,
                w: spot.rect.w,
                h: spot.rect.h,
                kind: match spot.media.kind {
                    ppt_core::scene::MediaKind::Video => "video".to_string(),
                    ppt_core::scene::MediaKind::Audio => "audio".to_string(),
                },
                part: spot.media.part,
                loop_play: spot.media.loop_play,
                // PowerPoint 的「裁剪视频」不改动原文件，只记一段区间。
                // 不把区间交给播放器，就会把老师裁掉的片头片尾一起放出来
                trim_start_ms: spot.media.trim.map(|t| t.start_ms).unwrap_or(0),
                trim_end_ms: spot.media.trim.and_then(|t| t.end_ms),
            })
            .collect())
    })
}

/// 把内嵌媒体导出为磁盘文件，返回绝对路径。
///
/// 前端拿它交给 `convertFileSrc()`，用 `<video>/<audio>` 流式播放。
/// 同一份课件（按内容指纹分目录）只导出一次。
#[tauri::command(async)]
fn extract_media(
    part: String,
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    // 部件名来自课件内部，属于不可信输入：这里做一次路径穿越校验，
    // 再用「扁平化文件名」彻底消除目录层级（见 `media_file_name`）
    if part.contains("..") || part.contains('\\') {
        return Err(format!("非法的媒体部件名：{part}"));
    }

    let (bytes, fingerprint) = with_pipeline(&state, |p| {
        Ok((p.media_bytes(&part).map_err(err_msg)?, p.fingerprint().to_string()))
    })?;

    let dir = media_root(&app)?.join(&fingerprint);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("无法创建媒体目录 {}：{e}", dir.display()))?;

    let path = dir.join(media_file_name(&part));

    // 已导出过且大小一致就直接复用（指纹保证了内容不会变）
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() == bytes.len() as u64 {
            return Ok(path.to_string_lossy().to_string());
        }
    }

    std::fs::write(&path, &bytes)
        .map_err(|e| format!("无法写出媒体文件 {}：{e}", path.display()))?;
    log::info!("已导出媒体 {} → {}（{} 字节）", part, path.display(), bytes.len());
    Ok(path.to_string_lossy().to_string())
}

/// 部件名 → 扁平文件名。
///
/// `ppt/media/media1.mp4` → `ppt_media_media1.mp4`。
/// 把 `/` 换成 `_` 之后，文件名里不可能再出现目录分隔符，
/// 于是「拼接路径」这一步天然无法越出媒体目录。
fn media_file_name(part: &str) -> String {
    part.trim_start_matches('/').replace('/', "_")
}

/// 允许用系统默认程序打开的协议白名单。
///
/// 课件是**不可信的**（老师从网上下载、家长转发）：
/// 超链接里塞一个 `file:///C:/Windows/System32/...` 或自定义协议，
/// 就能借我们的手在本机启动程序。因此只放行真正需要浏览器处理的三种协议。
const ALLOWED_URL_SCHEMES: &[&str] = &["http://", "https://", "mailto:"];

/// 校验课件里的超链接能不能交给系统打开。
///
/// 白名单之外的协议一律拒绝（见 [`ALLOWED_URL_SCHEMES`]）。
/// 这里**不再需要**挡 `"`、`^` 之类的字符 —— 交给外壳的路径不经过命令行解析，
/// 那些字符只是 URL 里的普通字符（见 [`shell_execute_open`]）。
fn validate_external_url(url: &str) -> Result<(), String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err("链接为空".to_string());
    }

    let lower = trimmed.to_ascii_lowercase();
    if !ALLOWED_URL_SCHEMES.iter().any(|s| lower.starts_with(s)) {
        return Err(format!(
            "已阻止打开该链接：仅支持 http / https / mailto。原始值：{trimmed}"
        ));
    }
    Ok(())
}

/// 用系统默认程序打开一个外部链接。
#[tauri::command]
fn open_external(url: String) -> Result<(), String> {
    validate_external_url(&url)?;
    shell_execute_open(url.trim()).map_err(|e| format!("无法打开链接：{e}"))
}

/// 交给 Windows 外壳，用「默认程序」打开一个链接或文件。
pub fn shell_execute_open(target: &str) -> Result<(), String> {
    shell_execute(target, None)
}

/// 交给 Windows 外壳去运行一个程序（可带参数）。
///
/// # 为什么不用 `cmd /C start`
///
/// 以前是 `cmd /C start "" <url>`。问题出在 URL 里的 `&`：
/// cmd.exe 把它当**命令分隔符**，于是再普通不过的带参链接
/// （`https://x.com/?q=a&p=b`、Google 学术、知网检索结果……）会被拆成两条命令，
/// 后一条还带着半截参数。表现就是老师点了链接**毫无反应**，
/// 或者弹出莫名其妙的「找不到文件」。
///
/// 「加引号就安全」也不成立：Rust 的 `Command` 只在参数**含空格**时才加引号，
/// 含 `&` 而不含空格的 URL 根本不加，`start` 收到的是一串裸参数。
/// 而补上引号又要自己去操心 `^`、`"` 的转义 —— 那是在手工复刻 cmd 的解析器。
///
/// `ShellExecuteW` 不经过任何命令行解析：目标原样交给外壳去判定用什么程序打开。
/// 顺带把「引号逃逸注入第二条命令」那一整类风险也消掉了。
///
/// # 升级为什么也走它
///
/// 安装程序带「需要管理员权限」的清单，用 `CreateProcess` 直接起会以
/// `ERROR_ELEVATION_REQUIRED(740)` 失败；交给外壳才会弹出那个
/// 「是否允许此应用对你的设备进行更改」的系统提示。
///
/// 返回值 `HINSTANCE` 在 <= 32 时是错误码（这是 ShellExecute 的历史约定）。
#[cfg(windows)]
pub fn shell_execute(target: &str, params: Option<&str>) -> Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    let verb = wide("open");
    let file = wide(target);
    // 参数的宽字符串必须活到调用之后
    let args = params.map(wide);
    let args_ptr = args.as_ref().map_or(PCWSTR::null(), |a| PCWSTR(a.as_ptr()));
    let ret = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            args_ptr,
            None,
            SW_SHOWNORMAL,
        )
    };
    let code = ret.0 as isize;
    if code <= 32 {
        return Err(format!("系统没有打开它（ShellExecute 错误码 {code}）"));
    }
    Ok(())
}

/// 非 Windows：交给系统惯例的「打开」命令。
#[cfg(not(windows))]
pub fn shell_execute(target: &str, params: Option<&str>) -> Result<(), String> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut cmd = std::process::Command::new(opener);
    cmd.arg(target);
    if let Some(p) = params {
        cmd.arg(p);
    }
    cmd.spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// 把「文件读不出来」翻成老师能照着做的话。
///
/// 原始系统错误对老师毫无意义（`os error 362` 是什么？），
/// 但它背后的原因就那么几种，每一种都有明确的下一步。认不出的再给原文，
/// 至少能让懂行的人拿去搜。
fn describe_open_failure(path: &Path, e: &std::io::Error) -> String {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string());

    // 362 = ERROR_CLOUD_FILE_PROVIDER_NOT_RUNNING
    const CLOUD_FILE_PROVIDER_NOT_RUNNING: i32 = 362;
    // 32 = ERROR_SHARING_VIOLATION，5 = ERROR_ACCESS_DENIED
    const SHARING_VIOLATION: i32 = 32;
    const ACCESS_DENIED: i32 = 5;

    match e.raw_os_error() {
        Some(CLOUD_FILE_PROVIDER_NOT_RUNNING) => format!(
            "打不开「{name}」：它还在云盘上，本机只有个「占位」文件。\
             请先在文件资源管理器里右键它、选「始终保留在此设备上」，\
             等图标上的云朵变成绿色对勾再打开"
        ),
        Some(SHARING_VIOLATION) => format!(
            "打不开「{name}」：文件正被别的程序占用。\
             请先关掉 PowerPoint / WPS 里打开的它，再试一次"
        ),
        Some(ACCESS_DENIED) => format!(
            "打不开「{name}」：没有读取权限。\
             如果它在别人共享的文件夹或 U 盘里，先把它复制到本机桌面再打开"
        ),
        _ => format!("打不开「{name}」：读取文件失败（{e}）"),
    }
}

/// 遇到我们放映不了、但机器上别的程序能开的格式，顺手替老师打开它。
///
/// 老师双击一个 `.ppt`，要的是「看到里面的内容」。旧版二进制格式我们暂时
/// 放不了，但他机器上装着 PowerPoint 或 WPS —— 与其让他自己去找那个文件、
/// 再自己想起来「另存为」，不如就地打开，并明确告诉他转换入口在哪。
///
/// 打开失败（比如这台机器根本没装 Office）不算错误，退回纯文字说明即可。
fn handoff_to_default_app(path: &Path, hint: &str) -> String {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string());

    if shell_execute_open(&path.to_string_lossy()).is_ok() {
        // 已经替他把文件交给能打开它的程序了；补这一句，免得他以为点了没反应
        format!("{hint}。已用系统默认程序打开「{name}」")
    } else {
        // 这台机器可能压根没装 Office：那就只剩说明，别谎称打开了
        hint.to_string()
    }
}

fn main() {
    // 日志：开发时输出到 stderr，便于 `cargo tauri dev` 观察
    env_logger_init();

    // `AppState` 全部字段都有合理的默认值（空文档、空缓存、未预热的转换器）
    let state = AppState::default();

    tauri::Builder::default()
        // 单实例必须**第一个**注册：第二个进程要被尽早挡下并退出，
        // 别把字体索引、WebView2 这些重活再干一遍（见 `resident`）
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            resident::on_second_instance(app, &argv);
        }))
        .plugin(tauri_plugin_dialog::init())
        .manage(state)
        .manage(resident::Resident::new())
        .setup(|app| {
            // `OpenPPTView.exe --quit` 且**没有别的实例在跑**时，这一份就是被叫来退出的：
            // 直接退出，别把窗口弹出来。
            //
            // 为什么放在 setup 里：如果已经有实例在跑，单实例插件在更早的
            // 插件初始化阶段就把 `--quit` 转交给它并结束了本进程 ——
            // 能走到这儿说明「没有别人在跑」，那退出就是唯一该做的事。
            // 卸载程序删文件之前就是这么请应用让开位置的。
            if std::env::args().skip(1).any(|a| a == "--quit") {
                log::info!("收到 --quit 且无其它实例，直接退出");
                app.handle().exit(0);
                return Ok(());
            }

            // 文件关联自愈：默认打开方式指着我们、ProgID 却被删了（卸载干过这事），
            // 双击课件会**毫无反应**。启动时顺手补回来，见 `repair_if_dangling`。
            if let Ok(exe) = std::env::current_exe() {
                if associations::repair_if_dangling(&exe) {
                    log::info!("文件关联指向了已不存在的 ProgID，已重新登记");
                }
            }

            // 关窗后进程留在后台，托盘是它的出口
            resident::setup_tray(app.handle())?;
            resident::spawn_idle_watchdog(app.handle().clone());

            // 预热导出内核：`Converter::spawn` 只是起一条线程，
            // 真正贵的 `Application` 创建（约 2 秒）发生在后台 ——
            // 等老师第一次打开课件时它已经热好了，那 2 秒就白赚了。
            //
            // 放在 `setup` 而不是更早：此刻窗口已经创建，这一步不会推迟
            // 窗口出现的时刻（见 `resident` 里对启动时间的讲究）。
            {
                use tauri::Manager;
                let state = app.state::<AppState>();
                state.converter();
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            // 关窗 = 隐藏，不是退出（见 `resident::on_close_requested`）
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                resident::on_close_requested(window, api);
            }
        })
        .invoke_handler(tauri::generate_handler![
            startup_info,
            take_pending_open,
            ready_to_quit,
            install_preference,
            open_document,
            render_page,
            render_page_meta,
            prefetch_page,
            prefetch_around,
            prefetch_thumbnails,
            prewarm_all,
            set_scale,
            page_notes,
            page_links,
            page_media,
            page_anim,
            anim_layers,
            anim_layer_png,
            extract_media,
            open_external,
            load_annotations,
            save_annotations,
            annotations_exist,
            recent_files,
            close_document,
            runtime_stats,
            set_presentation_mode,
            is_presentation_mode,
            associations_status,
            register_associations,
            open_default_apps_settings,
            cache_usage,
            clear_cache,
            open_log_dir,
            check_update,
            start_update,
            quit_app,
        ])
        .run(tauri::generate_context!())
        .expect("OpenPPTView 启动失败");
}

/// 日志初始化：写文件（始终）+ stderr（开发时）。
///
/// # 为什么必须有文件日志
///
/// 这是个装在老师机器上的桌面应用。出问题时我们拿不到现场：
/// release 构建没有控制台，`eprintln!` 写进虚空；而「翻页一直转圈」
/// 这类毛病**只能靠日志区分**是渲染慢、编码慢、还是等锁等不到。
///
/// 之前为了「性能必须在 release 下测」这件事绕了一大圈，就是因为
/// release 什么也不输出。日志是排查的入场券，不该省。
///
/// 文件放在 `%LOCALAPPDATA%\OpenPPTView\logs\openpptview.log`，
/// 超过 [`LOG_MAX_BYTES`] 就轮转一份 `.1` —— 老机器的磁盘也要照顾。
fn env_logger_init() {
    use std::io::Write;

    struct FileLogger {
        file: std::sync::Mutex<Option<std::fs::File>>,
    }

    /// 单个日志文件的上限（超过就轮转）。
    const LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;

    impl log::Log for FileLogger {
        fn enabled(&self, meta: &log::Metadata) -> bool {
            meta.level() <= log::Level::Info
        }

        fn log(&self, record: &log::Record) {
            if !self.enabled(record.metadata()) {
                return;
            }
            let line = format!("[{}] {}\n", record.level(), record.args());
            // 开发时同时给 stderr，`cargo run` 里能直接看到
            if cfg!(debug_assertions) {
                eprint!("{line}");
            }
            if let Ok(mut guard) = self.file.lock() {
                if let Some(f) = guard.as_mut() {
                    let _ = f.write_all(line.as_bytes());
                }
            }
        }

        fn flush(&self) {
            if let Ok(mut guard) = self.file.lock() {
                if let Some(f) = guard.as_mut() {
                    let _ = f.flush();
                }
            }
        }
    }

    // 日志目录与文件；建不出来就退化成「只有 stderr」，不能因此启动失败
    let file = (|| {
        let dir = log_dir();
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("openpptview.log");
        // 轮转：上一次的留一份 `.1`，再早的丢弃
        if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > LOG_MAX_BYTES {
            let _ = std::fs::remove_file(dir.join("openpptview.log.1"));
            let _ = std::fs::rename(&path, dir.join("openpptview.log.1"));
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()
    })();

    static LOGGER: std::sync::OnceLock<FileLogger> = std::sync::OnceLock::new();
    let logger = LOGGER.get_or_init(|| FileLogger {
        file: std::sync::Mutex::new(file),
    });

    let _ = log::set_logger(logger);
    // 日志进的是文件，不再是控制台，所以 release 也开到 Info ——
    // 老师报问题时要的正是「打开/转换/慢渲染」这些过程记录
    log::set_max_level(log::LevelFilter::Info);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_link_whitelist_allows_real_urls_and_blocks_local_programs() {
        // 带 `&` 的检索链接必须放行 —— 正是这一类以前被 `cmd /C start`
        // 拆成两条命令，点了毫无反应
        assert!(validate_external_url("https://x.com/s?q=a&p=b#c").is_ok());
        assert!(validate_external_url("  http://example.com/a?x=1&y=2  ").is_ok());
        assert!(validate_external_url("mailto:teacher@school.edu").is_ok());
        assert!(validate_external_url("HTTPS://EXAMPLE.COM").is_ok());

        // 「借我们的手启动本机程序」的那一类，一律挡住
        assert!(validate_external_url("file:///C:/Windows/System32/calc.exe").is_err());
        assert!(validate_external_url("javascript:alert(1)").is_err());
        assert!(validate_external_url("ms-settings:defaultapps").is_err());
        assert!(validate_external_url("").is_err());
    }

    #[test]
    fn open_failure_message_names_the_file_and_the_next_step() {
        let path = Path::new(r"C:\课件\第二单元.ppt");

        // 362 = 云盘占位文件还没下载到本机
        let cloud = std::io::Error::from_raw_os_error(362);
        let msg = describe_open_failure(path, &cloud);
        assert!(msg.contains("第二单元.ppt"), "要说清是哪个文件：{msg}");
        assert!(msg.contains("始终保留在此设备上"), "要给出下一步：{msg}");

        // 32 = 文件被别的程序占着
        let busy = std::io::Error::from_raw_os_error(32);
        let msg = describe_open_failure(path, &busy);
        assert!(msg.contains("占用"), "要说清原因：{msg}");

        // 认不出的错误码：至少要留着原文，便于懂行的人去搜
        let other = std::io::Error::from_raw_os_error(1234);
        let msg = describe_open_failure(path, &other);
        assert!(msg.contains("1234"), "认不出也要保留原始错误码：{msg}");
    }
}
