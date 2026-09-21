//! # ppt-pipeline
//!
//! 幻灯片缓存与预渲染调度管线。
//!
//! ## 一次翻页发生了什么
//!
//! ```text
//! 老师按空格翻到第 5 页
//!   ├─ begin_generation()      作废队列里所有旧页的渲染任务
//!   ├─ render_now(5)           同步渲染当前页（内存命中则直接返回）
//!   │     ├─ 内存缓存命中？  → 立即返回，< 1ms
//!   │     ├─ 磁盘缓存命中？  → 解码 + 回填内存，~10ms
//!   │     └─ 渲染            → 解析场景 → 光栅化 → 写两级缓存
//!   └─ prefetch(4, 6)          异步预热相邻页（低优先级，可被抢占）
//! ```
//!
//! ## 三条设计取舍
//!
//! **① 当前页走同步路径，预取走异步路径。**
//! 若当前页也排队等异步结果，它会排在（可能正在跑的）缩略图任务后面。
//! 同步渲染当前页 + 异步预取相邻页，才能给出稳定的翻页延迟。
//!
//! **② 代际取消优先于队列顺序。**
//! 连翻 10 页时，前 9 页的任务全部作废，只渲染最终停留页。
//! 这是「翻页不追赶」的关键（见 [`scheduler`] 的模块文档）。
//!
//! **③ 场景图与位图分开缓存。**
//! 改变缩放（适应窗口 ↔ 100%）时位图失效但场景图仍有效，
//! 重新光栅化比重新解析 XML + 走继承链快得多。

pub mod cache;
pub mod scheduler;

pub use cache::{scale_bucket, CacheSource, DiskCache, MemoryCache};
pub use scheduler::{recommended_threads, Priority, RenderTask, Scheduler, WorkerPool};

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ppt_core::scene::{EffectState, LinkHotspot, MediaHotspot, PlayState, Rect, Scene, PLAY_ALL};
use ppt_core::{Bitmap, Error, Result, SharedSource};
use ppt_render::image::ImageCache;
use ppt_render::{RenderOptions, Renderer};
use ppt_text::FontContext;

/// 自带光栅化来源（PDF）的「原生档」长边像素数。
///
/// # 这个数是怎么定下来的
///
/// 解释 PDF 一页的代价**几乎与缩放无关**：实测同一页 1.05× 与 2.0× 都是
/// 约 52ms（瓶颈是内容流的解释，不是填充像素）。所以正确的做法是
/// **一页只解释一次**，把结果留成一张足够大的图，别的尺寸都从它缩下去。
///
/// 1920 的选择依据是「够 1080p 全屏投影清晰」：课件画布 960×540pt 时
/// 对应 2.0× 缩放，正好覆盖最大屏的全屏投影；再往上堆像素，
/// 课堂投影仪分辨不出来，内存却要成倍付出（每页 1920×1080 是 8.3MB）。
const NATIVE_LONG_EDGE: f32 = 1920.0;

/// 允许从原生档位图插值放大的上限（相对原生档的倍数）。
///
/// Windows 上 DPR 常是 1.25 / 1.5，1080p 全屏就会请求到 2.5~3.0 倍缩放，
/// 略高于原生档的 2.0。为这点差距重新解释一遍 PDF 不值得 —— 见 `render_direct`。
const UPSCALE_TOLERANCE: f32 = 1.5;

/// 自带光栅化来源的「原生档」缩放。
///
/// 长边默认取到 [`NATIVE_LONG_EDGE`]：够 1080p 全屏投影清晰，
/// 再往上堆像素对课堂投影没有意义，却要成倍吃内存。
///
/// 但**背景自带光栅化的来源可以自己说了算**：逐页位图后端（本机办公软件
/// 按老师屏幕宽度导出的图片）的固有分辨率就是那么多，管线若还按 1920 去算，
/// 会把同一页的缓存分到两个桶里 —— 每次都要重新解码缩放一遍。
///
/// 同步渲染路径与预取路径**必须用同一个口径**，否则预取预热的是 A 桶、
/// 显示要的是 B 桶，等于白干。
fn native_scale_for(source: &dyn ppt_core::DocumentSource) -> f32 {
    let size = source.default_page_size_pt();
    let long_edge = size.w.max(size.h).max(1.0);
    if let Some(hint) = source.native_scale_hint() {
        return hint.clamp(1.0, 8.0);
    }
    (NATIVE_LONG_EDGE / long_edge).clamp(1.0, 4.0)
}

/// 把位图缩放到目标尺寸（双线性）。
///
/// 只在**缩小**方向使用：`render_direct` 已经把「比原生档还大」的请求
/// 挡在外面了。放大走的是重新光栅化 —— 从原生图放大上去会糊。
fn resample(src: &Bitmap, width: u32, height: u32) -> Bitmap {
    if src.width == width && src.height == height {
        return src.clone();
    }
    // 只有预乘 RGBA8 能直接交给 tiny-skia；其它格式说明这条路径的前提被破坏了，
    // 此时原样返回**好过**画出颜色错误的图
    if src.format != ppt_core::PixelFormat::Rgba8Premultiplied {
        return src.clone();
    }

    let (Some(source), Some(mut target)) = (
        tiny_skia::PixmapRef::from_bytes(&src.data, src.width, src.height),
        tiny_skia::Pixmap::new(width, height),
    ) else {
        return src.clone();
    };

    let transform = tiny_skia::Transform::from_scale(
        width as f32 / src.width as f32,
        height as f32 / src.height as f32,
    );
    target.draw_pixmap(
        0,
        0,
        source,
        &tiny_skia::PixmapPaint {
            quality: tiny_skia::FilterQuality::Bilinear,
            ..Default::default()
        },
        transform,
        None,
    );

    Bitmap {
        width,
        height,
        format: src.format,
        data: target.data().to_vec(),
    }
}

/// 管线配置。
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// 渲染线程数。`None` 表示按 CPU 核心数自动选择。
    pub threads: Option<usize>,
    /// 内存中缓存的位图页数上限。
    pub memory_pages: usize,
    /// 位图缓存的内存上限（字节）。
    pub max_bitmap_bytes: usize,
    /// 图片解码缓存上限（字节）。
    pub image_cache_bytes: usize,
    /// 磁盘缓存上限（字节）。
    pub disk_budget_bytes: u64,
    /// 磁盘缓存目录；`None` 表示禁用磁盘缓存。
    pub disk_cache_dir: Option<PathBuf>,
    /// 单张位图的像素数上限。
    pub max_pixels: u64,
    /// 是否绘制阴影等效果。
    pub draw_effects: bool,
    /// 是否绘制降级占位。
    pub draw_placeholders: bool,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        PipelineConfig {
            threads: None,
            // 默认缓存 8 页：覆盖「当前页 + 前后各两三页 + 若干缩略图」
            memory_pages: 8,
            // 64MB：按主视图 1920×1080（约 8MB/页）算，约 8 页
            max_bitmap_bytes: 64 * 1024 * 1024,
            image_cache_bytes: 96 * 1024 * 1024,
            disk_budget_bytes: 512 * 1024 * 1024,
            disk_cache_dir: None,
            max_pixels: 16_000_000,
            draw_effects: true,
            draw_placeholders: true,
        }
    }
}

impl PipelineConfig {
    /// 低配档位：老机器上优先保证当前页响应。
    pub fn low_end() -> PipelineConfig {
        PipelineConfig {
            threads: Some(1),
            memory_pages: 4,
            max_bitmap_bytes: 24 * 1024 * 1024,
            image_cache_bytes: 32 * 1024 * 1024,
            max_pixels: 4_000_000,
            draw_effects: false,
            ..PipelineConfig::default()
        }
    }

    /// 高配档位。
    pub fn high_end() -> PipelineConfig {
        PipelineConfig {
            threads: Some(4),
            memory_pages: 16,
            max_bitmap_bytes: 256 * 1024 * 1024,
            image_cache_bytes: 256 * 1024 * 1024,
            max_pixels: 32_000_000,
            ..PipelineConfig::default()
        }
    }

    fn effective_threads(&self) -> usize {
        self.threads.unwrap_or_else(recommended_threads).max(1)
    }
}

/// 一次渲染请求的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderOutcome {
    /// 来自内存缓存。
    MemoryHit,
    /// 来自磁盘缓存。
    DiskHit,
    /// 实际渲染。
    Rendered,
}

impl RenderOutcome {
    pub fn source(self) -> CacheSource {
        match self {
            RenderOutcome::MemoryHit => CacheSource::Memory,
            RenderOutcome::DiskHit => CacheSource::Disk,
            RenderOutcome::Rendered => CacheSource::Rendered,
        }
    }
}

