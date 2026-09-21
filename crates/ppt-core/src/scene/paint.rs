//! 绘制属性：颜色、填充、描边、效果。

use serde::{Deserialize, Serialize};

use super::geom::{Insets, Point, Size};

/// 直通（非预乘）RGBA 颜色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Default for Color {
    fn default() -> Self {
        Color::BLACK
    }
}

impl Color {
    pub const TRANSPARENT: Color = Color {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };
    pub const BLACK: Color = Color {
        r: 0,
        g: 0,
        b: 0,
        a: 255,
    };
    pub const WHITE: Color = Color {
        r: 255,
        g: 255,
        b: 255,
        a: 255,
    };

    #[inline]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Color { r, g, b, a: 255 }
    }

    #[inline]
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Color { r, g, b, a }
    }

    /// 解析 OOXML `<a:srgbClr val="RRGGBB">` 形式的十六进制色值。
    pub fn from_hex(hex: &str) -> Option<Color> {
        let h = hex.trim().trim_start_matches('#');
        if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let v = u32::from_str_radix(h, 16).ok()?;
        Some(Color::rgb(
            ((v >> 16) & 0xFF) as u8,
            ((v >> 8) & 0xFF) as u8,
            (v & 0xFF) as u8,
        ))
    }

    #[inline]
    pub const fn with_alpha(self, a: u8) -> Color {
        Color { a, ..self }
    }

    #[inline]
    pub fn is_transparent(&self) -> bool {
        self.a == 0
    }

    /// 归一化到 `[0,1]` 的浮点数组，供渲染后端使用。
    #[inline]
    pub fn to_unit(self) -> [f32; 4] {
        [
            self.r as f32 / 255.0,
            self.g as f32 / 255.0,
            self.b as f32 / 255.0,
            self.a as f32 / 255.0,
        ]
    }

    /// 线性插值，用于渐变与动画。
    #[inline]
    pub fn lerp(self, other: Color, t: f32) -> Color {
        let t = t.clamp(0.0, 1.0);
        let f = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
        Color {
            r: f(self.r, other.r),
            g: f(self.g, other.g),
            b: f(self.b, other.b),
            a: f(self.a, other.a),
        }
    }

    /// 相对亮度（0.0 全黑，1.0 全白），按 sRGB 感知加权。
    ///
    /// 用于「深色底上默认用白字」这类需要判断明暗的场景。
    /// 权重取 ITU-R BT.709（0.2126 / 0.7152 / 0.0722），
    /// 比简单平均更贴近人眼对绿色更敏感的实际感知。
    #[inline]
    pub fn luminance(&self) -> f32 {
        (0.2126 * self.r as f32 + 0.7152 * self.g as f32 + 0.0722 * self.b as f32) / 255.0
    }

    /// 是否为「深色」（亮度低于中值）。
    #[inline]
    pub fn is_dark(&self) -> bool {
        self.luminance() < 0.5
    }

    /// 与不透明背景合成（用于需要先铺底色的场景，如荧光笔叠加）。
    #[inline]
    pub fn over(self, backdrop: Color) -> Color {
        let sa = self.a as f32 / 255.0;
        let ba = backdrop.a as f32 / 255.0;
        let oa = sa + ba * (1.0 - sa);
        if oa <= f32::EPSILON {
            return Color::TRANSPARENT;
        }
        let ch = |s: u8, b: u8| {
            ((s as f32 * sa + b as f32 * ba * (1.0 - sa)) / oa).round().clamp(0.0, 255.0) as u8
        };
        Color {
            r: ch(self.r, backdrop.r),
            g: ch(self.g, backdrop.g),
            b: ch(self.b, backdrop.b),
            a: (oa * 255.0).round() as u8,
        }
    }
}

/// 填充方式。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub enum Fill {
    /// 无填充（`a:noFill`）。
    #[default]
    None,
    Solid(Color),
    Gradient(GradientFill),
    Pattern(PatternFill),
    Image(ImageFill),
    /// `a:grpFill`：沿用组合形状的填充，由解析阶段向父级求解。
    Inherit,
}

impl Fill {
    #[inline]
    pub fn solid(c: Color) -> Fill {
        Fill::Solid(c)
    }

