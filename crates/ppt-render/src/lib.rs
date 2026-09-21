//! # ppt-render
//!
//! SceneGraph → 位图。整条渲染管线的最重一环。
//!
//! ## 渲染顺序
//!
//! ```text
//! 1. 背景（纯色 / 填充）
//! 2. 遍历节点树（深度优先，父在前子在后）
//!       ├─ 计算「局部 → 设备像素」的最终变换（父变换已烘焙进节点）
//!       ├─ 按几何类型绘制填充
//!       ├─ 绘制描边
//!       ├─ 图片：解码（带缓存与降采样）后绘制
//!       ├─ 表格：逐单元格绘制底纹/边框/文本
//!       └─ 文本：排版后按字形轮廓填充
//! 3. 降级占位（如果启用了 `debug_degraded`）
//! ```
//!
//! ## 两条性能约定
//!
//! **① 不可见节点零开销**：`Node::is_visible` 在解析层就做完了判断，
//! 渲染时直接跳过 —— 课件里大量存在的空占位符不会产生任何光栅化成本。
//!
//! **② 图片按显示尺寸解码**：见 [`image::DecodeRequest`]。
//! 一张 4000×3000 的照片放在 300×200pt 的框里，
//! 按原分辨率解码会浪费两个数量级的内存与时间。
//!
//! ## 输出格式
//!
//! 统一输出 [`ppt_core::Bitmap`]，格式为**预乘 RGBA8**。
//! 这是 tiny-skia 的原生格式，省掉一次全图遍历；
//! 编码为 PNG 交给 WebView 时由编码器负责还原直通 alpha。

pub mod convert;
pub mod image;
pub mod text;

use std::sync::Arc;

use tiny_skia::{Paint, Pixmap, PixmapPaint, Transform as SkTransform};

use ppt_core::scene::{
    transformed_bounds, Color, EffectState, Fill, Geometry, Insets, LineEnd, LineEndKind, Node,
    PathGeometry, PathSegment, PlayState, Point, Rect, Scene, SceneBackground, Size, Stroke, Table,
    TextBox, Transform,
};
use ppt_core::{Bitmap, Error, MediaProvider, PixelFormat, Result};
use ppt_text::{FontContext, LayoutOptions};

use image::{DecodeRequest, ImageCache, SharedImage};

/// 单独渲染动画图层时向外扩的余量（pt）。
///
/// 描边、阴影、发光都会画到形状包围盒之外；不留余量的话动画过程中
/// 图层边缘会被切出一条硬边。
const LAYER_MARGIN_PT: f32 = 8.0;

/// 某个形状的图层在画布上占的矩形（已外扩并夹回画布）。
///
/// 渲染端与调用方必须用**同一个**矩形：调用方拿它定位图层，
/// 渲染端拿它决定位图尺寸，算错一处图层就会错位。
///
/// `state` 是「这一步之前」的播放状态（已播的强调动画要算进位置与大小），
/// `extra` 是这一步可能出现的形变 —— 放大过程里图层会超出静止时的包围盒，
/// 不留出这些余量，动画放到一半就会被切边。
pub fn layer_bounds(
    scene: &Scene,
    shape_id: u32,
    state: PlayState,
    extra: &[EffectState],
) -> Option<Rect> {
    let node = scene.walk().find(|n| n.shape_id == Some(shape_id))?;
    if !node.is_visible() {
        return None;
    }
    let size = scene.size_pt;
    let base = node.canvas_bounds_at(state);
    let mut r = base;
    for e in extra {
        r = r.union(&transformed_bounds(base, e));
    }
    let r = r
        .expand(LAYER_MARGIN_PT)
        .intersect(&Rect::new(0.0, 0.0, size.w, size.h));
    (!r.is_empty()).then_some(r)
}

/// 绕 `center` 的形变矩阵：先平移到原点 → 缩放 → 旋转 → 平移回中心 → 叠加位移。
///
/// tiny-skia 的 `pre_concat(b)` 是「右乘」，即 b 先作用，所以这里
/// 从最外层往里逐级 pre_concat。
fn emphasis_affine(s: &EffectState, center: ppt_core::scene::Point) -> SkTransform {
    SkTransform::from_translate(s.dx, s.dy)
        .pre_concat(SkTransform::from_translate(center.x, center.y))
        .pre_concat(SkTransform::from_rotate(s.rotate))
        .pre_concat(SkTransform::from_scale(s.scale, s.scale))
        .pre_concat(SkTransform::from_translate(-center.x, -center.y))
}

/// 渲染参数。
#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    /// 输出像素 = pt × scale。
    ///
    /// 例如 960×540pt 的幻灯片在 1080p 屏幕上全屏显示时，
    /// scale 约为 2.0（1920/960）。
    pub scale: f32,
    /// 像素数上限，超出时自动下调 scale。
    ///
    /// 老机器上一次分配几十 MB 的位图会明显卡顿，
    /// 因此需要硬性保护。默认 1600 万像素（约 64MB 的 RGBA 缓冲）。
    pub max_pixels: u64,
    /// 是否绘制阴影、发光、柔化边缘等高开销效果。
    ///
    /// 低配档位可关闭，用「无效果」换取流畅。
    pub draw_effects: bool,
    /// 是否把降级占位（SmartArt/图表）画出来。
    ///
    /// 放映时开启，让老师看到「这里有内容但未渲染」；
    /// 生成缩略图时可关闭，画面更接近原始观感。
    pub draw_placeholders: bool,
    /// 动画播放状态（位掩码，见 [`ppt_core::scene::PlayState`]）。
    ///
    /// 放映时由前端按点击逐步置位；缩略图与静态导出一律用
    /// [`ppt_core::scene::PLAY_ALL`]。
    pub state: PlayState,
    /// 文本排版参数。
    pub layout: LayoutOptions,
    /// 内部用：正在渲染「单个形状的图层」。
    ///
    /// 图层是给前端做 60fps 合成的，必须**无视**该形状自身的构建步 ——
    /// 正在飞入的答案在「已播步」里还没出现，可它的图层正是要拿来飞的。
    #[doc(hidden)]
    pub layer_mode: bool,
    /// 内部用：图层只保留第 `st..=end` 段（`None` = 整框）。
    #[doc(hidden)]
    pub layer_paragraphs: Option<(u32, u32)>,
}

impl RenderOptions {
    /// 排版参数，已把「播放状态」并进去。
    ///
    /// 段落级的出场要由排版层跳过字形，节点级的出场由 `draw_node` 判断；
    /// 两者读必须是同一个数，所以在这里合流，免得调用方漏设其一。
    fn layout_for(&self) -> LayoutOptions {
        LayoutOptions {
            // 图层模式下**无视**构建步（节点级同理，见 `layer_mode`）：
            // 正在飞入的那一段在「已播步」里还没出现，可它的图层正是要拿来飞的。
            // 少了这一手，图层会是一张空图 —— 而且不报错，只是什么都看不见。
            reveal: if self.layer_mode {
                ppt_core::scene::PLAY_ALL
            } else {
                self.state
            },
            paragraph_only: self.layer_paragraphs,
            ..self.layout
        }
    }
}

impl Default for RenderOptions {
    fn default() -> Self {
        RenderOptions {
            scale: 1.0,
            max_pixels: 16_000_000,
            draw_effects: true,
            draw_placeholders: true,
            // 默认全部显示：静态导出与缩略图不该停在某一动画步上
            state: ppt_core::scene::PLAY_ALL,
            layout: LayoutOptions::default(),
            layer_mode: false,
            layer_paragraphs: None,
        }
    }
}

impl RenderOptions {
    /// 为缩略图场景构造（低分辨率、关闭高开销效果）。
    pub fn thumbnail(scale: f32) -> RenderOptions {
        RenderOptions {
            scale,
            draw_effects: false,
            draw_placeholders: false,
            layout: LayoutOptions {
                solve_autofit: false,
                ..LayoutOptions::default()
            },
            ..RenderOptions::default()
        }
    }

    /// 按像素上限修正后的实际缩放比。
    pub fn effective_scale(&self, size: Size) -> f32 {
        let scale = self.scale.max(0.01);
        let pixels = (size.w * scale) as f64 * (size.h * scale) as f64;
        if pixels <= self.max_pixels as f64 || pixels <= 0.0 {
            return scale;
        }
        // 按面积开方缩放，保证不超过上限
        let k = (self.max_pixels as f64 / pixels).sqrt();
        (scale * k as f32).max(0.01)
    }

    /// 该参数下输出的像素尺寸。
    pub fn output_size(&self, size: Size) -> (u32, u32) {
        let s = self.effective_scale(size);
        (
            (size.w * s).round().max(1.0) as u32,
            (size.h * s).round().max(1.0) as u32,
        )
    }
}

/// 渲染一页所需的可复用资源。
///
/// # 为什么图片缓存是 `Arc` 而字形缓存不是
///
/// 调度层会为**每个工作线程各建一个 `Renderer`** ——
/// 这样多个页面可以真正并行光栅化（`Renderer::render` 需要 `&mut self`，
/// 共享一个实例就只能串行）。此时：
///
/// - **字体索引**（构建一次几十毫秒）与**图片缓存**（一张大图几 MB）
///   是重量级资源，必须跨线程共享，否则内存会翻几倍；
/// - **字形轮廓缓存**很轻（一页几百个字形，每个几百字节），
///   各线程各存一份反而省掉了锁竞争 —— 字形提取是热路径。
pub struct Renderer {
    fonts: Arc<FontContext>,
    images: Arc<ImageCache>,
    glyphs: text::GlyphCache,
}

impl Renderer {
    pub fn new(fonts: Arc<FontContext>) -> Renderer {
        Renderer {
            fonts,
            images: Arc::new(ImageCache::default()),
            glyphs: text::GlyphCache::new(),
        }
    }

    /// 与其它渲染器共享图片缓存。
    ///
    /// 调度层用它让多个工作线程复用同一份已解码图片。
    pub fn with_shared_image_cache(
        fonts: Arc<FontContext>,
        images: Arc<ImageCache>,
    ) -> Renderer {
        Renderer {
            fonts,
            images,
            glyphs: text::GlyphCache::new(),
        }
    }

    /// 共享的图片缓存句柄（供调度层传递给其它线程）。
    pub fn image_cache(&self) -> Arc<ImageCache> {
        Arc::clone(&self.images)
    }

    /// 指定图片缓存上限（低配机器可调小）。
    pub fn with_image_cache(mut self, max_bytes: usize) -> Renderer {
        self.images = Arc::new(ImageCache::new(max_bytes));
        self
    }

    pub fn fonts(&self) -> &FontContext {
        &self.fonts
    }

    /// 清空图片与字形缓存（切换课件时调用，避免把上一份课件的图留在内存里）。
    pub fn clear_caches(&mut self) {
        self.images.clear();
        self.glyphs.clear();
    }

    /// 渲染一个场景。
    pub fn render(
        &mut self,
        scene: &Scene,
        opts: &RenderOptions,
        media: &dyn MediaProvider,
    ) -> Result<Bitmap> {
        self.render_hiding(scene, opts, media, &[])
    }

    /// 渲染一个场景，但**跳过**指定形状。
    ///
    /// 强调动画（放大、旋转）的前端合成要用它：被强调的对象在底帧里必须消失，
    /// 否则「静止的原件 + 动起来的图层」会叠成重影。
    pub fn render_hiding(
        &mut self,
        scene: &Scene,
        opts: &RenderOptions,
        media: &dyn MediaProvider,
        hide: &[u32],
    ) -> Result<Bitmap> {
        let size = scene.size_pt;
        if size.is_empty() {
            return Err(Error::Render("幻灯片尺寸无效".to_string()));
        }

        let scale = opts.effective_scale(size);
        let (w, h) = opts.output_size(size);
        if w == 0 || h == 0 || w > 32768 || h > 32768 {
            return Err(Error::Render(format!("输出尺寸异常：{w}×{h}")));
        }

        let mut pixmap = Pixmap::new(w, h).ok_or_else(|| {
            Error::Render(format!("无法分配 {w}×{h} 的位图缓冲"))
        })?;

        // 画布变换：pt → 设备像素
        let canvas = SkTransform::from_scale(scale, scale);

        // 1) 背景
        self.draw_background(&mut pixmap, &scene.background, size, canvas, media);

        // 2) 节点
        for node in &scene.nodes {
            self.draw_node(&mut pixmap, node, canvas, opts, media, 1.0, hide);
        }

        Ok(Bitmap {
            width: w,
            height: h,
            format: PixelFormat::Rgba8Premultiplied,
            // tiny-skia 的 Pixmap 已是预乘 RGBA8，直接取走缓冲省掉一次全图拷贝
            data: pixmap.take(),
        })
    }

