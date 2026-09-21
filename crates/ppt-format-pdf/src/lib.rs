//! ppt-format-pdf —— PDF 格式支持：页面栅格化与 `DocumentSource` 适配。
//!
//! # 为什么 PDF 不走 SceneGraph
//!
//! OOXML 是「描述怎么画」的格式，可以整体转成一份可复用的矢量场景图：
//! 换个缩放档只需重新光栅化，不必重新解析。
//!
//! PDF 则是**一串顺序执行的绘图指令**，没有稳定可提取的中间表示 ——
//! 字体子集、透明度组、软遮罩、裁剪路径都依赖解释器的运行状态。
//! 硬抽一层 SceneGraph 只会既慢又不准，因此这里把缩放直接交给
//! [`DocumentSource::rasterize_page`]，由 hayro 一次解释到位。
//!
//! # 线程模型：为什么每次渲染新建缓存
//!
//! hayro 的 `RenderCache` 内部是 `Rc<RefCell<..>>`，**不是 `Send`**，
//! 无法放进我们的渲染线程池。这里的取舍是每次渲染新建一个缓存：
//! 单页渲染时缓存命中的主要是页内的字体与图像，跨页复用收益有限；
//! 而外层的两级位图缓存已经保证同一页通常只渲染一次。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{render, RenderCache, RenderSettings};

use ppt_core::document::{Bitmap, DocumentSource, MediaProvider, PageContent, PixelFormat};
use ppt_core::scene::Size;
use ppt_core::{DocFormat, Error, Result};

/// 指纹取多少个字节的摘要（与 pptx 侧一致：16 字节 → 32 个十六进制字符）。
const FINGERPRINT_BYTES: usize = 16;

/// 指纹采样的首尾各取多少字节。
///
/// PDF 动辄几十上百 MB，为了算指纹把整份文件过一遍哈希是纯粹的浪费。
/// 头尾采样 + 文件长度已经能让「内容不同则指纹不同」在实践中成立 ——
/// 与 pptx 侧按 ZIP 中央目录取指纹是同一个思路。
const FINGERPRINT_SAMPLE: usize = 64 * 1024;

/// 打开一份 PDF。
pub struct PdfSource {
    path: PathBuf,
    /// 文件字节。`Pdf` 内部也持有同一份 `Arc`，
    /// 这里额外留一个引用是为了算指纹与诊断时能直接读到原始字节。
    bytes: Arc<Vec<u8>>,
    pdf: Pdf,
    /// 每页尺寸（pt，已计入页面旋转）。
    ///
    /// 打开时一次算完：`render_dimensions()` 只读页字典的 Box，
    /// 不解释内容流，因此对 200 页的文档也是微秒级。
    sizes: Vec<Size>,
    fingerprint: String,
}

impl PdfSource {
    /// 打开一个 `.pdf` 文件。
    ///
    /// 只读取交叉引用表与页树，**不解释任何内容流** ——
    /// 与 pptx 侧的惰性约定保持一致。
    pub fn open(path: impl AsRef<Path>) -> Result<PdfSource> {
        let path = path.as_ref().to_path_buf();
        let bytes = Arc::new(std::fs::read(&path).map_err(|e| {
            Error::other(format!("无法读取 PDF 文件 {}：{e}", path.display()))
        })?);

        if bytes.is_empty() {
            return Err(Error::other("PDF 文件为空"));
        }

        let fingerprint = fingerprint_of(&bytes);

        let pdf = Pdf::new(Arc::clone(&bytes)).map_err(|e| {
            // 加密文档是最常见的失败原因，单独给出可操作的提示
            Error::UnsupportedFormat(format!(
                "无法打开该 PDF（{e:?}）。若文档有密码保护，请先解除加密"
            ))
        })?;

        let sizes: Vec<Size> = pdf
            .pages()
            .iter()
            .map(|p| {
                let (w, h) = p.render_dimensions();
                // 损坏的页可能给出 0 或 NaN，兜底成 A4 比例以免后续除零
                if w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0 {
                    Size::new(w, h)
                } else {
                    Size::new(595.0, 842.0)
                }
            })
            .collect();

        if sizes.is_empty() {
            return Err(Error::other("该 PDF 没有任何页面"));
        }

        Ok(PdfSource {
            path,
            bytes,
            pdf,
            sizes,
            fingerprint,
        })
    }

    /// 按索引取页尺寸（pt）。
    pub fn page_size_pt(&self, index: usize) -> Option<Size> {
        self.sizes.get(index).copied()
    }