    #[inline]
    pub fn is_visible(&self) -> bool {
        match self {
            Fill::None => false,
            Fill::Solid(c) => !c.is_transparent(),
            Fill::Inherit => false,
            Fill::Gradient(g) => g.stops.iter().any(|s| !s.color.is_transparent()),
            Fill::Pattern(_) => true,
            Fill::Image(_) => true,
        }
    }

    /// 该填充是否依赖「子级/自身需要先有形状轮廓」才能绘制（如图片填充的裁剪）。
    #[inline]
    pub fn needs_shape_clip(&self) -> bool {
        matches!(self, Fill::Image(_) | Fill::Pattern(_))
    }
}

/// 渐变色标。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GradientStop {
    /// 位置，`0.0..=1.0`。
    pub pos: f32,
    pub color: Color,
}

/// 渐变类型。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum GradientKind {
    /// 线性渐变，`angle_deg` 为 OOXML 语义的顺时针角度（0 = 从左到右）。
    Linear { angle_deg: f32, scaled: bool },
    /// 径向渐变。
    Radial {
        /// `true` 时渐变沿椭圆形路径扩展，`false` 时为正圆。
        ellipse: bool,
        /// 焦点相对位置。
        focus: Option<(f32, f32)>,
        /// 渐变填充范围（相对形状包围盒的 0..1 矩形）；`None` 表示铺满。
        fill_to_rect: Option<RelativeRect>,
    },
    /// 矩形（四方）渐变。
    Rect {
        fill_to_rect: Option<RelativeRect>,
    },
    /// 形状跟随渐变。
    Shape {
        fill_to_rect: Option<RelativeRect>,
    },
}

/// 相对包围盒的矩形，四个分量均为 `0.0..=1.0` 的比例。
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct RelativeRect {
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
}

impl RelativeRect {
    pub const FULL: RelativeRect = RelativeRect {
        l: 0.0,
        t: 0.0,
        r: 1.0,
        b: 1.0,
    };

    #[inline]
    pub fn is_full(&self) -> bool {
        *self == RelativeRect::FULL
    }
}

/// 渐变填充。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GradientFill {
    pub kind: GradientKind,
    pub stops: Vec<GradientStop>,
    /// 渐变的平铺矩形（相对形状包围盒）。`None` 时使用形状包围盒。
    pub tile_rect: Option<RelativeRect>,
    /// 是否随形状一起旋转。
    pub rotate_with_shape: bool,
    /// 是否翻转。
    pub flip: GradientFlip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GradientFlip {
    pub x: bool,
    pub y: bool,
}

impl GradientFill {
    /// 按位置排序后的色标；解析阶段可能乱序，渲染前统一整理。
    pub fn sorted_stops(&self) -> Vec<GradientStop> {
        let mut v = self.stops.clone();
        v.sort_by(|a, b| a.pos.partial_cmp(&b.pos).unwrap_or(std::cmp::Ordering::Equal));
        v
    }
}

/// 图案填充。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PatternFill {
    /// OOXML `prstPattType` 值，如 `pct20`、`ltHorz`。
    pub preset: String,
    pub foreground: Color,
    pub background: Color,
}

/// 图片填充。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageFill {
    pub image: ImageRef,
    /// 拉伸填充时的源矩形（相对图片，0..1）。
    pub stretch: Option<RelativeRect>,
    pub tile: Option<ImageTile>,
}

/// 图片平铺参数。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ImageTile {
    pub tx: f32,
    pub ty: f32,
    pub sx: f32,
    pub sy: f32,
    pub flip: GradientFlip,
    /// 平铺对齐方式（OOXML `algn`）。
    pub align: TileAlign,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TileAlign {
    #[default]
    TopLeft,
    Top,
    TopRight,
    Left,
    Center,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
}

/// 图片引用。
///
/// **注意**：这里只记录部件名（如 `ppt/media/image1.png`），
/// 真正解压 + 解码延迟到渲染阶段按需进行，这是「不整体解析」的关键一环。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageRef {
    /// 包内部件名。
    pub part: String,
    /// 源裁剪矩形（相对比例 0..1），对应 OOXML `a:srcRect`。
    pub src_rect: RelativeRect,
    pub dpi: Option<f32>,
    pub rotate_with_shape: bool,
    pub compression: Option<ImageCompression>,
    /// `a:alphaModFix` 透明度修正（0..1）。
    pub alpha: Option<f32>,
    /// 原生尺寸（pt），用于按显示尺寸决定解码降采样倍率。
    pub native_size: Option<Size>,
}

