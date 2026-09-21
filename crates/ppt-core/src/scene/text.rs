//! 文本模型。
//!
//! 设计要点：**解析阶段只做「结构还原」，不做排版**。
//! 断行、整形、自动缩放都在 `ppt-text` 里完成，
//! 因为排版需要访问系统字体，属于渲染期依赖，不应污染场景图。

use serde::{Deserialize, Serialize};

use super::geom::Insets;
use super::paint::Color;

/// 一个文本框。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TextBox {
    pub body: BodyProps,
    pub paragraphs: Vec<Paragraph>,
}

impl TextBox {
    /// 纯文本内容（用于降级渲染、文本提取与快照测试）。
    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        for (i, p) in self.paragraphs.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            for r in &p.runs {
                out.push_str(&r.text);
            }
        }
        out
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.paragraphs
            .iter()
            .all(|p| p.runs.iter().all(|r| r.text.is_empty()))
    }
}

/// 文本框（`a:bodyPr`）的布局属性。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BodyProps {
    pub anchor: VerticalAnchor,
    /// `anchorCtr="1"`：内容在锚定方向之外还做水平居中。
    pub anchor_center: bool,
    pub wrap: TextWrap,
    pub insets: Insets,
    pub auto_fit: AutoFit,
    pub direction: TextDirection,
    pub columns: Vec<TextColumn>,
    /// 文本整体旋转角度（`rot` 属性，度）。
    pub rotation_deg: Option<f32>,
}