    /// 原始字节（诊断用）。
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// 光栅化一页。
    ///
    /// `max_pixels` 超限时按比例下调缩放：一张 A0 图纸按屏幕缩放渲染
    /// 会瞬间吃掉几百 MB，对「适合老电脑」是致命的。
    pub fn rasterize(&self, index: usize, scale: f32, max_pixels: u64) -> Result<Bitmap> {
        self.check_index(index)?;
        let page = &self.pdf.pages()[index];

        let (w_pt, h_pt) = page.render_dimensions();
        let scale = effective_scale(w_pt, h_pt, scale, max_pixels);

        // vello 的 Pixmap 用 u16 存宽高，超出直接拒绝而不是静默截断
        let out_w = (w_pt * scale).round().max(1.0);
        let out_h = (h_pt * scale).round().max(1.0);
        if out_w > u16::MAX as f32 || out_h > u16::MAX as f32 {
            return Err(Error::render(format!(
                "页面尺寸过大（{out_w}×{out_h} 像素），无法渲染"
            )));
        }
        let (out_w, out_h) = (out_w as u16, out_h as u16);

        // 每次渲染新建缓存，原因见模块文档的「线程模型」
        let cache = RenderCache::new();
        let settings = InterpreterSettings::default();
        let render_settings = RenderSettings {
            x_scale: scale,
            y_scale: scale,
            width: Some(out_w),
            height: Some(out_h),
            // PDF 页面本身没有背景色，缺省补白 ——
            // 否则透明底会在暗色主题下变成一片黑
            bg_color: WHITE,
        };

        let pixmap = render(page, &cache, &settings, &render_settings);

        // vello 的 Pixmap 与我们的内部位图同为**预乘 RGBA8**，可直接搬运
        let data = pixmap.data_as_u8_slice().to_vec();
        let bitmap = Bitmap {
            width: out_w as u32,
            height: out_h as u32,
            format: PixelFormat::Rgba8Premultiplied,
            data,
        };
        if !bitmap.is_consistent() {
            return Err(Error::render("PDF 光栅化产出的位图长度与尺寸不一致"));
        }
        Ok(bitmap)
    }
}

impl MediaProvider for PdfSource {
    fn read_media(&self, part: &str) -> Result<Vec<u8>> {
        // PDF 的字体与图像都在文件内部由解释器直接取用，
        // 没有「按部件名读取」的概念
        Err(Error::MissingPart(part.to_string()))
    }

    fn has_media(&self, _part: &str) -> bool {
        false
    }
}

impl DocumentSource for PdfSource {
    fn page_count(&self) -> usize {
        self.sizes.len()
    }

    fn default_page_size_pt(&self) -> Size {
        // 首页尺寸：老师手里的 PDF 几乎都是通版尺寸
        self.sizes.first().copied().unwrap_or(Size::new(595.0, 842.0))
    }

    fn page_content(&self, index: usize) -> Result<PageContent> {
        // 这里返回错误是有意的：PDF 没有场景图。
        // 调用方应先看 `rasterizes_directly()`，否则拿到的页码无法对应任何可渲染内容。
        self.check_index(index)?;
        Err(Error::other(
            "PDF 没有矢量场景图，请通过 rasterize_page 获取页面位图",
        ))
    }

    fn rasterizes_directly(&self) -> bool {
        true
    }

    fn rasterize_page(&self, index: usize, scale: f32, max_pixels: u64) -> Result<Bitmap> {
        self.rasterize(index, scale, max_pixels)
    }

    fn title(&self) -> Option<String> {
        // 不解析 PDF 元数据：课件文件名就是老师认得的标题，
        // 而 Info 字典里的标题常是「Microsoft Word - xxx.doc」这类噪声
        self.path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
    }

    fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    fn source_path(&self) -> Option<&Path> {
        Some(&self.path)
    }

    fn format(&self) -> DocFormat {
        DocFormat::Pdf
    }
}

/// 把缩放限制在像素预算内。
///
/// 宁可稍糊也不能 OOM：老机器上内存比清晰度更稀缺。
fn effective_scale(w_pt: f32, h_pt: f32, scale: f32, max_pixels: u64) -> f32 {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let w = w_pt.max(1.0) as f64;
    let h = h_pt.max(1.0) as f64;
    let wanted = w * h * (scale as f64) * (scale as f64);
    if wanted <= max_pixels as f64 {
        return scale;
    }
    let k = (max_pixels as f64 / (w * h)).sqrt();
    (k as f32).max(0.01)
}