impl ImageRef {
    pub fn new(part: impl Into<String>) -> ImageRef {
        ImageRef {
            part: part.into(),
            src_rect: RelativeRect::FULL,
            dpi: None,
            rotate_with_shape: false,
            compression: None,
            alpha: None,
            native_size: None,
        }
    }

    /// 源裁剪是否为整图。
    #[inline]
    pub fn is_full_bleed(&self) -> bool {
        self.src_rect.is_full()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageCompression {
    None,
    Jpeg,
    Png,
    Tiff,
}

/// 描边填充。绝大多数情况是纯色，但 OOXML 允许渐变描边。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StrokeFill {
    None,
    Solid(Color),
    Gradient(GradientFill),
    /// 图片描边（罕见）。
    Image(ImageRef),
}

/// 描边。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stroke {
    pub fill: StrokeFill,
    pub width_pt: f32,
    pub dash: DashStyle,
    /// `DashStyle::Custom` 时的虚线段长度（单位：pt）。
    pub custom_dash: Vec<f32>,
    pub cap: LineCap,
    pub join: LineJoin,
    pub miter_limit: f32,
    pub align: PenAlign,
    pub head_end: Option<LineEnd>,
    pub tail_end: Option<LineEnd>,
}

impl Default for Stroke {
    fn default() -> Self {
        Stroke {
            fill: StrokeFill::Solid(Color::BLACK),
            width_pt: 1.0,
            dash: DashStyle::Solid,
            custom_dash: Vec::new(),
            cap: LineCap::Flat,
            join: LineJoin::Round,
            miter_limit: 8.0,
            align: PenAlign::Center,
            head_end: None,
            tail_end: None,
        }
    }
}

impl Stroke {
    /// 描边是否可见（宽度为 0 或颜色全透明时不可见）。
    #[inline]
    pub fn is_visible(&self) -> bool {
        if self.width_pt <= 0.0 {
            return false;
        }
        match &self.fill {
            StrokeFill::None => false,
            StrokeFill::Solid(c) => !c.is_transparent(),
            StrokeFill::Gradient(g) => g.stops.iter().any(|s| !s.color.is_transparent()),
            StrokeFill::Image(_) => true,
        }
    }

    /// 主色（用于需要单色的简化渲染路径）。
    pub fn primary_color(&self) -> Option<Color> {
        match &self.fill {
            StrokeFill::None => None,
            StrokeFill::Solid(c) => Some(*c),
            StrokeFill::Gradient(g) => g.stops.first().map(|s| s.color),
            StrokeFill::Image(_) => Some(Color::BLACK),
        }
    }
}

/// 虚线样式（对应 OOXML `a:prstDash`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum DashStyle {
    #[default]
    Solid,
    Dot,
    Dash,
    LgDash,
    DashDot,
    LgDashDot,
    LgDashDotDot,
    SysDash,
    SysDot,
    SysDashDot,
    SysDashDotDot,
    /// `custDash`：使用 `Stroke::custom_dash`。
    Custom,
}

impl DashStyle {
    /// 返回以「描边宽度的倍数」表示的 (实线长度, 空白长度) 序列。
    /// 与 OOXML 预设虚线的定义一致。
    pub fn pattern(self, custom: &[f32]) -> Vec<f32> {
        match self {
            DashStyle::Solid => Vec::new(),
            DashStyle::Dot => vec![1.0, 3.0],
            DashStyle::Dash => vec![4.0, 3.0],
            DashStyle::LgDash => vec![8.0, 3.0],
            DashStyle::DashDot => vec![4.0, 3.0, 1.0, 3.0],
            DashStyle::LgDashDot => vec![8.0, 3.0, 1.0, 3.0],
            DashStyle::LgDashDotDot => vec![8.0, 3.0, 1.0, 3.0, 1.0, 3.0],
            DashStyle::SysDash => vec![3.0, 1.0],
            DashStyle::SysDot => vec![1.0, 1.0],
            DashStyle::SysDashDot => vec![3.0, 1.0, 1.0, 1.0],
            DashStyle::SysDashDotDot => vec![3.0, 1.0, 1.0, 1.0, 1.0, 1.0],
            DashStyle::Custom => {
                if custom.is_empty() {
                    Vec::new()
                } else {
                    custom.to_vec()
                }
            }
        }
    }
}

