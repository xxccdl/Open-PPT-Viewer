//! 从「本机办公软件导出的逐页位图」读取页面。
//!
//! 写入方是 [`crate::WarmRequest`]（走 COM 让 WPS / Office 逐页导出 PNG），
//! 本模块负责按同一约定读回来。**路径规则只在这里定义一处** ——
//! 写和读各写一份迟早会不一致，那是最难查的一类 bug。
//!
//! # 为什么值得为它写一个 [`DocumentSource`]
//!
//! 画面来自逐页位图，而链接/媒体/动画/备注仍然来自原始 OOXML：
//! 这正是管线早就支持的「画面来源 + 语义来源」双源结构
//! （见 `Pipeline::with_interaction`）。因此这里只需要实现「出图」这一件事，
//! 缓存、预取、调度、缩略图全部由现有管线复用。
//!
//! # 动画为什么要**按步**存图
//!
//! 整页拍平的一张图里什么都有，包括这一步还不该露面的答案 ——
//! 拿它当每一帧，老师点下去只会发现「弹出的都是已经显示的内容」。
//! 所以出图时会把「这一步还不该出现」的形状隐掉，把每一步都存成一张图
//! （`<页码>.<步数>.png`），`<页码>.png` 则是全部播完的样子。
//! 这样每一帧都是办公软件自己画的，保真与逐元素弹出可以同时成立。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ppt_core::{Bitmap, DocFormat, DocumentSource, Error, MediaProvider, PixelFormat, Result, Size};

use crate::StepPlanner;

/// 某一页 PNG 的路径。
///
/// **写方（转换器）与读方（本模块）必须共用这一个函数。**
pub fn page_path(dir: &Path, page: usize) -> PathBuf {
    dir.join(format!("{page}.png"))
}

/// 「已播 `steps_played` 步」那一帧的路径。
///
/// 与 `WpsRasterSource` 同为读方，`warm_pages_impl` 为写方，共用此函数。
pub fn step_path(dir: &Path, page: usize, steps_played: usize) -> PathBuf {
    dir.join(format!("{page}.{steps_played}.png"))
}

/// 出图产物是否已经存在于 `dir`（用于判断「这份课件热过了吗」）。
pub fn page_exists(dir: &Path, page: usize) -> bool {
    page_path(dir, page).is_file()
}

/// 该目录里已就绪的页数。
pub fn ready_count(dir: &Path, page_count: usize) -> usize {
    (0..page_count).filter(|&p| page_exists(dir, p)).count()
}

/// 这一页的动画分帧还缺不缺（`total` 是步数）。
///
/// 存在的意义是**让老缓存能自愈**：只出过整页图（这之前版本的行为）的课件，
/// 再打开一次就该把缺的分帧补出来，而不是因为「整页图已经有了」被整页跳过。
pub fn steps_missing(dir: &Path, page: usize, total: usize) -> bool {
    (0..total).any(|k| !step_path(dir, page, k).is_file())
}

/// 出图目录里「动画分帧已经出过一遍」的标记文件名。
///
/// # 为什么需要一个标记文件，而不是去数分帧文件
///
/// 「这一份课件的分帧齐了没有」要么把每一页都解析一遍（打开课件那 2 秒里
/// 付不起这个代价），要么就得有个写下来的结论。标记就是这个结论。
///
/// 它由出图任务在**全部页的分帧都成功之后**写下；中途被打断（老师关了应用）
/// 就不会有，下次打开重新补一遍（已经出好的那些各自跳过）。
const STEPS_MARKER: &str = "steps.done";

/// 动画分帧是否已经出过一遍。
pub fn steps_done(dir: &Path) -> bool {
    dir.join(STEPS_MARKER).is_file()
}

/// 记下「动画分帧已出过一遍」。
pub fn mark_steps_done(dir: &Path) -> std::io::Result<()> {
    std::fs::write(dir.join(STEPS_MARKER), b"1")
}