/// 渲染管线。
///
/// 生命周期内持有：文档句柄、字体索引、两级缓存、调度器与工作线程。
/// 打开新课件时应重建实例（或调用 [`Pipeline::clear_caches`]）。
pub struct Pipeline {
    /// 出**画面**的来源。
    ///
    /// 通常是解析后的 OOXML 场景图；但若本机装了 WPS/Office，这里会是
    /// 它们导出的矢量 PDF —— 画得 100% 准（见 `ppt-convert`）。
    source: SharedSource,
    /// 出**语义**的来源：链接在哪、哪里有视频、这一页怎么动、备注写了什么。
    ///
    /// # 为什么必须与画面来源分开
    ///
    /// 「谁画得准」和「谁知道语义」是两个不同的问题。矢量 PDF 画得准，
    /// 但它把超链接、媒体、动画**全都拍平成了像素** —— 问它「这一页
    /// 哪里能点」，答案只能是「不知道」。
    ///
    /// 所以当画面来自 PDF 时，这里仍然挂着原始 OOXML 解析器：
    /// 画面用 WPS 内核，交互用自研解析。少了这一层，课件就退化成
    /// 一份不能点的幻灯片图片。
    interactive: Option<SharedSource>,
    config: PipelineConfig,
    fingerprint: String,

    /// 共享的位图/场景缓存。
    cache: Arc<Mutex<MemoryCache>>,
    /// 磁盘缓存（未配置时为 `None`）。
    disk: Option<Arc<Mutex<DiskCache>>>,
    /// 渲染器池：每个工作线程取一个，用完归还。
    ///
    /// 用「池」而不是每线程一个 `thread_local`，是因为
    /// `WorkerPool` 不保证线程与任务的固定绑定关系。
    renderers: Arc<Mutex<Vec<Renderer>>>,
    /// 共享的字体索引。
    fonts: Arc<FontContext>,

    scheduler: Arc<Scheduler>,
    pool: WorkerPool,
    stop: Arc<AtomicBool>,

    /// 当前缩放档位（预取时用）。
    scale_bucket: AtomicU64,
    /// 统计：命中与渲染次数。
    hits: AtomicU64,
    misses: AtomicU64,
}

impl Pipeline {
    /// 画面与语义同源的普通管线。
    pub fn new(
        source: SharedSource,
        fonts: Arc<FontContext>,
        config: PipelineConfig,
    ) -> Result<Pipeline> {
        Pipeline::with_interaction(source, None, fonts, config)
    }

    /// 「画面来源」与「交互来源」分开的管线。
    ///
    /// `interactive` 给出时，画面由 `source` 出（可以是 WPS 导出的矢量 PDF），
    /// 而链接/媒体/动画/备注一律问 `interactive`（原始 OOXML）。
    pub fn with_interaction(
        source: SharedSource,
        interactive: Option<SharedSource>,
        fonts: Arc<FontContext>,
        config: PipelineConfig,
    ) -> Result<Pipeline> {
        let fingerprint = source.fingerprint().to_string();

        // 磁盘缓存：打开失败只记录告警，不影响主流程 ——
        // 缓存是加速手段，不该因为目录权限问题让课件打不开
        let disk = match &config.disk_cache_dir {
            Some(dir) => match DiskCache::open(dir, config.disk_budget_bytes) {
                Ok(c) => Some(Arc::new(Mutex::new(c))),
                Err(e) => {
                    log::warn!("磁盘缓存不可用，已降级为纯内存缓存：{e}");
                    None
                }
            },
            None => None,
        };

        let cache = Arc::new(Mutex::new(MemoryCache::new(
            config.memory_pages,
            config.max_bitmap_bytes,
        )));

        // 渲染器池：预建线程数那么多，避免首次渲染时现建（含字体索引共享）
        let shared_images = Arc::new(ImageCache::new(config.image_cache_bytes));
        let threads = config.effective_threads();
        let mut pool_renderers = Vec::with_capacity(threads);
        for _ in 0..threads {
            pool_renderers.push(Renderer::with_shared_image_cache(
                Arc::clone(&fonts),
                Arc::clone(&shared_images),
            ));
        }
        let renderers = Arc::new(Mutex::new(pool_renderers));

        let scheduler = Arc::new(Scheduler::new());
        let stop = Arc::new(AtomicBool::new(false));

        // 工作线程的任务处理函数
        let job_source = Arc::clone(&source);
        let job_fonts = Arc::clone(&fonts);
        let job_cache = Arc::clone(&cache);
        let job_disk = disk.clone();
        let job_renderers = Arc::clone(&renderers);
        let job_fingerprint = fingerprint.clone();
        let job_config = config.clone();

        let pool = WorkerPool::start(Arc::clone(&scheduler), threads, move |task| {
            render_task(
                task,
                &job_source,
                &job_fonts,
                &job_cache,
                job_disk.as_deref(),
                &job_renderers,
                &job_fingerprint,
                &job_config,
            )
        });

        Ok(Pipeline {
            source,
            interactive,
            config,
            fingerprint,
            cache,
            disk,
            renderers,
            fonts,
            scheduler,
            pool,
            stop,
            scale_bucket: AtomicU64::new(scale_bucket(1.0) as u64),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        })
    }

    /// 回答语义问题（链接、媒体、动画、备注）时该问哪个来源。
    #[inline]
    fn semantic_source(&self) -> &SharedSource {
        self.interactive.as_ref().unwrap_or(&self.source)
    }

    /// 请求的缩放是不是「接近原始分辨率」。
    ///
    /// 用来决定能不能把现成的 PNG 直接传给显示端：
    ///
    /// - 窗口、全屏、放大 —— 要的尺寸和原图差不多甚至更大，直接用没问题，
    ///   浏览器缩放很快，还省掉后端整条解码链
    /// - **缩略图** —— 要的只有原图的零头，传整页图就是白传（一张 1MB、
    ///   解码后 4.7MB 内存），39 张缩略图能吃掉几百 MB
    ///
    /// 后者走原来那条「后端缩好再传小的」的路。
    pub fn wants_native_raster(&self, scale: f32) -> bool {
        let native = native_scale_for(self.source.as_ref());
        scale >= native * 0.5
    }

    /// 「这一步已经有一张现成的 PNG」的话，把它的路径给出去。
    ///
    /// 调用方拿到就可以直接把文件传给显示端，省掉
    /// 「解码 → 缩放 → 转预乘 → 传原始像素」那一整条链 ——
    /// 详见 [`ppt_core::DocumentSource::raster_png_path`]。
    ///
    /// `state` 是播放状态：`PLAY_ALL` 取整页图，其余按已播步数取分帧。
    pub fn display_png_path(
        &self,
        page: usize,
        state: PlayState,
    ) -> Option<std::path::PathBuf> {
        let steps = if state == PLAY_ALL {
            None
        } else {
            Some(state.count_ones() as usize)
        };
        self.source.raster_png_path(page, steps)
    }

    /// 页数。
    pub fn page_count(&self) -> usize {
        self.source.page_count()
    }

    /// 当前渲染线程数。
    pub fn thread_count(&self) -> usize {
        self.pool.thread_count()
    }

    /// 内容指纹（磁盘缓存目录名）。
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// 设置当前缩放比。
    ///
    /// 会作废队列中旧缩放档的任务，并让后续预取使用新档位。
    pub fn set_scale(&self, scale: f32) {
        self.scale_bucket
            .store(scale_bucket(scale) as u64, Ordering::Relaxed);

        // 自带光栅化的来源（PDF）只有「原生档」一种产物，与显示缩放无关，
        // 所以换缩放档**不该**作废已排队的预热任务。
        //
        // 这一条曾经闯过祸：窗口里刚打开课件时排了整本预热，
        // 老师一按全屏，几百个预热任务全被作废，缓存重新变冷 ——
        // 于是「进全屏之后翻页慢十倍」。场景图来源则相反：
        // 换个缩放档确实要重新光栅化，作废旧任务是对的。
        if !self.source.rasterizes_directly() {
            self.scheduler.begin_generation();
        }
    }

    fn current_bucket(&self) -> u32 {
        self.scale_bucket.load(Ordering::Relaxed) as u32
    }

    /// 同步取一页的位图（当前页走这条路径）。
    ///
    /// 顺序：内存 → 磁盘 → 渲染。命中缓存时不会触碰工作线程，
    /// 因此不会与正在跑的预取任务争抢。
    pub fn render_now(&self, page: usize, scale: f32) -> Result<(Arc<Bitmap>, RenderOutcome)> {
        self.render_now_at(page, scale, PLAY_ALL)
    }

