//! 文档抽象层：格式无关的页面来源接口。
//!
//! 上层（调度、缓存、UI、标注）只依赖本模块的 trait，
//! 不关心底层是 PPTX 的 SceneGraph 还是 PDF 的位图。

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::scene::{Scene, Size};

/// 支持的文件格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DocFormat {
    /// Office Open XML 演示文稿（.pptx / .ppsx / .potx）。
    Pptx,
    /// 便携式文档（.pdf）。
    Pdf,
    /// PowerPoint 97-2003 二进制（.ppt）—— 后续阶段支持，当前仅识别。
    PptLegacy,
    /// Word 二进制/压缩文档（.doc/.docx）—— 后续阶段支持，当前仅识别。
    WordLegacy,
    /// Excel 文档 —— 后续阶段支持，当前仅识别。
    Excel,
}

impl DocFormat {
    /// 当前版本是否已实现渲染。
    #[inline]
    pub fn is_supported(self) -> bool {
        matches!(self, DocFormat::Pptx | DocFormat::Pdf)
    }

    /// 面向用户的格式名。
    pub fn display_name(self) -> &'static str {
        match self {
            DocFormat::Pptx => "PowerPoint 演示文稿",
            DocFormat::Pdf => "PDF 文档",
            DocFormat::PptLegacy => "PowerPoint 97-2003 演示文稿",
            DocFormat::WordLegacy => "Word 文档",
            DocFormat::Excel => "Excel 工作簿",
        }
    }

    /// 当前版本遇到该格式时给用户的说明。
    ///
    /// 说清楚**这是什么、现在怎么办**，而不是「将在后续版本支持」——
    /// 老师拿着一份打不开的课件站在讲台上，「以后会支持」帮不了他。
    /// 调用方（`ppt-app`）会在此基础上补一句「已经替你用系统默认程序打开了」。
    pub fn unsupported_hint(self) -> Option<&'static str> {
        match self {
            DocFormat::Pptx | DocFormat::Pdf => None,
            DocFormat::PptLegacy => Some(
                "这是旧版 Office 二进制文件（97-2003），而且扩展名看不出是哪一种。\
                 演示文稿（.ppt / .pps / .pot）本版本会自动转成 .pptx 再打开；\
                 若它是 Word / Excel 文档，请用对应程序打开",
            ),
            DocFormat::WordLegacy => Some(
                "这是 Word 文档，不是演示文稿；本版本只放映 .pptx 课件与 .pdf 文档",
            ),
            DocFormat::Excel => Some(
                "这是 Excel 工作簿，不是演示文稿；本版本只放映 .pptx 课件与 .pdf 文档",
            ),
        }
    }
}

/// ZIP 容器魔数。
const MAGIC_ZIP: [u8; 4] = [0x50, 0x4B, 0x03, 0x04];
/// 空 ZIP / 分卷 ZIP 也可作为合法容器出现。
const MAGIC_ZIP_EMPTY: [u8; 4] = [0x50, 0x4B, 0x05, 0x06];
/// CFB（OLE2 复合文档）魔数：旧版 .ppt/.doc/.xls 使用。
const MAGIC_CFB: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
/// PDF 头。
const MAGIC_PDF: &[u8] = b"%PDF-";

/// 依据魔数探测格式（不依赖文件扩展名）。
///
/// OOXML 与旧版二进制 Office 格式都基于容器，具体是哪种文档
/// 需要进一步看容器内部；这里先给出「容器级」判断，
/// 由各格式解析器在打开时做二次确认。
pub fn detect_format(bytes: &[u8]) -> Option<DocFormat> {
    if bytes.len() >= 4 && (bytes[..4] == MAGIC_ZIP || bytes[..4] == MAGIC_ZIP_EMPTY) {
        // ZIP 容器：OOXML 家族（pptx/docx/xlsx 均如此）
        return Some(DocFormat::Pptx);
    }
    if bytes.len() >= 8 && bytes[..8] == MAGIC_CFB {
        return Some(DocFormat::PptLegacy);
    }
    if bytes.len() >= MAGIC_PDF.len() && &bytes[..MAGIC_PDF.len()] == MAGIC_PDF {
        return Some(DocFormat::Pdf);
    }
    None
}

