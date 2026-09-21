//! 画界面用的最小图元集合。
//!
//! 全部基于 tiny-skia（与本项目渲染课件用的是同一套光栅化库），
//! 文字则直接复用 `ppt-render` 的文本管线 —— 中文断行、字体回退
//! 这些坑在那边已经趟平了，安装程序没必要再实现一遍。

use ppt_core::scene::{
    BodyProps, FontSet, Insets, Paragraph, RunProps, Size as PtSize, TextBox, TextRun, Transform,
    VerticalAnchor,
};
use ppt_text::FontContext;
use tiny_skia::{
    Color, FillRule, Paint, Path, PathBuilder, Pixmap, Rect, Transform as SkTransform,
};

use crate::theme;

/// 一个矩形（左上角 + 宽高），比 tiny-skia 的 `Rect` 好用：UI 布局全是这种描述。
#[derive(Debug, Clone, Copy)]
pub struct Box2 {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Box2 {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Box2 {
        Box2 { x, y, w, h }
    }
    pub fn rect(&self) -> Rect {
        Rect::from_xywh(self.x, self.y, self.w, self.h).unwrap_or_else(|| {
            Rect::from_xywh(self.x, self.y, self.w.max(1.0), self.h.max(1.0)).unwrap()
        })
    }
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px <= self.x + self.w && py >= self.y && py <= self.y + self.h
    }
    pub fn inset(&self, d: f32) -> Box2 {
        Box2::new(self.x + d, self.y + d, (self.w - d * 2.0).max(1.0), (self.h - d * 2.0).max(1.0))
    }
}

/// 圆角矩形路径。
pub fn rounded_path(b: Box2, r: f32) -> Path {
    let r = r.min(b.w / 2.0).min(b.h / 2.0).max(0.0);
    let (x, y, w, h) = (b.x, b.y, b.w, b.h);
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish().unwrap_or_else(|| PathBuilder::from_rect(b.rect()))
}

fn solid(color: Color) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(color);
    p.anti_alias = true;
    p
}

/// 纯色圆角矩形。
pub fn fill_round(pixmap: &mut Pixmap, b: Box2, r: f32, color: Color) {
    let path = rounded_path(b, r);
    pixmap.fill_path(&path, &solid(color), FillRule::Winding, SkTransform::identity(), None);
}

/// 竖向渐变的圆角矩形（顶部浅、底部深）。
pub fn fill_round_gradient(pixmap: &mut Pixmap, b: Box2, r: f32, top: Color, bottom: Color) {
    let path = rounded_path(b, r);
    let shader = tiny_skia::LinearGradient::new(
        tiny_skia::Point::from_xy(b.x, b.y),
        tiny_skia::Point::from_xy(b.x, b.y + b.h),
        vec![
            tiny_skia::GradientStop::new(0.0, top),
            tiny_skia::GradientStop::new(1.0, bottom),
        ],
        tiny_skia::SpreadMode::Pad,
        SkTransform::identity(),
    );
    let mut paint = Paint::default();
    if let Some(sh) = shader {
        paint.shader = sh;
    } else {
        paint.set_color(bottom);
    }
    paint.anti_alias = true;
    pixmap.fill_path(&path, &paint, FillRule::Winding, SkTransform::identity(), None);
}

/// 圆角矩形描边。
pub fn stroke_round(pixmap: &mut Pixmap, b: Box2, r: f32, color: Color, width: f32) {
    let path = rounded_path(b, r);
    pixmap.stroke_path(
        &path,
        &solid(color),
        &tiny_skia::Stroke { width, ..Default::default() },
        SkTransform::identity(),
        None,
    );
}