    /// 取「动画播放状态为 `state`」的位图。
    ///
    /// 动画中间帧**只进内存缓存、不落盘**：一页每步都写盘会让缓存目录迅速膨胀，
    /// 而中间帧本来就要靠重渲染得到（场景图已缓存，一次光栅化很便宜）。
    /// 只有全部播完的那一帧才算这一页的正身，才值得落盘。
    pub fn render_now_at(
        &self,
        page: usize,
        scale: f32,
        state: PlayState,
    ) -> Result<(Arc<Bitmap>, RenderOutcome)> {
        self.source.check_index(page)?;
        let bucket = scale_bucket(scale);
        let is_full = state == PLAY_ALL;

        // 1) 内存
        if let Some(bmp) = self
            .cache
            .lock()
            .ok()
            .and_then(|mut c| c.get_bitmap_at(page, bucket, state))
        {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok((bmp, RenderOutcome::MemoryHit));
        }

        // 2) 磁盘（只有完整帧才有落盘副本）
        if is_full {
            if let Some(bmp) = self.load_from_disk(page, bucket) {
                let bmp = Arc::new(bmp);
                if let Ok(mut c) = self.cache.lock() {
                    c.put_bitmap(page, bucket, Arc::clone(&bmp));
                }
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Ok((bmp, RenderOutcome::DiskHit));
            }
        }

        // 3) 实际渲染
        self.misses.fetch_add(1, Ordering::Relaxed);
        let bmp = Arc::new(self.render_page(page, scale, state)?);
        if let Ok(mut c) = self.cache.lock() {
            c.put_bitmap_at(page, bucket, state, Arc::clone(&bmp));
        }
        if is_full {
            self.store_to_disk(page, bucket, &bmp);
        }
        Ok((bmp, RenderOutcome::Rendered))
    }

    /// 渲染一页，但**跳过**指定形状（强调动画合成时的底帧）。
    ///
    /// 不走位图缓存：「跳过哪些形状」是播放状态之外的另一维信息，
    /// 塞进缓存键会把缓存结构撑复杂，而它每一步只取一次。
    pub fn render_frame_hiding(
        &self,
        page: usize,
        scale: f32,
        state: PlayState,
        hide: &[u32],
    ) -> Result<Bitmap> {
        if hide.is_empty() {
            return self.render_page(page, scale, state);
        }

        // 「摘掉某个形状」只有认识形状的来源做得到。
        //
        // 画面可能来自本机办公软件出的位图或 PDF（它把一切拍平成了像素），
        // 但语义来源仍是原始 OOXML —— 强调动画的底帧必须由它来出，
        // 否则「静止的原件」和「动起来的图层」会叠成重影。
        let src = self.semantic_source();
        if src.rasterizes_directly() {
            // 打开的就是 .pdf 文件：没有形状概念，原样返回
            return self.render_direct(page, scale, state);
        }

        let scene = self.scene(page)?;
        let opts = self.render_options(scale, state);

        let mut renderer = self.take_renderer()?;
        let result = renderer.render_hiding(&scene, &opts, src.as_ref(), hide);
        self.put_renderer(renderer);

        result
    }

    /// 取某个形状的**动画图层**（单独光栅化、透明背景）。
    ///
    /// 图层不进缓存：一步只取一次，而且它只为这一次动画服务 ——
    /// 缓存下来反而会在缩放变化后留下尺寸对不上的旧图。
    /// 一次光栅化只有几毫秒，比维护一类带尺寸的缓存划算。
    pub fn render_layer(
        &self,
        page: usize,
        shape_id: u32,
        paragraphs: Option<(u32, u32)>,
        state: PlayState,
        extra: &[EffectState],
        scale: f32,
    ) -> Result<Option<(Bitmap, Rect)>> {
        // 图层是「某个形状单独画一遍」，只有认识形状的来源做得到。
        // 画面来自 PDF 时语义来源仍是 OOXML（见 `semantic_source`），
        // 所以这里问的是它 —— 否则放大/旋转这类强调动画会完全没有图层。
        let src = self.semantic_source();
        if src.rasterizes_directly() {
            return Ok(None);
        }
        let scene = self.scene(page)?;
        let opts = self.render_options(scale, state);

        let mut renderer = self.take_renderer()?;
        let result = renderer.render_layer(
            &scene,
            shape_id,
            paragraphs,
            extra,
            &opts,
            src.as_ref(),
        );
        self.put_renderer(renderer);

        result
    }

    /// 取一页的场景图（带缓存）。
    ///
    /// 缩放变化时位图失效但场景图仍有效，因此单独暴露。
    ///
    /// 取的是**语义来源**的场景图：画面可能由 WPS 导出的 PDF 提供，
    /// 但「这一页有哪些形状、怎么动」只有原始 OOXML 知道。
    pub fn scene(&self, page: usize) -> Result<Arc<Scene>> {
        let src = self.semantic_source();
        src.check_index(page)?;
        if let Some(s) = self.cache.lock().ok().and_then(|mut c| c.get_scene(page)) {
            return Ok(s);
        }

        let scene = match src.page_content(page)? {
            ppt_core::PageContent::Scene(s) => s,
            ppt_core::PageContent::Bitmap(_) => {
                return Err(Error::other(
                    "该格式直接产出位图，请使用 render_now 获取页面",
                ))
            }
        };
        let scene = Arc::new(*scene);
        if let Ok(mut c) = self.cache.lock() {
            c.put_scene(page, Arc::clone(&scene));
        }
        Ok(scene)
    }

    /// 取演讲者备注。
    ///
    /// 备注读取失败时返回 `Ok(None)` 而不是错误 ——
    /// 缺备注不应影响放映。
    pub fn notes(&self, page: usize) -> Result<Option<String>> {
        let src = self.semantic_source();
        src.check_index(page)?;
        Ok(src.notes(page).unwrap_or(None))
    }

    /// 取一页里所有可点击的热区（超链接、动作按钮）。
    ///
    /// 注意问的是**语义来源**而不是画面来源：画面可能是 WPS 导出的 PDF，
    /// 它把链接拍平成了像素。少了这一句，「按钮点不动」。
    pub fn links(&self, page: usize) -> Result<Vec<LinkHotspot>> {
        let src = self.semantic_source();
        src.check_index(page)?;
        Ok(self.scene(page)?.link_hotspots())
    }

    /// 取一页里所有可播放的媒体热区（视频/音频）。
    ///
    /// 会滤掉**部件实际不存在**的项：`r:embed` 指向的媒体可能因课件被
    /// 二次编辑而缺失（另存为、精简包等）。留着一个点不动的播放按钮
    /// 比直接不显示更糟 —— 老师会以为程序坏了。
    pub fn media_items(&self, page: usize) -> Result<Vec<MediaHotspot>> {
        let src = self.semantic_source();
        src.check_index(page)?;
        Ok(self
            .scene(page)?
            .media_hotspots()
            .into_iter()
            .filter(|spot| src.has_media(&spot.media.part))
            .collect())
    }

    /// 读取一个媒体部件的字节（供应用层导出给播放器）。
    pub fn media_bytes(&self, part: &str) -> Result<Vec<u8>> {
        self.semantic_source().read_media(part)
    }

    /// 异步预取一页。
    pub fn prefetch(&self, page: usize, priority: Priority) {
        if page >= self.page_count() {
            return;
        }
        let bucket = self.current_bucket();
        // 已在缓存里就不必再排一次队
        if self
            .cache
            .lock()
            .ok()
            .and_then(|mut c| c.get_bitmap(page, bucket))
            .is_some()
        {
            return;
        }

        self.scheduler.submit(RenderTask {
            page,
            scale_bucket: bucket,
            priority,
            generation: self.scheduler.generation(),
        });
    }

    /// 预取当前页周围的页。
    ///
    /// `before`/`after` 分别控制向前/向后预取的页数。
    /// 默认配置是前后各 2 页 —— 覆盖老师「回翻看上一页」与
    /// 「提前翻看下一页」两种最常见的动作。
    pub fn prefetch_around(&self, current: usize, before: usize, after: usize) {
        let count = self.page_count();
        for d in 1..=after {
            let p = current + d;
            if p < count {
                // 越近的页优先级越高
                let priority = if d == 1 {
                    Priority::Adjacent
                } else {
                    Priority::Prefetch
                };
                self.prefetch(p, priority);
            }
        }
        for d in 1..=before {
            let p = current.checked_sub(d);
            if let Some(p) = p {
                let priority = if d == 1 {
                    Priority::Adjacent
                } else {
                    Priority::Prefetch
                };
                self.prefetch(p, priority);
            }
        }
    }