/// 依据扩展名推测格式（作为魔数探测失败时的兜底提示）。
pub fn detect_format_by_extension(path: &str) -> Option<DocFormat> {
    let lower = path.to_ascii_lowercase();
    let ext = lower.rsplit('.').next()?;
    match ext {
        // OOXML 家族的六种演示文稿扩展名：演示文稿、放映、模板，
        // 以及各自的「启用宏」变体。漏掉哪一种，那种课件就会报
        // 「无法识别的文件格式」—— 而它其实完全能渲染（.pptm/.ppsm 只是多了宏，
        // 宏在放映器里本来也不执行）。
        "pptx" | "pptm" | "ppsx" | "ppsm" | "potx" | "potm" => Some(DocFormat::Pptx),
        "pdf" => Some(DocFormat::Pdf),
        "ppt" | "pps" | "pot" => Some(DocFormat::PptLegacy),
        "doc" | "docx" | "rtf" => Some(DocFormat::WordLegacy),
        "xls" | "xlsx" | "csv" => Some(DocFormat::Excel),
        _ => None,
    }
}

/// 像素格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PixelFormat {
    /// 预乘 alpha 的 RGBA8。
    ///
    /// 这是整条管线的规范内部格式：
    /// - CPU 光栅器（tiny-skia）与 PDF 光栅器（hayro）都原生产出预乘结果；
    /// - 编辑器/合成可直接做 `src + dst*(1-src.a)`，无需反复乘除；
    /// - 交给 WebView 前由 `png` 编码器负责还原为直通 alpha。
    #[default]
    Rgba8Premultiplied,
    /// 直通 alpha 的 RGBA8（仅在需要与外部 API 对接时使用）。
    Rgba8,
}

impl PixelFormat {
    #[inline]
    pub fn bytes_per_pixel(self) -> usize {
        4
    }
}

/// 一张位图。
///
/// 行间无 padding，`data.len() == width * height * 4`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub data: Vec<u8>,
}

impl Bitmap {
    /// 创建一张透明（全 0）位图。
    pub fn new_transparent(width: u32, height: u32) -> Bitmap {
        Bitmap {
            width,
            height,
            format: PixelFormat::Rgba8Premultiplied,
            data: vec![0u8; (width as usize) * (height as usize) * 4],
        }
    }

    /// 创建一张填充指定直通 RGBA 颜色的位图。
    pub fn new_filled(width: u32, height: u32, color: crate::scene::Color) -> Bitmap {
        let mut bmp = Bitmap::new_transparent(width, height);
        bmp.fill(color);
        bmp
    }

    /// 用直通 RGBA 颜色填满。
    pub fn fill(&mut self, color: crate::scene::Color) {
        // 转为预乘
        let a = color.a as u16;
        let pr = ((color.r as u16 * a + 127) / 255) as u8;
        let pg = ((color.g as u16 * a + 127) / 255) as u8;
        let pb = ((color.b as u16 * a + 127) / 255) as u8;
        for px in self.data.chunks_exact_mut(4) {
            px[0] = pr;
            px[1] = pg;
            px[2] = pb;
            px[3] = color.a;
        }
    }

    #[inline]
    pub fn stride(&self) -> usize {
        self.width as usize * 4
    }

    #[inline]
    pub fn pixel_count(&self) -> usize {
        self.width as usize * self.height as usize
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0 || self.data.is_empty()
    }

    /// 校验缓冲区长度与尺寸是否自洽。
    pub fn is_consistent(&self) -> bool {
        self.data.len() == self.pixel_count() * 4
    }

