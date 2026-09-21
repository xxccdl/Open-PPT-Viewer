//! 图片解码与缓存。
//!
//! # 三个关键优化
//!
//! **① 延迟解码**：解析阶段只记录部件名（[`ppt_core::scene::ImageRef::part`]），
//! 真正的解压与解码推迟到渲染时才发生。这样打开课件时不会为了
//! 未访问页面的图片付出代价。
//!
//! **② 按显示尺寸降采样**：一张 4000×3000 的照片放在 300×200pt 的框里，
//! 若按原分辨率解码再缩放，内存与 CPU 都浪费了 100 倍以上。
//! 因此在解码前就按「目标显示尺寸 × 超采样系数」算出需要的像素数，
//! 用 `image` 的缩放在解码后立即降采样。
//!
//! **③ 直接产出 `tiny_skia::Pixmap`**：绕开「中间 Vec → 再转 Pixmap」的
//! 二次分配与全图拷贝。图片在课件里是最重的资源，
//! 每帧为一张 1920×1080 的图多拷 8MB 是不可接受的。
//!
//! 缓存按「部件名 + 目标尺寸档」索引：同一张图在不同页以不同大小出现时
//! 各自缓存一份，避免互相反复重采样。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tiny_skia::Pixmap;

use ppt_core::scene::{ImageRef, RelativeRect, Size};

/// 缓存中的图片句柄。
pub type SharedImage = Arc<Pixmap>;

/// 图片缓存。
///
/// 两级键：部件名 → 目标尺寸档。同一图片的多个尺寸档各自保留，
/// 因为缩略图与大图的需求差异很大，强行共用会有一方被浪费。
pub struct ImageCache {
    entries: Mutex<HashMap<(String, u32), SharedImage>>,
    /// 缓存的总字节数上限。
    max_bytes: usize,
    current_bytes: Mutex<usize>,
}

impl std::fmt::Debug for ImageCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageCache")
            .field("entries", &self.entries.lock().map(|e| e.len()).unwrap_or(0))
            .field("max_bytes", &self.max_bytes)
            .finish()
    }
}

/// 默认缓存上限（128MB）。
///
/// 取这个值是因为：一张 1920×1080 的预乘位图约 8MB，
/// 128MB 能容纳十几张大图，足够覆盖「当前页 + 预热的相邻页」；
/// 而 4GB 内存的老机器也能承受这个常驻量。
const DEFAULT_MAX_BYTES: usize = 128 * 1024 * 1024;

impl Default for ImageCache {
    fn default() -> Self {
        ImageCache::new(DEFAULT_MAX_BYTES)
    }
}

impl ImageCache {
    pub fn new(max_bytes: usize) -> ImageCache {
        ImageCache {
            entries: Mutex::new(HashMap::new()),
            max_bytes,
            current_bytes: Mutex::new(0),
        }
    }

    /// 取缓存中的图片。
    pub fn get(&self, part: &str, size_bucket: u32) -> Option<SharedImage> {
        let key = (part.to_string(), size_bucket);
        self.entries.lock().ok()?.get(&key).cloned()
    }

    /// 写入缓存；超出上限时整体清空。
    ///
    /// 采用「清空」而不是 LRU 逐出，是因为图片大小差异极大，
    /// 逐出策略需要额外记账；而渲染管线的上层（`ppt-pipeline`）
    /// 已有按页的缓存与预渲染调度，这里只需防止无界增长。
    pub fn insert(&self, part: &str, size_bucket: u32, image: SharedImage) {
        let bytes = image.data().len();
        let key = (part.to_string(), size_bucket);

        if let Ok(mut cur) = self.current_bytes.lock() {
            if *cur + bytes > self.max_bytes {
                if let Ok(mut entries) = self.entries.lock() {
                    entries.clear();
                }
                *cur = 0;
            }
            *cur += bytes;
        }
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(key, image);
        }
    }

    /// 已缓存的图片数。
    pub fn len(&self) -> usize {
        self.entries.lock().map(|e| e.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 清空缓存（切换课件时调用）。
    pub fn clear(&self) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.clear();
        }
        if let Ok(mut cur) = self.current_bytes.lock() {
            *cur = 0;
        }
    }
}