impl Default for BodyProps {
    fn default() -> Self {
        BodyProps {
            anchor: VerticalAnchor::Top,
            anchor_center: false,
            wrap: TextWrap::Square,
            // OOXML 默认内边距：左右 0.1 英寸 = 7.2pt，上下 0.05 英寸 = 3.6pt
            insets: Insets {
                left: 7.2,
                top: 3.6,
                right: 7.2,
                bottom: 3.6,
            },
            auto_fit: AutoFit::NoAutofit,
            direction: TextDirection::Horizontal,
            columns: Vec::new(),
            rotation_deg: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum VerticalAnchor {
    #[default]
    Top,
    Middle,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TextWrap {
    /// 自动换行（默认）。
    #[default]
    Square,
    /// `wrap="none"`：不换行，可溢出形状。
    None,
}

/// 自动缩放策略。
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub enum AutoFit {
    /// `noAutofit`：文本可能溢出形状。
    #[default]
    NoAutofit,
    /// `normAutofit`：整体缩放字号以适应形状。
    NormAutofit {
        /// 字体缩放比例（1.0 表示不缩放）。
        font_scale: f32,
        /// 行距缩减比例（0.0 表示不缩减）。
        line_space_reduction: f32,
    },
    /// `spAutoFit`：形状高度适应文本。
    SpAutoFit,
}

impl AutoFit {
    /// 是否需要排版引擎参与二分求解。
    ///
    /// 注意 `normAutofit` 在 OOXML 中通常已由 PowerPoint 预先算好 `fontScale`，
    /// 只有当缺少该属性时才需要我们自己求解。
    #[inline]
    pub fn needs_solving(&self) -> bool {
        matches!(
            self,
            AutoFit::NormAutofit {
                font_scale,
                ..
            } if (*font_scale - 1.0).abs() < f32::EPSILON
        )
    }

    #[inline]
    pub fn font_scale(&self) -> f32 {
        match self {
            AutoFit::NormAutofit { font_scale, .. } => *font_scale,
            _ => 1.0,
        }
    }

    #[inline]
    pub fn line_space_reduction(&self) -> f32 {
        match self {
            AutoFit::NormAutofit {
                line_space_reduction,
                ..
            } => *line_space_reduction,
            _ => 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TextDirection {
    #[default]
    Horizontal,
    /// `vert="vert"`：自上而下，字形整体旋转 90°。
    Rotate90,
    /// `vert="vert270"`：自下而上。
    Rotate270,
    /// `vert="eaVert"`：东亚竖排，字形不旋转。
    Stacked,
    /// `vert="wordArtVertRtl"`。
    StackedRightToLeft,
}

/// 多栏文本的一栏。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TextColumn {
    pub width_pt: f32,
    pub spacing_pt: f32,
}

/// 段落。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Paragraph {
    pub runs: Vec<TextRun>,
    pub align: TextAlign,
    /// 动画构建步：整段在第几步出现 / 消失。
    ///
    /// 放在段落上而不是形状上，是为了「项目符号逐条弹出」——
    /// 课件里最常见的动画形态。未出现的段落**照样参与排版占位**，
    /// 只是不画：否则后面的段落会往上窜，与 PowerPoint 的表现不符。
    pub build: super::Build,
    /// 缩进层级（0 起）。
    pub level: u8,
    pub bullet: Option<Bullet>,
    pub line_spacing: LineSpacing,
    pub space_before_pt: f32,
    pub space_after_pt: f32,
    /// 首行缩进（正值悬挂，负值首行缩进），来自 `a:pPr/@indent`。
    pub indent_pt: f32,
    /// 段左边距，来自 `a:pPr/@marL`。
    pub margin_left_pt: f32,
    /// 段落默认字符属性（`a:defRPr` / `a:endParaRPr`）。
    pub default_run: RunProps,
    /// 从右到左书写。
    pub rtl: bool,
    /// 东亚文字对齐方式。
    pub font_align: FontAlign,
    /// `eaLnBrk`：东亚文字换行规则。
    pub ea_line_break: bool,
    /// 制表位。
    pub tab_stops: Vec<TabStop>,
}

impl Paragraph {
    /// 段落纯文本。
    pub fn text(&self) -> String {
        let mut s = String::new();
        for r in &self.runs {
            s.push_str(&r.text);
        }
        s
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.runs.iter().all(|r| r.text.is_empty())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TextAlign {
    #[default]
    Left,
    Center,
    Right,
    /// 两端对齐（最后一行左对齐）。
    Justify,
    /// 两端对齐（含最后一行）。
    JustifyAll,
    Distributed,
    ThaiDistributed,
}

/// 东亚文字对齐方式（`a:pPr/@fontAlgn`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum FontAlign {
    #[default]
    Auto,
    Top,
    Center,
    Baseline,
    Bottom,
}

/// 行距。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum LineSpacing {
    /// 百分比（1.0 = 单倍行距）。
    Percent(f32),
    /// 固定磅值。
    Points(f32),
}

impl Default for LineSpacing {
    fn default() -> Self {
        // OOXML 未指定时默认单倍行距
        LineSpacing::Percent(1.0)
    }
}

/// 制表位。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TabStop {
    pub position_pt: f32,
    pub align: TabAlign,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TabAlign {
    #[default]
    Left,
    Center,
    Right,
    Decimal,
}

/// 项目符号。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bullet {
    pub kind: BulletKind,
    pub color: Option<Color>,
    /// 符号相对字号的百分比（1.0 = 100%）。
    pub size_pct: Option<f32>,
    /// 符号所用字体（如 `Wingdings`）。
    pub font: Option<String>,
    /// 悬挂缩进的左边距（`marL`，pt）。
    pub margin_left_pt: f32,
    /// 悬挂量（`indent`，pt）。
    pub hanging_pt: f32,
    /// 自动编号的起始值。
    pub start_at: Option<u32>,
    /// 自动编号样式，如 `arabicPeriod`、`cjkIdeographPeriod`。
    pub number_type: Option<String>,
}

impl Bullet {
    /// 无符号（`a:buNone`）。
    pub fn none() -> Bullet {
        Bullet {
            kind: BulletKind::None,
            color: None,
            size_pct: None,
            font: None,
            margin_left_pt: 0.0,
            hanging_pt: 0.0,
            start_at: None,
            number_type: None,
        }
    }

    #[inline]
    pub fn is_visible(&self) -> bool {
        !matches!(self.kind, BulletKind::None)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BulletKind {
    None,
    /// `a:buChar`：指定字符。
    Char(String),
    /// `a:buAutoNum`：自动编号。
    AutoNumber,
}

/// 一个文本片段（连续同格式文本）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextRun {
    pub text: String,
    pub props: RunProps,
    pub hyperlink: Option<Hyperlink>,
    /// 域字段（页码、日期等）。
    pub field: Option<FieldKind>,
}

impl TextRun {
    pub fn new(text: impl Into<String>) -> TextRun {
        TextRun {
            text: text.into(),
            props: RunProps::default(),
            hyperlink: None,
            field: None,
        }
    }
}

/// 字符属性。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunProps {
    pub font: FontSet,
    /// 字号（pt）。默认 18pt（OOXML 默认 1800）。
    pub size_pt: f32,
    pub bold: bool,
    pub italic: bool,
    pub underline: UnderlineStyle,
    pub strike: StrikeStyle,
    pub color: Option<Color>,
    pub highlight: Option<Color>,
    /// 字间距（pt，对应 `spc` 的 1/100 pt）。
    pub spacing_pt: f32,
    /// 基线偏移百分比（正值为上标，负值为下标）。
    pub baseline_pct: f32,
    pub caps: TextCaps,
    /// 语言标签，影响断词与断行规则。
    pub language: Option<String>,
}

impl Default for RunProps {
    fn default() -> Self {
        RunProps {
            font: FontSet::default(),
            size_pt: 18.0,
            bold: false,
            italic: false,
            underline: UnderlineStyle::None,
            strike: StrikeStyle::None,
            color: None,
            highlight: None,
            spacing_pt: 0.0,
            baseline_pct: 0.0,
            caps: TextCaps::None,
            language: None,
        }
    }
}

impl RunProps {
    /// 是否含有需要特殊处理的装饰（渲染器据此决定是否走复杂路径）。
    #[inline]
    pub fn has_decoration(&self) -> bool {
        self.underline != UnderlineStyle::None || self.strike != StrikeStyle::None
    }
}

/// 字体集合。OOXML 允许为拉丁文 / 东亚 / 复杂文种分别指定字体。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct FontSet {
    pub latin: Option<String>,
    /// 东亚字体（中文）。
    pub ea: Option<String>,
    /// 复杂文种字体。
    pub cs: Option<String>,
    /// 符号字体（`a:sym`）。
    pub symbol: Option<String>,
}

impl FontSet {
    pub fn from_latin(name: impl Into<String>) -> FontSet {
        FontSet {
            latin: Some(name.into()),
            ..Default::default()
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.latin.is_none() && self.ea.is_none() && self.cs.is_none() && self.symbol.is_none()
    }

    /// 合并：`self` 中缺失的字段用 `other` 补齐。
    pub fn or(&self, other: &FontSet) -> FontSet {
        FontSet {
            latin: self.latin.clone().or_else(|| other.latin.clone()),
            ea: self.ea.clone().or_else(|| other.ea.clone()),
            cs: self.cs.clone().or_else(|| other.cs.clone()),
            symbol: self.symbol.clone().or_else(|| other.symbol.clone()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum UnderlineStyle {
    #[default]
    None,
    Single,
    Double,
    Heavy,
    Dotted,
    DottedHeavy,
    Dash,
    DashHeavy,
    DashLong,
    DashLongHeavy,
    DotDash,
    DotDashHeavy,
    DotDotDash,
    DotDotDashHeavy,
    Wavy,
    WavyHeavy,
    WavyDouble,
}

impl UnderlineStyle {
    /// 是否需要双线绘制。
    #[inline]
    pub fn is_double(self) -> bool {
        matches!(self, UnderlineStyle::Double | UnderlineStyle::WavyDouble)
    }

    /// 是否为波浪线。
    #[inline]
    pub fn is_wavy(self) -> bool {
        matches!(
            self,
            UnderlineStyle::Wavy | UnderlineStyle::WavyHeavy | UnderlineStyle::WavyDouble
        )
    }

    /// 是否为粗线。
    #[inline]
    pub fn is_heavy(self) -> bool {
        matches!(
            self,
            UnderlineStyle::Heavy
                | UnderlineStyle::DottedHeavy
                | UnderlineStyle::DashHeavy
                | UnderlineStyle::DashLongHeavy
                | UnderlineStyle::DotDashHeavy
                | UnderlineStyle::DotDotDashHeavy
                | UnderlineStyle::WavyHeavy
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum StrikeStyle {
    #[default]
    None,
    Single,
    Double,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TextCaps {
    #[default]
    None,
    Small,
    All,
}

/// 超链接。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hyperlink {
    pub target: HyperlinkTarget,
    pub tooltip: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HyperlinkTarget {
    Url(String),
    /// 跳转到指定幻灯片（0 起索引）。
    Slide(usize),
    NextSlide,
    PreviousSlide,
    FirstSlide,
    LastSlide,
    EndShow,
    OtherFile(String),
}

/// 域字段类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FieldKind {
    SlideNumber,
    Date,
    Time,
    DateTime,
    Footer,
    Header,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_body_insets_match_ooxml() {
        let b = BodyProps::default();
        assert!((b.insets.left - 7.2).abs() < 1e-4);
        assert!((b.insets.top - 3.6).abs() < 1e-4);
    }

    #[test]
    fn textbox_plain_text_joins_paragraphs_with_newline() {
        let tb = TextBox {
            body: BodyProps::default(),
            paragraphs: vec![
                Paragraph {
                    runs: vec![TextRun::new("第一段")],
                    ..Default::default()
                },
                Paragraph {
                    runs: vec![TextRun::new("第二段")],
                    ..Default::default()
                },
            ],
        };
        assert_eq!(tb.plain_text(), "第一段\n第二段");
    }

    #[test]
    fn textbox_empty_detection() {
        let tb = TextBox {
            body: BodyProps::default(),
            paragraphs: vec![Paragraph::default()],
        };
        assert!(tb.is_empty());
        let tb2 = TextBox {
            body: BodyProps::default(),
            paragraphs: vec![Paragraph {
                runs: vec![TextRun::new("x")],
                ..Default::default()
            }],
        };
        assert!(!tb2.is_empty());
    }

    #[test]
    fn autofit_defaults() {
        assert_eq!(AutoFit::default(), AutoFit::NoAutofit);
        assert_eq!(AutoFit::NoAutofit.font_scale(), 1.0);
        assert_eq!(
            AutoFit::NormAutofit {
                font_scale: 0.8,
                line_space_reduction: 0.1
            }
            .font_scale(),
            0.8
        );
    }

    #[test]
    fn autofit_needs_solving_only_when_unscaled() {
        assert!(!AutoFit::NoAutofit.needs_solving());
        assert!(AutoFit::NormAutofit {
            font_scale: 1.0,
            line_space_reduction: 0.0
        }
        .needs_solving());
        assert!(!AutoFit::NormAutofit {
            font_scale: 0.75,
            line_space_reduction: 0.0
        }
        .needs_solving());
    }

    #[test]
    fn fontset_merge_prefers_self() {
        let a = FontSet::from_latin("Arial");
        let b = FontSet {
            latin: Some("宋体".into()),
            ea: Some("微软雅黑".into()),
            ..Default::default()
        };
        let m = a.or(&b);
        assert_eq!(m.latin.as_deref(), Some("Arial"));
        assert_eq!(m.ea.as_deref(), Some("微软雅黑"));
    }

    #[test]
    fn fontset_empty_detection() {
        assert!(FontSet::default().is_empty());
        assert!(!FontSet::from_latin("Arial").is_empty());
    }

    #[test]
    fn underline_classification() {
        assert!(!UnderlineStyle::Single.is_double());
        assert!(UnderlineStyle::Double.is_double());
        assert!(UnderlineStyle::WavyDouble.is_double());
        assert!(UnderlineStyle::Wavy.is_wavy());
        assert!(UnderlineStyle::DashHeavy.is_heavy());
        assert!(!UnderlineStyle::Dash.is_heavy());
    }

    #[test]
    fn bullet_none_is_invisible() {
        assert!(!Bullet::none().is_visible());
        let b = Bullet {
            kind: BulletKind::Char("•".into()),
            color: None,
            size_pct: None,
            font: None,
            margin_left_pt: 18.0,
            hanging_pt: -18.0,
            start_at: None,
            number_type: None,
        };
        assert!(b.is_visible());
    }

    #[test]
    fn runprops_default_font_size_is_18pt() {
        assert!((RunProps::default().size_pt - 18.0).abs() < 1e-4);
    }

    #[test]
    fn paragraph_text_concatenates_runs() {
        let p = Paragraph {
            runs: vec![TextRun::new("Hello"), TextRun::new(" "), TextRun::new("世界")],
            ..Default::default()
        };
        assert_eq!(p.text(), "Hello 世界");
    }

    #[test]
    fn default_line_spacing_is_single() {
        assert_eq!(LineSpacing::default(), LineSpacing::Percent(1.0));
    }
}