    /// 取某个像素的直通 RGBA（主要用于测试与调试）。
    pub fn pixel(&self, x: u32, y: u32) -> Option<crate::scene::Color> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let i = (y as usize * self.stride()) + (x as usize * 4);
        let d = self.data.get(i..i + 4)?;
        let a = d[3] as u16;
        let un = |v: u8| -> u8 {
            // a == 0 时反预乘无意义（会除以 0），直接当作全透明
            match (v as u16 * 255 + a / 2).checked_div(a) {
                Some(q) => q.min(255) as u8,
                None => 0,
            }
        };
        match self.format {
            PixelFormat::Rgba8Premultiplied => Some(crate::scene::Color::rgba(
                un(d[0]),
                un(d[1]),
                un(d[2]),
                d[3],
            )),
            PixelFormat::Rgba8 => Some(crate::scene::Color::rgba(d[0], d[1], d[2], d[3])),
        }
    }

    /// 估算内存占用（字节）。
    #[inline]
    pub fn memory_bytes(&self) -> usize {
        self.data.len()
    }

    /// 转为直通 alpha 的 RGBA 缓冲（用于需要直通语义的外部接口）。
    pub fn to_straight_rgba(&self) -> Vec<u8> {
        if self.format == PixelFormat::Rgba8 {
            return self.data.clone();
        }
        let mut out = self.data.clone();
        for px in out.chunks_exact_mut(4) {
            let a = px[3] as u16;
            if a == 0 {
                px[..3].fill(0);
            } else if a != 255 {
                for c in &mut px[..3] {
                    *c = ((*c as u16 * 255 + a / 2) / a).min(255) as u8;
                }
            }
        }
        out
    }
}

/// 一页的内容：要么是待光栅化的场景图，要么是已经光栅化的位图。
#[derive(Debug, Clone)]
pub enum PageContent {
    /// 需要由 `ppt-render` 光栅化。
    Scene(Box<Scene>),
    /// 后端已经产出位图（PDF 路径）。
    Bitmap(Box<Bitmap>),
}

impl PageContent {
    /// 该页的显示尺寸（pt）。
    #[inline]
    pub fn size_pt(&self) -> Size {
        match self {
            PageContent::Scene(s) => s.size_pt,
            PageContent::Bitmap(_) => Size::ZERO,
        }
    }

    #[inline]
    pub fn is_scene(&self) -> bool {
        matches!(self, PageContent::Scene(_))
    }
}

/// 媒体资源提供者。
///
/// 渲染器通过它按需拉取图片/媒体字节，从而避免在解析阶段
/// 就把整包媒体解压进内存。
pub trait MediaProvider: Send + Sync {
    /// 读取一个包内部件（图片、媒体等）。
    fn read_media(&self, part: &str) -> Result<Vec<u8>>;

    /// 部件是否存在（避免为缺失资源抛错）。
    fn has_media(&self, part: &str) -> bool {
        self.read_media(part).is_ok()
    }
}

/// 格式无关的文档来源。
pub trait DocumentSource: MediaProvider {
    /// 页数。
    fn page_count(&self) -> usize;

    /// 文档级默认页面尺寸（pptX 取自 `presentation.xml`，PDF 取首页）。
    ///
    /// 这是**廉价**调用：不触发任何页面解析，用于首屏布局占位与缩略图排布。
    fn default_page_size_pt(&self) -> Size;

    /// 取一页内容。这是唯一可能触发页面解析的方法，必须保持惰性。
    fn page_content(&self, index: usize) -> Result<PageContent>;

    /// 该格式是否**自带光栅化**（PDF 等「页本身就是绘图指令」的后端）。
    ///
    /// 返回 `true` 时，管线不会走 `page_content` + `ppt-render`，
    /// 而是把缩放直接交给 [`DocumentSource::rasterize_page`] ——
    /// 这类格式没有稳定可复用的矢量场景图（字体子集、透明度组、
    /// 裁剪都依赖解释器状态），硬套一层 SceneGraph 只是白饶一圈。
    fn rasterizes_directly(&self) -> bool {
        false
    }