    /// 把一个形状**单独**渲染成一张位图，供前端做 60fps 的图层合成。
    ///
    /// # 为什么要「单独渲染」
    ///
    /// 一步动画的中间帧没有别的办法拿到：让后端逐帧光栅化，一帧
    /// 1920×1080 要几十毫秒，只能到 20fps 上下，动画看着就是卡的。
    /// 但动画里的对象**只有一个仿射变换加一个透明度在变**，
    /// 只要把它的图层给前端，浏览器合成是 60fps 且几乎不花时间。
    ///
    /// 返回 `(位图, 它在画布 pt 空间的位置)`；形状不存在或不可见时返回 `None`。
    ///
    /// `paragraphs` 只保留文本的第 `st..=end` 段：动画可以只作用在文本框的
    /// 某一段上（`p:txEl/p:pRg`），整框一起飞会把已经露出来的那几段也带走。
    ///
    /// `extra` 是这一步可能出现的形变（`from`/`to`）：放大过程中图层会超出
    /// 静止时的包围盒，不留余量动画放到一半就会被切边。
    pub fn render_layer(
        &mut self,
        scene: &Scene,
        shape_id: u32,
        paragraphs: Option<(u32, u32)>,
        extra: &[EffectState],
        opts: &RenderOptions,
        media: &dyn MediaProvider,
    ) -> Result<Option<(Bitmap, Rect)>> {
        let Some(node) = scene.walk().find(|n| n.shape_id == Some(shape_id)) else {
            return Ok(None);
        };
        if !node.is_visible() {
            return Ok(None);
        }

        let size = scene.size_pt;
        // 图层要留出描边、阴影、发光的外扩余量，否则动画过程中会被切边。
        // 外扩后再夹回画布 —— 画布之外本来就看不到，没必要为它分配像素。
        let Some(bbox) = layer_bounds(scene, shape_id, opts.state, extra) else {
            return Ok(None);
        };

        let scale = opts.effective_scale(size);
        let w = ((bbox.w * scale).ceil() as u32).max(1);
        let h = ((bbox.h * scale).ceil() as u32).max(1);
        if w > 32768 || h > 32768 {
            return Err(Error::Render(format!("图层尺寸异常：{w}×{h}")));
        }

        let mut pixmap = Pixmap::new(w, h)
            .ok_or_else(|| Error::Render(format!("无法分配 {w}×{h} 的图层缓冲")))?;
        // 局部坐标 → 图层像素：先缩放到设备像素，再把图层左上角挪到原点
        let layer = SkTransform::from_translate(-bbox.x * scale, -bbox.y * scale)
            .pre_concat(SkTransform::from_scale(scale, scale));

        let mut layer_opts = *opts;
        layer_opts.layer_mode = true;
        layer_opts.layer_paragraphs = paragraphs;
        self.draw_node(&mut pixmap, node, layer, &layer_opts, media, 1.0, &[]);

        Ok(Some((
            Bitmap {
                width: w,
                height: h,
                format: PixelFormat::Rgba8Premultiplied,
                data: pixmap.take(),
            },
            bbox,
        )))
    }

    /// 绘制背景。
    fn draw_background(
        &mut self,
        pixmap: &mut Pixmap,
        bg: &SceneBackground,
        size: Size,
        canvas: SkTransform,
        media: &dyn MediaProvider,
    ) {
        match bg {
            SceneBackground::None => {}
            SceneBackground::Solid(c) => {
                // 纯色背景走全图填充，比构造路径更快
                pixmap.fill(convert::to_sk_color(*c));
            }
            SceneBackground::Fill(fill) => {
                let bbox = (0.0, 0.0, size.w, size.h);
                if let Some(paint) = convert::paint_for_fill(fill, bbox) {
                    if let Some(rect) = tiny_skia::Rect::from_xywh(0.0, 0.0, size.w, size.h) {
                        pixmap.fill_rect(rect, &paint, canvas, None);
                    }
                } else if let Fill::Image(img_fill) = fill {
                    // 图片背景：整页作为形状，走遮罩绘制路径。
                    // 背景就长在画布坐标系里（单位已经是 pt），也没有额外透明度
                    if let Some(path) = rect_path(0.0, 0.0, size.w, size.h) {
                        self.draw_image_fill(
                            pixmap,
                            &path,
                            canvas,
                            img_fill,
                            bbox,
                            1.0,
                            1.0,
                            media,
                        );
                    }
                } else if let Some(c) = bg.primary_color() {
                    // 填充解析失败时至少铺一层主色，避免页面全透明
                    pixmap.fill(convert::to_sk_color(c));
                }
            }
        }
    }

    /// 绘制单个节点及其子节点。
    fn draw_node(
        &mut self,
        pixmap: &mut Pixmap,
        node: &Node,
        canvas: SkTransform,
        opts: &RenderOptions,
        media: &dyn MediaProvider,
        inherited_opacity: f32,
        hide: &[u32],
    ) {
        if !node.is_visible() {
            return;
        }

        // 要单独成层的形状（强调动画的原件）在底帧里不画
        if node.shape_id.is_some_and(|id| hide.contains(&id)) {
            return;
        }

        // 动画还没播到这一步：整块不画（图层模式除外，见 `layer_mode`）
        if !opts.layer_mode && !node.build.visible_in(opts.state) {
            return;
        }

        let emph = node.emphasis_state(opts.state);
        let opacity = (inherited_opacity * node.opacity * emph.opacity).clamp(0.0, 1.0);
        if opacity <= 0.0 {
            return;
        }

        // 节点自身的变换，再叠加「已播强调动画」留下的形变（绕包围盒中心缩放/旋转）。
        // 强调形变是在**画布空间**里作用的，所以要套在节点变换之外：
        // `canvas · 形变 · node.transform`（tiny-skia 的 pre_concat 是右乘，先作用的在后面）
        let mut base = canvas;
        if !emph.is_identity() {
            let c = node.canvas_bounds().center();
            base = base.pre_concat(emphasis_affine(&emph, c));
        }
        let node_transform = base.pre_concat(convert::to_sk_transform(node.transform));

        // 占位几何在关闭占位绘制时跳过
        if !opts.draw_placeholders && node.geometry.is_degraded() {
            return;
        }

        let local_bbox = bbox_of(node);

        match &node.geometry {
            Geometry::Table(table) => {
                self.draw_table(
                    pixmap,
                    table,
                    node_transform,
                    node.effective_scale(),
                    opts,
                    media,
                    opacity,
                );
                return;
            }
            Geometry::Image(img_ref) => {
                self.draw_picture(pixmap, img_ref, node, node_transform, media, opacity);
                return;
            }
            _ => {}
        }

        // 效果（阴影/发光）画在**形状之前** —— 也就是压在形状底下。
        //
        // 模糊是用三个偏移填充近似的（tiny-skia 没有高斯模糊），
        // 它们和形状本身大面积重叠。画在填充之后就等于拿半透明黑把形状又刷了
        // 三层：白框变灰框（实测 255 → 115），课件里那些白色圆角标注框
        // 「cardinal numbers 基数词」「Where can we see…」全跟着发灰。
        // PowerPoint 的阴影本来就在形状下面，顺序换过来才对得上。
        if opts.draw_effects {
            self.draw_effects(pixmap, node, node_transform, local_bbox, opacity);
        }

        // 填充
        if node.fill.is_visible() {
            if let Some(paint) = convert::paint_for_fill(&node.fill, local_bbox) {
                if let Some(path) = self.geometry_path(node) {
                    let paint = with_opacity(paint, opacity);
                    pixmap.fill_path(&path, &paint, convert::FILL_RULE, node_transform, None);
                }
            } else if let Fill::Image(img_fill) = &node.fill {
                // 图片填充走遮罩路径（见 `convert::paint_for_fill` 的说明）
                if let Some(path) = self.geometry_path(node) {
                    self.draw_image_fill(
                        pixmap,
                        &path,
                        node_transform,
                        img_fill,
                        local_bbox,
                        node.effective_scale(),
                        opacity,
                        media,
                    );
                }
            }
        }

        // 描边
        if let Some(stroke) = &node.stroke {
            if stroke.is_visible() {
                if let (Some(path), Some(sk_stroke)) = (
                    self.geometry_path(node),
                    // 线宽是绝对磅值，而组合子形状的局部单位被缩过 ——
                    // 见 `convert::stroke_for_scaled`
                    convert::stroke_for_scaled(stroke, 1.0 / node.effective_scale()),
                ) {
                    if let Some(paint) = convert::paint_for_stroke(&stroke.fill, local_bbox) {
                        let paint = with_opacity(paint, opacity);
                        pixmap.stroke_path(
                            &path,
                            &paint,
                            &sk_stroke,
                            node_transform,
                            None,
                        );
                        // 箭头画在描边之后：它是线帽的延伸，压在线上面
                        self.draw_line_ends(
                            pixmap,
                            node,
                            stroke,
                            &paint,
                            node_transform,
                        );
                    }
                }
            }
        }

        // 文本
        if let Some(tb) = &node.text {
            if !tb.is_empty() {
                // 这个节点的「一个局部单位折合多少磅」。
                //
                // 顶层形状的局部坐标就是磅值（组合以外的地方都是 1.0），
                // 而 `p:grpSp` 的子形状活在**被组合缩放过的坐标系**里：
                // 组合的 `a:ext` 与 `a:chExt` 之比就是那个缩放比。
                let unit = node.effective_scale();
                let area = text_area(node, tb, unit);
                // 文字用「抵消掉自身翻转」的那份变换：PowerPoint 翻转形状时
                // 形状镜像、文字仍然正着，见 `Node::text_transform`。
                // 绝大多数节点两份是同一个矩阵。
                let text_tf = base.pre_concat(convert::to_sk_transform(
                    node.text_transform.unwrap_or(node.transform),
                ));
                // 排版在**物理 pt 空间**里做，再把单位除回去。
                //
                // 关键是「字号是绝对的」：`a:rPr/@sz="2400"` 永远表示 24pt，
                // 跟组合把坐标缩到多小毫无关系。课件里那种「图标 + 文字」的小组合，
                // chExt 只有几千、ext 却是几百万 EMU（比值 0.04 上下），
                // 局部 24pt 被缩放后只剩 1pt —— 整块文字等于没画出来。
                // 这一乘一除看着琐碎，正是「Lead-in」「How do we use numbers…」
                // 这些标题在页面上一片空白的原因。
                let text_tf = text_tf.pre_concat(SkTransform::from_scale(1.0 / unit, 1.0 / unit));
                text::draw_text(
                    pixmap,
                    &self.fonts,
                    tb,
                    area,
                    // 传「画布变换 ∘ 节点变换」：文本和几何共用同一条变换链，
                    // 少乘一次画布缩放会让文字在高分辨率/放映时偏位、偏小
                    convert::from_sk_transform(text_tf),
                    opacity,
                    node.fill.primary_color_for_text(),
                    &mut self.glyphs,
                    opts.layout_for(),
                );
            }
        }

        // 子节点（组合形状）。
        //
        // 注意传的是 `canvas` 而不是 `node_transform`：
        // SceneGraph 约定**子节点的 transform 已经是相对画布的绝对变换**
        // （解析阶段就把父级变换烘焙进去了，见 `ppt-format-pptx::shape`）。
        // 若在这里再乘一次父变换，组合形状内的所有内容都会位移两次。
        for child in &node.children {
            self.draw_node(pixmap, child, canvas, opts, media, opacity, hide);
        }
    }