/// 柔和投影：叠若干层逐渐收小、逐渐变实的圆角矩形。
///
/// 没有高斯模糊，但 UI 上的投影本来就淡，这样叠出来的效果够用，
/// 而且完全确定（每次渲染结果一致）。
pub fn soft_shadow(pixmap: &mut Pixmap, b: Box2, r: f32, spread: f32, alpha: u8) {
    let steps = 10;
    for i in (0..steps).rev() {
        let t = i as f32 / steps as f32;
        let grow = spread * t;
        let a = (alpha as f32 * (1.0 - t * 0.75) / steps as f32 * 2.2) as u8;
        if a == 0 {
            continue;
        }
        let c = Color::from_rgba8(0x0B, 0x1F, 0x3A, a.max(2));
        fill_round(pixmap, b.inset(-grow), r + grow, c);
    }
}

/// 画一个圆点（单选项的圆、状态点）。
pub fn fill_circle(pixmap: &mut Pixmap, cx: f32, cy: f32, r: f32, color: Color) {
    let mut pb = PathBuilder::new();
    pb.push_circle(cx, cy, r);
    if let Some(path) = pb.finish() {
        pixmap.fill_path(&path, &solid(color), FillRule::Winding, SkTransform::identity(), None);
    }
}

/// 勾选标记（对号）：用两条线段画出来，省得为图标引入依赖。
pub fn draw_tick(pixmap: &mut Pixmap, b: Box2, color: Color, width: f32) {
    let mut pb = PathBuilder::new();
    pb.move_to(b.x + b.w * 0.22, b.y + b.h * 0.52);
    pb.line_to(b.x + b.w * 0.42, b.y + b.h * 0.72);
    pb.line_to(b.x + b.w * 0.78, b.y + b.h * 0.28);
    if let Some(path) = pb.finish() {
        pixmap.stroke_path(
            &path,
            &solid(color),
            &tiny_skia::Stroke {
                width,
                line_cap: tiny_skia::LineCap::Round,
                line_join: tiny_skia::LineJoin::Round,
                ..Default::default()
            },
            SkTransform::identity(),
            None,
        );
    }
}

/// 叉号（关闭按钮）。
pub fn draw_cross(pixmap: &mut Pixmap, b: Box2, color: Color, width: f32) {
    let mut pb = PathBuilder::new();
    pb.move_to(b.x + b.w * 0.28, b.y + b.h * 0.28);
    pb.line_to(b.x + b.w * 0.72, b.y + b.h * 0.72);
    pb.move_to(b.x + b.w * 0.72, b.y + b.h * 0.28);
    pb.line_to(b.x + b.w * 0.28, b.y + b.h * 0.72);
    if let Some(path) = pb.finish() {
        pixmap.stroke_path(
            &path,
            &solid(color),
            &tiny_skia::Stroke {
                width,
                line_cap: tiny_skia::LineCap::Round,
                ..Default::default()
            },
            SkTransform::identity(),
            None,
        );
    }
}

/// 背景：底部一片很淡的水彩。
///
/// 照 WPS 安装程序那种观感 —— 大片留白，只在下方铺几团柔和的彩色，
/// 让「白」不至于发死。做法是每团画几十层同心的低透明度圆，
/// 中心密、边缘淡，等价于一次高斯模糊，但完全确定、也不拖慢启动。
pub fn wash_background(pixmap: &mut Pixmap, s: f32, w: f32, h: f32) {
    // (中心 x 比例, 中心 y 比例, 半径, 颜色, 最深处透明度)
    let blobs: &[(f32, f32, f32, (u8, u8, u8), u8)] = &[
        (0.14, 0.88, 240.0, (0x8E, 0xC4, 0xF2), 54),
        (0.40, 1.00, 300.0, (0xC0, 0xD0, 0xF8), 44),
        (0.70, 0.90, 260.0, (0x9E, 0xD8, 0xEC), 40),
        (0.94, 1.04, 220.0, (0xCB, 0xC3, 0xF4), 38),
        (0.26, 0.70, 130.0, (0xC4, 0xE2, 0xF8), 22),
        (0.82, 0.64, 110.0, (0xCB, 0xDF, 0xF6), 18),
    ];
    let layers = 26;
    for (fx, fy, r, (cr, cg, cb), peak) in blobs {
        let cx = w * fx * s;
        let cy = h * fy * s;
        let r0 = r * s;
        for i in 0..layers {
            let t = i as f32 / layers as f32;
            let rad = r0 * (1.0 - t * 0.86);
            if rad <= 1.0 {
                continue;
            }
            let a = (peak / layers) as f32 * (1.0 + t * 2.2);
            let a = a.min(255.0) as u8;
            if a == 0 {
                continue;
            }
            fill_circle(pixmap, cx, cy, rad, Color::from_rgba8(*cr, *cg, *cb, a));
        }
    }
}