/// 内容指纹：文件长度 + 首尾各 64 KiB 的 SHA-256 摘要（取前 16 字节）。
fn fingerprint_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(b"openpptview-pdf-v1");
    hasher.update((bytes.len() as u64).to_le_bytes());

    let head = &bytes[..bytes.len().min(FINGERPRINT_SAMPLE)];
    hasher.update(head);
    if bytes.len() > FINGERPRINT_SAMPLE {
        let tail_start = bytes.len() - FINGERPRINT_SAMPLE;
        hasher.update(&bytes[tail_start..]);
    }

    let digest = hasher.finalize();
    digest[..FINGERPRINT_BYTES]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手写一份最小可用的 PDF：两页，各画一个实心矩形。
    ///
    /// 不依赖任何外部样本文件 —— 单测要能在任何机器上跑。
    fn minimal_pdf() -> Vec<u8> {
        let objects: Vec<&str> = vec![
            // 1: Catalog
            "<< /Type /Catalog /Pages 2 0 R >>",
            // 2: Pages
            "<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 >>",
            // 3: Page 1
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Contents 4 0 R >>",
            // 4: Page 1 content
            "<< /Length 44 >>\nstream\n1 0 0 rg 10 10 80 40 re f\n0 0 1 rg 100 10 80 40 re f\nendstream",
            // 5: Page 2，尺寸不同，用于验证「每页尺寸独立」
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 200] /Contents 6 0 R >>",
            // 6: Page 2 content
            "<< /Length 47 >>\nstream\n0 0.6 0 rg 20 20 60 160 re f\n1 1 0 rg 20 150 60 30 re f\nendstream",
        ];

        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(b"%PDF-1.7\n");
        let mut offsets = vec![0usize];
        for (i, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }

        let xref_at = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
        out.extend_from_slice(b"0000000000 65535 f \n");
        for off in &offsets[1..] {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    fn write_pdf(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, minimal_pdf()).expect("写入临时 PDF 失败");
        path
    }

    #[test]
    fn opens_and_reports_pages_and_sizes() {
        let path = write_pdf("openpptview-pdf-basic.pdf");
        let src = PdfSource::open(&path).expect("应能打开");

        assert_eq!(src.page_count(), 2);
        assert!(src.rasterizes_directly());
        assert_eq!(src.format(), DocFormat::Pdf);

        let first = src.page_size_pt(0).unwrap();
        assert!((first.w - 200.0).abs() < 0.01, "实际 {}", first.w);
        assert!((first.h - 100.0).abs() < 0.01, "实际 {}", first.h);

        // 第二页尺寸不同：不能被首页尺寸带偏
        let second = src.page_size_pt(1).unwrap();
        assert!((second.w - 100.0).abs() < 0.01, "实际 {}", second.w);
        assert!((second.h - 200.0).abs() < 0.01, "实际 {}", second.h);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rasterizes_page_with_expected_geometry() {
        let path = write_pdf("openpptview-pdf-raster.pdf");
        let src = PdfSource::open(&path).unwrap();

        let bmp = src.rasterize(0, 1.0, 64_000_000).expect("应能光栅化");
        assert_eq!((bmp.width, bmp.height), (200, 100));
        assert!(bmp.is_consistent());
        assert_eq!(bmp.format, PixelFormat::Rgba8Premultiplied);

        // PDF 的坐标原点在左下角：内容流里 y=10 的矩形应出现在**图像下方**
        // （矩形从底边往上 40pt 高，即图像 y 60..90 区间）
        let red = bmp.pixel(20, 60).expect("应取到像素");
        assert!(
            red.r > 200 && red.g < 60 && red.b < 60,
            "底部的红色矩形应在图像下方，实际取到 {red:?}"
        );

        // 右上角应为白色背景（y=10 靠近图像顶部，那里没有内容）
        let bg = bmp.pixel(20, 10).expect("应取到像素");
        assert!(
            bg.r > 240 && bg.g > 240 && bg.b > 240,
            "顶部应为白色背景，实际取到 {bg:?}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rasterize_respects_pixel_budget() {
        let path = write_pdf("openpptview-pdf-budget.pdf");
        let src = PdfSource::open(&path).unwrap();

        // 预算只够 200×100 的十分之一：缩放应被下调而不是报错
        let bmp = src.rasterize(0, 4.0, 2_000).expect("预算不足也应能出图");
        assert!(
            (bmp.width as u64) * (bmp.height as u64) <= 2_500,
            "输出像素数应落在预算附近，实际 {}×{}",
            bmp.width,
            bmp.height
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn out_of_range_page_is_rejected() {
        let path = write_pdf("openpptview-pdf-range.pdf");
        let src = PdfSource::open(&path).unwrap();
        assert!(src.rasterize(9, 1.0, 64_000_000).is_err());
        assert!(src.rasterize_page(9, 1.0, 64_000_000).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn page_content_points_to_rasterize_instead_of_faking_a_scene() {
        let path = write_pdf("openpptview-pdf-content.pdf");
        let src = PdfSource::open(&path).unwrap();
        let err = src.page_content(0).expect_err("PDF 不应产出场景图");
        assert!(
            err.to_string().contains("rasterize_page"),
            "错误信息应指路到 rasterize_page：{err}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_empty_and_garbage_files() {
        let bad = std::env::temp_dir().join("openpptview-pdf-bad.pdf");
        std::fs::write(&bad, b"").unwrap();
        assert!(PdfSource::open(&bad).is_err(), "空文件应被拒绝");

        std::fs::write(&bad, b"not a pdf at all").unwrap();
        assert!(PdfSource::open(&bad).is_err(), "非 PDF 应被拒绝");

        let _ = std::fs::remove_file(&bad);
    }

    #[test]
    fn fingerprint_is_stable_and_content_addressed() {
        let a = fingerprint_of(b"aaa");
        let b = fingerprint_of(b"aaa");
        let c = fingerprint_of(b"bbb");
        assert_eq!(a, b, "同样内容指纹应一致（磁盘缓存依赖它）");
        assert_ne!(a, c);
        assert_eq!(a.len(), FINGERPRINT_BYTES * 2);
    }
}