    /// 取节点的几何路径（把专用几何类型转成路径）。
    fn geometry_path(&self, node: &Node) -> Option<tiny_skia::Path> {
        match &node.geometry {
            Geometry::Rect => {
                let b = node.local_bbox?;
                rect_path(0.0, 0.0, b.w, b.h)
            }
            Geometry::RoundRect { rx_pt, ry_pt } => {
                let b = node.local_bbox?;
                round_rect_path(b.w, b.h, *rx_pt, *ry_pt)
            }
            Geometry::Ellipse => {
                let b = node.local_bbox?;
                ellipse_path(b.w / 2.0, b.h / 2.0, b.w / 2.0, b.h / 2.0)
            }
            Geometry::Path(p) => convert::to_sk_path(p),
            Geometry::Placeholder { .. } => {
                let b = node.local_bbox?;
                rect_path(0.0, 0.0, b.w, b.h)
            }
            Geometry::None | Geometry::Image(_) | Geometry::Table(_) => {
                // 组合容器与图片/表格不走通用填充路径
                match node.local_bbox {
                    Some(b) if node.fill.is_visible() => rect_path(0.0, 0.0, b.w, b.h),
                    _ => None,
                }
            }
        }
    }

    /// 绘制图片。
    fn draw_picture(
        &mut self,
        pixmap: &mut Pixmap,
        img_ref: &ppt_core::scene::ImageRef,
        node: &Node,
        node_transform: SkTransform,
        media: &dyn MediaProvider,
        opacity: f32,
    ) {
        let bbox = bbox_of(node);
        let Some(img) = self.load_pixmap(img_ref, bbox, node.effective_scale(), media) else {
            return;
        };

        // 图片像素尺寸 → 形状局部尺寸的映射
        let iw = img.width() as f32;
        let ih = img.height() as f32;
        let (bw, bh) = (bbox.2.max(0.001), bbox.3.max(0.001));
        if iw <= 0.0 || ih <= 0.0 {
            return;
        }
        let sx = bw / iw;
        let sy = bh / ih;

        let transform = node_transform.pre_concat(SkTransform::from_scale(sx, sy));
        let paint = convert::image_paint(opacity);
        // 显式走 `Pixmap::as_ref` 拿到 PixmapRef（`Arc::as_ref` 会返回 &Pixmap）
        pixmap.draw_pixmap(0, 0, Pixmap::as_ref(&img), &paint, transform, None);
    }

    /// 绘制表格。
    ///
    /// `unit` 是「一个局部单位折合多少磅」（见 `Node::effective_scale`），
    /// 表格也可能落在被缩放的组合里。
    #[allow(clippy::too_many_arguments)]
    fn draw_table(
        &mut self,
        pixmap: &mut Pixmap,
        table: &Table,
        node_transform: SkTransform,
        unit: f32,
        opts: &RenderOptions,
        media: &dyn MediaProvider,
        opacity: f32,
    ) {
        if table.is_empty() {
            return;
        }

        // 行高先按内容长一遍：PowerPoint 里 `a:tr/@h` 是**最小值**，
        // 单元格装不下时行会自己变高。不这么做的话，单元格里的文字
        // 会直接压到下一行上去（真实课件里最常见的一类「串行」）。
        let heights = self.resolve_row_heights(table, opts);
        let mut tops: Vec<f32> = Vec::with_capacity(heights.len());
        let mut acc = 0.0f32;
        for h in &heights {
            tops.push(acc);
            acc += h;
        }

        // 单元格边框画在中心线上，因此按半线宽外扩
        for (r, row) in table.cells.iter().enumerate() {
            for (c, cell) in row.iter().enumerate() {
                if !cell.is_drawable() {
                    continue;
                }

                let x = table.col_offset_pt(c);
                let y = tops.get(r).copied().unwrap_or_else(|| table.row_offset_pt(r));
                let w: f32 = table
                    .columns
                    .iter()
                    .skip(c)
                    .take(cell.col_span.max(1) as usize)
                    .sum();
                let h: f32 = heights
                    .iter()
                    .skip(r)
                    .take(cell.row_span.max(1) as usize)
                    .sum();

                // 底纹
                if cell.fill.is_visible() {
                    let bbox = (x, y, w, h);
                    if let Some(paint) = convert::paint_for_fill(&cell.fill, bbox) {
                        let paint = with_opacity(paint, opacity);
                        if let Some(path) = rect_path(x, y, w, h) {
                            pixmap.fill_path(
                                &path,
                                &paint,
                                convert::FILL_RULE,
                                node_transform,
                                None,
                            );
                        }
                    } else if let Fill::Image(img_fill) = &cell.fill {
                        if let Some(path) = rect_path(x, y, w, h) {
                            self.draw_image_fill(
                                pixmap,
                                &path,
                                node_transform,
                                img_fill,
                                bbox,
                                unit,
                                opacity,
                                media,
                            );
                        }
                    }
                }

                // 边框
                self.draw_cell_borders(
                    pixmap,
                    &cell.borders,
                    (x, y, w, h),
                    node_transform,
                    opacity,
                );

                // 单元格文本
                if let Some(tb) = &cell.text {
                    if !tb.is_empty() {
                        // 与普通文本框同理：排版在物理 pt 空间里做，
                        // 组合把局部单位缩过的话要乘回来（见 `text_area`）
                        let area = Size::new(
                            (w * unit - cell.margins.horizontal()).max(0.0),
                            (h * unit - cell.margins.vertical()).max(0.0),
                        );
                        let inner = convert::from_sk_transform(node_transform)
                            .multiply(&Transform::translate(x, y))
                            .multiply(&Transform::scale(1.0 / unit, 1.0 / unit))
                            .multiply(&Transform::translate(
                                cell.margins.left,
                                cell.margins.top,
                            ));
                        let mut tb_with_anchor = tb.clone();
                        tb_with_anchor.body.anchor = cell.anchor;
                        text::draw_text(
                            pixmap,
                            &self.fonts,
                            &tb_with_anchor,
                            area,
                            inner,
                            opacity,
                            Color::BLACK,
                            &mut self.glyphs,
                            opts.layout_for(),
                        );
                    }
                }
            }
        }
    }

    /// 算出每一行的**实际**高度。
    ///
    /// PowerPoint 里 `a:tr/@h` 是行高的**最小值**：单元格内容装不下时行会长高，
    /// 整张表也跟着变高。少了这一步，单元格里的文字就会压到下一行上去 ——
    /// 真实课件里最常见的「串行」都是这么来的。
    ///
    /// 量高度时把可用高度放到很大：行高本来就该跟着内容长，
    /// 不该反过来让 `normAutofit` 把字缩到能塞进原来那一行。
    fn resolve_row_heights(&self, table: &Table, opts: &RenderOptions) -> Vec<f32> {
        /// 「无限高」用一个大到不会触发的实数，避免 Inf 参与运算。
        const UNBOUNDED_PT: f32 = 100_000.0;

        let mut heights = table.rows.clone();
        let layout = LayoutOptions {
            solve_autofit: false,
            ..opts.layout_for()
        };

        for (r, row) in table.cells.iter().enumerate() {
            if r >= heights.len() {
                break;
            }
            for (c, cell) in row.iter().enumerate() {
                // 跨行的单元格不单独参与：它的高度由被跨的那几行共同提供
                if cell.row_span != 1 {
                    continue;
                }
                let Some(tb) = &cell.text else { continue };
                if tb.is_empty() {
                    continue;
                }
                let w: f32 = table
                    .columns
                    .iter()
                    .skip(c)
                    .take(cell.col_span.max(1) as usize)
                    .sum();
                let area_w = (w - cell.margins.horizontal()).max(0.0);
                if area_w <= 0.0 {
                    continue;
                }

                let laid = text::measure_text(
                    &self.fonts,
                    tb,
                    Size::new(area_w, UNBOUNDED_PT),
                    layout,
                );
                let need = laid.height + cell.margins.vertical();
                if need > heights[r] {
                    heights[r] = need;
                }
            }
        }

        heights
    }

    /// 表格排完之后的总高度。
    ///
    /// 给核对工具用：课件里 `p:graphicFrame` 的 `a:ext/@cy` 就是
    /// PowerPoint 排完之后写下的表格高度，两者一比就知道行高模型准不准。
    pub fn table_height(&self, table: &Table, opts: &RenderOptions) -> f32 {
        self.resolve_row_heights(table, opts).iter().sum()
    }

    /// 绘制单元格的六条边框。
    fn draw_cell_borders(
        &mut self,
        pixmap: &mut Pixmap,
        borders: &ppt_core::scene::TableBorders,
        rect: (f32, f32, f32, f32),
        node_transform: SkTransform,
        opacity: f32,
    ) {
        let (x, y, w, h) = rect;

        let mut draw_one = |line: Option<&ppt_core::scene::BorderLine>,
                            from: (f32, f32),
                            to: (f32, f32)| {
            let Some(line) = line else { return };
            if !line.is_visible() {
                return;
            }

            let mut pb = tiny_skia::PathBuilder::new();
            pb.move_to(from.0, from.1);
            pb.line_to(to.0, to.1);
            let Some(path) = pb.finish() else { return };

            let mut paint = Paint::default();
            paint.set_color(convert::to_sk_color(apply_opacity(line.color, opacity)));
            paint.anti_alias = true;

            let mut stroke = tiny_skia::Stroke::default();
            stroke.width = line.width_pt;
            stroke.line_cap = tiny_skia::LineCap::Butt;

            pixmap.stroke_path(&path, &paint, &stroke, node_transform, None);
        };

        draw_one(borders.top.as_ref(), (x, y), (x + w, y));
        draw_one(borders.bottom.as_ref(), (x, y + h), (x + w, y + h));
        draw_one(borders.left.as_ref(), (x, y), (x, y + h));
        draw_one(borders.right.as_ref(), (x + w, y), (x + w, y + h));
        draw_one(borders.tl_to_br.as_ref(), (x, y), (x + w, y + h));
        draw_one(borders.tr_to_bl.as_ref(), (x + w, y), (x, y + h));
    }