    /// 后端自认为的「原生档」缩放（每 pt 出多少像素），`None` 表示由管线定。
    ///
    /// 用途只有一个：让管线知道**按什么分辨率出图最划算**。
    ///
    /// 对逐页位图后端（本机办公软件按屏幕宽度导出的每页图片）尤其重要 ——
    /// 固有分辨率就那么多，管线若还按固定长边去算，要么白缩放一次，
    /// 要么把同一页的缓存分到两个桶里，等于每次都重新出图。
    fn native_scale_hint(&self) -> Option<f32> {
        None
    }

    /// 来源是否**自己已经把光栅结果存下来了**（本机办公软件导出的逐页图片）。
    ///
    /// 为真时管线不再往自己的磁盘缓存里存一份原始 RGBA —— 那是纯粹的重复：
    /// 一页 1920×1080 的原始位图是 8.3MB，39 页就多占 320MB，
    /// 还会把位图缓存的预算挤爆，把别的课件的缓存顶出去。
    fn has_own_raster_cache(&self) -> bool {
        false
    }

    /// 按缩放直接光栅化一页。
    ///
    /// 仅当 [`DocumentSource::rasterizes_directly`] 为真时被调用。
    /// `max_pixels` 是输出像素数上限：**超出时后端应自行下调实际缩放**，
    /// 而不是报错 —— 宁可稍糊一点，也不能让课件打不开。
    fn rasterize_page(&self, index: usize, scale: f32, max_pixels: u64) -> Result<Bitmap> {
        let _ = (index, scale, max_pixels);
        Err(Error::other("该格式不支持直接光栅化"))
    }

    /// 取「这一页播到第 `steps_played` 步」时的位图。
    ///
    /// # 为什么它和 [`DocumentSource::rasterize_page`] 是两件事
    ///
    /// `rasterize_page` 给的是**整页拍平的一张图** —— 上面什么都有，
    /// 包括这一步还不该露面的答案。拿它当每一帧，老师点下去只会发现
    /// 「弹出的都是已经显示的内容」，逐元素弹出就没了。
    ///
    /// 逐页位图后端（本机办公软件导出的每页图片）在出图时**可以把
    /// 「这一步还不该出现」的形状隐掉**，因此它能按步给出真正的各帧。
    /// 这是唯一能同时做到「画面 100% 保真」和「逐元素弹出」的路径：
    /// 让作者工具自己画每一帧，而不是让我们猜。
    ///
    /// 返回 `Ok(None)` 表示本来源不出这种帧（场景图后端、PDF，
    /// 或这一页本来就没有动画），调用方照常按形状可见性自己渲染。
    ///
    /// `steps_played` 是**已播的步数**（0 = 一步都没播）。
    fn stepped_raster(
        &self,
        index: usize,
        steps_played: usize,
        scale: f32,
        max_pixels: u64,
    ) -> Result<Option<Bitmap>> {
        let _ = (index, steps_played, scale, max_pixels);
        Ok(None)
    }

    /// 这一页（这一步）有没有一份**现成的 PNG 文件**可以直接送去显示。
    ///
    /// # 为什么值得开这条路
    ///
    /// 逐页位图后端天然有：办公软件写出来的就是 PNG。而默认那条路要
    /// 「读文件 → 解码 → 缩放 → 转预乘 → 按**原始像素**传 8MB」，
    /// 每一步都是白花的 —— 那张图本来就是给眼睛看的，解码和缩放
    /// 交给浏览器（有 GPU）比我们在后端做快得多。
    ///
    /// 实测：同一页从 ~100ms 降到 ~10ms，传输量小一个数量级。
    ///
    /// `steps_played`：`None` = 全部播完，`Some(k)` = 已播 k 步。
    /// 返回 `Some` 只代表**文件在**，不保证内容符合调用方的缩放要求 ——
    /// 显示端本来就要把它铺到画布上，缩放由它自己做。
    fn raster_png_path(&self, index: usize, steps_played: Option<usize>) -> Option<PathBuf> {
        let _ = (index, steps_played);
        None
    }

    /// 演讲者备注（PDF 无此概念，返回 `None`）。
    fn notes(&self, index: usize) -> Result<Option<String>> {
        let _ = index;
        Ok(None)
    }