/// 自绘勾选框：选中是蓝底白钩，一眼能看出「勾上了没有」。
pub fn draw_checkbox(pixmap: &mut Pixmap, b: Box2, on: bool, hovered: bool, s: f32) {
    if on {
        fill_round_gradient(
            pixmap,
            b,
            b.w * 0.28,
            theme::primary_top(),
            theme::primary_bottom(),
        );
        draw_tick(pixmap, b.inset(b.w * 0.24), theme::white(), 2.6 * s);
    } else {
        fill_round(pixmap, b, b.w * 0.28, theme::page());
        stroke_round(
            pixmap,
            b,
            b.w * 0.28,
            if hovered { theme::ink_faint() } else { theme::line() },
            1.6 * s,
        );
    }
}

/// 画产品标识：蓝底圆角方块 + 白卡片 + 文字条 + 珊瑚圆点。
///
/// 和应用图标（`crates/ppt-app/icons/icon.png`）是同一个符号 ——
/// 安装时看到的、装完在任务栏和桌面看到的，必须是同一个东西。
pub fn draw_mark(pixmap: &mut Pixmap, x: f32, y: f32, size: f32) {
    let plate = Box2::new(x, y, size, size);
    fill_round_gradient(pixmap, plate, size * 0.235, theme::brand_top(), theme::brand_bottom());

    let card = Box2::new(
        x + size * 0.235,
        y + size * 0.255,
        size * 0.53,
        size * 0.365,
    );
    fill_round(pixmap, card, size * 0.055, theme::white());

    let line = Color::from_rgba8(0x6F, 0xA0, 0xE6, 255);
    let lh = size * 0.042;
    for (x0, x1, yy) in [(0.30, 0.585, 0.325), (0.30, 0.645, 0.425), (0.30, 0.50, 0.525)] {
        fill_round(
            pixmap,
            Box2::new(x + size * x0, y + size * yy, size * (x1 - x0), lh),
            lh / 2.0,
            line,
        );
    }

    fill_circle(pixmap, x + size * 0.675, y + size * 0.545, size * 0.085, theme::coral());

    let bar = Color::from_rgba8(0xAE, 0xC2, 0xE2, 255);
    let bh = size * 0.045;
    fill_round(
        pixmap,
        Box2::new(x + size * 0.30, y + size * 0.715, size * 0.36, bh),
        bh / 2.0,
        bar,
    );
}

// ---------------------------------------------------------------------------
// 文字
// ---------------------------------------------------------------------------

/// 文本绘制上下文：持有字体索引一次，之后反复复用。
pub struct TextCtx {
    fonts: FontContext,
    cache: ppt_render::text::GlyphCache,
    latin: String,
    ea: String,
}

impl TextCtx {
    /// 建索引。扫描系统字体要一两百毫秒，界面上做一次就够。
    pub fn new() -> TextCtx {
        let fonts = FontContext::new();
        let ea = pick_font(&fonts, &[theme::FONT_UI, theme::FONT_UI_FALLBACK, "SimSun"]);
        let latin = pick_font(&fonts, &[theme::FONT_LATIN, theme::FONT_UI_FALLBACK]);
        log::debug!("安装界面字体：latin={latin} ea={ea}");
        TextCtx {
            fonts,
            cache: ppt_render::text::GlyphCache::new(),
            latin,
            ea,
        }
    }