    /// 批量预取全部缩略图（低优先级，可被翻页抢占）。
    pub fn prefetch_thumbnails(&self, thumb_scale: f32) {
        let bucket = scale_bucket(thumb_scale);
        let generation = self.scheduler.generation();
        let count = self.page_count();

        let tasks = (0..count)
            .filter(|p| {
                self.cache
                    .lock()
                    .ok()
                    .and_then(|mut c| c.get_bitmap(*p, bucket))
                    .is_none()
            })
            .map(|page| RenderTask {
                page,
                scale_bucket: bucket,
                priority: Priority::Thumbnail,
                generation,
            });

        self.scheduler.submit_all(tasks);
    }

    /// 预热**整本**课件（低优先级，任何翻页都会插到前面）。
    ///
    /// # 为什么 PDF 需要这个
    ///
    /// 场景图来源解析一次就能反复廉价光栅化，预热与否差别不大。
    /// 但自带光栅化的来源（PDF）每页都要把内容流完整解释一遍 ——
    /// 实测 50~360ms/页，不预热的话**老师第一次翻到哪页就卡在哪页**。
    ///
    /// 这里一次性把全书按原生档排进后台队列：之后无论翻到哪一页，
    /// 显示路径都只是「内存命中 + 缩小几毫秒」。
    ///
    /// 队列是 `Priority::Thumbnail`（最低档），所以老师任何操作
    /// 都会抢在它前面执行 —— 预热只吃系统空档。
    pub fn prewarm_all(&self) {
        // 非自带光栅化的来源不需要：解析一次就够，光栅化本来就便宜
        if !self.source.rasterizes_directly() {
            return;
        }

        let bucket = scale_bucket(native_scale_for(self.source.as_ref()));
        let generation = self.scheduler.generation();

        let tasks = (0..self.page_count())
            .filter(|page| {
                self.cache
                    .lock()
                    .ok()
                    .and_then(|mut c| c.get_bitmap(*page, bucket))
                    .is_none()
            })
            .map(|page| RenderTask {
                page,
                // 注意：这里给的是**原生档**桶号。工作线程对自带光栅化的来源
                // 会忽略请求桶、统一写原生桶（见 `run_prefetch`），
                // 所以这个字段在这里只起「保持接口一致」的作用。
                scale_bucket: bucket,
                priority: Priority::Thumbnail,
                generation,
            });

        self.scheduler.submit_all(tasks);
    }

    /// 尝试取一页已渲染好的位图（异步预取的结果）。
    pub fn try_take(&self, page: usize, scale: f32) -> Option<Arc<Bitmap>> {
        let bucket = scale_bucket(scale);
        self.cache
            .lock()
            .ok()
            .and_then(|mut c| c.get_bitmap(page, bucket))
    }

    /// 通知管线「已导航到第 `page` 页」。
    ///
    /// 作废队列中的旧任务，然后同步渲染当前页并预取相邻页。
    /// 这是 UI 层每次翻页应该调用的唯一入口。
    pub fn navigate(&self, page: usize, scale: f32) -> Result<Arc<Bitmap>> {
        // 先作废旧任务，避免它们在当前页之前占用工作线程
        self.scheduler.begin_generation();
        self.scale_bucket
            .store(scale_bucket(scale) as u64, Ordering::Relaxed);

        let (bmp, _) = self.render_now(page, scale)?;
        self.prefetch_around(page, 2, 2);
        Ok(bmp)
    }

    /// 队列中待处理的任务数。
    pub fn pending_tasks(&self) -> usize {
        self.scheduler.pending()
    }