    /// 画线条两端的箭头（`a:headEnd` / `a:tailEnd`）。
    ///
    /// # 为什么必须画
    ///
    /// 解析层一直把箭头类型存进了 [`Stroke`]，但渲染器从没用过 ——
    /// 于是课件里那些箭头连接符变成了一根**实心横条**。
    /// 《Unit 2》第 11 页那三条 4.5pt 的橙色箭头（`prstGeom="straightConnector1"`
    /// 配 `a:tailEnd type="arrow"`）在页面上就是三道突兀的橙杠，
    /// 老师看了只会以为渲染坏了。
    ///
    /// # 尺寸与朝向
    ///
    /// 箭头尺寸是**相对线宽**的倍数（`w`/`len` 的 sm/med/lg = 2/3/5），
    /// 所以先要把线宽折回局部单位（组合里的子形状局部单位被缩过），
    /// 再按「尖端落在线的端点、身子朝线内」摆好。
    fn draw_line_ends(
        &mut self,
        pixmap: &mut Pixmap,
        node: &Node,
        stroke: &Stroke,
        paint: &Paint<'static>,
        node_transform: SkTransform,
    ) {
        if stroke.head_end.is_none() && stroke.tail_end.is_none() {
            return;
        }
        // 闭合形状（矩形/椭圆）没有「端点」，OOXML 也规定不画
        let Geometry::Path(geom) = &node.geometry else {
            return;
        };
        let unit = node.effective_scale();
        let width = stroke.width_pt / unit;
        if width <= 0.0 {
            return;
        }

        for (end, at_tail) in [(stroke.head_end, false), (stroke.tail_end, true)] {
            let Some(end) = end else { continue };
            if end.kind == LineEndKind::None {
                continue;
            }
            let Some((tip, dir)) = open_end(geom, at_tail) else {
                continue;
            };

            let len = width * end.length_scale;
            let half_w = width * end.width_scale * 0.5;
            if len <= 0.0 || half_w <= 0.0 {
                continue;
            }
            // 垂直于线、指向线的一侧
            let nrm = Point::new(-dir.y, dir.x);

            match end.kind {
                LineEndKind::None => {}
                LineEndKind::Triangle => {
                    let pts = [
                        tip,
                        Point::new(tip.x - dir.x * len + nrm.x * half_w, tip.y - dir.y * len + nrm.y * half_w),
                        Point::new(tip.x - dir.x * len - nrm.x * half_w, tip.y - dir.y * len - nrm.y * half_w),
                    ];
                    self.fill_polygon(pixmap, &pts, paint, node_transform);
                }
                LineEndKind::Stealth => {
                    // 凹背三角：尾边中点向尖端方向凹进去一截
                    let notch = 0.6;
                    let pts = [
                        tip,
                        Point::new(tip.x - dir.x * len + nrm.x * half_w, tip.y - dir.y * len + nrm.y * half_w),
                        Point::new(tip.x - dir.x * len * notch, tip.y - dir.y * len * notch),
                        Point::new(tip.x - dir.x * len - nrm.x * half_w, tip.y - dir.y * len - nrm.y * half_w),
                    ];
                    self.fill_polygon(pixmap, &pts, paint, node_transform);
                }
                LineEndKind::Diamond => {
                    let cx = tip.x - dir.x * len * 0.5;
                    let cy = tip.y - dir.y * len * 0.5;
                    let back = Point::new(tip.x - dir.x * len, tip.y - dir.y * len);
                    let pts = [
                        tip,
                        Point::new(cx + nrm.x * half_w, cy + nrm.y * half_w),
                        back,
                        Point::new(cx - nrm.x * half_w, cy - nrm.y * half_w),
                    ];
                    self.fill_polygon(pixmap, &pts, paint, node_transform);
                }
                LineEndKind::Oval => {
                    if let Some(path) = oval_path(tip, dir, nrm, len, half_w) {
                        pixmap.fill_path(
                            &path,
                            paint,
                            convert::FILL_RULE,
                            node_transform,
                            None,
                        );
                    }
                }
                LineEndKind::Arrow => {
                    // 开口箭头：两条短线从尖端往回张开
                    let back = Point::new(tip.x - dir.x * len, tip.y - dir.y * len);
                    let spread = 0.35 * half_w.max(width);
                    let mut pb = tiny_skia::PathBuilder::new();
                    let a = Point::new(back.x + nrm.x * spread, back.y + nrm.y * spread);
                    let b = Point::new(back.x - nrm.x * spread, back.y - nrm.y * spread);
                    pb.move_to(a.x, a.y);
                    pb.line_to(tip.x, tip.y);
                    pb.line_to(b.x, b.y);
                    if let Some(path) = pb.finish() {
                        let mut sk = tiny_skia::Stroke::default();
                        sk.width = width;
                        sk.line_cap = tiny_skia::LineCap::Round;
                        sk.line_join = tiny_skia::LineJoin::Round;
                        pixmap.stroke_path(&path, paint, &sk, node_transform, None);
                    }
                }
            }
        }
    }

    /// 用给定画笔填一个多边形（局部坐标）。
    fn fill_polygon(
        &self,
        pixmap: &mut Pixmap,
        points: &[Point],
        paint: &Paint<'static>,
        node_transform: SkTransform,
    ) {
        if points.len() < 3 {
            return;
        }
        let mut pb = tiny_skia::PathBuilder::new();
        pb.move_to(points[0].x, points[0].y);
        for p in &points[1..] {
            pb.line_to(p.x, p.y);
        }
        pb.close();
        if let Some(path) = pb.finish() {
            pixmap.fill_path(&path, paint, convert::FILL_RULE, node_transform, None);
        }
    }

    /// 绘制阴影与发光。
    ///
    /// 实现方式是「把形状路径先填充为纯色，再用模糊近似」。
    /// tiny-skia 没有内置模糊滤镜，这里用「多重半透明偏移填充」近似 ——
    /// 对课件的浅阴影效果足够，而且完全避开了分配大缓冲的开销。
    fn draw_effects(
        &mut self,
        pixmap: &mut Pixmap,
        node: &Node,
        node_transform: SkTransform,
        local_bbox: (f32, f32, f32, f32),
        opacity: f32,
    ) {
        let Some(shadow) = &node.effects.outer_shadow else {
            return;
        };
        if shadow.color.is_transparent() {
            return;
        }

        let Some(path) = self.geometry_path(node) else {
            return;
        };
        let _ = local_bbox;

        // 阴影方向：OOXML 的 0 度指向正右方、顺时针为正
        //
        // 距离与模糊半径都是**绝对磅值**，但这个偏移是加在局部坐标系里的
        // （见下面的 `node_transform.pre_concat`）。组合里的子形状局部单位被缩过，
        // 不折回去的话，组合内 3pt 的阴影会被放大成影分身。
        let inv_unit = 1.0 / node.effective_scale();
        let rad = shadow.direction_deg.to_radians();
        let dx = shadow.distance_pt * rad.cos() * inv_unit;
        let dy = shadow.distance_pt * rad.sin() * inv_unit;

        // 用 3 层递减透明度的偏移填充近似模糊：
        // 层数越多越柔和，3 层在 6pt 以内的模糊半径下已看不出台阶
        let samples = 3;
        let blur = shadow.blur_pt.max(0.0) * inv_unit;

        for i in 0..samples {
            let t = if samples <= 1 {
                0.0
            } else {
                i as f32 / (samples - 1) as f32
            };
            // 由内向外交替扩散
            let spread = blur * t * 0.5;
            let layer_alpha = shadow.color.a as f32
                * opacity
                * (1.0 - t * 0.55)
                / samples as f32
                * 1.6;

            let offset = SkTransform::from_translate(dx + spread, dy + spread);
            let mut paint = Paint::default();
            paint.set_color(convert::to_sk_color(Color::rgba(
                shadow.color.r,
                shadow.color.g,
                shadow.color.b,
                layer_alpha.clamp(0.0, 255.0) as u8,
            )));
            paint.anti_alias = true;

            pixmap.fill_path(
                &path,
                &paint,
                convert::FILL_RULE,
                node_transform.pre_concat(offset),
                None,
            );
        }
    }

    /// 用图片填充一条路径。
    ///
    /// # 为什么不用 tiny-skia 的 `Pattern` 着色器
    ///
    /// `Pattern` 借用了位图，其生命周期无法与 `Paint<'static>` 共存，
    /// 而渲染器的画笔需要跨调用返回。改用「按路径建遮罩 → 透过遮罩绘制位图」：
    ///
    /// 1. 用形状路径填充一张 alpha 遮罩（设备像素空间）；
    /// 2. 把图片按 bbox 映射后，透过该遮罩绘制。
    ///
    /// 这样既绕开生命周期约束，也顺带省掉了平铺位图的缓存开销。
    #[allow(clippy::too_many_arguments)]
    fn draw_image_fill(
        &mut self,
        pixmap: &mut Pixmap,
        path: &tiny_skia::Path,
        node_transform: SkTransform,
        img_fill: &ppt_core::scene::ImageFill,
        bbox: (f32, f32, f32, f32),
        unit: f32,
        opacity: f32,
        media: &dyn MediaProvider,
    ) {
        let Some(img) = self.load_pixmap(&img_fill.image, bbox, unit, media) else {
            return;
        };

        let (x, y, w, h) = bbox;
        let iw = img.width() as f32;
        let ih = img.height() as f32;
        if iw <= 0.0 || ih <= 0.0 || w <= 0.0 || h <= 0.0 {
            return;
        }

        // 1) 形状遮罩
        let Some(mut mask) = tiny_skia::Mask::new(pixmap.width(), pixmap.height()) else {
            return;
        };
        mask.fill_path(path, convert::FILL_RULE, true, node_transform);

        let paint = convert::image_paint(opacity);

        // 2) 按平铺或拉伸决定图片的排布
        match &img_fill.tile {
            Some(tile) => {
                let sx = tile.sx.max(0.01);
                let sy = tile.sy.max(0.01);
                let tw = iw * sx;
                let th = ih * sy;
                if tw <= 0.5 || th <= 0.5 {
                    return;
                }

                let (ox, oy) = tile_origin(tile.align, x, y, w, h, tw, th);
                // 平铺数量上限，避免异常参数导致绘制次数爆炸
                let cols = ((w / tw).ceil() as i32).clamp(1, 64);
                let rows = ((h / th).ceil() as i32).clamp(1, 64);

                for row in 0..rows {
                    for col in 0..cols {
                        let tx = ox + col as f32 * tw;
                        let ty = oy + row as f32 * th;
                        let t = node_transform
                            .pre_concat(SkTransform::from_translate(tx, ty))
                            .pre_concat(SkTransform::from_scale(sx, sy));
                        pixmap.draw_pixmap(
                            0,
                            0,
                            Pixmap::as_ref(&img),
                            &paint,
                            t,
                            Some(&mask),
                        );
                    }
                }
            }
            None => {
                // 拉伸：保持宽高比、居中裁剪（cover），与 PowerPoint 观感一致
                let scale = (w / iw).max(h / ih);
                let tw = iw * scale;
                let th = ih * scale;
                let ox = x + (w - tw) / 2.0;
                let oy = y + (h - th) / 2.0;

                let t = node_transform
                    .pre_concat(SkTransform::from_translate(ox, oy))
                    .pre_concat(SkTransform::from_scale(scale, scale));
                pixmap.draw_pixmap(0, 0, Pixmap::as_ref(&img), &paint, t, Some(&mask));
            }
        }
    }

    /// 从媒体提供者加载并解码图片（带缓存）。
    ///
    /// `unit` 是「一个局部单位折合多少磅」（见 `Node::effective_scale`）。
    /// 解码目标尺寸必须按**物理磅值**算：组合子形状的局部尺寸是先把 EMU
    /// 转成 pt、再被组合缩放过的，直接拿它当「要多少像素」会把一张地图
    /// 解成 1×1 —— 然后在页面上放大成一片模糊的色块。
    fn load_pixmap(
        &mut self,
        img_ref: &ppt_core::scene::ImageRef,
        bbox: (f32, f32, f32, f32),
        unit: f32,
        media: &dyn MediaProvider,
    ) -> Option<SharedImage> {
        if !image::should_decode(img_ref) {
            return None;
        }

        let display = image::display_size(Size::new(bbox.2, bbox.3), unit);
        // 解码目标按画布缩放估计；这里用 1.0 的基准，
        // 具体缩放由渲染器的变换承担，避免为不同缩放各解一份
        let req = DecodeRequest::from_display(display, 1.0, img_ref.src_rect);
        let bucket = req.size_bucket();

        if let Some(hit) = self.images.get(&img_ref.part, bucket) {
            return Some(hit);
        }

        let bytes = media.read_media(&img_ref.part).ok()?;
        let decoded = std::sync::Arc::new(image::decode(&bytes, req)?);
        self.images
            .insert(&img_ref.part, bucket, std::sync::Arc::clone(&decoded));
        Some(decoded)
    }

    /// 已缓存的图片数（诊断用）。
    pub fn cached_image_count(&self) -> usize {
        self.images.len()
    }

    /// 已缓存的字形数（诊断用）。
    pub fn cached_glyph_count(&self) -> usize {
        self.glyphs.len()
    }
}

/// 取节点的局部包围盒（缺省时退回零矩形）。
fn bbox_of(node: &Node) -> (f32, f32, f32, f32) {
    match node.local_bbox {
        Some(b) => (b.x, b.y, b.w, b.h),
        None => {
            let b = node.local_bounds();
            (b.x, b.y, b.w, b.h)
        }
    }
}