/// 线端点样式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum LineCap {
    #[default]
    Flat,
    Round,
    Square,
}

/// 线连接样式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum LineJoin {
    Round,
    #[default]
    Bevel,
    Miter,
}

/// 描边相对路径的位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PenAlign {
    #[default]
    Center,
    Inset,
    Outset,
}

/// 线条端点装饰（`a:headEnd` / `a:tailEnd`）。
///
/// 尺寸按 OOXML 的规矩是**相对线宽的倍数**：`w="sm|med|lg"` 分别对应
/// 2 / 3 / 5 倍，缺省是 `med`。所以同一种箭头，线一粗就跟着变大。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LineEnd {
    pub kind: LineEndKind,
    /// 横向（垂直于线）的倍数。
    pub width_scale: f32,
    /// 沿线的长度倍数。
    pub length_scale: f32,
}

impl LineEnd {
    /// 按 OOXML 缺省尺寸（`w="med" len="med"`）构造。
    #[inline]
    pub fn new(kind: LineEndKind) -> LineEnd {
        LineEnd {
            kind,
            width_scale: 3.0,
            length_scale: 3.0,
        }
    }
}

/// 箭头样式（`a:headEnd/@type`、`a:tailEnd/@type`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LineEndKind {
    /// `type="none"`：不画。
    None,
    /// 实心三角。
    Triangle,
    /// 凹背三角（“隐形”箭头）。
    Stealth,
    /// 菱形。
    Diamond,
    /// 圆点。
    Oval,
    /// 开口箭头（两条线组成的 V）。
    Arrow,
}

/// 形状效果的集合。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Effects {
    pub outer_shadow: Option<OuterShadow>,
    pub inner_shadow: Option<InnerShadow>,
    pub glow: Option<Glow>,
    pub soft_edge_pt: Option<f32>,
    pub blur_pt: Option<f32>,
    pub reflection: Option<Reflection>,
}