/// 解码参数。
#[derive(Debug, Clone, Copy)]
pub struct DecodeRequest {
    /// 目标显示宽度（设备像素）。
    pub target_width: u32,
    /// 目标显示高度（设备像素）。
    pub target_height: u32,
    /// 源裁剪区域（相对比例）。
    pub src_rect: RelativeRect,
    /// 解码前的额外超采样倍率。
    ///
    /// 留一点余量（默认 1.5）是为了在渲染器做旋转/非整数缩放时
    /// 仍有足够像素可用，避免边缘发虚。
    pub supersample: f32,
}

impl DecodeRequest {
    /// 由形状尺寸与渲染缩放比构造解码请求。
    pub fn from_display(display: Size, scale: f32, src_rect: RelativeRect) -> DecodeRequest {
        let supersample = 1.5;
        DecodeRequest {
            target_width: ((display.w * scale * supersample).ceil() as u32).max(1),
            target_height: ((display.h * scale * supersample).ceil() as u32).max(1),
            src_rect,
            supersample,
        }
    }

    /// 该请求对应的缓存档位（用于把相近尺寸归到同一档）。
    ///
    /// 按 2 的幂分档：尺寸差在 2 倍以内复用同一份解码结果，
    /// 避免「同一张图被要求 301px 和 302px 各解码一次」。
    pub fn size_bucket(&self) -> u32 {
        let m = self.target_width.max(self.target_height).max(1);
        (m as f32).log2().ceil() as u32
    }
}

/// 图片在画布上的物理显示尺寸（磅）。
///
/// `bbox` 是节点的**局部**尺寸，`unit` 是「一个局部单位等于多少 pt」
/// （见 `ppt_core::scene::Node::effective_scale`）。
///
/// # 为什么不能直接用局部尺寸
///
/// 组合（`p:grpSp`）的子形状活在**被缩放的局部坐标系**里：`a:ext` 与 `a:chExt`
/// 之比可以小到 0.04。直接拿局部尺寸当「要解多少像素」，会把课件里那张
/// 830×586 的世界地图解码成 **1×1**，再放大铺满 361×231pt ——
/// 屏幕上就是一片模糊的粉灰色块，完全看不出是地图。
pub fn display_size(bbox: Size, unit: f32) -> Size {
    Size::new((bbox.w * unit).max(1.0), (bbox.h * unit).max(1.0))
}

/// 解码一张图片为 tiny-skia 位图。
///
/// `bytes` 是部件的原始字节；失败时返回 `None` 并由调用方记录告警 ——
/// 单张图片坏掉不应让整页渲染失败。
pub fn decode(bytes: &[u8], req: DecodeRequest) -> Option<Pixmap> {
    let img = image::load_from_memory(bytes).ok()?;
    let (full_w, full_h) = (img.width(), img.height());
    if full_w == 0 || full_h == 0 {
        return None;
    }

    // 先按 srcRect 裁剪出可见区域
    let cropped = crop_to_src_rect(img, req.src_rect);
    let (cw, ch) = (cropped.width(), cropped.height());
    if cw == 0 || ch == 0 {
        return None;
    }

    // 再按目标尺寸降采样。只在确实需要缩小时做，放大交给渲染器的双线性采样，
    // 避免为放大浪费内存。
    let target_w = req.target_width.min(cw).max(1);
    let target_h = req.target_height.min(ch).max(1);

    let scaled = if target_w < cw || target_h < ch {
        // 用 Triangle 滤波：质量与速度的平衡点，
        // Lanczos3 在 4GB 老机上开销过大
        cropped.resize_exact(target_w, target_h, image::imageops::FilterType::Triangle)
    } else {
        cropped
    };

    let rgba = scaled.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    let mut data = rgba.into_raw();

    // tiny-skia 要求预乘 alpha；`image` 产出的是直通 alpha
    premultiply_in_place(&mut data);

    let size = tiny_skia::IntSize::from_wh(w, h)?;
    Pixmap::from_vec(data, size)
}

/// 按相对比例裁剪图片。
fn crop_to_src_rect(
    img: image::DynamicImage,
    src: RelativeRect,
) -> image::DynamicImage {
    if src.is_full() {
        return img;
    }
    let (w, h) = (img.width() as f32, img.height() as f32);
    let x = (src.l.clamp(0.0, 1.0) * w).floor() as u32;
    let y = (src.t.clamp(0.0, 1.0) * h).floor() as u32;
    let r = (src.r.clamp(0.0, 1.0) * w).ceil() as u32;
    let b = (src.b.clamp(0.0, 1.0) * h).ceil() as u32;

    let cw = r.saturating_sub(x).max(1).min(img.width().saturating_sub(x).max(1));
    let ch = b.saturating_sub(y).max(1).min(img.height().saturating_sub(y).max(1));

    img.crop_imm(x, y, cw, ch)
}