    /// 缓存命中次数。
    pub fn hit_count(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// 缓存未命中（实际渲染）次数。
    pub fn miss_count(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// 被代际取消丢弃的任务数。
    pub fn dropped_count(&self) -> u64 {
        self.scheduler.dropped_count()
    }

    /// 内存缓存中的位图页数。
    pub fn cached_bitmap_count(&self) -> usize {
        self.cache.lock().map(|c| c.bitmap_count()).unwrap_or(0)
    }

    /// 内存缓存中的场景图数。
    pub fn cached_scene_count(&self) -> usize {
        self.cache.lock().map(|c| c.scene_count()).unwrap_or(0)
    }

    /// 内存缓存占用的字节数。
    pub fn cached_bitmap_bytes(&self) -> usize {
        self.cache.lock().map(|c| c.bitmap_bytes()).unwrap_or(0)
    }

    /// 清空全部缓存（内存 + 磁盘 + 渲染器内部缓存）。
    pub fn clear_caches(&self) {
        if let Ok(mut c) = self.cache.lock() {
            c.clear();
        }
        if let Ok(mut r) = self.renderers.lock() {
            for renderer in r.iter_mut() {
                renderer.clear_caches();
            }
        }
        if let Some(disk) = &self.disk {
            if let Ok(mut d) = disk.lock() {
                if let Err(e) = d.clear() {
                    log::warn!("清空磁盘缓存失败：{e}");
                }
            }
        }
    }

    /// 删除本课件的磁盘缓存。
    pub fn purge_disk_cache(&self) {
        if let Some(disk) = &self.disk {
            if let Ok(mut d) = disk.lock() {
                d.purge_document(&self.fingerprint);
            }
        }
    }

    /// 阻塞等待队列清空（基准测试与同步场景用）。
    pub fn wait_idle(&self, timeout: std::time::Duration) -> bool {
        self.scheduler.wait_idle(&self.stop, timeout)
    }

    /// 停机。
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.pool.shutdown();
    }

    fn load_from_disk(&self, page: usize, bucket: u32) -> Option<Bitmap> {
        let disk = self.disk.as_ref()?;
        let d = disk.lock().ok()?;
        d.load(&self.fingerprint, page, bucket)
    }

    fn store_to_disk(&self, page: usize, bucket: u32, bmp: &Bitmap) {
        let Some(disk) = &self.disk else {
            return;
        };
        if let Ok(mut d) = disk.lock() {
            d.store(&self.fingerprint, page, bucket, bmp);
        }
    }

    /// 同步渲染一页（不经过缓存）。
    fn render_page(&self, page: usize, scale: f32, state: PlayState) -> Result<Bitmap> {
        // 自带光栅化的格式（PDF / 逐页位图）：没有可复用的场景图
        if self.source.rasterizes_directly() {
            return self.render_direct(page, scale, state);
        }

        let scene = self.scene(page)?;
        let opts = self.render_options(scale, state);

        let mut renderer = self.take_renderer()?;
        let result = renderer.render(&scene, &opts, self.source.as_ref());
        self.put_renderer(renderer);

        result
    }

    /// 自带光栅化来源（PDF）的渲染：先取**原生档**位图，再缩到请求尺寸。
    ///
    /// # 为什么不能「按请求的缩放直接光栅化」
    ///
    /// PDF 是顺序绘图指令流，没有可复用的场景图，每渲染一页都要把内容流
    /// 完整解释一遍。而这份解释的代价**几乎与缩放无关** —— 实测同一页
    /// 1.05× 与 2.0× 都是 ~52ms（瓶颈是解释，不是填充像素）。
    ///
    /// 于是「每换一个缩放档就重解释一次」的代价被放大了好几倍：
    /// 进全屏、适应窗口↔100%、生成缩略图、动画底帧，每一项都要重付一次。
    /// 老师感受到的就是「哪里都卡」。
    ///
    /// 所以这里先出一张**足够大**的原生位图，任何更小的尺寸都由它缩下去：
    /// 缩小只要几毫秒，而且全屏、缩放、缩略图可以共用同一张。
    fn render_direct(&self, page: usize, scale: f32, state: PlayState) -> Result<Bitmap> {
        // 动画中间帧优先问来源「按步出图」的能力。
        //
        // 逐页位图后端（本机办公软件出的图）在出图时就把「这一步还不该露面」
        // 的形状隐掉了，所以它能给出真正的各帧；而**整页拍平的那一张**
        // 上面什么都有（包括答案），拿它当每一帧就会变成
        // 「弹出的都是已经显示的内容」（见 `DocumentSource::stepped_raster`）。
        //
        // 全部播完（`PLAY_ALL`）和没有动画的页会返回 `Ok(None)`，
        // 继续走下面这条有原生档缓存的路 —— 那一张图本来就只有一种样子。
        if state != PLAY_ALL {
            if let Some(bmp) = self.source.stepped_raster(
                page,
                state.count_ones() as usize,
                scale,
                self.config.max_pixels,
            )? {
                return Ok(bmp);
            }
        }

        let native_scale = self.native_scale();
        let bucket = scale_bucket(native_scale);

        // 请求的就是原生档：直接用它，不必再解释一遍
        if (scale - native_scale).abs() <= native_scale * 0.02 {
            let native = self.native_bitmap(page, native_scale, bucket)?;
            return Ok((*native).clone());
        }

        // 比原生档大、但在可接受的放大范围内（DPR 1.25 / 1.5 的机器上，
        // 1080p 全屏会请求到 2.4~3.0×，略高于原生档 2.0）：
        // 仍然从原生图插值放大。
        //
        // 为什么值得「损失一点锐度」：另一条路是按请求重新解释 PDF 内容流，
        // 那是 50~360ms/页 的代价 —— 老师感知到的就是「一进全屏翻页慢十倍」。
        // 1.5 倍以内的双线性放大在教室投影上肉眼分辨不出来，这笔账划算。
        if scale <= native_scale * UPSCALE_TOLERANCE {
            let native = self.native_bitmap(page, native_scale, bucket)?;
            let size = self.source.default_page_size_pt();
            let target_w = ((size.w * scale).round() as u32).max(1);
            let target_h = ((size.h * scale).round() as u32).max(1);
            return Ok(resample(&native, target_w, target_h));
        }

        // 真正的大幅放大（老师要看细节）：精度优先，按请求重新光栅化
        self.source
            .rasterize_page(page, scale, self.config.max_pixels)
    }

    /// 直接光栅化来源的「原生档」缩放。
    fn native_scale(&self) -> f32 {
        native_scale_for(self.source.as_ref())
    }

    /// 取（或生成）原生档位图，走内存 + 磁盘两级缓存。
    ///
    /// 存的桶号是**原生档**而不是请求档，所以同一页无论在窗口、全屏还是
    /// 缩略图里被要求多少次，都只会真正解释一次。
    fn native_bitmap(&self, page: usize, native_scale: f32, bucket: u32) -> Result<Arc<Bitmap>> {
        if let Some(bmp) = self
            .cache
            .lock()
            .ok()
            .and_then(|mut c| c.get_bitmap(page, bucket))
        {
            return Ok(bmp);
        }
        // 来源自己存着这份光栅（逐页 PNG）时，只管内存缓存：
        // 它本来就在磁盘上，再存一份原始 RGBA 是把缓存预算白白吃掉
        let owns_raster = self.source.has_own_raster_cache();

        if !owns_raster {
            if let Some(bmp) = self.load_from_disk(page, bucket) {
                let bmp = Arc::new(bmp);
                if let Ok(mut c) = self.cache.lock() {
                    c.put_bitmap(page, bucket, Arc::clone(&bmp));
                }
                return Ok(bmp);
            }
        }

        let bmp = Arc::new(
            self.source
                .rasterize_page(page, native_scale, self.config.max_pixels)?,
        );
        if let Ok(mut c) = self.cache.lock() {
            c.put_bitmap(page, bucket, Arc::clone(&bmp));
        }
        if !owns_raster {
            self.store_to_disk(page, bucket, &bmp);
        }
        Ok(bmp)
    }

    fn render_options(&self, scale: f32, state: PlayState) -> RenderOptions {
        RenderOptions {
            scale,
            max_pixels: self.config.max_pixels,
            draw_effects: self.config.draw_effects,
            draw_placeholders: self.config.draw_placeholders,
            state,
            ..RenderOptions::default()
        }
    }

    /// 从池中取一个渲染器；池空时现建一个。
    fn take_renderer(&self) -> Result<Renderer> {
        if let Ok(mut pool) = self.renderers.lock() {
            if let Some(r) = pool.pop() {
                return Ok(r);
            }
        }
        // 池空说明并发度超出预期（如调用方直接 render_now），
        // 现建一个即可，代价只是一次图片缓存共享的 Arc 克隆
        Ok(Renderer::with_shared_image_cache(
            Arc::clone(&self.fonts),
            Arc::new(ImageCache::new(self.config.image_cache_bytes)),
        ))
    }

    fn put_renderer(&self, renderer: Renderer) {
        if let Ok(mut pool) = self.renderers.lock() {
            // 限制池大小，避免异常情况下无限增长
            if pool.len() < self.config.effective_threads() + 2 {
                pool.push(renderer);
            }
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// 工作线程的任务处理函数。
///
/// 独立成自由函数而非 `Pipeline` 的方法，是因为工作线程不能持有
/// `Pipeline` 的引用（那会形成自引用）。因此这里显式传入所需的几项资源。
#[allow(clippy::too_many_arguments)]
fn render_task(
    task: RenderTask,
    source: &SharedSource,
    fonts: &Arc<FontContext>,
    cache: &Arc<Mutex<MemoryCache>>,
    disk: Option<&Mutex<DiskCache>>,
    renderers: &Arc<Mutex<Vec<Renderer>>>,
    fingerprint: &str,
    config: &PipelineConfig,
) -> bool {
    // 再查一次缓存：任务可能排队期间已被其它线程渲染完
    if let Ok(mut c) = cache.lock() {
        if c.get_bitmap(task.page, task.scale_bucket).is_some() {
            return true;
        }
    }

    let scale = task.scale_bucket as f32 / 100.0;

    // 自带光栅化来源（PDF）：预取的目的不是「按请求档出一张图」，
    // 而是**把这一页解释一次** —— 解释代价与缩放无关，
    // 所以直接按原生档出图、存进原生桶；显示路径无论什么缩放都会命中它。
    //
    // 如果这里按 `task.scale_bucket` 出图，缩略图预取（0.18×）存的是 0.18 的桶、
    // 全屏翻转要的是 2.0 的桶，两边永远不会相遇 —— 等于把 PDF 又解释一遍。
    if source.rasterizes_directly() {
        let native_scale = native_scale_for(source.as_ref());
        let native_bucket = scale_bucket(native_scale);

        if let Ok(mut c) = cache.lock() {
            if c.get_bitmap(task.page, native_bucket).is_some() {
                return true;
            }
        }

        let bmp = match source.rasterize_page(task.page, native_scale, config.max_pixels) {
            Ok(b) => Arc::new(b),
            // 「还没生成好」是逐页出图后端的**正常状态**（后面的页在排队），
            // 不是故障：不记警告，等它出好了下次预取自然命中
            Err(e) if e.is_not_ready() => return false,
            Err(e) => {
                log::warn!("预取第 {} 页失败：{e}", task.page + 1);
                return false;
            }
        };
        if let Ok(mut c) = cache.lock() {
            c.put_bitmap(task.page, native_bucket, Arc::clone(&bmp));
        }
        // 来源自己就存着这份光栅（逐页 PNG），管线不必再存一份原始 RGBA
        if !source.has_own_raster_cache() {
            if let Some(disk) = disk {
                if let Ok(mut d) = disk.lock() {
                    d.store(fingerprint, task.page, native_bucket, &bmp);
                }
            }
        }
        return true;
    }

    let rendered = {
        // 解析场景（场景图缓存在这里复用）
        let scene = match get_or_build_scene(source, cache, task.page) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("预取第 {} 页失败：{e}", task.page + 1);
                return false;
            }
        };

        // 从池中取渲染器；池空时现建一个（仍共享字体与图片缓存，
        // 只是字形缓存需要重新预热）
        let mut renderer = match renderers.lock() {
            Ok(mut pool) => pool.pop(),
            Err(_) => None,
        }
        .unwrap_or_else(|| {
            Renderer::with_shared_image_cache(
                Arc::clone(fonts),
                Arc::new(ImageCache::new(config.image_cache_bytes)),
            )
        });

        let opts = RenderOptions {
            scale,
            max_pixels: config.max_pixels,
            draw_effects: config.draw_effects,
            // 缩略图不画降级占位，画面更接近原始观感
            draw_placeholders: config.draw_placeholders && task.priority != Priority::Thumbnail,
            ..RenderOptions::default()
        };

        let result = renderer.render(&scene, &opts, source.as_ref());

        if let Ok(mut pool) = renderers.lock() {
            if pool.len() < config.effective_threads() + 2 {
                pool.push(renderer);
            }
        }
        result
    };

    let bmp = match rendered {
        Ok(b) => Arc::new(b),
        Err(e) => {
            log::warn!("预取第 {} 页渲染失败：{e}", task.page + 1);
            return false;
        }
    };

    if let Ok(mut c) = cache.lock() {
        c.put_bitmap(task.page, task.scale_bucket, Arc::clone(&bmp));
    }
    if let Some(disk) = disk {
        if let Ok(mut d) = disk.lock() {
            d.store(fingerprint, task.page, task.scale_bucket, &bmp);
        }
    }
    true
}

/// 取场景图（优先缓存）。
fn get_or_build_scene(
    source: &SharedSource,
    cache: &Arc<Mutex<MemoryCache>>,
    page: usize,
) -> Result<Arc<Scene>> {
    if let Ok(mut c) = cache.lock() {
        if let Some(s) = c.get_scene(page) {
            return Ok(s);
        }
    }

    let scene = match source.page_content(page)? {
        ppt_core::PageContent::Scene(s) => Arc::new(*s),
        ppt_core::PageContent::Bitmap(_) => {
            return Err(Error::other("该格式直接产出位图"));
        }
    };

    if let Ok(mut c) = cache.lock() {
        c.put_scene(page, Arc::clone(&scene));
    }
    Ok(scene)
}

/// 未使用导入守卫：`HashMap` 供后续按页索引的扩展使用。
#[allow(dead_code)]
fn _assert_types(_: HashMap<usize, usize>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use ppt_core::scene::{linear_state, step_bit, Color, Hyperlink, HyperlinkTarget, Size};
    use ppt_core::{DocFormat, DocumentSource, MediaProvider, PageContent};

    /// 内存中的假文档：每页一个纯色矩形，页号决定颜色。
    struct FakeDoc {
        pages: usize,
        size: Size,
        /// 给页内形状挂一个链接（测试点击导航用）。
        link: Option<HyperlinkTarget>,
        /// 页内形状改成「第 1 步才出现」（测试动画分步渲染用）。
        animated: bool,
    }

    impl FakeDoc {
        fn new(pages: usize) -> Arc<FakeDoc> {
            Arc::new(FakeDoc {
                pages,
                size: Size::new(100.0, 50.0),
                link: None,
                animated: false,
            })
        }

        fn with_link(self: &Arc<Self>, target: HyperlinkTarget) -> Arc<FakeDoc> {
            Arc::new(FakeDoc {
                pages: self.pages,
                size: self.size,
                link: Some(target),
                animated: self.animated,
            })
        }

        fn with_animation(self: &Arc<Self>) -> Arc<FakeDoc> {
            Arc::new(FakeDoc {
                pages: self.pages,
                size: self.size,
                link: self.link.clone(),
                animated: true,
            })
        }
    }

    impl MediaProvider for FakeDoc {
        fn read_media(&self, part: &str) -> Result<Vec<u8>> {
            Err(Error::MissingPart(part.to_string()))
        }
    }

    impl DocumentSource for FakeDoc {
        fn page_count(&self) -> usize {
            self.pages
        }

        fn default_page_size_pt(&self) -> Size {
            self.size
        }

        fn page_content(&self, index: usize) -> Result<PageContent> {
            self.check_index(index)?;
            let mut scene = Scene::new(self.size);
            // 每页一个不同亮度的矩形，便于区分
            let v = (index * 7 % 200 + 20) as u8;
            scene.background = ppt_core::scene::SceneBackground::Solid(Color::rgb(v, v, v));
            scene.nodes.push(ppt_core::scene::Node {
                id: format!("p{index}"),
                transform: ppt_core::scene::Transform::translate(10.0, 10.0),
                local_bbox: Some(ppt_core::scene::Rect::new(0.0, 0.0, 20.0, 10.0)),
                geometry: ppt_core::scene::Geometry::Rect,
                fill: ppt_core::scene::Fill::Solid(Color::rgb(255, 0, 0)),
                hyperlink: self.link.clone().map(|target| Hyperlink {
                    target,
                    tooltip: None,
                }),
                build: if self.animated {
                    ppt_core::scene::Build {
                        appear: Some(1),
                        disappear: None,
                    }
                } else {
                    ppt_core::scene::Build::ALWAYS
                },
                ..Default::default()
            });
            Ok(PageContent::Scene(Box::new(scene)))
        }

        fn fingerprint(&self) -> &str {
            "fake-doc-fingerprint"
        }

        fn format(&self) -> DocFormat {
            DocFormat::Pptx
        }
    }

    fn fonts() -> Arc<FontContext> {
        // 管线测试不依赖真实字体；用空索引即可（只画几何）
        Arc::new(FontContext::empty())
    }

    fn pipeline_with(pages: usize, config: PipelineConfig) -> Pipeline {
        Pipeline::new(FakeDoc::new(pages), fonts(), config).expect("管线应能创建")
    }

    fn temp_cache_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn renders_current_page_synchronously() {
        let p = pipeline_with(10, PipelineConfig::default());
        let (bmp, outcome) = p.render_now(0, 1.0).unwrap();

        assert_eq!(outcome, RenderOutcome::Rendered);
        assert_eq!((bmp.width, bmp.height), (100, 50));
        assert!(bmp.is_consistent());
        // 矩形位置 (10,10)-(30,20) 应为红色
        assert_eq!(bmp.pixel(20, 15), Some(Color::rgb(255, 0, 0)));

        p.shutdown();
    }

    #[test]
    fn animation_steps_do_not_share_cache_entries() {
        // 这一步错了就是「点了一下没反应」：拿到的是上一步的缓存图
        let doc = FakeDoc::new(1).with_animation();
        let p = Pipeline::new(doc, fonts(), PipelineConfig::default()).unwrap();

        let red = |bmp: &ppt_core::Bitmap| {
            bmp.data
                .chunks_exact(4)
                .filter(|px| px[0] > 200 && px[1] < 60 && px[2] < 60)
                .count()
        };

        let (step0, first) = p.render_now_at(0, 1.0, 0).unwrap();
        assert_eq!(first, RenderOutcome::Rendered);
        assert_eq!(red(&step0), 0, "第 0 步那块还不该出现");

        let (step1, _) = p.render_now_at(0, 1.0, linear_state(1)).unwrap();
        assert!(red(&step1) > 0, "第 1 步应出现");

        // 回到第 0 步：必须命中**第 0 步**的缓存，而不是第 1 步的
        let (again, outcome) = p.render_now_at(0, 1.0, 0).unwrap();
        assert_eq!(outcome, RenderOutcome::MemoryHit);
        assert_eq!(red(&again), 0, "第 0 步的缓存不该被第 1 步污染");

        p.shutdown();
    }

    #[test]
    fn trigger_step_state_does_not_leak_into_linear_state() {
        // 触发器动画允许「跳着播」：先点按钮播第 2 步，而线性轴上一个都没播。
        // 单个数表达不了这两个状态，掩码可以。
        // 夹具里那个红块是「第 1 步出现」，所以跳到第 2 步时它**仍然该藏着**。
        let doc = FakeDoc::new(1).with_animation();
        let p = Pipeline::new(doc, fonts(), PipelineConfig::default()).unwrap();

        let red = |bmp: &ppt_core::Bitmap| {
            bmp.data
                .chunks_exact(4)
                .filter(|px| px[0] > 200 && px[1] < 60 && px[2] < 60)
                .count()
        };

        // 线性两步：都在了
        let (linear2, _) = p.render_now_at(0, 1.0, linear_state(2)).unwrap();
        assert!(red(&linear2) > 0);
        // 只播第 2 步（掩码 bit1 单独置位）：第 1 步才出现的东西不该被带出来
        let (only2, _) = p.render_now_at(0, 1.0, step_bit(2)).unwrap();
        assert_eq!(red(&only2), 0, "跳过第 1 步时它不该出现");

        // 掩码相同则命中同一份缓存
        let (again, outcome) = p.render_now_at(0, 1.0, linear_state(2)).unwrap();
        assert_eq!(outcome, RenderOutcome::MemoryHit);
        assert_eq!(red(&again), red(&linear2));

        p.shutdown();
    }

    #[test]
    fn second_render_hits_memory_cache() {
        let p = pipeline_with(5, PipelineConfig::default());

        let (_, first) = p.render_now(2, 1.0).unwrap();
        assert_eq!(first, RenderOutcome::Rendered);

        let (_, second) = p.render_now(2, 1.0).unwrap();
        assert_eq!(second, RenderOutcome::MemoryHit, "同页同缩放应命中内存");

        assert_eq!(p.hit_count(), 1);
        assert_eq!(p.miss_count(), 1);

        p.shutdown();
    }

    #[test]
    fn different_scale_does_not_hit_bitmap_cache() {
        let p = pipeline_with(5, PipelineConfig::default());
        let _ = p.render_now(0, 1.0).unwrap();
        let (_, outcome) = p.render_now(0, 2.0).unwrap();
        assert_eq!(outcome, RenderOutcome::Rendered, "不同缩放档应重新渲染");

        // 但场景图应被复用（解析只做一次）
        assert_eq!(p.cached_scene_count(), 1);

        p.shutdown();
    }

    #[test]
    fn links_are_exposed_for_click_navigation() {
        // 解析层早就把超链接解析出来了，但一直没有出口 ——
        // 前端拿不到热区，放映时点「动作按钮」毫无反应
        let doc = FakeDoc::new(3);
        let linked = doc.with_link(HyperlinkTarget::Slide(2));
        let p = Pipeline::new(linked, fonts(), PipelineConfig::default()).unwrap();

        let links = p.links(0).unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].link.target, HyperlinkTarget::Slide(2));
        // 热区应落在节点位置上（局部 20×10 平移到 (10,10)）
        assert_eq!(links[0].rect, ppt_core::scene::Rect::new(10.0, 10.0, 20.0, 10.0));

        assert!(p.links(9).is_err(), "越界页应报错");
    }