impl Effects {
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.outer_shadow.is_none()
            && self.inner_shadow.is_none()
            && self.glow.is_none()
            && self.soft_edge_pt.is_none()
            && self.blur_pt.is_none()
            && self.reflection.is_none()
    }

    /// 是否含有成本较高的效果（用于低配档位降级判断）。
    #[inline]
    pub fn has_expensive_effect(&self) -> bool {
        self.soft_edge_pt.is_some() || self.blur_pt.is_some() || self.reflection.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OuterShadow {
    pub color: Color,
    pub blur_pt: f32,
    pub distance_pt: f32,
    pub direction_deg: f32,
    pub scale_x: f32,
    pub scale_y: f32,
    pub align: RectAlign,
    pub rotate_with_shape: bool,
    /// 阴影倾斜（`sx`/`sy`，通常为 0）。
    pub skew_x_deg: f32,
    pub skew_y_deg: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct InnerShadow {
    pub color: Color,
    pub blur_pt: f32,
    pub distance_pt: f32,
    pub direction_deg: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Glow {
    pub color: Color,
    pub radius_pt: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Reflection {
    pub blur_pt: f32,
    pub start_alpha: f32,
    pub end_alpha: f32,
    pub distance_pt: f32,
    pub direction_deg: f32,
    pub fade_direction_deg: f32,
}

/// 阴影对齐方式，决定缩放/倾斜的基准点。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RectAlign {
    #[default]
    TopLeft,
    Top,
    TopRight,
    Left,
    Center,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
}

/// 计算阴影偏移向量。
pub fn shadow_offset(distance_pt: f32, direction_deg: f32) -> Point {
    // OOXML 中阴影方向 0 度指向正右方，顺时针为正。
    let r = direction_deg.to_radians();
    Point::new(distance_pt * r.cos(), distance_pt * r.sin())
}

/// 高斯近似：给出模糊半径下，3 次盒式滤波的等效半径。
pub fn box_blur_radius_for(gaussian_sigma: f32) -> f32 {
    if gaussian_sigma <= 0.0 {
        return 0.0;
    }
    // 经典近似：sigma ≈ (n^2 - 1) / (2 * sqrt(3n^2 + 4)) 的反解，实践中直接用 1.5 * sigma
    (gaussian_sigma * 1.5).max(1.0)
}

/// 计算内边距与形状尺寸的关系，返回可用文本区域尺寸。
pub fn text_area_size(extent: Size, insets: Insets) -> Size {
    Size::new(
        (extent.w - insets.horizontal()).max(0.0),
        (extent.h - insets.vertical()).max(0.0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_parsing() {
        assert_eq!(Color::from_hex("FF8000"), Some(Color::rgb(255, 128, 0)));
        assert_eq!(Color::from_hex("#0000ff"), Some(Color::rgb(0, 0, 255)));
        assert_eq!(Color::from_hex("xyz"), None);
        assert_eq!(Color::from_hex("12345"), None);
    }

    #[test]
    fn color_lerp_endpoints() {
        let a = Color::rgb(0, 0, 0);
        let b = Color::rgb(255, 255, 255);
        assert_eq!(a.lerp(b, 0.0), a);
        assert_eq!(a.lerp(b, 1.0), b);
        assert_eq!(a.lerp(b, 0.5), Color::rgb(128, 128, 128));
    }

    #[test]
    fn color_over_opaque_backdrop() {
        let red_half = Color::rgba(255, 0, 0, 128);
        let white = Color::WHITE;
        let out = red_half.over(white);
        assert_eq!(out.a, 255);
        assert!(out.r > 200 && out.g > 100 && out.g < 160);
    }

    #[test]
    fn fill_visibility() {
        assert!(!Fill::None.is_visible());
        assert!(Fill::Solid(Color::WHITE).is_visible());
        assert!(!Fill::Solid(Color::TRANSPARENT).is_visible());
        assert!(!Fill::Inherit.is_visible());
    }

    #[test]
    fn stroke_visibility_zero_width() {
        let mut s = Stroke {
            width_pt: 0.0,
            ..Default::default()
        };
        assert!(!s.is_visible());
        s.width_pt = 2.0;
        assert!(s.is_visible());
    }

    #[test]
    fn dash_patterns_are_nonempty_except_solid() {
        assert!(DashStyle::Solid.pattern(&[]).is_empty());
        for d in [
            DashStyle::Dot,
            DashStyle::Dash,
            DashStyle::LgDash,
            DashStyle::DashDot,
            DashStyle::SysDot,
        ] {
            assert!(!d.pattern(&[]).is_empty(), "{:?} 应有虚线模式", d);
        }
        // 自定义虚线为空时退化为实线
        assert!(DashStyle::Custom.pattern(&[]).is_empty());
        assert_eq!(DashStyle::Custom.pattern(&[2.0, 1.0]), vec![2.0, 1.0]);
    }

    #[test]
    fn gradient_stops_sorted() {
        let g = GradientFill {
            kind: GradientKind::Linear {
                angle_deg: 0.0,
                scaled: true,
            },
            stops: vec![
                GradientStop {
                    pos: 1.0,
                    color: Color::WHITE,
                },
                GradientStop {
                    pos: 0.0,
                    color: Color::BLACK,
                },
            ],
            tile_rect: None,
            rotate_with_shape: true,
            flip: GradientFlip::default(),
        };
        let s = g.sorted_stops();
        assert_eq!(s[0].pos, 0.0);
        assert_eq!(s[1].pos, 1.0);
    }

    #[test]
    fn shadow_offset_directions() {
        let p = shadow_offset(10.0, 0.0);
        assert!((p.x - 10.0).abs() < 1e-3 && p.y.abs() < 1e-3);
        let p = shadow_offset(10.0, 90.0);
        assert!(p.x.abs() < 1e-3 && (p.y - 10.0).abs() < 1e-3);
    }

    #[test]
    fn text_area_shrinks_by_insets() {
        let s = text_area_size(Size::new(100.0, 50.0), Insets::uniform(5.0));
        assert_eq!(s, Size::new(90.0, 40.0));
    }

    #[test]
    fn text_area_never_negative() {
        let s = text_area_size(Size::new(10.0, 10.0), Insets::uniform(50.0));
        assert_eq!(s, Size::ZERO);
    }

    #[test]
    fn image_ref_full_bleed() {
        let i = ImageRef::new("ppt/media/image1.png");
        assert!(i.is_full_bleed());
    }
}