/// 计算文本框的可用区域（物理 pt，已扣除内边距）。
///
/// `unit` 是「一个局部单位折合多少磅」（见 `Node::effective_scale`）。
/// 顶层形状恒为 1.0；组合里的子形状则是个远大于 1 的系数 ——
/// 局部坐标是先把 EMU 当磅值换算、再被组合缩放过的，必须乘回去才是真实磅值。
/// 而**内边距本身就是磅值**（`lIns`/`tIns` 记的是 EMU），不参与那个缩放。
fn text_area(node: &Node, tb: &TextBox, unit: f32) -> Size {
    let bbox = bbox_of(node);
    let insets = tb.body.insets;
    Size::new(
        (bbox.2 * unit - insets.horizontal()).max(0.0),
        (bbox.3 * unit - insets.vertical()).max(0.0),
    )
}

/// 平铺图片时，由对齐方式求第一个贴图的左上角。
fn tile_origin(
    align: ppt_core::scene::TileAlign,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    tw: f32,
    th: f32,
) -> (f32, f32) {
    use ppt_core::scene::TileAlign as A;
    match align {
        A::TopLeft => (x, y),
        A::Top => (x + (w - tw) / 2.0, y),
        A::TopRight => (x + w - tw, y),
        A::Left => (x, y + (h - th) / 2.0),
        A::Center => (x + (w - tw) / 2.0, y + (h - th) / 2.0),
        A::Right => (x + w - tw, y + (h - th) / 2.0),
        A::BottomLeft => (x, y + h - th),
        A::Bottom => (x + (w - tw) / 2.0, y + h - th),
        A::BottomRight => (x + w - tw, y + h - th),
    }
}

/// 构造矩形路径。
fn rect_path(x: f32, y: f32, w: f32, h: f32) -> Option<tiny_skia::Path> {
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    tiny_skia::Rect::from_xywh(x, y, w, h).map(|r| {
        let mut pb = tiny_skia::PathBuilder::new();
        pb.push_rect(r);
        pb.finish().unwrap_or_else(|| {
            // `push_rect` 后 finish 必然成功；这里给出兜底避免 unwrap
            tiny_skia::PathBuilder::new().finish().expect("空路径构造失败")
        })
    })
}

/// 取一条开放路径的端点，以及「指向端点外侧」的单位方向。
///
/// 箭头就画在这两个位置上。`at_tail` 为真取终点（`tailEnd`），否则取起点（`headEnd`）。
/// 闭合子路径（矩形、椭圆本身）没有端点 —— OOXML 也规定这类形状不画端点箭头。
fn open_end(geom: &PathGeometry, at_tail: bool) -> Option<(Point, Point)> {
    let sp = geom.subpaths.first()?;
    let mut pts: Vec<Point> = Vec::with_capacity(sp.segments.len() + 1);
    pts.push(sp.start);
    let mut closed = false;
    for seg in &sp.segments {
        match seg {
            PathSegment::Line(p) => pts.push(*p),
            PathSegment::Quad { to, .. }
            | PathSegment::Cubic { to, .. }
            | PathSegment::Arc { to, .. } => pts.push(*to),
            PathSegment::Close => closed = true,
        }
    }
    if closed || pts.len() < 2 {
        return None;
    }

    let (tip, inner) = if at_tail {
        (*pts.last()?, pts[pts.len() - 2])
    } else {
        (pts[0], pts[1])
    };
    let v = Point::new(tip.x - inner.x, tip.y - inner.y);
    let len = (v.x * v.x + v.y * v.y).sqrt();
    (len > 1e-6).then(|| (tip, Point::new(v.x / len, v.y / len)))
}

/// 端点圆点（`a:tailEnd type="oval"`）：以端点为中心的一个椭圆。
///
/// `dir` 是沿线的单位方向、`nrm` 是它的法向；椭圆沿 `dir` 的半轴是 `len/2`，
/// 沿 `nrm` 的是 `half_w`。用四段三次贝塞尔逼近（k ≈ 0.5523）。
fn oval_path(tip: Point, dir: Point, nrm: Point, len: f32, half_w: f32) -> Option<tiny_skia::Path> {
    const K: f32 = 0.552_284_75;
    let (a, b) = (len * 0.5, half_w);
    let cx = tip.x - dir.x * a;
    let cy = tip.y - dir.y * a;
    // 以 (沿线, 法向) 为轴的局部坐标取点
    let at = |u: f32, v: f32| {
        Point::new(
            cx + dir.x * u * a + nrm.x * v * b,
            cy + dir.y * u * a + nrm.y * v * b,
        )
    };

    let front = at(1.0, 0.0);
    let right = at(0.0, 1.0);
    let back = at(-1.0, 0.0);
    let left = at(0.0, -1.0);

    let mut pb = tiny_skia::PathBuilder::new();
    pb.move_to(front.x, front.y);
    for (c1, c2, to) in [
        (at(1.0, K), at(K, 1.0), right),
        (at(-K, 1.0), at(-1.0, K), back),
        (at(-1.0, -K), at(-K, -1.0), left),
        (at(K, -1.0), at(1.0, -K), front),
    ] {
        pb.cubic_to(c1.x, c1.y, c2.x, c2.y, to.x, to.y);
    }
    pb.close();
    pb.finish()
}

/// 构造圆角矩形路径。
fn round_rect_path(w: f32, h: f32, rx: f32, ry: f32) -> Option<tiny_skia::Path> {
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let rx = rx.abs().min(w / 2.0);
    let ry = ry.abs().min(h / 2.0);
    if rx < 0.01 && ry < 0.01 {
        return rect_path(0.0, 0.0, w, h);
    }

    // 用三次贝塞尔逼近四个圆角（k ≈ 0.5523）
    const K: f32 = 0.552_284_75;
    let mut pb = tiny_skia::PathBuilder::new();
    pb.move_to(rx, 0.0);
    pb.line_to(w - rx, 0.0);
    pb.cubic_to(w - rx + K * rx, 0.0, w, ry - K * ry, w, ry);
    pb.line_to(w, h - ry);
    pb.cubic_to(w, h - ry + K * ry, w - rx + K * rx, h, w - rx, h);
    pb.line_to(rx, h);
    pb.cubic_to(rx - K * rx, h, 0.0, h - ry + K * ry, 0.0, h - ry);
    pb.line_to(0.0, ry);
    pb.cubic_to(0.0, ry - K * ry, rx - K * rx, 0.0, rx, 0.0);
    pb.close();
    pb.finish()
}

/// 构造椭圆路径。
fn ellipse_path(cx: f32, cy: f32, rx: f32, ry: f32) -> Option<tiny_skia::Path> {
    if rx <= 0.0 || ry <= 0.0 {
        return None;
    }
    let mut pb = tiny_skia::PathBuilder::new();
    pb.push_oval(tiny_skia::Rect::from_xywh(cx - rx, cy - ry, rx * 2.0, ry * 2.0)?);
    pb.finish()
}