/// 一份「按页位图」文档。
pub struct WpsRasterSource {
    dir: PathBuf,
    /// 位图的固有缩放（每 pt 多少像素）—— 出图时按老师屏幕算出来的。
    native_scale: f32,
    page_count: usize,
    size_pt: Size,
    fingerprint: String,
    title: Option<String>,
    /// 出图任务是否已经排完。
    ///
    /// 用来把「还没轮到」和「这张出不来」分开：任务跑完之前缺页是正常等待，
    /// 跑完之后还缺就是真的没有 —— 前者该转圈，后者该说实话。
    warm_done: Arc<AtomicBool>,
    /// 已确认出好的页。纯内存缓存，避免每次渲染都去 stat 磁盘。
    ///
    /// **只记「有」，不记「没有」**：出图是后台逐页推进的，
    /// 「现在还没有」是个会过期的答案。把 `false` 也缓存下来，
    /// 会让先被问到的页永远停在「没有」上 —— 实测的表现就是
    /// 缩略图第 2 页之后全空白、翻页报「第 2 页未能生成」，
    /// 而同一时刻的日志里写着「39 页任务、0 页失败」。
    ready: Mutex<HashSet<usize>>,
    /// 「这一页每一步该藏哪些形状」的算账人（由 `ppt-app` 提供，它手上有解析器）。
    ///
    /// 出图方与读取方共用同一份 —— 两边步数对不上，帧就对不上。
    plans: StepPlanner,
    /// 页 → 步数。只在老师真的点动画时才追问，所以在打开课件那条路上没有开销。
    step_counts: Mutex<std::collections::HashMap<usize, usize>>,
}