    #[test]
    fn pages_without_links_yield_an_empty_list() {
        let p = Pipeline::new(FakeDoc::new(2), fonts(), PipelineConfig::default()).unwrap();
        assert!(p.links(0).unwrap().is_empty());
    }

    /// 假装的「自带光栅化」来源（相当于 WPS/Office 导出的 PDF）。
    ///
    /// 它只会出像素：没有场景图，对链接、媒体、动画一无所知。
    struct FakePdf {
        pages: usize,
        size: Size,
    }

    impl MediaProvider for FakePdf {
        fn read_media(&self, part: &str) -> Result<Vec<u8>> {
            Err(Error::MissingPart(part.to_string()))
        }
    }

    impl DocumentSource for FakePdf {
        fn page_count(&self) -> usize {
            self.pages
        }
        fn default_page_size_pt(&self) -> Size {
            self.size
        }
        fn page_content(&self, index: usize) -> Result<PageContent> {
            self.check_index(index)?;
            Err(Error::other("PDF 没有矢量场景图"))
        }
        fn rasterizes_directly(&self) -> bool {
            true
        }
        fn rasterize_page(&self, index: usize, scale: f32, _max_pixels: u64) -> Result<Bitmap> {
            self.check_index(index)?;
            let w = ((self.size.w * scale).round() as u32).max(1);
            let h = ((self.size.h * scale).round() as u32).max(1);
            Ok(Bitmap::new_filled(w, h, Color::rgb(200, 200, 200)))
        }
        fn fingerprint(&self) -> &str {
            // 必须与 FakeDoc 不同：磁盘缓存按指纹分目录，
            // 撞在一起会让两个来源的产物互相覆盖
            "fake-pdf-fingerprint"
        }
        fn format(&self) -> DocFormat {
            DocFormat::Pdf
        }
    }