/// 把直通 RGBA 就地转为预乘 RGBA。
fn premultiply_in_place(data: &mut [u8]) {
    for px in data.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a == 255 {
            continue;
        }
        if a == 0 {
            px[..3].fill(0);
            continue;
        }
        for c in &mut px[..3] {
            *c = ((*c as u32 * a + 127) / 255) as u8;
        }
    }
}

/// 该图片引用是否值得解码。
///
/// 部件名为空说明关系解析失败（见 `ppt-format-pptx` 的容错策略），
/// 此时画不出任何东西，直接跳过比让渲染器去做一次必然失败的加载更省事。
pub fn should_decode(img: &ImageRef) -> bool {
    !img.part.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ppt_core::scene::Color;

    /// 生成一张纯色 PNG 字节（用于测试解码路径）。
    fn solid_png(w: u32, h: u32, color: Color) -> Vec<u8> {
        let mut img = image::RgbaImage::new(w, h);
        for px in img.pixels_mut() {
            *px = image::Rgba([color.r, color.g, color.b, color.a]);
        }
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("生成测试 PNG 失败");
        out.into_inner()
    }

    #[test]
    fn decodes_png_at_target_size() {
        let bytes = solid_png(200, 100, Color::rgb(255, 0, 0));
        let req = DecodeRequest {
            target_width: 50,
            target_height: 25,
            src_rect: RelativeRect::FULL,
            supersample: 1.0,
        };
        let img = decode(&bytes, req).expect("应能解码");
        assert_eq!((img.width(), img.height()), (50, 25));
        assert_eq!(img.data().len(), 50 * 25 * 4);
    }

    #[test]
    fn decode_does_not_upscale_beyond_source() {
        let bytes = solid_png(10, 10, Color::rgb(0, 255, 0));
        let req = DecodeRequest {
            target_width: 500,
            target_height: 500,
            src_rect: RelativeRect::FULL,
            supersample: 1.0,
        };
        let img = decode(&bytes, req).expect("应能解码");
        // 放大交给渲染器，解码阶段不做无谓插值
        assert_eq!((img.width(), img.height()), (10, 10));
    }

    #[test]
    fn decode_applies_src_rect_crop() {
        let bytes = solid_png(100, 100, Color::rgb(0, 0, 255));
        let req = DecodeRequest {
            target_width: 100,
            target_height: 100,
            src_rect: RelativeRect {
                l: 0.0,
                t: 0.0,
                r: 0.5,
                b: 0.5,
            },
            supersample: 1.0,
        };
        let img = decode(&bytes, req).expect("应能解码");
        assert_eq!((img.width(), img.height()), (50, 50), "应裁出左上四分之一");
    }

    #[test]
    fn decode_rejects_garbage() {
        let req = DecodeRequest {
            target_width: 10,
            target_height: 10,
            src_rect: RelativeRect::FULL,
            supersample: 1.0,
        };
        assert!(decode(b"not an image at all", req).is_none());
        assert!(decode(&[], req).is_none());
    }

    #[test]
    fn premultiply_is_applied() {
        let bytes = solid_png(2, 2, Color::rgba(255, 0, 0, 128));
        let req = DecodeRequest {
            target_width: 2,
            target_height: 2,
            src_rect: RelativeRect::FULL,
            supersample: 1.0,
        };
        let img = decode(&bytes, req).unwrap();
        let data = img.data();
        // 红色分量应被预乘为约 128
        assert!(
            (data[0] as i32 - 128).abs() <= 2,
            "预乘后红色应约为 128，实际 {}",
            data[0]
        );
        assert_eq!(data[3], 128, "alpha 不变");
    }

    #[test]
    fn premultiply_leaves_opaque_pixels_untouched() {
        let mut data = vec![10, 20, 30, 255];
        premultiply_in_place(&mut data);
        assert_eq!(data, vec![10, 20, 30, 255]);
    }

    #[test]
    fn premultiply_zeros_fully_transparent() {
        let mut data = vec![200, 100, 50, 0];
        premultiply_in_place(&mut data);
        assert_eq!(data, vec![0, 0, 0, 0]);
    }

    #[test]
    fn size_bucket_groups_similar_sizes() {
        let mk = |w: u32| DecodeRequest {
            target_width: w,
            target_height: w,
            src_rect: RelativeRect::FULL,
            supersample: 1.0,
        };
        // 300 与 400 落在同一档（2^9 = 512）
        assert_eq!(mk(300).size_bucket(), mk(400).size_bucket());
        // 100 与 600 分属不同档
        assert_ne!(mk(100).size_bucket(), mk(600).size_bucket());
    }

    #[test]
    fn decode_request_from_display_applies_supersample() {
        let req = DecodeRequest::from_display(Size::new(100.0, 50.0), 2.0, RelativeRect::FULL);
        // 100 * 2.0 * 1.5 = 300
        assert_eq!(req.target_width, 300);
        assert_eq!(req.target_height, 150);
    }

    #[test]
    fn decode_request_never_zero_size() {
        let req = DecodeRequest::from_display(Size::ZERO, 0.0, RelativeRect::FULL);
        assert_eq!(req.target_width, 1);
        assert_eq!(req.target_height, 1);
    }

    #[test]
    fn cache_stores_and_returns_entries() {
        let cache = ImageCache::new(1024 * 1024);
        let img = Arc::new(Pixmap::new(2, 2).unwrap());
        cache.insert("ppt/media/a.png", 3, Arc::clone(&img));
        assert!(cache.get("ppt/media/a.png", 3).is_some());
        // 不同档位互不影响
        assert!(cache.get("ppt/media/a.png", 5).is_none());
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn cache_clears_when_exceeding_budget() {
        // 上限只够放一个 2×2 的位图（16 字节）
        let cache = ImageCache::new(20);
        let img = Arc::new(Pixmap::new(2, 2).unwrap());
        cache.insert("a", 1, Arc::clone(&img));
        cache.insert("b", 1, Arc::clone(&img));
        // 超出上限应触发清空，避免常驻内存无界增长
        assert!(cache.len() <= 1, "实际 {}", cache.len());
    }

    #[test]
    fn cache_clear_empties_everything() {
        let cache = ImageCache::new(1024 * 1024);
        cache.insert("a", 1, Arc::new(Pixmap::new(1, 1).unwrap()));
        assert!(!cache.is_empty());
        cache.clear();
        assert!(cache.is_empty());
        assert!(cache.get("a", 1).is_none());
    }

    #[test]
    fn decoded_image_reports_pixel_dimensions() {
        let img = Pixmap::new(2, 3).unwrap();
        assert_eq!((img.width(), img.height()), (2, 3));
        assert_eq!(img.data().len(), 2 * 3 * 4);
    }

    #[test]
    fn decode_handles_jpeg() {
        // 用 JPEG 走一遍解码分支，确认多格式支持
        let img = image::RgbaImage::from_pixel(8, 8, image::Rgba([0, 0, 0, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut out, image::ImageFormat::Jpeg)
            .unwrap();
        let bytes = out.into_inner();

        let req = DecodeRequest {
            target_width: 8,
            target_height: 8,
            src_rect: RelativeRect::FULL,
            supersample: 1.0,
        };
        let decoded = decode(&bytes, req).expect("应能解码 JPEG");
        assert_eq!((decoded.width(), decoded.height()), (8, 8));
    }

    #[test]
    fn display_size_uses_physical_points_not_local_units() {
        // 组合里那张世界地图的真实数字：组合的 ext/chExt ≈ 558，
        // 局部尺寸只有 0.648×0.4395「pt」—— 乘回单位才是 361×245pt。
        // 不乘的话解码目标算出来是 1×1，地图化成一整片模糊色块。
        let local = Size::new(0.648, 0.4395);
        let physical = display_size(local, 558.0);
        assert!(
            (physical.w - 361.6).abs() < 0.5 && (physical.h - 245.2).abs() < 0.5,
            "应还原成物理磅值，实际 {physical:?}"
        );

        // 顶层形状的单位是 1，尺寸原样
        assert_eq!(display_size(Size::new(320.0, 180.0), 1.0).w, 320.0);

        // 退化输入也得给个至少 1 像素的目标，不能出现 0
        let tiny = display_size(Size::new(0.0, -3.0), 0.01);
        assert!(tiny.w >= 1.0 && tiny.h >= 1.0);
    }

    #[test]
    fn should_decode_requires_a_part_name() {
        let mut img = ImageRef::new("");
        assert!(!should_decode(&img));
        img.part = "ppt/media/a.png".to_string();
        assert!(should_decode(&img));
    }
}