impl WpsRasterSource {
    /// 构造。`native_scale` 必须与出图时用的宽度一致（见 [`WpsRasterSource::new`] 的调用方）。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        dir: PathBuf,
        native_scale: f32,
        page_count: usize,
        size_pt: Size,
        fingerprint: String,
        title: Option<String>,
        warm_done: Arc<AtomicBool>,
        plans: StepPlanner,
    ) -> WpsRasterSource {
        WpsRasterSource {
            dir,
            native_scale: native_scale.max(0.05),
            page_count,
            size_pt,
            fingerprint,
            title,
            warm_done,
            ready: Mutex::new(HashSet::new()),
            plans,
            step_counts: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// 出图目录。
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 这一页有几步动画（带缓存）。
    fn step_count(&self, page: usize) -> usize {
        if let Ok(cache) = self.step_counts.lock() {
            if let Some(&n) = cache.get(&page) {
                return n;
            }
        }
        let n = (self.plans)(page).len();
        if let Ok(mut cache) = self.step_counts.lock() {
            cache.insert(page, n);
        }
        n
    }

    /// 该页是否已经出好。
    ///
    /// 没出好的时候只 stat 一次磁盘（微秒级），**不留否定结论** ——
    /// 后台随时可能刚好把这一页写出来。
    fn is_ready(&self, page: usize) -> bool {
        if let Ok(cache) = self.ready.lock() {
            if cache.contains(&page) {
                return true;
            }
        }
        if !page_exists(&self.dir, page) {
            return false;
        }
        if let Ok(mut cache) = self.ready.lock() {
            cache.insert(page);
        }
        true
    }

    /// 解码一页 PNG，并缩放到 `scale`。
    fn decode(&self, page: usize, scale: f32, max_pixels: u64) -> Result<Bitmap> {
        self.decode_file(&page_path(&self.dir, page), page, scale, max_pixels)
    }

    /// 解码指定 PNG，并缩放到 `scale`。
    fn decode_file(
        &self,
        path: &Path,
        page: usize,
        scale: f32,
        max_pixels: u64,
    ) -> Result<Bitmap> {
        let img = image::open(path)
            .map_err(|e| Error::other(format!("无法读取第 {} 页的图：{e}", page + 1)))?
            .to_rgba8();

        // 目标尺寸：按页面 pt 尺寸 × 请求缩放。
        //
        // 超出 `max_pixels` 时**下调缩放而不是报错** —— 宁可稍糊一点，
        // 也不能让课件在这一页打不开（与 `DocumentSource` 的约定一致）。
        let w_pt = self.size_pt.w.max(1.0);
        let h_pt = self.size_pt.h.max(1.0);
        let cap = max_pixels.max(1) as f32;
        let mut target_scale = scale.max(0.01);
        let max_scale = (cap / (w_pt * h_pt)).sqrt();
        if target_scale > max_scale {
            target_scale = max_scale;
        }
        let tw = ((w_pt * target_scale).round() as u32).max(1);
        let th = ((h_pt * target_scale).round() as u32).max(1);

        let scaled = if tw != img.width() || th != img.height() {
            // 出图是按屏幕宽度定的，这里通常只是缩到缩略图那么大 ——
            // 用 Triangle（双线性）足够，且比 Lanczos 快一个数量级
            image::imageops::resize(&img, tw, th, image::imageops::FilterType::Triangle)
        } else {
            img
        };

        // 管线内部一律用**预乘** RGBA；PNG 是直通的，必须转
        let mut data = scaled.into_raw();
        for px in data.chunks_exact_mut(4) {
            let a = px[3] as u16;
            px[0] = ((px[0] as u16 * a + 127) / 255) as u8;
            px[1] = ((px[1] as u16 * a + 127) / 255) as u8;
            px[2] = ((px[2] as u16 * a + 127) / 255) as u8;
        }

        Ok(Bitmap {
            width: tw,
            height: th,
            format: PixelFormat::Rgba8Premultiplied,
            data,
        })
    }
}

impl MediaProvider for WpsRasterSource {
    fn read_media(&self, part: &str) -> Result<Vec<u8>> {
        // 媒体部件由语义来源（原始 OOXML）提供，见 `Pipeline::media_bytes`
        Err(Error::MissingPart(part.to_string()))
    }
}

impl DocumentSource for WpsRasterSource {
    fn page_count(&self) -> usize {
        self.page_count
    }

    fn default_page_size_pt(&self) -> Size {
        self.size_pt
    }

    fn page_content(&self, index: usize) -> Result<ppt_core::PageContent> {
        self.check_index(index)?;
        Err(Error::other(
            "位图来源不产出场景图；请调用 rasterize_page",
        ))
    }

    fn rasterizes_directly(&self) -> bool {
        true
    }

    fn native_scale_hint(&self) -> Option<f32> {
        Some(self.native_scale)
    }

    fn has_own_raster_cache(&self) -> bool {
        // 每页 PNG 就在 `dir` 里，管线不必再存一份原始 RGBA
        true
    }

    fn rasterize_page(&self, index: usize, scale: f32, max_pixels: u64) -> Result<Bitmap> {
        self.check_index(index)?;

        if self.is_ready(index) {
            return self.decode(index, scale, max_pixels);
        }

        // 还没轮到这一页：如实说「正在生成」，让前端转圈等，
        // **不要**在这里偷偷换成自研渲染 —— 那正是老师反复看到的「画面不对」
        if !self.warm_done.load(Ordering::Relaxed) {
            return Err(Error::NotReady { page: index });
        }

        Err(Error::other(format!(
            "第 {} 页未能生成：本机办公软件导出这一页失败，请检查该页是否含异常对象",
            index + 1
        )))
    }

    /// 取「这一页播到第 `steps_played` 步」的那一帧。
    ///
    /// 出图时已经把每一步都存成了一张图（见本模块顶部说明），所以这里
    /// 只是按 `steps_played` 找文件。**每一帧都是办公软件自己画的** ——
    /// 这是保真与「逐元素弹出」能同时成立的原因。
    fn stepped_raster(
        &self,
        index: usize,
        steps_played: usize,
        scale: f32,
        max_pixels: u64,
    ) -> Result<Option<Bitmap>> {
        self.check_index(index)?;

        let total = self.step_count(index);
        // 这一页没有动画，或者已经全播完：任何一步都是那唯一/最后一张图。
        // 返回 `None` 让调用方走普通路径 —— 那条路有原生档缓存，
        // 不必为「同一张图」再解码一次。
        if total == 0 || steps_played >= total {
            return Ok(None);
        }

        let path = step_path(&self.dir, index, steps_played);
        if path.is_file() {
            return self
                .decode_file(&path, index, scale, max_pixels)
                .map(Some);
        }

        // 这一帧还没出好：如实说「正在生成」，让前端等 ——
        // 绝不拿「全部播完」的那张顶替，那正是老师说的
        // 「弹出的都是已经显示的内容」。
        if !self.warm_done.load(Ordering::Relaxed) {
            return Err(Error::NotReady { page: index });
        }

        // 出图任务已经收尾却还是没有这一帧（导出这一页时失败了）：
        // 用整页图兜底，总比空白强。日志里能查到原因。
        Ok(None)
    }

    /// 这一步的 PNG 就在磁盘上，直接告诉调用方它在哪。
    ///
    /// 走这条路就不必「读 → 解码 → 缩放 → 转预乘 → 传 8MB 原始像素」了，
    /// 见 [`DocumentSource::raster_png_path`]。
    fn raster_png_path(&self, index: usize, steps_played: Option<usize>) -> Option<std::path::PathBuf> {
        if index >= self.page_count {
            return None;
        }
        let path = match steps_played {
            // 全部播完（以及没有动画的页）就是整页图
            None => page_path(&self.dir, index),
            Some(k) => {
                let total = self.step_count(index);
                if total == 0 || k >= total {
                    page_path(&self.dir, index)
                } else {
                    step_path(&self.dir, index, k)
                }
            }
        };
        path.is_file().then_some(path)
    }

    fn title(&self) -> Option<String> {
        self.title.clone()
    }

    fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    fn format(&self) -> DocFormat {
        // 画面由办公软件导出，但这份文档本身仍是 pptx
        DocFormat::Pptx
    }
}