/// 给画笔乘上整体不透明度。
fn with_opacity(mut paint: Paint<'static>, opacity: f32) -> Paint<'static> {
    if opacity >= 1.0 {
        return paint;
    }
    // 颜色型画笔直接乘 alpha；渐变/图片型画笔的透明度由各自色标的 alpha 承担
    if let tiny_skia::Shader::SolidColor(mut col) = paint.shader.clone() {
        col.set_alpha(col.alpha() * opacity.clamp(0.0, 1.0));
        paint.set_color(col);
    }
    paint
}

/// 颜色乘不透明度。
fn apply_opacity(c: Color, opacity: f32) -> Color {
    if opacity >= 1.0 {
        return c;
    }
    Color::rgba(
        c.r,
        c.g,
        c.b,
        (c.a as f32 * opacity.clamp(0.0, 1.0)).round() as u8,
    )
}

/// 辅助：从填充推一个适合文本的颜色。
trait FillTextColor {
    fn primary_color_for_text(&self) -> Color;
}

impl FillTextColor for Fill {
    fn primary_color_for_text(&self) -> Color {
        match self {
            // 深色底上的文本若没指定颜色，默认应是白色
            Fill::Solid(c) if c.luminance() < 0.5 => Color::WHITE,
            _ => Color::BLACK,
        }
    }
}

/// 把位图打成「直接喂给前端 canvas」的原始帧：`[宽 u32le][高 u32le][直通 RGBA8…]`。
///
/// # 为什么不用 PNG
///
/// 这些位图的**唯一去处**就是 `<canvas>`。走 PNG 意味着
/// 「压缩 → IPC → 解压」三跳，而实测 1920×1080 光压缩就要 **66ms/页** ——
/// 比一张幻灯片本身的排版还贵，而且这 66ms 是纯粹白花的：
/// 解压出来的像素和压缩前**一模一样**。
///
/// 原始帧省掉的正是这段压缩。代价是 IPC 载荷变大（1920×1080 约 8.3MB，
/// PNG 约 1MB），但本机内存拷贝比 deflate 便宜一个数量级。
///
/// # 为什么必须还原预乘
///
/// 前端的 `ImageData` 收的是**直通** alpha。把预乘数据直接塞进去，
/// 半透明像素会偏暗（alpha=128 的白会变成中灰）。
/// 全不透明的像素（`a == 255`）预乘与直通一致，直接抄过去 ——
/// 课件画面绝大多数像素走的就是这条零成本分支。
pub fn encode_frame(bitmap: &Bitmap) -> Result<Vec<u8>> {
    if !bitmap.is_consistent() {
        return Err(Error::render("位图缓冲长度与尺寸不一致"));
    }

    // 一次分配到位，之后全部按下标写入。
    //
    // **不要用 `extend_from_slice(&[u8; 4])` 逐像素追加**：那会在每个像素上
    // 走一次带容量检查的函数调用，1920×1080 就是 200 万次 —— 比 PNG 编码
    // 还慢。这个函数在 UI 线程上跑，慢一点就是肉眼可见的卡顿。
    let mut out = vec![0u8; 8 + bitmap.data.len()];
    out[0..4].copy_from_slice(&bitmap.width.to_le_bytes());
    out[4..8].copy_from_slice(&bitmap.height.to_le_bytes());
    let dst = &mut out[8..];

    match bitmap.format {
        PixelFormat::Rgba8 => dst.copy_from_slice(&bitmap.data),
        PixelFormat::Rgba8Premultiplied => {
            for (d, s) in dst.chunks_exact_mut(4).zip(bitmap.data.chunks_exact(4)) {
                let a = s[3];
                if a == 255 {
                    // 不透明像素预乘与直通完全一致 —— 直接整块抄，零浮点运算。
                    // 课件画面绝大多数像素走的是这条分支。
                    d.copy_from_slice(s);
                } else if a == 0 {
                    d.copy_from_slice(&[0, 0, 0, 0]);
                } else {
                    // 四舍五入而不是截断：整数除法会整体偏暗
                    let inv = 255.0 / a as f32;
                    d[0] = (s[0] as f32 * inv + 0.5).min(255.0) as u8;
                    d[1] = (s[1] as f32 * inv + 0.5).min(255.0) as u8;
                    d[2] = (s[2] as f32 * inv + 0.5).min(255.0) as u8;
                    d[3] = a;
                }
            }
        }
    }

    Ok(out)
}

/// 一帧「什么都没有」的占位（1×1 全透明）。
///
/// 前端靠尺寸判断「这一层不存在」。用固定字节而不是现算，
/// 是因为它每次内容都一样，没必要付出一次分配的代价。
pub fn empty_frame() -> Vec<u8> {
    vec![
        1, 0, 0, 0, // 宽 = 1
        1, 0, 0, 0, // 高 = 1
        0, 0, 0, 0, // 一个全透明像素
    ]
}

/// 把位图编码成 PNG。
///
/// 仅用于**导出到磁盘**这类需要真文件的场景（缩略图落盘、另存为图片）。
/// 送往 WebView 渲染的位图请走 [`encode_frame`] —— 那条路不该付压缩的代价。
pub fn encode_png(bitmap: &Bitmap) -> Result<Vec<u8>> {
    if !bitmap.is_consistent() {
        return Err(Error::render("位图缓冲长度与尺寸不一致"));
    }

    let size = tiny_skia::IntSize::from_wh(bitmap.width, bitmap.height)
        .ok_or_else(|| Error::render("位图尺寸无效"))?;

    // 预乘数据直接构造 Pixmap：tiny-skia 的编码器会在写出时
    // 还原为 PNG 要求的直通 alpha
    let pixmap = tiny_skia::Pixmap::from_vec(bitmap.data.clone(), size)
        .ok_or_else(|| Error::render("无法构造待编码位图"))?;

    pixmap
        .encode_png()
        .map_err(|e| Error::render(format!("PNG 编码失败：{e}")))
}

/// 未使用导入守卫：`Insets` / `Stroke` / `Rect` 经节点字段间接使用。
#[allow(dead_code)]
fn _assert_types(_: Insets, _: Stroke, _: Rect, _: PixmapPaint) {}

#[cfg(test)]
mod tests {
    use super::*;
    use ppt_core::scene::{
        linear_state, BodyProps, Build, Bullet, BulletKind, EmphasisStep, EffectState, Paragraph,
        Point, RunProps, SceneBackground, StrikeStyle, TableCell, TextRun, UnderlineStyle,
        VerticalAnchor, PLAY_ALL,
    };

    /// 不提供任何媒体的空实现（测试里只画几何与文本）。
    struct NoMedia;

    impl MediaProvider for NoMedia {
        fn read_media(&self, part: &str) -> Result<Vec<u8>> {
            Err(Error::MissingPart(part.to_string()))
        }
    }

    fn fonts() -> Option<Arc<FontContext>> {
        let ctx = FontContext::new();
        if ctx.font_count() == 0 {
            return None;
        }
        Some(Arc::new(ctx))
    }

    fn simple_scene(w: f32, h: f32) -> Scene {
        Scene::new(Size::new(w, h))
    }

    fn rect_node(x: f32, y: f32, w: f32, h: f32, fill: Fill) -> Node {
        Node {
            id: "r".into(),
            transform: Transform::translate(x, y),
            local_bbox: Some(Rect::new(0.0, 0.0, w, h)),
            geometry: Geometry::Rect,
            fill,
            ..Default::default()
        }
    }

    fn text_node(text: &str, x: f32, y: f32, size_pt: f32) -> Node {
        Node {
            id: "t".into(),
            transform: Transform::translate(x, y),
            local_bbox: Some(Rect::new(0.0, 0.0, 300.0, 60.0)),
            geometry: Geometry::None,
            text: Some(TextBox {
                body: BodyProps::default(),
                paragraphs: vec![Paragraph {
                    runs: vec![TextRun {
                        text: text.to_string(),
                        props: RunProps {
                            size_pt,
                            ..Default::default()
                        },
                        hyperlink: None,
                        field: None,
                    }],
                    ..Default::default()
                }],
            }),
            ..Default::default()
        }
    }

    fn pixel_at(bmp: &Bitmap, x: u32, y: u32) -> Color {
        bmp.pixel(x, y).unwrap_or(Color::TRANSPARENT)
    }

    #[test]
    fn render_options_scale_limits_pixels() {
        let opts = RenderOptions {
            scale: 10.0,
            max_pixels: 1_000_000,
            ..RenderOptions::default()
        };
        let (w, h) = opts.output_size(Size::new(960.0, 540.0));
        assert!(
            (w as u64) * (h as u64) <= 1_000_000,
            "像素数应受上限约束，实际 {w}×{h}"
        );
        // 宽高比应保持
        let ratio = w as f32 / h as f32;
        assert!((ratio - 960.0 / 540.0).abs() < 0.01, "宽高比应保持，实际 {ratio}");
    }

    #[test]
    fn render_options_keeps_scale_within_limit() {
        let opts = RenderOptions {
            scale: 1.0,
            max_pixels: 16_000_000,
            ..RenderOptions::default()
        };
        assert!((opts.effective_scale(Size::new(960.0, 540.0)) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn thumbnail_options_disable_expensive_features() {
        let opts = RenderOptions::thumbnail(0.2);
        assert!(!opts.draw_effects);
        assert!(!opts.draw_placeholders);
        assert!(!opts.layout.solve_autofit);
    }

    #[test]
    fn renders_solid_background() {
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let mut scene = simple_scene(100.0, 50.0);
        scene.background = SceneBackground::Solid(Color::rgb(0, 128, 255));

        let bmp = r.render(&scene, &RenderOptions::default(), &NoMedia).unwrap();
        assert_eq!((bmp.width, bmp.height), (100, 50));
        assert!(bmp.is_consistent());
        assert_eq!(pixel_at(&bmp, 50, 25), Color::rgb(0, 128, 255));
    }

    #[test]
    fn renders_rect_with_fill() {
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let mut scene = simple_scene(200.0, 100.0);
        scene.background = SceneBackground::Solid(Color::WHITE);
        scene.nodes.push(rect_node(
            20.0,
            20.0,
            60.0,
            40.0,
            Fill::Solid(Color::rgb(255, 0, 0)),
        ));

        let bmp = r.render(&scene, &RenderOptions::default(), &NoMedia).unwrap();
        assert_eq!(pixel_at(&bmp, 50, 40), Color::rgb(255, 0, 0), "矩形内应为红色");
        assert_eq!(pixel_at(&bmp, 5, 5), Color::WHITE, "矩形外应为白色");
    }

    /// 翻转形状里的文字，画出来必须和「同一个形状但不翻转」一模一样。
    ///
    /// PowerPoint 的语义：翻转形状时形状镜像、文字仍然正着
    /// （「文本在翻转的对象里不会被自动翻转」）。
    /// 解析阶段用 `Node::text_transform` 抵消掉这次翻转，这里验证渲染确实照做。
    #[test]
    fn flipped_shape_renders_its_text_upright() {
        let Some(fonts) = fonts() else { return };

        let extent = Size::new(200.0, 60.0);
        let off = Point::new(20.0, 20.0);
        let plain_tf = Transform::from_ooxml(off, extent, 0.0, false, false);

        let make = |flip: bool, cancel: bool| {
            let mut n = text_node("REPORT", 0.0, 0.0, 28.0);
            n.transform = Transform::from_ooxml(off, extent, 0.0, flip, false);
            n.local_bbox = Some(Rect::new(0.0, 0.0, extent.w, extent.h));
            n.text_transform = if flip && cancel { Some(plain_tf) } else { None };
            n
        };
        let render = |node: Node| {
            let mut r = Renderer::new(fonts.clone());
            let mut scene = simple_scene(240.0, 100.0);
            scene.background = SceneBackground::Solid(Color::WHITE);
            scene.nodes.push(node);
            r.render(&scene, &RenderOptions::default(), &NoMedia).unwrap()
        };

        let plain = render(make(false, false));
        let flipped = render(make(true, true));
        let mirrored = render(make(true, false));

        assert_eq!(plain.data, flipped.data, "抵消翻转后应当和不翻转画得完全一样");
        assert_ne!(
            plain.data, mirrored.data,
            "不抵消时文字确实画反了 —— 否则这条测试证明不了任何事"
        );
    }

    #[test]
    fn renders_ellipse_with_transparency_outside() {
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let mut scene = simple_scene(100.0, 100.0);
        scene.background = SceneBackground::Solid(Color::WHITE);
        scene.nodes.push(Node {
            id: "e".into(),
            transform: Transform::IDENTITY,
            local_bbox: Some(Rect::new(0.0, 0.0, 100.0, 100.0)),
            geometry: Geometry::Ellipse,
            fill: Fill::Solid(Color::rgb(0, 0, 255)),
            ..Default::default()
        });

        let bmp = r.render(&scene, &RenderOptions::default(), &NoMedia).unwrap();
        // 中心应是蓝色
        assert_eq!(pixel_at(&bmp, 50, 50), Color::rgb(0, 0, 255));
        // 四角应在椭圆外，保持白色
        assert_eq!(pixel_at(&bmp, 1, 1), Color::WHITE);
        assert_eq!(pixel_at(&bmp, 98, 98), Color::WHITE);
    }

    #[test]
    fn renders_round_rect_corners_as_background() {
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let mut scene = simple_scene(100.0, 100.0);
        scene.background = SceneBackground::Solid(Color::WHITE);
        scene.nodes.push(Node {
            id: "rr".into(),
            transform: Transform::IDENTITY,
            local_bbox: Some(Rect::new(0.0, 0.0, 100.0, 100.0)),
            geometry: Geometry::RoundRect {
                rx_pt: 30.0,
                ry_pt: 30.0,
            },
            fill: Fill::Solid(Color::rgb(0, 200, 0)),
            ..Default::default()
        });

        let bmp = r.render(&scene, &RenderOptions::default(), &NoMedia).unwrap();
        assert_eq!(pixel_at(&bmp, 50, 50), Color::rgb(0, 200, 0), "中心应填充");
        // 圆角足够大时四角应露出背景
        assert_eq!(pixel_at(&bmp, 1, 1), Color::WHITE, "圆角处应露出背景");
    }

    #[test]
    fn renders_stroke_on_path() {
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let mut scene = simple_scene(100.0, 100.0);
        scene.background = SceneBackground::Solid(Color::WHITE);

        let mut node = rect_node(20.0, 20.0, 60.0, 60.0, Fill::None);
        node.stroke = Some(Stroke {
            fill: ppt_core::scene::StrokeFill::Solid(Color::rgb(0, 0, 0)),
            width_pt: 6.0,
            ..Stroke::default()
        });
        scene.nodes.push(node);

        let bmp = r.render(&scene, &RenderOptions::default(), &NoMedia).unwrap();
        // 边框中心线在局部坐标 0 处，即画布 (20, 50) 附近
        let on_edge = pixel_at(&bmp, 20, 50);
        assert!(on_edge.luminance() < 0.5, "边框处应有深色像素，实际 {on_edge:?}");
        // 内部应保持背景色
        assert_eq!(pixel_at(&bmp, 50, 50), Color::WHITE, "无填充时内部应为背景");
    }

    #[test]
    fn hidden_node_is_not_rendered() {
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let mut scene = simple_scene(100.0, 100.0);
        scene.background = SceneBackground::Solid(Color::WHITE);

        let mut node = rect_node(0.0, 0.0, 100.0, 100.0, Fill::Solid(Color::rgb(255, 0, 0)));
        node.hidden = true;
        scene.nodes.push(node);

        let bmp = r.render(&scene, &RenderOptions::default(), &NoMedia).unwrap();
        assert_eq!(pixel_at(&bmp, 50, 50), Color::WHITE, "隐藏节点不应被绘制");
    }

    #[test]
    fn zero_opacity_node_is_not_rendered() {
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let mut scene = simple_scene(100.0, 100.0);
        scene.background = SceneBackground::Solid(Color::WHITE);
        let mut node = rect_node(0.0, 0.0, 100.0, 100.0, Fill::Solid(Color::rgb(255, 0, 0)));
        node.opacity = 0.0;
        scene.nodes.push(node);

        let bmp = r.render(&scene, &RenderOptions::default(), &NoMedia).unwrap();
        assert_eq!(pixel_at(&bmp, 50, 50), Color::WHITE);
    }

    #[test]
    fn build_step_gates_node_visibility() {
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let mut scene = simple_scene(100.0, 100.0);
        scene.background = SceneBackground::Solid(Color::WHITE);
        let mut node = rect_node(10.0, 10.0, 40.0, 40.0, Fill::Solid(Color::rgb(255, 0, 0)));
        node.build = Build {
            appear: Some(2),
            disappear: None,
        };
        scene.nodes.push(node);

        let at = |r: &mut Renderer, state: PlayState| {
            r.render(
                &scene,
                &RenderOptions {
                    state,
                    ..RenderOptions::default()
                },
                &NoMedia,
            )
            .unwrap()
        };

        assert_eq!(
            pixel_at(&at(&mut r, 0), 20, 20),
            Color::WHITE,
            "第 1 步时还不该出现"
        );
        assert_eq!(
            pixel_at(&at(&mut r, linear_state(2)), 20, 20),
            Color::rgb(255, 0, 0),
            "推到第 2 步就该出现"
        );
        // 默认值是「全部显示」，静态导出与缩略图不受影响
        assert_eq!(pixel_at(&at(&mut r, PLAY_ALL), 20, 20), Color::rgb(255, 0, 0));
    }

    #[test]
    fn build_step_can_also_hide() {
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let mut scene = simple_scene(100.0, 100.0);
        scene.background = SceneBackground::Solid(Color::WHITE);
        let mut node = rect_node(10.0, 10.0, 40.0, 40.0, Fill::Solid(Color::rgb(0, 0, 255)));
        // 出场动画的反面：第 1 步之后消失
        node.build = Build {
            appear: None,
            disappear: Some(1),
        };
        scene.nodes.push(node);

        let at = |r: &mut Renderer, state: PlayState| {
            r.render(
                &scene,
                &RenderOptions {
                    state,
                    ..RenderOptions::default()
                },
                &NoMedia,
            )
            .unwrap()
        };
        assert_eq!(pixel_at(&at(&mut r, 0), 20, 20), Color::rgb(0, 0, 255));
        assert_eq!(pixel_at(&at(&mut r, linear_state(1)), 20, 20), Color::WHITE);
    }

    #[test]
    fn layer_contains_only_its_own_shape() {
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let mut scene = simple_scene(100.0, 100.0);
        scene.background = SceneBackground::Solid(Color::WHITE);
        // 只给红块一个课件形状 id：图层是按 `spid` 找的
        let mut red = rect_node(20.0, 20.0, 40.0, 40.0, Fill::Solid(Color::rgb(255, 0, 0)));
        red.shape_id = Some(7);
        scene.nodes.push(red.clone());
        scene.nodes.push(rect_node(
            0.0,
            0.0,
            100.0,
            100.0,
            Fill::Solid(Color::rgb(0, 0, 255)),
        ));

        let (layer, rect) = r
            .render_layer(&scene, 7, None, &[], &RenderOptions::default(), &NoMedia)
            .unwrap()
            .expect("应能取到图层");

        // 图层只包住红块 + 外扩余量，不会把整页捎上
        assert!(rect.w < 70.0, "图层不该有整页那么大：{rect:?}");
        assert!(rect.x < 20.0 && rect.y < 20.0, "要留出外扩余量：{rect:?}");
        // 蓝块被整页铺在底下，但图层里不该有它 —— 图层只有红块
        let blue = layer
            .data
            .chunks_exact(4)
            .filter(|px| px[2] > 200 && px[0] < 60)
            .count();
        assert_eq!(blue, 0, "图层里不该混进其它形状");
        let red_px = layer
            .data
            .chunks_exact(4)
            .filter(|px| px[0] > 200 && px[2] < 60)
            .count();
        assert!(red_px > 0, "红块应该在图层的位图里");
    }

    #[test]
    fn layer_bounds_accounts_for_extra_scale() {
        let Some(fonts) = fonts() else { return };
        let _ = fonts;

        let mut scene = simple_scene(100.0, 100.0);
        let mut n = rect_node(10.0, 10.0, 40.0, 40.0, Fill::Solid(Color::rgb(255, 0, 0)));
        n.shape_id = Some(3);
        scene.nodes.push(n);

        let plain = layer_bounds(&scene, 3, 0, &[]).expect("能算出边界");
        let grown = layer_bounds(
            &scene,
            3,
            0,
            &[EffectState {
                scale: 2.0,
                ..EffectState::IDENTITY
            }],
        )
        .expect("能算出边界");
        // 放大到 2 倍：包围盒要比静止时大一圈，否则动画放到一半就被切边。
        // 放大后的矩形会超出画布，最终被夹回画布 —— 所以这里只能比「变大」，
        // 不能精确等于 2 倍
        assert!(grown.w > plain.w * 1.3, "{plain:?} → {grown:?}");
        assert!(grown.h > plain.h * 1.3, "{plain:?} → {grown:?}");
    }

    #[test]
    fn emphasis_state_bends_the_static_frame() {
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let mut scene = simple_scene(100.0, 100.0);
        scene.background = SceneBackground::Solid(Color::WHITE);
        let mut n = rect_node(40.0, 40.0, 20.0, 20.0, Fill::Solid(Color::rgb(255, 0, 0)));
        n.shape_id = Some(1);
        // 第 2 步把它放大到 3 倍（绕包围盒中心）
        n.emphasis = vec![EmphasisStep {
            step: 2,
            to: EffectState {
                scale: 3.0,
                ..EffectState::IDENTITY
            },
        }];
        scene.nodes.push(n);

        let mut shot = |state: PlayState| {
            r.render(
                &scene,
                &RenderOptions {
                    state,
                    ..RenderOptions::default()
                },
                &NoMedia,
            )
            .unwrap()
        };

        assert_eq!(pixel_at(&shot(0), 30, 50), Color::WHITE, "第 1 步还没放大");
        assert_eq!(pixel_at(&shot(0), 50, 50), Color::rgb(255, 0, 0));
        // 播完之后停在三倍大 —— 这就是「强调动画是持续的」
        let after = shot(ppt_core::scene::step_bit(2));
        assert_eq!(pixel_at(&after, 30, 50), Color::rgb(255, 0, 0));
        assert_eq!(pixel_at(&after, 50, 50), Color::rgb(255, 0, 0));
    }

    #[test]
    fn hidden_shapes_are_left_out_of_the_frame() {
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let mut scene = simple_scene(100.0, 100.0);
        scene.background = SceneBackground::Solid(Color::WHITE);
        let mut n = rect_node(10.0, 10.0, 40.0, 40.0, Fill::Solid(Color::rgb(255, 0, 0)));
        n.shape_id = Some(5);
        scene.nodes.push(n);

        let opts = RenderOptions::default();
        let shown = r.render_hiding(&scene, &opts, &NoMedia, &[]).unwrap();
        let hidden = r.render_hiding(&scene, &opts, &NoMedia, &[5]).unwrap();
        assert_eq!(pixel_at(&shown, 20, 20), Color::rgb(255, 0, 0));
        assert_eq!(
            pixel_at(&hidden, 20, 20),
            Color::WHITE,
            "摘掉之后底帧里不该有它（强调动画要靠这个避免重影）"
        );
    }

    #[test]
    fn group_children_use_absolute_transforms() {
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let mut scene = simple_scene(200.0, 200.0);
        scene.background = SceneBackground::Solid(Color::WHITE);

        // 子节点的 transform 是**绝对**的（解析阶段已把组合偏移烘焙进去），
        // 因此这里显式写 100,100，而不是相对组合的 0,0
        let child = rect_node(100.0, 100.0, 50.0, 50.0, Fill::Solid(Color::rgb(255, 0, 0)));
        let group = Node {
            id: "g".into(),
            transform: Transform::translate(100.0, 100.0),
            geometry: Geometry::None,
            local_bbox: Some(Rect::new(0.0, 0.0, 200.0, 200.0)),
            children: vec![child],
            ..Default::default()
        };
        scene.nodes.push(group);

        let bmp = r.render(&scene, &RenderOptions::default(), &NoMedia).unwrap();
        assert_eq!(
            pixel_at(&bmp, 120, 120),
            Color::rgb(255, 0, 0),
            "子节点按其绝对变换绘制"
        );
        assert_eq!(pixel_at(&bmp, 20, 20), Color::WHITE, "原位置应无内容");
    }

    #[test]
    fn nested_group_opacity_multiplies() {
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let mut scene = simple_scene(100.0, 100.0);
        scene.background = SceneBackground::Solid(Color::WHITE);

        let mut child = rect_node(0.0, 0.0, 100.0, 100.0, Fill::Solid(Color::rgb(0, 0, 0)));
        child.opacity = 0.5;
        let group = Node {
            id: "g".into(),
            transform: Transform::IDENTITY,
            opacity: 0.5,
            geometry: Geometry::None,
            local_bbox: Some(Rect::new(0.0, 0.0, 100.0, 100.0)),
            children: vec![child],
            ..Default::default()
        };
        scene.nodes.push(group);

        let bmp = r.render(&scene, &RenderOptions::default(), &NoMedia).unwrap();
        let c = pixel_at(&bmp, 50, 50);
        // 0.5 × 0.5 = 0.25 黑色叠在白色上 → 约 75% 白
        assert!(c.luminance() > 0.5, "组合不透明度应相乘，实际 {c:?}");
    }

    #[test]
    fn renders_text_into_bitmap() {
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let mut scene = simple_scene(300.0, 100.0);
        scene.background = SceneBackground::Solid(Color::WHITE);
        scene.nodes.push(text_node("课程", 10.0, 20.0, 40.0));

        let bmp = r.render(&scene, &RenderOptions::default(), &NoMedia).unwrap();

        let dark = bmp
            .data
            .chunks_exact(4)
            .filter(|px| px[0] < 128 && px[3] > 0)
            .count();
        assert!(dark > 50, "应画出文字像素，实际 {dark}");
    }

    #[test]
    fn degraded_placeholder_is_drawn_when_enabled() {
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let mut scene = simple_scene(200.0, 100.0);
        scene.background = SceneBackground::Solid(Color::WHITE);
        scene.nodes.push(Node {
            id: "p".into(),
            transform: Transform::IDENTITY,
            local_bbox: Some(Rect::new(0.0, 0.0, 200.0, 100.0)),
            geometry: Geometry::placeholder("图表"),
            fill: Fill::Solid(Color::rgb(200, 200, 200)),
            ..Default::default()
        });

        let on = RenderOptions {
            draw_placeholders: true,
            ..RenderOptions::default()
        };
        let bmp = r.render(&scene, &on, &NoMedia).unwrap();
        assert_ne!(pixel_at(&bmp, 100, 50), Color::WHITE, "启用时应画占位");

        let off = RenderOptions {
            draw_placeholders: false,
            ..RenderOptions::default()
        };
        let bmp2 = r.render(&scene, &off, &NoMedia).unwrap();
        assert_eq!(pixel_at(&bmp2, 100, 50), Color::WHITE, "关闭时不应画占位");
    }

    #[test]
    fn table_cells_are_rendered() {
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let mut scene = simple_scene(200.0, 100.0);
        scene.background = SceneBackground::Solid(Color::WHITE);
        scene.nodes.push(Node {
            id: "tbl".into(),
            transform: Transform::IDENTITY,
            local_bbox: Some(Rect::new(0.0, 0.0, 200.0, 100.0)),
            geometry: Geometry::Table(Table {
                columns: vec![100.0, 100.0],
                rows: vec![50.0, 50.0],
                cells: vec![
                    vec![
                        TableCell {
                            fill: Fill::Solid(Color::rgb(255, 0, 0)),
                            ..Default::default()
                        },
                        TableCell::default(),
                    ],
                    vec![TableCell::default(), TableCell::default()],
                ],
                ..Default::default()
            }),
            ..Default::default()
        });

        let bmp = r.render(&scene, &RenderOptions::default(), &NoMedia).unwrap();
        assert_eq!(pixel_at(&bmp, 50, 25), Color::rgb(255, 0, 0), "第一个单元格应填充");
        assert_eq!(pixel_at(&bmp, 150, 25), Color::WHITE, "第二个单元格无填充");
    }

    #[test]
    fn table_row_grows_to_fit_wrapped_text() {
        // PowerPoint 里 `a:tr/@h` 是行高的**最小值**：装不下时长高。
        // 不这么做的话，单元格里第二行起就会压到下一行上去 ——
        // 真实课件里最常见的「串行」都是这么来的。
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let mut scene = simple_scene(200.0, 200.0);
        scene.background = SceneBackground::Solid(Color::WHITE);
        scene.nodes.push(Node {
            id: "tbl".into(),
            transform: Transform::IDENTITY,
            local_bbox: Some(Rect::new(0.0, 0.0, 100.0, 10.0)),
            geometry: Geometry::Table(Table {
                columns: vec![100.0],
                // 行高只给 10pt，连一行 12pt 的字都装不下
                rows: vec![10.0, 30.0],
                cells: vec![
                    vec![TableCell {
                        text: Some(TextBox {
                            body: BodyProps::default(),
                            paragraphs: vec![Paragraph {
                                runs: vec![TextRun::new("中文中文中文中文中文中文")],
                                ..Default::default()
                            }],
                            ..Default::default()
                        }),
                        ..Default::default()
                    }],
                    vec![TableCell {
                        fill: Fill::Solid(Color::rgb(255, 0, 0)),
                        ..Default::default()
                    }],
                ],
                ..Default::default()
            }),
            ..Default::default()
        });

        let bmp = r.render(&scene, &RenderOptions::default(), &NoMedia).unwrap();

        // 第二行（红底）的顶边必须被挤到第一行文字之下，
        // 而不是按指定的 10pt 直接压在第一行文字上
        let first_red_row = (0..bmp.height as usize)
            .find(|y| {
                (0..bmp.width as usize).any(|x| {
                    let i = (y * bmp.width as usize + x) * 4;
                    bmp.data[i] > 200 && bmp.data[i + 1] < 60 && bmp.data[i + 2] < 60
                })
            })
            .expect("第二行的红底应该画出来");
        assert!(
            first_red_row > 30,
            "红底压在 y={first_red_row}，说明行高没跟着内容长"
        );
    }

    #[test]
    fn table_merged_cells_cover_spanned_area() {
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let mut scene = simple_scene(200.0, 100.0);
        scene.background = SceneBackground::Solid(Color::WHITE);
        scene.nodes.push(Node {
            id: "tbl".into(),
            transform: Transform::IDENTITY,
            local_bbox: Some(Rect::new(0.0, 0.0, 200.0, 100.0)),
            geometry: Geometry::Table(Table {
                columns: vec![100.0, 100.0],
                rows: vec![50.0, 50.0],
                cells: vec![
                    vec![
                        TableCell {
                            col_span: 2,
                            fill: Fill::Solid(Color::rgb(0, 0, 255)),
                            ..Default::default()
                        },
                        TableCell {
                            h_merge: true,
                            ..Default::default()
                        },
                    ],
                    vec![TableCell::default(), TableCell::default()],
                ],
                ..Default::default()
            }),
            ..Default::default()
        });

        let bmp = r.render(&scene, &RenderOptions::default(), &NoMedia).unwrap();
        // 合并后的区块应整体填充
        assert_eq!(pixel_at(&bmp, 50, 25), Color::rgb(0, 0, 255));
        assert_eq!(pixel_at(&bmp, 150, 25), Color::rgb(0, 0, 255), "跨列区域应被主格填充");
    }

    #[test]
    fn scale_changes_output_resolution() {
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let scene = simple_scene(100.0, 50.0);
        let opts = RenderOptions {
            scale: 2.0,
            ..RenderOptions::default()
        };
        let bmp = r.render(&scene, &opts, &NoMedia).unwrap();
        assert_eq!((bmp.width, bmp.height), (200, 100));
    }

    /// 文字墨迹的像素包围盒（左上右下）。
    fn ink_bbox(bmp: &Bitmap) -> Option<(u32, u32, u32, u32)> {
        let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
        for y in 0..bmp.height {
            for x in 0..bmp.width {
                let i = ((y * bmp.width + x) * 4) as usize;
                if bmp.data[i] < 128 {
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x);
                    y1 = y1.max(y);
                }
            }
        }
        (x0 != u32::MAX).then_some((x0, y0, x1, y1))
    }

    #[test]
    fn text_scales_with_canvas_resolution() {
        // 曾经的 bug：`draw_text` 只拿到节点变换、把画布缩放丢掉了，
        // 于是文字既不随分辨率变大、又停在未缩放的位置上 ——
        // 「解析出来的版式在 dump 工具里是对的，一进应用全偏」。
        // 因为所有单测都在 scale = 1.0 下跑，这个 bug 长期被掩盖，
        // 所以这里专门用两个不同 scale 交叉验证。
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let mut scene = simple_scene(200.0, 60.0);
        scene.background = SceneBackground::Solid(Color::WHITE);
        scene.nodes.push(text_node("Hi", 60.0, 20.0, 24.0));

        let render_at = |r: &mut Renderer, scale: f32| -> (u32, u32, u32, u32) {
            let opts = RenderOptions {
                scale,
                ..RenderOptions::default()
            };
            let bmp = r.render(&scene, &opts, &NoMedia).unwrap();
            ink_bbox(&bmp).expect("应渲染出文字墨迹")
        };

        let a = render_at(&mut r, 1.0);
        let b = render_at(&mut r, 2.0);

        // 换算回 pt 空间后，两次渲染的墨迹位置应一致
        for (name, lo, hi) in [
            ("左", a.0, b.0),
            ("上", a.1, b.1),
            ("右", a.2, b.2),
            ("下", a.3, b.3),
        ] {
            let pt_lo = lo as f32;
            let pt_hi = hi as f32 / 2.0;
            assert!(
                (pt_lo - pt_hi).abs() <= 1.5,
                "{name}边界在 pt 空间应重合：scale=1 为 {pt_lo}，scale=2 换算后为 {pt_hi}"
            );
        }

        // 像素宽度应接近两倍（未被缩放的话会几乎相等）
        let wide1 = a.2 - a.0;
        let wide2 = b.2 - b.0;
        assert!(
            wide2 as f32 > wide1 as f32 * 1.7,
            "scale=2 时墨迹宽度应接近两倍：{wide1} → {wide2}"
        );
    }

    #[test]
    fn transform_scale_maps_geometry_correctly() {
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let mut scene = simple_scene(100.0, 100.0);
        scene.background = SceneBackground::Solid(Color::WHITE);
        // 10×10 的矩形放大 5 倍后应覆盖 50×50
        let mut node = rect_node(0.0, 0.0, 10.0, 10.0, Fill::Solid(Color::rgb(255, 0, 0)));
        node.transform = Transform::scale(5.0, 5.0);
        scene.nodes.push(node);

        let bmp = r.render(&scene, &RenderOptions::default(), &NoMedia).unwrap();
        assert_eq!(pixel_at(&bmp, 25, 25), Color::rgb(255, 0, 0));
        assert_eq!(pixel_at(&bmp, 60, 60), Color::WHITE, "放大后不应波及更远");
    }

    #[test]
    fn empty_scene_renders_background_only() {
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let mut scene = simple_scene(50.0, 50.0);
        scene.background = SceneBackground::Solid(Color::rgb(1, 2, 3));

        let bmp = r.render(&scene, &RenderOptions::default(), &NoMedia).unwrap();
        assert_eq!(pixel_at(&bmp, 25, 25), Color::rgb(1, 2, 3));
    }

    #[test]
    fn background_fill_with_gradient() {
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let mut scene = simple_scene(200.0, 100.0);
        scene.background = SceneBackground::Fill(Fill::Gradient(ppt_core::scene::GradientFill {
            kind: ppt_core::scene::GradientKind::Linear {
                angle_deg: 0.0,
                scaled: true,
            },
            stops: vec![
                ppt_core::scene::GradientStop {
                    pos: 0.0,
                    color: Color::BLACK,
                },
                ppt_core::scene::GradientStop {
                    pos: 1.0,
                    color: Color::WHITE,
                },
            ],
            tile_rect: None,
            rotate_with_shape: true,
            flip: Default::default(),
        }));

        let bmp = r.render(&scene, &RenderOptions::default(), &NoMedia).unwrap();
        let left = pixel_at(&bmp, 5, 50);
        let right = pixel_at(&bmp, 195, 50);
        assert!(left.luminance() < right.luminance(), "渐变应从暗到亮");
    }

    #[test]
    fn invalid_scene_size_returns_error() {
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let scene = simple_scene(0.0, 0.0);
        assert!(r.render(&scene, &RenderOptions::default(), &NoMedia).is_err());
    }

    #[test]
    fn image_without_media_is_skipped_not_fatal() {
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let mut scene = simple_scene(100.0, 100.0);
        scene.background = SceneBackground::Solid(Color::WHITE);
        scene.nodes.push(Node {
            id: "pic".into(),
            transform: Transform::IDENTITY,
            local_bbox: Some(Rect::new(0.0, 0.0, 100.0, 100.0)),
            geometry: Geometry::Image(ppt_core::scene::ImageRef::new("ppt/media/missing.png")),
            ..Default::default()
        });

        // 图片读不到时应静默跳过，不让整页失败
        let bmp = r.render(&scene, &RenderOptions::default(), &NoMedia).unwrap();
        assert_eq!(pixel_at(&bmp, 50, 50), Color::WHITE);
    }

    #[test]
    fn renderer_is_reusable_across_pages() {
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let mut scene = simple_scene(100.0, 100.0);
        scene.background = SceneBackground::Solid(Color::WHITE);
        scene.nodes.push(text_node("重复", 10.0, 20.0, 30.0));

        let a = r.render(&scene, &RenderOptions::default(), &NoMedia).unwrap();
        let glyphs_after_first = r.cached_glyph_count();
        let b = r.render(&scene, &RenderOptions::default(), &NoMedia).unwrap();

        assert_eq!(a.data, b.data, "重复渲染应得到一致结果");
        assert_eq!(
            r.cached_glyph_count(),
            glyphs_after_first,
            "字形缓存应被复用"
        );
    }

    #[test]
    fn clear_caches_resets_state() {
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let mut scene = simple_scene(100.0, 100.0);
        scene.nodes.push(text_node("x", 0.0, 0.0, 20.0));
        let _ = r.render(&scene, &RenderOptions::default(), &NoMedia).unwrap();

        r.clear_caches();
        assert_eq!(r.cached_glyph_count(), 0);
        assert_eq!(r.cached_image_count(), 0);
    }

    #[test]
    fn bitmap_output_format_is_premultiplied() {
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let scene = simple_scene(10.0, 10.0);
        let bmp = r.render(&scene, &RenderOptions::default(), &NoMedia).unwrap();
        assert_eq!(bmp.format, PixelFormat::Rgba8Premultiplied);
    }

    #[test]
    fn many_nodes_render_without_panic() {
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let mut scene = simple_scene(400.0, 300.0);
        scene.background = SceneBackground::Solid(Color::WHITE);
        for i in 0..100 {
            let x = (i % 10) as f32 * 40.0;
            let y = (i / 10) as f32 * 30.0;
            let mut node = rect_node(
                x,
                y,
                30.0,
                20.0,
                Fill::Solid(Color::rgb((i * 2) as u8, 100, 200)),
            );
            node.id = format!("n{i}");
            scene.nodes.push(node);
        }

        let bmp = r.render(&scene, &RenderOptions::default(), &NoMedia).unwrap();
        assert!(bmp.is_consistent());
        assert_ne!(pixel_at(&bmp, 15, 10), Color::WHITE);
    }

    #[test]
    fn extreme_scale_is_clamped_by_max_pixels() {
        let Some(fonts) = fonts() else { return };
        let mut r = Renderer::new(fonts);

        let scene = simple_scene(960.0, 540.0);
        let opts = RenderOptions {
            scale: 100.0,
            max_pixels: 500_000,
            ..RenderOptions::default()
        };
        let bmp = r.render(&scene, &opts, &NoMedia).unwrap();
        assert!(
            (bmp.width as u64) * (bmp.height as u64) <= 500_000,
            "像素保护应生效，实际 {}×{}",
            bmp.width,
            bmp.height
        );
    }
}