    /// **画面来自 PDF 时，链接与场景图必须仍然由 OOXML 回答。**
    ///
    /// 这是「画面用 WPS 内核、交互用自研解析」这条架构的地基。
    /// 曾经踩过的坑：整条管线都被换成 PDF 源之后，`links()`/`media_items()`
    /// 见到「自带光栅化」就直接返回空 —— 结果按钮点不动、视频播不了、
    /// 动画整段消失，课件退化成一份不能点的幻灯片图片。
    #[test]
    fn semantic_queries_survive_a_pdf_backed_display_source() {
        let display: SharedSource = Arc::new(FakePdf {
            pages: 3,
            size: Size::new(100.0, 50.0),
        });
        let ooxml: SharedSource = FakeDoc::new(3).with_link(HyperlinkTarget::Slide(2));

        let p = Pipeline::with_interaction(
            display,
            Some(ooxml),
            fonts(),
            PipelineConfig::default(),
        )
        .unwrap();

        // 画面：由 FakePdf 出（纯色块，说明确实走的是「自带光栅化」那条路）
        let (bmp, _) = p.render_now(0, 1.0).unwrap();
        assert_eq!((bmp.width, bmp.height), (100, 50));

        // 语义：必须仍然来自 OOXML，否则前端拿不到任何热区
        let links = p.links(0).unwrap();
        assert_eq!(links.len(), 1, "PDF 画面下链接仍然要能取到");
        assert_eq!(links[0].link.target, HyperlinkTarget::Slide(2));

        assert!(
            p.scene(0).is_ok(),
            "场景图必须来自 OOXML —— 动画、备注、媒体热区全靠它"
        );

        p.shutdown();
    }

    #[test]
    fn scene_is_cached_and_reused() {
        let p = pipeline_with(5, PipelineConfig::default());
        let s1 = p.scene(1).unwrap();
        let s2 = p.scene(1).unwrap();
        assert!(Arc::ptr_eq(&s1, &s2), "同一页的场景图应复用同一份");
        assert_eq!(p.cached_scene_count(), 1);
        p.shutdown();
    }