    fn make_box(&self, s: &str, size: f32, bold: bool, color: Color) -> TextBox {
        let c = color.to_color_u8();
        let props = RunProps {
            size_pt: size,
            bold,
            color: Some(ppt_core::scene::Color::rgba(c.red(), c.green(), c.blue(), c.alpha())),
            font: FontSet {
                latin: Some(self.latin.clone()),
                ea: Some(self.ea.clone()),
                ..Default::default()
            },
            // 标语言设为中文：弯引号这类「宽窄随语境变」的标点会走东亚字体
            language: Some("zh-CN".to_string()),
            ..Default::default()
        };
        TextBox {
            body: BodyProps {
                anchor: VerticalAnchor::Top,
                insets: Insets {
                    left: 0.0,
                    top: 0.0,
                    right: 0.0,
                    bottom: 0.0,
                },
                ..Default::default()
            },
            paragraphs: vec![Paragraph {
                runs: vec![TextRun {
                    text: s.to_string(),
                    props,
                    hyperlink: None,
                    field: None,
                }],
                ..Default::default()
            }],
        }
    }

    /// 画一段文字，返回它占用的高度（pt）。
    ///
    /// `x` / `y` 是**设备像素**（调用方已经把布局乘过 DPI 系数了），
    /// `size` 与 `wrap_w` 是**磅**；`scale` 只用来把磅换算成像素。
    /// 早先这里把 x/y 又乘了一遍 scale，结果控件（矢量）按 DPI 放大、
    /// 文字却跑到控件外面去 —— 高 DPI 屏上一眼就能看出来。
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        pixmap: &mut Pixmap,
        s: &str,
        x: f32,
        y: f32,
        size: f32,
        bold: bool,
        color: Color,
        wrap_w: f32,
        scale: f32,
    ) -> f32 {
        if s.is_empty() {
            return size * 1.2;
        }
        let tb = self.make_box(s, size, bold, color);
        let area = PtSize::new(wrap_w.max(1.0), 4000.0);
        let t = Transform::translate(x, y).multiply(&Transform::scale(scale, scale));
        let layout = ppt_render::text::draw_text(
            pixmap,
            &self.fonts,
            &tb,
            area,
            t,
            1.0,
            ppt_core::scene::Color::BLACK,
            &mut self.cache,
            ppt_text::LayoutOptions::default(),
        );
        layout.height
    }

    /// 只量高度，不画（用于把内容垂直摆正）。
    pub fn measure(&self, s: &str, size: f32, bold: bool, wrap_w: f32) -> f32 {
        self.measure_layout(s, size, bold, wrap_w).0
    }

    /// 量一行的宽度（用于把按钮文字横向居中）。
    pub fn measure_w(&self, s: &str, size: f32, bold: bool) -> f32 {
        self.measure_layout(s, size, bold, 100_000.0).1
    }

    fn measure_layout(&self, s: &str, size: f32, bold: bool, wrap_w: f32) -> (f32, f32) {
        let tb = self.make_box(s, size, bold, Color::BLACK);
        let area = PtSize::new(wrap_w.max(1.0), 4000.0);
        let l = ppt_render::text::measure_text(
            &self.fonts,
            &tb,
            area,
            ppt_text::LayoutOptions::default(),
        );
        (l.height, l.width)
    }

    /// 一行文字的基线相对行顶的偏移比例，用来把文字与控件的垂直中心对齐。
    pub fn line_height(&self, size: f32) -> f32 {
        size * 1.2
    }
}

fn pick_font(fonts: &FontContext, candidates: &[&str]) -> String {
    for c in candidates {
        if fonts.has_family(c) {
            return (*c).to_string();
        }
    }
    candidates.last().unwrap_or(&"SimSun").to_string()
}