    /// 文档标题。
    fn title(&self) -> Option<String> {
        None
    }

    /// 内容指纹，用于磁盘缓存目录命名。
    fn fingerprint(&self) -> &str;

    /// 源文件路径（若有）。
    fn source_path(&self) -> Option<&std::path::Path> {
        None
    }

    fn format(&self) -> DocFormat;

    /// 校验页索引并给出统一错误。
    fn check_index(&self, index: usize) -> Result<()> {
        let n = self.page_count();
        if index >= n {
            return Err(Error::other(format!(
                "页码超出范围：第 {} 页（共 {} 页）",
                index + 1,
                n
            )));
        }
        Ok(())
    }
}

/// 线程安全的共享文档句柄。
pub type SharedSource = Arc<dyn DocumentSource>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_zip_container_as_pptx() {
        assert_eq!(detect_format(b"PK\x03\x04rest"), Some(DocFormat::Pptx));
        assert_eq!(detect_format(b"PK\x05\x06"), Some(DocFormat::Pptx));
    }

    #[test]
    fn detect_pdf_container() {
        assert_eq!(detect_format(b"%PDF-1.7\n..."), Some(DocFormat::Pdf));
        assert_eq!(detect_format(b"%PDF-"), Some(DocFormat::Pdf));
    }

    #[test]
    fn detect_cfb_container_as_legacy_ppt() {
        let bytes = [0xD0u8, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1, 0x00];
        assert_eq!(detect_format(&bytes), Some(DocFormat::PptLegacy));
    }

    #[test]
    fn detect_unknown_and_truncated_input() {
        assert_eq!(detect_format(b""), None);
        assert_eq!(detect_format(b"PK"), None);
        assert_eq!(detect_format(b"random bytes"), None);
        // 恰好 PDF 长度但内容不符
        assert_eq!(detect_format(b"%PDX-"), None);
    }

    #[test]
    fn extension_hints() {
        assert_eq!(detect_format_by_extension("a/b/课件.PPTX"), Some(DocFormat::Pptx));
        assert_eq!(detect_format_by_extension("x.pdf"), Some(DocFormat::Pdf));
        assert_eq!(detect_format_by_extension("x.ppt"), Some(DocFormat::PptLegacy));
        assert_eq!(detect_format_by_extension("x.docx"), Some(DocFormat::WordLegacy));
        assert_eq!(detect_format_by_extension("x.unknown"), None);
        assert_eq!(detect_format_by_extension("noext"), None);
    }

    #[test]
    fn every_ooxml_presentation_extension_is_accepted() {
        // 演示文稿、放映、模板，以及各自的「启用宏」变体 —— 六种都是
        // 同一套 OOXML 结构，都能渲染。漏一种，那种课件就被报成
        // 「无法识别的文件格式」，老师只会以为文件坏了。
        for ext in ["pptx", "pptm", "ppsx", "ppsm", "potx", "potm"] {
            assert_eq!(
                detect_format_by_extension(&format!("课件.{ext}")),
                Some(DocFormat::Pptx),
                ".{ext} 应被当作可渲染的演示文稿"
            );
        }
        // 旧版二进制：`ppt` / `pps` / `pot` 是同一个 CFB 家族，都归为「仅识别」
        for ext in ["ppt", "pps", "pot"] {
            assert_eq!(
                detect_format_by_extension(&format!("课件.{ext}")),
                Some(DocFormat::PptLegacy),
                ".{ext} 是旧版二进制格式"
            );
        }
    }

    #[test]
    fn supported_format_flags() {
        assert!(DocFormat::Pptx.is_supported());
        assert!(DocFormat::Pdf.is_supported());
        assert!(!DocFormat::PptLegacy.is_supported());
        assert!(DocFormat::PptLegacy.unsupported_hint().is_some());
        assert!(DocFormat::Pptx.unsupported_hint().is_none());
    }

    #[test]
    fn legacy_ppt_hint_says_what_to_do_not_what_is_coming() {
        // 提示语必须落到「怎么办」上。
        //
        // 曾经写的是「旧版 .ppt 格式将在后续版本支持」—— 老师拿着一份打不开的
        // 课件站在讲台上，这句话帮不了他：他还得自己猜怎么转格式、转成什么。
        //
        // 现在演示文稿（.ppt / .pps / .pot）由 `ppt-app` 转成 .pptx 后自动打开，
        // 走到这条提示的只剩「OLE2 容器、但扩展名看不出是哪一种」，
        // 所以这里要说清「拿对应程序打开」，而不是让老师去找转换入口。
        let hint = DocFormat::PptLegacy.unsupported_hint().unwrap();
        assert!(hint.contains("请用对应程序打开"), "要指出怎么办：{hint}");
        assert!(hint.contains(".ppt"), "要说清哪几种扩展名会被自动转换：{hint}");
        assert!(!hint.contains("后续版本"), "不要只说「以后会支持」：{hint}");
    }

    #[test]
    fn bitmap_construction_and_consistency() {
        let b = Bitmap::new_transparent(4, 3);
        assert_eq!(b.stride(), 16);
        assert_eq!(b.pixel_count(), 12);
        assert_eq!(b.data.len(), 48);
        assert!(b.is_consistent());
        assert!(!b.is_empty());
    }

    #[test]
    fn bitmap_fill_produces_premultiplied_values() {
        let mut b = Bitmap::new_transparent(2, 2);
        b.fill(crate::scene::Color::rgba(255, 0, 0, 128));
        // 预乘后红色分量约为 128
        assert_eq!(b.data[3], 128);
        assert!((b.data[0] as i32 - 128).abs() <= 1);
        assert_eq!(b.data[1], 0);
    }

    #[test]
    fn bitmap_fill_opaque_is_exact() {
        let mut b = Bitmap::new_transparent(1, 1);
        b.fill(crate::scene::Color::rgb(10, 20, 30));
        assert_eq!(&b.data[..4], &[10, 20, 30, 255]);
    }

    #[test]
    fn bitmap_pixel_roundtrip() {
        let mut b = Bitmap::new_transparent(2, 2);
        b.fill(crate::scene::Color::rgb(200, 100, 50));
        assert_eq!(b.pixel(1, 1), Some(crate::scene::Color::rgb(200, 100, 50)));
        assert_eq!(b.pixel(5, 5), None);
    }

    #[test]
    fn bitmap_to_straight_rgba_unpremultiplies() {
        let mut b = Bitmap::new_transparent(1, 1);
        b.fill(crate::scene::Color::rgba(255, 128, 64, 128));
        let straight = b.to_straight_rgba();
        // 还原后应接近原始值（±2 为预乘舍入误差）
        assert!((straight[0] as i32 - 255).abs() <= 2, "r = {}", straight[0]);
        assert!((straight[1] as i32 - 128).abs() <= 2, "g = {}", straight[1]);
        assert!((straight[2] as i32 - 64).abs() <= 2, "b = {}", straight[2]);
        assert_eq!(straight[3], 128);
    }

    #[test]
    fn straight_format_to_straight_is_identity() {
        let b = Bitmap {
            width: 1,
            height: 1,
            format: PixelFormat::Rgba8,
            data: vec![1, 2, 3, 4],
        };
        assert_eq!(b.to_straight_rgba(), vec![1, 2, 3, 4]);
    }

    #[test]
    fn inconsistent_bitmap_detected() {
        let b = Bitmap {
            width: 10,
            height: 10,
            format: PixelFormat::Rgba8Premultiplied,
            data: vec![0; 4],
        };
        assert!(!b.is_consistent());
        assert_eq!(b.memory_bytes(), 4);
    }

    #[test]
    fn pixel_format_bytes_per_pixel() {
        assert_eq!(PixelFormat::Rgba8.bytes_per_pixel(), 4);
        assert_eq!(PixelFormat::Rgba8Premultiplied.bytes_per_pixel(), 4);
    }
}