    #[test]
    fn disk_cache_provides_cross_instance_hits() {
        let dir = temp_cache_dir("openpptview-pipeline-disk");
        let config = PipelineConfig {
            disk_cache_dir: Some(dir.clone()),
            ..PipelineConfig::default()
        };

        // 第一个实例：渲染并写入磁盘
        {
            let p = pipeline_with(3, config.clone());
            let (_, o) = p.render_now(0, 1.0).unwrap();
            assert_eq!(o, RenderOutcome::Rendered);
            p.shutdown();
        }

        // 第二个实例（模拟重新打开课件）：应命中磁盘
        {
            let p = pipeline_with(3, config);
            let (_, o) = p.render_now(0, 1.0).unwrap();
            assert_eq!(o, RenderOutcome::DiskHit, "跨实例应命中磁盘缓存");
            assert_eq!(p.hit_count(), 1);
            p.shutdown();
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_cache_miss_for_different_page() {
        let dir = temp_cache_dir("openpptview-pipeline-disk-miss");
        let config = PipelineConfig {
            disk_cache_dir: Some(dir.clone()),
            ..PipelineConfig::default()
        };

        let p = pipeline_with(5, config);
        let _ = p.render_now(0, 1.0).unwrap();
        let (_, o) = p.render_now(1, 1.0).unwrap();
        assert_eq!(o, RenderOutcome::Rendered, "未缓存的页应实际渲染");
        p.shutdown();

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prefetch_fills_cache_asynchronously() {
        let p = pipeline_with(10, PipelineConfig::default());

        p.prefetch(3, Priority::Adjacent);
        assert!(p.pending_tasks() >= 1);

        assert!(
            p.wait_idle(std::time::Duration::from_secs(5)),
            "预取应能完成"
        );
        assert!(p.try_take(3, 1.0).is_some(), "预取后应能从缓存取到");
        assert_eq!(p.cached_bitmap_count(), 1);

        p.shutdown();
    }

    #[test]
    fn prefetch_around_queues_neighbors() {
        let p = pipeline_with(20, PipelineConfig::default());
        p.prefetch_around(10, 2, 2);
        // 前后各 2 页 = 4 个任务
        assert_eq!(p.pending_tasks(), 4);

        assert!(p.wait_idle(std::time::Duration::from_secs(10)));
        for page in [8, 9, 11, 12] {
            assert!(p.try_take(page, 1.0).is_some(), "第 {} 页应已预热", page + 1);
        }
        // 更远的页不应被预热
        assert!(p.try_take(7, 1.0).is_none());
        assert!(p.try_take(13, 1.0).is_none());

        p.shutdown();
    }

    #[test]
    fn prefetch_around_handles_boundaries() {
        let p = pipeline_with(3, PipelineConfig::default());
        // 第 0 页：不应越界到负数
        p.prefetch_around(0, 2, 2);
        assert_eq!(p.pending_tasks(), 2, "只有向后 2 页有效");

        assert!(p.wait_idle(std::time::Duration::from_secs(5)));
        assert!(p.try_take(1, 1.0).is_some());
        assert!(p.try_take(2, 1.0).is_some());

        p.shutdown();
    }

    #[test]
    fn prefetch_ignores_out_of_range_page() {
        let p = pipeline_with(3, PipelineConfig::default());
        p.prefetch(99, Priority::Adjacent);
        assert_eq!(p.pending_tasks(), 0, "越界页不应入队");
        p.shutdown();
    }

    #[test]
    fn navigate_cancels_stale_prefetch() {
        let p = pipeline_with(50, PipelineConfig::default());

        // 模拟快速翻页：每次 navigate 都会作废上一批预取
        for page in 0..10 {
            let _ = p.navigate(page, 1.0).unwrap();
        }

        assert!(
            p.dropped_count() > 0,
            "快速翻页应产生被取消的预取任务，实际 {}",
            p.dropped_count()
        );

        p.shutdown();
    }

    #[test]
    fn navigate_returns_current_page_immediately() {
        let p = pipeline_with(20, PipelineConfig::default());

        // 即使队列里堆满缩略图任务，当前页也应立即返回
        p.prefetch_thumbnails(0.2);
        let bmp = p.navigate(5, 1.0).unwrap();
        assert_eq!((bmp.width, bmp.height), (100, 50));

        p.shutdown();
    }

    #[test]
    fn thumbnail_batch_is_queued_and_completes() {
        let p = pipeline_with(8, PipelineConfig::default());
        p.prefetch_thumbnails(0.2);

        assert_eq!(p.pending_tasks(), 8);
        assert!(p.wait_idle(std::time::Duration::from_secs(10)));

        // 缩略图是 20×10 像素
        let thumb = p.try_take(0, 0.2).expect("缩略图应已生成");
        assert_eq!((thumb.width, thumb.height), (20, 10));

        // 主视图档位不受影响
        assert!(p.try_take(0, 1.0).is_none());

        p.shutdown();
    }

    #[test]
    fn thumbnails_do_not_block_current_page() {
        let p = pipeline_with(50, PipelineConfig::default());
        p.prefetch_thumbnails(0.2);

        // 当前页请求插在缩略图批处理中间
        let start = std::time::Instant::now();
        let bmp = p.navigate(25, 1.0).unwrap();
        let elapsed = start.elapsed();

        assert!(bmp.width > 0);
        // 当前页是同步渲染，不应等待 50 个缩略图任务
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "当前页不应被缩略图阻塞，实际 {elapsed:?}"
        );

        p.shutdown();
    }

    #[test]
    fn out_of_range_page_returns_error() {
        let p = pipeline_with(3, PipelineConfig::default());
        assert!(p.render_now(99, 1.0).is_err());
        assert!(p.scene(99).is_err());
        p.shutdown();
    }

    #[test]
    fn clear_caches_empties_everything() {
        let dir = temp_cache_dir("openpptview-pipeline-clear");
        let config = PipelineConfig {
            disk_cache_dir: Some(dir.clone()),
            ..PipelineConfig::default()
        };
        let p = pipeline_with(5, config);

        let _ = p.render_now(0, 1.0).unwrap();
        assert!(p.cached_bitmap_count() > 0);

        p.clear_caches();
        assert_eq!(p.cached_bitmap_count(), 0);
        assert_eq!(p.cached_scene_count(), 0);

        // 清空后应重新渲染而非命中磁盘
        let (_, o) = p.render_now(0, 1.0).unwrap();
        assert_eq!(o, RenderOutcome::Rendered);

        p.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn purge_disk_cache_removes_this_document_only() {
        let dir = temp_cache_dir("openpptview-pipeline-purge");
        let config = PipelineConfig {
            disk_cache_dir: Some(dir.clone()),
            ..PipelineConfig::default()
        };
        let p = pipeline_with(3, config);
        let _ = p.render_now(0, 1.0).unwrap();

        p.purge_disk_cache();
        // 内存仍有效，但磁盘已清空
        let _ = p.render_now(1, 1.0).unwrap();
        p.shutdown();

        // 新实例应重新渲染（磁盘缓存已被清理）
        let p2 = pipeline_with(3, PipelineConfig {
            disk_cache_dir: Some(dir.clone()),
            ..PipelineConfig::default()
        });
        let (_, o) = p2.render_now(0, 1.0).unwrap();
        assert_eq!(o, RenderOutcome::Rendered, "磁盘缓存应已被清除");
        p2.shutdown();

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn without_disk_dir_pipeline_still_works() {
        let config = PipelineConfig {
            disk_cache_dir: None,
            ..PipelineConfig::default()
        };
        let p = pipeline_with(3, config);
        let (_, o) = p.render_now(0, 1.0).unwrap();
        assert_eq!(o, RenderOutcome::Rendered);
        assert_eq!(o.source(), CacheSource::Rendered);
        p.shutdown();
    }

    #[test]
    fn set_scale_changes_prefetch_bucket() {
        let p = pipeline_with(10, PipelineConfig::default());
        p.set_scale(2.0);
        p.prefetch(1, Priority::Adjacent);
        assert!(p.wait_idle(std::time::Duration::from_secs(5)));

        // 2.0 档位应有缓存
        let bmp = p.try_take(1, 2.0).expect("应按新缩放档预取");
        assert_eq!((bmp.width, bmp.height), (200, 100));
        // 1.0 档位不应有
        assert!(p.try_take(1, 1.0).is_none());

        p.shutdown();
    }

    #[test]
    fn memory_budget_limits_cached_pages() {
        let config = PipelineConfig {
            // 每页 100×50×4 = 20000 字节；预算只够 2 页
            memory_pages: 20,
            max_bitmap_bytes: 45_000,
            ..PipelineConfig::default()
        };
        let p = pipeline_with(20, config);

        for page in 0..6 {
            let _ = p.render_now(page, 1.0).unwrap();
        }

        assert!(
            p.cached_bitmap_bytes() <= 45_000,
            "内存占用应受预算约束，实际 {}",
            p.cached_bitmap_bytes()
        );
        assert!(p.cached_bitmap_count() <= 2);

        p.shutdown();
    }

    #[test]
    fn low_end_config_is_conservative() {
        let c = PipelineConfig::low_end();
        assert_eq!(c.threads, Some(1));
        assert!(!c.draw_effects);
        assert!(c.memory_pages <= 4);
        assert!(c.max_pixels <= 4_000_000);
    }

    #[test]
    fn high_end_config_is_generous() {
        let c = PipelineConfig::high_end();
        assert_eq!(c.threads, Some(4));
        assert!(c.memory_pages >= 16);
        assert!(c.max_bitmap_bytes >= 256 * 1024 * 1024);
    }

    #[test]
    fn parallel_rendering_uses_multiple_threads() {
        let config = PipelineConfig {
            threads: Some(3),
            ..PipelineConfig::default()
        };
        let p = pipeline_with(30, config);
        assert_eq!(p.thread_count(), 3, "应启动 3 个工作线程");

        p.prefetch_thumbnails(0.5);
        assert_eq!(p.pending_tasks(), 30);
        assert!(p.wait_idle(std::time::Duration::from_secs(30)));

        // 注意：不能断言「缓存里有 30 页」—— 内存缓存按字节预算淘汰，
        // 默认只保留 8 页。这里验证的是「30 个任务全部被处理完」。
        assert!(p.cached_bitmap_count() > 0, "应有渲染结果进入缓存");
        assert!(
            p.cached_bitmap_count() <= 8,
            "内存缓存应受页数上限约束，实际 {}",
            p.cached_bitmap_count()
        );

        p.shutdown();
    }

    #[test]
    fn thumbnail_batch_completes_with_all_pages() {
        let p = pipeline_with(30, PipelineConfig::default());
        p.prefetch_thumbnails(0.5);
        assert!(p.wait_idle(std::time::Duration::from_secs(30)));

        // 逐页确认都渲染过（缓存可能已淘汰，因此用调度器的完成计数判断）
        assert_eq!(p.pending_tasks(), 0, "队列应已清空");
        assert_eq!(p.dropped_count(), 0, "无导航时不应有任务被丢弃");
        assert!(
            p.miss_count() == 0,
            "缩略图走异步路径，不应计入同步渲染的未命中计数"
        );

        p.shutdown();
    }

    #[test]
    fn pipeline_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Pipeline>();
        assert_send_sync::<PipelineConfig>();
    }

    #[test]
    fn concurrent_navigation_from_multiple_threads_is_safe() {
        let p = Arc::new(pipeline_with(20, PipelineConfig::default()));

        let handles: Vec<_> = (0..4)
            .map(|i| {
                let p = Arc::clone(&p);
                std::thread::spawn(move || {
                    // 每个线程读不同页，模拟多窗口/多标签
                    p.render_now(i * 3, 1.0).is_ok()
                })
            })
            .collect();

        for h in handles {
            assert!(h.join().expect("线程不应 panic"));
        }

        p.shutdown();
    }

    #[test]
    fn repeated_navigation_is_stable() {
        let p = pipeline_with(10, PipelineConfig::default());

        // 来回翻页 20 次，不应出现死锁或缓存错乱
        for _ in 0..10 {
            for page in [0usize, 5, 9, 5] {
                let bmp = p.navigate(page, 1.0).unwrap();
                assert!(bmp.width > 0);
            }
        }

        assert!(p.hit_count() > 0, "回翻应命中缓存");
        p.shutdown();
    }
}
