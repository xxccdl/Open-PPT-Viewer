//! 文本解析：`a:txBody` → [`TextBox`]。
//!
//! # 属性继承链
//!
//! OOXML 的文本属性来自四个层级，优先级从低到高：
//!
//! 1. 演示文稿默认（`ppt/presentation.xml` 的 `p:defaultTextStyle`）
//! 2. 版式/母版的 `p:txStyles`（`titleStyle` / `bodyStyle` / `otherStyle`）
//! 3. 形状自身的 `a:lstStyle`（按大纲级别 `lvl1pPr`…`lvl9pPr`）
//! 4. 段落 `a:pPr` 与 `a:pPr/a:defRPr`
//! 5. 文本块 `a:r/a:rPr`
//!
//! 本模块只负责第 3~5 层；第 1~2 层由调用方通过 [`TextStyles`]
//! 作为「已继承的默认值」传入。
//!
//! 这样切分的理由：第 1~2 层是**每份课件解析一次**的全局信息，
//! 而第 3~5 层是每页每形状都要跑的，把两者分开可以让热路径只处理局部数据。

use ppt_core::scene::{
    AutoFit, BodyProps, Build, Bullet, BulletKind, Color, FieldKind, FontAlign, FontSet, Hyperlink,
    HyperlinkTarget, Insets, LineSpacing, Paragraph, RunProps, StrikeStyle, TabAlign, TabStop,
    TextAlign, TextBox, TextCaps, TextDirection, TextRun, UnderlineStyle, VerticalAnchor,
};
use ppt_core::units;
use ppt_core::XmlNode;

use crate::color::{self, ColorScheme};
use crate::paint::RelResolver;

/// 大纲级别的最大数量（`lvl1`…`lvl9`）。
pub const MAX_LEVELS: usize = 9;

/// 某一级别的默认文本样式。
#[derive(Debug, Clone, Default)]
pub struct LevelStyle {
    pub run: RunProps,
    pub bullet: Option<Bullet>,
    pub align: Option<TextAlign>,
    pub line_spacing: Option<LineSpacing>,
    pub space_before_pt: Option<f32>,
    pub space_after_pt: Option<f32>,
    pub margin_left_pt: Option<f32>,
    pub indent_pt: Option<f32>,
    pub tab_stops: Vec<TabStop>,
    /// 该级是否显式声明了「无项目符号」。
    pub bullet_disabled: bool,
}

/// 一组按级别索引的文本样式（来自 `a:lstStyle` 或母版的 `p:txStyles`）。
#[derive(Debug, Clone, Default)]
pub struct TextStyles {
    pub levels: Vec<LevelStyle>,
}

impl TextStyles {
    /// 解析 `a:lstStyle`。
    ///
    /// `a:lstStyle` 的子元素是 `a:lvl1pPr`…`a:lvl9pPr`（1 起计数），
    /// 内部结构与 `a:pPr` 相同。
    ///
    /// `seed` 是从版式/母版继承来的字符默认值。之所以在这里就播种进去，
    /// 是因为 [`RunProps`] 的字段是非 `Option` 的具体值 ——
    /// 事后无法判断「某级到底有没有指定字号」，只能一开始就以继承值为基线，
    /// 让 `lstStyle` 中出现的属性逐项覆盖它。
    pub fn parse(
        lst_style: Option<&XmlNode>,
        scheme: Option<&ColorScheme>,
        seed: &RunProps,
    ) -> TextStyles {
        let base = TextStyles {
            levels: vec![
                LevelStyle {
                    run: seed.clone(),
                    ..LevelStyle::default()
                };
                MAX_LEVELS
            ],
        };
        base.overlay(lst_style, scheme)
    }

    /// 在已有样式之上叠加一层 `a:lstStyle`。
    ///
    /// # 为什么必须叠加而不是替换
    ///
    /// OOXML 的文本样式是分层继承的：
    ///
    /// ```text
    /// 母版 p:txStyles（角色级基线：标题 40pt 粗体、正文带项目符号）
    ///   └─ 版式占位符的 a:lstStyle（可能只改了行距）
    ///        └─ 形状自身的 a:lstStyle（可能只是个空壳 <a:lstStyle/>）
    /// ```
    ///
    /// 现实中大量课件（尤其是 WPS / 第三方工具导出的）会在 `p:txBody` 里
    /// 放一个**空的** `a:lstStyle`。若把它当作「用自身替换基线」，
    /// 母版定义的字号、颜色、项目符号会全部丢失，标题退化成 18pt 黑体左对齐。
    /// 因此这里按级别逐项覆盖，未声明的属性沿用下层。
    pub fn overlay(
        &self,
        lst_style: Option<&XmlNode>,
        scheme: Option<&ColorScheme>,
    ) -> TextStyles {
        let Some(lst) = lst_style else {
            return self.clone();
        };

        let mut out = if self.levels.is_empty() {
            TextStyles {
                levels: vec![LevelStyle::default(); MAX_LEVELS],
            }
        } else {
            self.clone()
        };

        // `a:defPPr` 是整份列表的默认值，落到所有级别上。
        // 必须逐级计算：`parse_paragraph_props` 以该级现有属性为基线，
        // 若统一用第 1 级做基线会把各级的差异抹平。
        if let Some(def) = lst.child("defPPr") {
            for lvl in out.levels.iter_mut() {
                let base = parse_paragraph_props(def, scheme, &lvl.run);
                apply_level(&base, lvl);
            }
        }

        for child in &lst.children {
            let Some(idx) = level_index(&child.name) else {
                continue;
            };
            if idx >= out.levels.len() {
                continue;
            }
            let inherited = out.levels[idx].run.clone();
            let parsed = parse_paragraph_props(child, scheme, &inherited);
            apply_level(&parsed, &mut out.levels[idx]);
        }

        out
    }

    /// 取指定级别的样式；越界时返回最后一级，避免调用方到处判空。
    pub fn level(&self, lvl: usize) -> &LevelStyle {
        if self.levels.is_empty() {
            return &EMPTY_LEVEL;
        }
        let idx = lvl.min(self.levels.len().saturating_sub(1));
        &self.levels[idx]
    }

    /// 该级别生效的字符默认值。
    ///
    /// `inherited` 在「本形状完全没有 `a:lstStyle`」时作为兜底 ——
    /// 此时 `styles` 是 [`TextStyles::default`]，没有任何级别信息。
    pub fn effective_run(&self, lvl: usize, inherited: &RunProps) -> RunProps {
        if self.levels.is_empty() {
            inherited.clone()
        } else {
            self.level(lvl).run.clone()
        }
    }
}

static EMPTY_LEVEL: LevelStyle = LevelStyle {
    run: RunProps {
        font: FontSet {
            latin: None,
            ea: None,
            cs: None,
            symbol: None,
        },
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
    },
    bullet: None,
    align: None,
    line_spacing: None,
    space_before_pt: None,
    space_after_pt: None,
    margin_left_pt: None,
    indent_pt: None,
    tab_stops: Vec::new(),
    bullet_disabled: false,
};

/// `lvl1pPr`…`lvl9pPr` → 0 起的索引。
fn level_index(name: &str) -> Option<usize> {
    let rest = name.strip_prefix("lvl")?;
    let digits = rest.strip_suffix("pPr")?;
    digits.parse::<usize>().ok().map(|n| n.saturating_sub(1))
}

/// 解析一次「段落属性」元素（`a:pPr` 或 `a:lvlNpPr` 结构相同）。
///
/// 返回带 `Option` 的部分结果，便于按层合并。
#[derive(Debug, Clone, Default)]
struct PartialParaProps {
    run: RunProps,
    align: Option<TextAlign>,
    line_spacing: Option<LineSpacing>,
    space_before_pt: Option<f32>,
    space_after_pt: Option<f32>,
    margin_left_pt: Option<f32>,
    indent_pt: Option<f32>,
    bullet: Option<Bullet>,
    bullet_disabled: bool,
    tab_stops: Vec<TabStop>,
    rtl: Option<bool>,
    font_align: Option<FontAlign>,
    ea_line_break: Option<bool>,
}

fn parse_paragraph_props(
    node: &XmlNode,
    scheme: Option<&ColorScheme>,
    inherited_run: &RunProps,
) -> PartialParaProps {
    let mut out = PartialParaProps {
        run: inherited_run.clone(),
        ..Default::default()
    };

    out.align = node.attr("algn").and_then(parse_text_align);
    out.margin_left_pt = node.attr_f64("marL").map(units::emu_to_pt);
    // `indent` 为负值表示悬挂缩进；OOXML 用它表示「首行相对 marL 的偏移」
    out.indent_pt = node.attr_f64("indent").map(units::emu_to_pt);
    out.rtl = node.attr_bool("rtl");
    out.font_align = node.attr("fontAlgn").and_then(parse_font_align);
    out.ea_line_break = node.attr_bool("eaLnBrk");

    if let Some(ls) = node.child("lnSpc") {
        out.line_spacing = parse_spacing(ls);
    }
    if let Some(sp) = node.child("spcBef") {
        out.space_before_pt = parse_spacing_point(sp);
    }
    if let Some(sp) = node.child("spcAft") {
        out.space_after_pt = parse_spacing_point(sp);
    }

    // 项目符号：`buNone` / `buChar` / `buAutoNum` 三选一
    if node.has_child("buNone") {
        out.bullet_disabled = true;
    } else if let Some(bc) = node.child("buChar") {
        out.bullet = Some(Bullet {
            kind: BulletKind::Char(bc.attr("char").unwrap_or("•").to_string()),
            color: None,
            size_pct: None,
            font: None,
            margin_left_pt: out.margin_left_pt.unwrap_or(0.0),
            hanging_pt: out.indent_pt.unwrap_or(0.0),
            start_at: None,
            number_type: None,
        });
    } else if let Some(an) = node.child("buAutoNum") {
        out.bullet = Some(Bullet {
            kind: BulletKind::AutoNumber,
            color: None,
            size_pct: None,
            font: None,
            margin_left_pt: out.margin_left_pt.unwrap_or(0.0),
            hanging_pt: out.indent_pt.unwrap_or(0.0),
            start_at: an.attr_u32("startAt"),
            number_type: an.attr("type").map(|s| s.to_string()),
        });
    }

    // 项目符号的附加属性只在其存在时生效
    if let Some(b) = out.bullet.as_mut() {
        if let Some(c) = node
            .child("buClr")
            .and_then(|n| color::parse_solid_fill(n, scheme))
        {
            b.color = Some(c);
        }
        if let Some(sz) = node.child("buSzPct").and_then(|n| n.attr_f64("val")) {
            b.size_pct = Some((sz / 100_000.0) as f32);
        }
        if let Some(f) = node.child("buFont").and_then(|n| n.attr("typeface")) {
            b.font = Some(f.to_string());
        }
    }

    if let Some(tabs) = node.child("tabLst") {
        for t in tabs.children_named("tab") {
            if let Some(pos) = t.attr_f64("pos") {
                out.tab_stops.push(TabStop {
                    position_pt: units::emu_to_pt(pos),
                    align: match t.attr("algn") {
                        Some("ctr") => TabAlign::Center,
                        Some("r") => TabAlign::Right,
                        Some("dec") => TabAlign::Decimal,
                        _ => TabAlign::Left,
                    },
                });
            }
        }
    }

    // 段落默认字符属性
    if let Some(def) = node.child("defRPr") {
        out.run = parse_run_props(Some(def), &out.run, scheme);
    }

    out
}

fn apply_level(p: &PartialParaProps, lvl: &mut LevelStyle) {
    lvl.run = p.run.clone();
    if p.align.is_some() {
        lvl.align = p.align;
    }
    if p.line_spacing.is_some() {
        lvl.line_spacing = p.line_spacing;
    }
    if p.space_before_pt.is_some() {
        lvl.space_before_pt = p.space_before_pt;
    }
    if p.space_after_pt.is_some() {
        lvl.space_after_pt = p.space_after_pt;
    }
    if p.margin_left_pt.is_some() {
        lvl.margin_left_pt = p.margin_left_pt;
    }
    if p.indent_pt.is_some() {
        lvl.indent_pt = p.indent_pt;
    }
    if p.bullet.is_some() {
        lvl.bullet = p.bullet.clone();
        lvl.bullet_disabled = false;
    } else if p.bullet_disabled {
        lvl.bullet = None;
        lvl.bullet_disabled = true;
    }
    if !p.tab_stops.is_empty() {
        lvl.tab_stops = p.tab_stops.clone();
    }
}

fn parse_text_align(v: &str) -> Option<TextAlign> {
    Some(match v {
        "l" => TextAlign::Left,
        "ctr" => TextAlign::Center,
        "r" => TextAlign::Right,
        "just" => TextAlign::Justify,
        "justLow" => TextAlign::JustifyAll,
        "dist" => TextAlign::Distributed,
        "thaiDist" => TextAlign::ThaiDistributed,
        _ => return None,
    })
}

fn parse_font_align(v: &str) -> Option<FontAlign> {
    Some(match v {
        "auto" => FontAlign::Auto,
        "t" => FontAlign::Top,
        "ctr" => FontAlign::Center,
        "base" => FontAlign::Baseline,
        "b" => FontAlign::Bottom,
        _ => return None,
    })
}

/// 解析 `a:lnSpc`。
fn parse_spacing(node: &XmlNode) -> Option<LineSpacing> {
    if let Some(pct) = node.child("spcPct") {
        let raw = pct.attr("val")?;
        // OOXML 允许 "150%" 与 "150000" 两种写法，先按字符串判断再解析数字，
        // 否则 f64 解析会在带 % 时直接失败
        let ratio = if let Some(s) = raw.trim().strip_suffix('%') {
            s.trim().parse::<f64>().ok()? / 100.0
        } else {
            raw.trim().parse::<f64>().ok()? / 100_000.0
        };
        return Some(LineSpacing::Percent(ratio as f32));
    }
    if let Some(pts) = node.child("spcPts") {
        let v = pts.attr_f64("val")?;
        return Some(LineSpacing::Points(units::centipoint_to_pt(v)));
    }
    None
}

/// 解析 `a:spcBef` / `a:spcAft`。
///
/// # 只认点数形式（`spcPts`），百分比形式（`spcPct`）**故意忽略**
///
/// 看着像漏了，其实是实测定出来的：拿 `spAutoFit` 的框当标尺
/// （框里写的高度就是 PowerPoint 排版的实际高度），
/// 真实课件里 `spcBef><a:spcPct val="50000"` 的「to practise」单行框，
/// PowerPoint 写下的高度是 36.25pt = 正文 29.05pt + 内边距 7.2pt，
/// 也就是**一行 1.21em、一点额外间距都没加**。
/// 24pt 的 50% 本该多出 12pt，真加上去我们反而会比 PowerPoint 高出一大截。
///
/// 百分比形式在课件里非常普遍（某份课件里 33 处 spcBef、128 处 spcAft），
/// 但绝大多数是 `0`。真按百分比加间距的写法，至少在 PowerPoint 自己的
/// 高度里看不出来 —— 与其猜一个会撑破版面的解释，不如照实际渲染结果来。
fn parse_spacing_point(node: &XmlNode) -> Option<f32> {
    if let Some(pts) = node.child("spcPts") {
        return Some(units::centipoint_to_pt(pts.attr_f64("val")?));
    }
    None
}

/// 解析 `a:bodyPr`。
pub fn parse_body_props(body_pr: Option<&XmlNode>) -> BodyProps {
    let Some(bp) = body_pr else {
        return BodyProps::default();
    };

    let mut out = BodyProps::default();

    out.anchor = match bp.attr("anchor") {
        Some("ctr") => VerticalAnchor::Middle,
        Some("b") => VerticalAnchor::Bottom,
        _ => VerticalAnchor::Top,
    };
    out.anchor_center = bp.attr_bool_or("anchorCtr", false);
    out.wrap = match bp.attr("wrap") {
        Some("none") => ppt_core::scene::TextWrap::None,
        _ => ppt_core::scene::TextWrap::Square,
    };

    // 内边距：EMU，缺省时保留 OOXML 默认值（左右 0.1"、上下 0.05"）
    let mut insets = out.insets;
    if let Some(v) = bp.attr_f64("lIns") {
        insets.left = units::emu_to_pt(v);
    }
    if let Some(v) = bp.attr_f64("tIns") {
        insets.top = units::emu_to_pt(v);
    }
    if let Some(v) = bp.attr_f64("rIns") {
        insets.right = units::emu_to_pt(v);
    }
    if let Some(v) = bp.attr_f64("bIns") {
        insets.bottom = units::emu_to_pt(v);
    }
    out.insets = insets;

    if let Some(nf) = bp.child("normAutofit") {
        out.auto_fit = AutoFit::NormAutofit {
            font_scale: nf
                .attr_f64("fontScale")
                .map(|v| (v / 100_000.0) as f32)
                .unwrap_or(1.0),
            line_space_reduction: nf
                .attr_f64("lnSpcReduction")
                .map(|v| (v / 100_000.0) as f32)
                .unwrap_or(0.0),
        };
    } else if bp.has_child("spAutoFit") {
        out.auto_fit = AutoFit::SpAutoFit;
    } else if bp.has_child("noAutofit") {
        out.auto_fit = AutoFit::NoAutofit;
    }

    out.direction = match bp.attr("vert") {
        Some("vert") => TextDirection::Rotate90,
        Some("vert270") => TextDirection::Rotate270,
        Some("eaVert") => TextDirection::Stacked,
        Some("wordArtVertRtl") => TextDirection::StackedRightToLeft,
        _ => TextDirection::Horizontal,
    };

    if let Some(rot) = bp.attr_f64("rot") {
        out.rotation_deg = Some(units::ooxml_angle_to_deg(rot));
    }

    // 多栏：`a:numCol` 给出栏数，`a:spcCol` 给出间距
    if let Some(nc) = bp.attr_u32("numCol") {
        if nc > 1 {
            let spc = units::emu_to_pt(bp.attr_f64("spcCol").unwrap_or(0.0));
            // 栏宽需由总宽度均分，这里先记录栏数与间距，宽度置 0 由排版层补齐
            out.columns = (0..nc)
                .map(|_| ppt_core::scene::TextColumn {
                    width_pt: 0.0,
                    spacing_pt: spc,
                })
                .collect();
        }
    }

    out
}

/// 解析 `a:rPr`，未指定的字段沿用 `inherited`。
///
/// 之所以采用「传入继承值 + 只覆盖已指定字段」，是因为 [`RunProps`]
/// 的字段是非 `Option` 的具体值，无法区分「未设置」与「显式设为默认」。
pub fn parse_run_props(
    rpr: Option<&XmlNode>,
    inherited: &RunProps,
    scheme: Option<&ColorScheme>,
) -> RunProps {
    let mut out = inherited.clone();
    let Some(rpr) = rpr else {
        return out;
    };

    if let Some(sz) = rpr.attr_f64("sz") {
        out.size_pt = units::centipoint_to_pt(sz);
    }
    if let Some(b) = rpr.attr_bool("b") {
        out.bold = b;
    }
    if let Some(i) = rpr.attr_bool("i") {
        out.italic = i;
    }
    if let Some(u) = rpr.attr("u") {
        out.underline = parse_underline(u);
    }
    if let Some(s) = rpr.attr("strike") {
        out.strike = match s {
            "sngStrike" => StrikeStyle::Single,
            "dblStrike" => StrikeStyle::Double,
            _ => StrikeStyle::None,
        };
    }
    if let Some(spc) = rpr.attr_f64("spc") {
        out.spacing_pt = units::centipoint_to_pt(spc);
    }
    if let Some(bl) = rpr.attr_f64("baseline") {
        // 单位是 1/1000 百分比：30000 = 30%
        out.baseline_pct = (bl / 100_000.0) as f32;
    }
    if let Some(cap) = rpr.attr("cap") {
        out.caps = match cap {
            "small" => TextCaps::Small,
            "all" => TextCaps::All,
            _ => TextCaps::None,
        };
    }
    if let Some(lang) = rpr.attr("lang") {
        out.language = Some(lang.to_string());
    }

    // 颜色：`a:solidFill` 覆盖继承值；`a:noFill` 表示不绘制（保留原值以免文字消失）
    if let Some(sf) = rpr.child("solidFill") {
        if let Some(c) = color::parse_solid_fill(sf, scheme) {
            out.color = Some(c);
        }
    }
    if let Some(hl) = rpr.child("highlight") {
        out.highlight = color::parse_solid_fill(hl, scheme);
    }

    // 字体：四个文种各自独立覆盖
    if let Some(t) = rpr.child("latin").and_then(|n| n.attr("typeface")) {
        out.font.latin = Some(t.to_string());
    }
    if let Some(t) = rpr.child("ea").and_then(|n| n.attr("typeface")) {
        out.font.ea = Some(t.to_string());
    }
    if let Some(t) = rpr.child("cs").and_then(|n| n.attr("typeface")) {
        out.font.cs = Some(t.to_string());
    }
    if let Some(t) = rpr.child("sym").and_then(|n| n.attr("typeface")) {
        out.font.symbol = Some(t.to_string());
    }

    out
}

fn parse_underline(v: &str) -> UnderlineStyle {
    match v {
        "sng" => UnderlineStyle::Single,
        "dbl" => UnderlineStyle::Double,
        "heavy" => UnderlineStyle::Heavy,
        "dotted" => UnderlineStyle::Dotted,
        "dottedHeavy" => UnderlineStyle::DottedHeavy,
        "dash" => UnderlineStyle::Dash,
        "dashHeavy" => UnderlineStyle::DashHeavy,
        "dashLong" => UnderlineStyle::DashLong,
        "dashLongHeavy" => UnderlineStyle::DashLongHeavy,
        "dotDash" => UnderlineStyle::DotDash,
        "dotDashHeavy" => UnderlineStyle::DotDashHeavy,
        "dotDotDash" => UnderlineStyle::DotDotDash,
        "dotDotDashHeavy" => UnderlineStyle::DotDotDashHeavy,
        "wavy" => UnderlineStyle::Wavy,
        "wavyHeavy" => UnderlineStyle::WavyHeavy,
        "wavyDbl" => UnderlineStyle::WavyDouble,
        _ => UnderlineStyle::None,
    }
}

/// 链接解析上下文。
///
/// 把关系解析与幻灯片索引解析一起传入，因为 `ppaction://` 形式的
/// 内部跳转需要同时看关系类型与动作字符串。
pub struct LinkContext<'a> {
    pub resolve_rel: RelResolver<'a>,
}

/// 解析 `a:hlinkClick`。
pub fn parse_hyperlink(
    rpr: Option<&XmlNode>,
    ctx: &LinkContext<'_>,
) -> Option<Hyperlink> {
    let rpr = rpr?;
    let hl = rpr.child("hlinkClick")?;

    let tooltip = hl.attr("tooltip").map(|s| s.to_string());

    // `ppaction://...` 形式表示内部跳转
    if let Some(action) = hl.attr("action") {
        let lower = action.to_ascii_lowercase();
        let target = if lower.contains("hlinkshowjump") {
            // 形如 ppaction://hlinkshowjump?jump=nextslide
            let jump = action
                .split("jump=")
                .nth(1)
                .unwrap_or("")
                .to_ascii_lowercase();
            match jump.as_str() {
                "nextslide" => HyperlinkTarget::NextSlide,
                "previousslide" => HyperlinkTarget::PreviousSlide,
                "firstslide" => HyperlinkTarget::FirstSlide,
                "lastslide" => HyperlinkTarget::LastSlide,
                "endshow" => HyperlinkTarget::EndShow,
                _ => return None,
            }
        } else if lower.contains("hlinksldjump") {
            // 目标幻灯片由 `r:id` 指向的关系给出
            let part = hl.attr("id").and_then(|id| (ctx.resolve_rel)(id))?;
            let idx = slide_index_from_part(&part)?;
            HyperlinkTarget::Slide(idx)
        } else if lower.contains("hlinkfile") {
            HyperlinkTarget::OtherFile(action.to_string())
        } else {
            return None;
        };

        return Some(Hyperlink {
            target,
            tooltip,
        });
    }

    // 普通外部链接
    let part = hl.attr("id").and_then(|id| (ctx.resolve_rel)(id))?;
    Some(Hyperlink {
        target: HyperlinkTarget::Url(part),
        tooltip,
    })
}

/// 从 `ppt/slides/slideN.xml` 之类的部件名推断 0 起页索引。
fn slide_index_from_part(part: &str) -> Option<usize> {
    let name = part.rsplit('/').next()?;
    let digits: String = name.chars().filter(|c| c.is_ascii_digit()).collect();
    let n = digits.parse::<usize>().ok()?;
    Some(n.saturating_sub(1))
}

/// 解析 `a:txBody`。
///
/// `styles` 是本形状自身的 `a:lstStyle`，
/// `inherited_run` 是从版式/母版继承来的字符默认值（已合并过第 1~2 层）。
pub fn parse_text_box(
    tx_body: &XmlNode,
    styles: &TextStyles,
    inherited_run: &RunProps,
    scheme: Option<&ColorScheme>,
    link_ctx: &LinkContext<'_>,
) -> TextBox {
    let body = parse_body_props(tx_body.child("bodyPr"));

    let mut paragraphs = Vec::new();
    for p in tx_body.children_named("p") {
        paragraphs.push(parse_paragraph(
            p,
            styles,
            inherited_run,
            scheme,
            link_ctx,
        ));
    }

    TextBox { body, paragraphs }
}

/// 解析 `a:p`。
fn parse_paragraph(
    p: &XmlNode,
    styles: &TextStyles,
    inherited_run: &RunProps,
    scheme: Option<&ColorScheme>,
    link_ctx: &LinkContext<'_>,
) -> Paragraph {
    let p_pr = p.child("pPr");
    let lvl = p_pr.and_then(|n| n.attr_u32("lvl")).unwrap_or(0) as usize;
    let level = styles.level(lvl);

    // 合并顺序：版式/母版继承 → lstStyle 级别 → 段落 pPr → run rPr
    let base_run = styles.effective_run(lvl, inherited_run);
    let mut partial = parse_paragraph_props(
        p_pr.unwrap_or(&EMPTY_NODE),
        scheme,
        &base_run,
    );

    // lstStyle 提供段落级默认，pPr 已显式指定的部分优先
    if partial.align.is_none() {
        partial.align = level.align;
    }
    if partial.line_spacing.is_none() {
        partial.line_spacing = level.line_spacing;
    }
    if partial.space_before_pt.is_none() {
        partial.space_before_pt = level.space_before_pt;
    }
    if partial.space_after_pt.is_none() {
        partial.space_after_pt = level.space_after_pt;
    }
    if partial.margin_left_pt.is_none() {
        partial.margin_left_pt = level.margin_left_pt;
    }
    if partial.indent_pt.is_none() {
        partial.indent_pt = level.indent_pt;
    }
    if partial.bullet.is_none() && !partial.bullet_disabled && level.bullet.is_some() {
        // 沿用 lstStyle 的项目符号，但用本段的缩进值
        let mut b = level.bullet.clone().unwrap_or_else(Bullet::none);
        b.margin_left_pt = partial.margin_left_pt.unwrap_or(b.margin_left_pt);
        b.hanging_pt = partial.indent_pt.unwrap_or(b.hanging_pt);
        partial.bullet = Some(b);
    }
    if partial.bullet.is_none() && !partial.bullet_disabled {
        // 既没声明符号也没声明「无符号」：默认无符号。
        // 正文占位符的真正默认符号由母版的 `p:txStyles` 提供，
        // 已通过 `level.bullet` 合并到上一步。
        partial.bullet_disabled = true;
    }
    if partial.tab_stops.is_empty() {
        partial.tab_stops = level.tab_stops.clone();
    }

    let mut runs = Vec::new();
    for child in &p.children {
        match child.name.as_str() {
            "r" => {
                let rpr = parse_run_props(child.child("rPr"), &partial.run, scheme);
                runs.push(TextRun {
                    text: child
                        .child("t")
                        .map(|t| t.deep_text())
                        .unwrap_or_default(),
                    props: rpr,
                    hyperlink: parse_hyperlink(child.child("rPr"), link_ctx),
                    field: None,
                });
            }
            "br" => {
                // `a:br` 用换行符表示；渲染与排版层把 `\n` 当作强制断行
                let rpr = parse_run_props(child.child("rPr"), &partial.run, scheme);
                runs.push(TextRun {
                    text: "\n".to_string(),
                    props: rpr,
                    hyperlink: None,
                    field: None,
                });
            }
            "fld" => {
                let rpr = parse_run_props(child.child("rPr"), &partial.run, scheme);
                let field = child.attr("type").and_then(parse_field_kind);
                // `a:fld` 通常带一个 `a:t` 作为缓存文本（PowerPoint 预渲染的结果），
                // 有则直接用，无则留空由渲染层按类型生成
                let text = child
                    .child("t")
                    .map(|t| t.deep_text())
                    .unwrap_or_default();
                runs.push(TextRun {
                    text,
                    props: rpr,
                    hyperlink: parse_hyperlink(child.child("rPr"), link_ctx),
                    field,
                });
            }
            _ => {}
        }
    }

    // 末尾空 run（`a:endParaRPr`）不影响内容，但它的属性对空段落有意义
    let default_run = parse_run_props(p.child("endParaRPr"), &partial.run, scheme);

    Paragraph {
        runs,
        align: partial.align.unwrap_or_default(),
        // 动画步由 `timing` 模块在建好节点树之后写入
        build: Build::ALWAYS,
        level: lvl.min(u8::MAX as usize) as u8,
        bullet: if partial.bullet_disabled {
            None
        } else {
            partial.bullet
        },
        line_spacing: partial.line_spacing.unwrap_or_default(),
        space_before_pt: partial.space_before_pt.unwrap_or(0.0),
        space_after_pt: partial.space_after_pt.unwrap_or(0.0),
        indent_pt: partial.indent_pt.unwrap_or(0.0),
        margin_left_pt: partial.margin_left_pt.unwrap_or(0.0),
        default_run,
        rtl: partial.rtl.unwrap_or(false),
        font_align: partial.font_align.unwrap_or_default(),
        ea_line_break: partial.ea_line_break.unwrap_or(true),
        tab_stops: partial.tab_stops,
    }
}

fn parse_field_kind(v: &str) -> Option<FieldKind> {
    Some(match v {
        "slidenum" => FieldKind::SlideNumber,
        "datetimeFigureOut" | "datetime1" | "datetime2" | "datetime3" | "datetime4"
        | "datetime5" | "datetime6" | "datetime7" | "datetime8" | "datetime9"
        | "datetime10" | "datetime11" | "datetime12" | "datetime13" => FieldKind::DateTime,
        "datetime" => FieldKind::DateTime,
        "footer" => FieldKind::Footer,
        "header" => FieldKind::Header,
        _ => return None,
    })
}

/// 空节点占位，用于「`a:pPr` 缺失」的场景，避免大量 `Option` 分支。
static EMPTY_NODE: XmlNode = XmlNode {
    name: String::new(),
    attrs: Vec::new(),
    children: Vec::new(),
    text: String::new(),
};

/// 从 `a:txBody` 提取纯文本（用于降级渲染与文本提取）。
pub fn extract_plain_text(tx_body: &XmlNode) -> String {
    let mut out = String::new();
    for (i, p) in tx_body.children_named("p").enumerate() {
        if i > 0 {
            out.push('\n');
        }
        for r in p.children_named("r") {
            if let Some(t) = r.child("t") {
                out.push_str(&t.deep_text());
            }
        }
    }
    out
}

/// 构造一个「纯文本」文本框。
///
/// 用于降级路径：图表 / SmartArt / 嵌入对象里的文本被抽出来后，
/// 需要一个合法的 [`TextBox`] 才能交给排版与渲染。
/// 使用系统默认字号与自动换行，保证一定能显示出来。
pub fn make_plain_text_box(lines: &[String]) -> TextBox {
    let paragraphs = lines
        .iter()
        .map(|line| Paragraph {
            runs: vec![TextRun::new(line.clone())],
            ..Default::default()
        })
        .collect();
    TextBox {
        body: BodyProps {
            anchor: VerticalAnchor::Middle,
            ..Default::default()
        },
        paragraphs,
    }
}

/// 把文本框里的**主题字体引用**解析为具体字体名。
///
/// # 为什么必须在解析阶段做掉
///
/// OOXML 允许 `a:latin typeface="+mn-lt"` 这种「跟随主题」的写法，
/// 而母版的 `p:txStyles` 几乎全部用它。若把 `+mj-lt` 原样传给排版引擎，
/// 字体回退链会去查一个名叫「+mj-lt」的字体，结果只能走兜底，
/// 导致课件的字体层次完全丢失。
///
/// 排版引擎只认具体字体名，因此这一步是解析层的责任。
pub fn resolve_theme_fonts(tb: &mut TextBox, fonts: &crate::theme::FontScheme) {
    for para in &mut tb.paragraphs {
        resolve_font_set(&mut para.default_run.font, fonts);
        for run in &mut para.runs {
            resolve_font_set(&mut run.props.font, fonts);
        }
    }
}

fn resolve_font_set(font: &mut FontSet, scheme: &crate::theme::FontScheme) {
    // 四个文种槽位相互独立，逐个解析
    let slots = [
        &mut font.latin,
        &mut font.ea,
        &mut font.cs,
        &mut font.symbol,
    ];
    for slot in slots {
        let Some(name) = slot.as_ref() else {
            continue;
        };
        if !name.starts_with('+') {
            continue;
        }
        if let Some(resolved) = scheme.resolve_typeface(name) {
            *slot = Some(resolved.to_string());
        }
        // 主题里没有对应项时保留原值：宁可让回退链去兜底，
        // 也好过把字体名清空导致所有文本用同一个默认字体
    }
}

/// 判断文本框是否只含空白（用于跳过无谓的解析与渲染）。
pub fn is_blank_text_box(tb: &TextBox) -> bool {
    tb.paragraphs
        .iter()
        .all(|p| p.runs.iter().all(|r| r.text.trim().is_empty()))
}

/// 未使用导入守卫：`Insets` 由 `parse_body_props` 通过 `BodyProps` 间接使用。
#[allow(dead_code)]
fn _assert_types(_: Insets, _: Color) {}

#[cfg(test)]
mod tests {
    use super::*;
    use ppt_core::xml;

    fn n(xml_text: &str) -> XmlNode {
        xml::parse_root("t", xml_text).unwrap()
    }

    fn no_rel(_: &str) -> Option<String> {
        None
    }

    fn ctx() -> LinkContext<'static> {
        LinkContext {
            resolve_rel: &no_rel,
        }
    }

    fn parse_body(xml_text: &str) -> TextBox {
        parse_text_box(
            &n(xml_text),
            &TextStyles::default(),
            &RunProps::default(),
            None,
            &ctx(),
        )
    }

    /// 解析一段 `a:lstStyle`（以默认字符属性为基线）。
    fn styles(xml_text: &str) -> TextStyles {
        TextStyles::parse(Some(&n(xml_text)), None, &RunProps::default())
    }

    #[test]
    fn parse_simple_run() {
        let tb = parse_body(
            r#"<a:txBody>
                 <a:bodyPr/><a:lstStyle/>
                 <a:p><a:r><a:rPr lang="zh-CN" sz="2400" b="1"/><a:t>标题</a:t></a:r></a:p>
               </a:txBody>"#,
        );
        assert_eq!(tb.paragraphs.len(), 1);
        assert_eq!(tb.paragraphs[0].runs.len(), 1);
        assert_eq!(tb.paragraphs[0].runs[0].text, "标题");
        assert!((tb.paragraphs[0].runs[0].props.size_pt - 24.0).abs() < 1e-4);
        assert!(tb.paragraphs[0].runs[0].props.bold);
        assert_eq!(tb.plain_text(), "标题");
    }

    #[test]
    fn run_props_inherit_when_unspecified() {
        let inherited = RunProps {
            size_pt: 32.0,
            bold: true,
            color: Some(Color::rgb(255, 0, 0)),
            ..Default::default()
        };
        let tb = parse_text_box(
            &n(r#"<a:txBody><a:bodyPr/><a:p><a:r><a:t>x</a:t></a:r></a:p></a:txBody>"#),
            &TextStyles::default(),
            &inherited,
            None,
            &ctx(),
        );
        let p = &tb.paragraphs[0].runs[0].props;
        assert!((p.size_pt - 32.0).abs() < 1e-4, "应继承字号");
        assert!(p.bold, "应继承粗体");
        assert_eq!(p.color, Some(Color::rgb(255, 0, 0)), "应继承颜色");
    }

    #[test]
    fn explicit_false_overrides_inherited_true() {
        let inherited = RunProps {
            bold: true,
            italic: true,
            ..Default::default()
        };
        let tb = parse_text_box(
            &n(r#"<a:txBody><a:bodyPr/><a:p><a:r><a:rPr b="0" i="0"/><a:t>x</a:t></a:r></a:p></a:txBody>"#),
            &TextStyles::default(),
            &inherited,
            None,
            &ctx(),
        );
        let p = &tb.paragraphs[0].runs[0].props;
        assert!(!p.bold, "显式 b=0 应关闭继承的粗体");
        assert!(!p.italic);
    }

    #[test]
    fn multiple_paragraphs_preserved() {
        let tb = parse_body(
            r#"<a:txBody><a:bodyPr/>
                 <a:p><a:r><a:t>第一</a:t></a:r></a:p>
                 <a:p><a:r><a:t>第二</a:t></a:r></a:p>
               </a:txBody>"#,
        );
        assert_eq!(tb.paragraphs.len(), 2);
        assert_eq!(tb.plain_text(), "第一\n第二");
    }

    #[test]
    fn break_element_becomes_newline() {
        let tb = parse_body(
            r#"<a:txBody><a:bodyPr/>
                 <a:p><a:r><a:t>上</a:t></a:r><a:br/><a:r><a:t>下</a:t></a:r></a:p>
               </a:txBody>"#,
        );
        let runs = &tb.paragraphs[0].runs;
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[1].text, "\n");
    }

    #[test]
    fn field_element_parsed_with_cached_text() {
        let tb = parse_body(
            r#"<a:txBody><a:bodyPr/>
                 <a:p><a:fld id="{1}" type="slidenum"><a:t>3</a:t></a:fld></a:p>
               </a:txBody>"#,
        );
        let r = &tb.paragraphs[0].runs[0];
        assert_eq!(r.text, "3");
        assert_eq!(r.field, Some(FieldKind::SlideNumber));
    }

    #[test]
    fn field_kind_mapping() {
        assert_eq!(parse_field_kind("slidenum"), Some(FieldKind::SlideNumber));
        assert_eq!(parse_field_kind("footer"), Some(FieldKind::Footer));
        assert_eq!(parse_field_kind("header"), Some(FieldKind::Header));
        assert_eq!(parse_field_kind("datetime4"), Some(FieldKind::DateTime));
        assert_eq!(parse_field_kind("unknown"), None);
    }

    #[test]
    fn paragraph_alignment_and_level() {
        let tb = parse_body(
            r#"<a:txBody><a:bodyPr/>
                 <a:p><a:pPr algn="ctr" lvl="2"/><a:r><a:t>x</a:t></a:r></a:p>
               </a:txBody>"#,
        );
        let p = &tb.paragraphs[0];
        assert_eq!(p.align, TextAlign::Center);
        assert_eq!(p.level, 2);
    }

    #[test]
    fn all_alignment_values() {
        for (v, expected) in [
            ("l", TextAlign::Left),
            ("ctr", TextAlign::Center),
            ("r", TextAlign::Right),
            ("just", TextAlign::Justify),
            ("dist", TextAlign::Distributed),
        ] {
            let tb = parse_body(&format!(
                r#"<a:txBody><a:bodyPr/><a:p><a:pPr algn="{v}"/><a:r><a:t>x</a:t></a:r></a:p></a:txBody>"#
            ));
            assert_eq!(tb.paragraphs[0].align, expected, "对齐值 {v}");
        }
    }

    #[test]
    fn line_spacing_percent_and_points() {
        let pct = parse_body(
            r#"<a:txBody><a:bodyPr/>
                 <a:p><a:pPr><a:lnSpc><a:spcPct val="150000"/></a:lnSpc></a:pPr><a:r><a:t>x</a:t></a:r></a:p>
               </a:txBody>"#,
        );
        assert_eq!(pct.paragraphs[0].line_spacing, LineSpacing::Percent(1.5));

        let pts = parse_body(
            r#"<a:txBody><a:bodyPr/>
                 <a:p><a:pPr><a:lnSpc><a:spcPts val="2400"/></a:lnSpc></a:pPr><a:r><a:t>x</a:t></a:r></a:p>
               </a:txBody>"#,
        );
        assert_eq!(pts.paragraphs[0].line_spacing, LineSpacing::Points(24.0));
    }

    #[test]
    fn line_spacing_accepts_percent_suffix() {
        let tb = parse_body(
            r#"<a:txBody><a:bodyPr/>
                 <a:p><a:pPr><a:lnSpc><a:spcPct val="150%"/></a:lnSpc></a:pPr><a:r><a:t>x</a:t></a:r></a:p>
               </a:txBody>"#,
        );
        assert_eq!(tb.paragraphs[0].line_spacing, LineSpacing::Percent(1.5));
    }

    #[test]
    fn space_before_and_after() {
        let tb = parse_body(
            r#"<a:txBody><a:bodyPr/>
                 <a:p><a:pPr>
                   <a:spcBef><a:spcPts val="600"/></a:spcBef>
                   <a:spcAft><a:spcPts val="1200"/></a:spcAft>
                 </a:pPr><a:r><a:t>x</a:t></a:r></a:p>
               </a:txBody>"#,
        );
        let p = &tb.paragraphs[0];
        assert!((p.space_before_pt - 6.0).abs() < 1e-4);
        assert!((p.space_after_pt - 12.0).abs() < 1e-4);
    }

    #[test]
    fn margin_and_indent() {
        let tb = parse_body(
            r#"<a:txBody><a:bodyPr/>
                 <a:p><a:pPr marL="457200" indent="-228600"/><a:r><a:t>x</a:t></a:r></a:p>
               </a:txBody>"#,
        );
        let p = &tb.paragraphs[0];
        assert!((p.margin_left_pt - 36.0).abs() < 1e-3, "实际 {}", p.margin_left_pt);
        assert!((p.indent_pt + 18.0).abs() < 1e-3, "悬挂缩进应为负值");
    }

    #[test]
    fn bullet_char() {
        let tb = parse_body(
            r#"<a:txBody><a:bodyPr/>
                 <a:p><a:pPr marL="342900" indent="-342900">
                   <a:buFont typeface="Arial"/><a:buChar char="•"/>
                 </a:pPr><a:r><a:t>条目</a:t></a:r></a:p>
               </a:txBody>"#,
        );
        let b = tb.paragraphs[0].bullet.as_ref().expect("应解析出项目符号");
        assert_eq!(b.kind, BulletKind::Char("•".to_string()));
        assert_eq!(b.font.as_deref(), Some("Arial"));
        assert!(b.is_visible());
    }

    #[test]
    fn bullet_auto_number() {
        let tb = parse_body(
            r#"<a:txBody><a:bodyPr/>
                 <a:p><a:pPr><a:buAutoNum type="arabicPeriod" startAt="3"/></a:pPr><a:r><a:t>x</a:t></a:r></a:p>
               </a:txBody>"#,
        );
        let b = tb.paragraphs[0].bullet.as_ref().unwrap();
        assert_eq!(b.kind, BulletKind::AutoNumber);
        assert_eq!(b.start_at, Some(3));
        assert_eq!(b.number_type.as_deref(), Some("arabicPeriod"));
    }

    #[test]
    fn bullet_none_disables_bullet() {
        let tb = parse_body(
            r#"<a:txBody><a:bodyPr/>
                 <a:p><a:pPr><a:buNone/></a:pPr><a:r><a:t>x</a:t></a:r></a:p>
               </a:txBody>"#,
        );
        assert!(tb.paragraphs[0].bullet.is_none(), "buNone 应关闭项目符号");
    }

    #[test]
    fn bullet_color_size_and_font() {
        let tb = parse_body(
            r#"<a:txBody><a:bodyPr/>
                 <a:p><a:pPr>
                   <a:buClr><a:srgbClr val="FF0000"/></a:buClr>
                   <a:buSzPct val="80000"/>
                   <a:buFont typeface="Wingdings"/>
                   <a:buChar char="Ø"/>
                 </a:pPr><a:r><a:t>x</a:t></a:r></a:p>
               </a:txBody>"#,
        );
        let b = tb.paragraphs[0].bullet.as_ref().unwrap();
        assert_eq!(b.color, Some(Color::rgb(255, 0, 0)));
        assert!((b.size_pct.unwrap() - 0.8).abs() < 1e-6);
        assert_eq!(b.font.as_deref(), Some("Wingdings"));
    }

    #[test]
    fn lst_style_provides_level_defaults() {
        let styles = styles(
            r#"<a:lstStyle>
                 <a:lvl1pPr algn="ctr"><a:defRPr sz="4000" b="1"/></a:lvl1pPr>
               </a:lstStyle>"#,
        );
        let tb = parse_text_box(
            &n(r#"<a:txBody><a:bodyPr/><a:p><a:r><a:t>x</a:t></a:r></a:p></a:txBody>"#),
            &styles,
            &RunProps::default(),
            None,
            &ctx(),
        );
        let p = &tb.paragraphs[0];
        assert_eq!(p.align, TextAlign::Center, "应从 lstStyle 继承对齐");
        assert!((p.runs[0].props.size_pt - 40.0).abs() < 1e-4, "应继承字号");
        assert!(p.runs[0].props.bold, "应继承粗体");
    }

    #[test]
    fn lst_style_level_index_mapping() {
        assert_eq!(level_index("lvl1pPr"), Some(0));
        assert_eq!(level_index("lvl9pPr"), Some(8));
        assert_eq!(level_index("defPPr"), None);
        assert_eq!(level_index("lvl0pPr"), Some(0), "越界输入应被夹紧");
        assert_eq!(level_index("bogus"), None);
    }

    #[test]
    fn paragraph_props_override_lst_style() {
        let styles = styles(r#"<a:lstStyle><a:lvl1pPr algn="ctr"/></a:lstStyle>"#);
        let tb = parse_text_box(
            &n(r#"<a:txBody><a:bodyPr/>
                   <a:p><a:pPr algn="r"/><a:r><a:t>x</a:t></a:r></a:p>
                 </a:txBody>"#),
            &styles,
            &RunProps::default(),
            None,
            &ctx(),
        );
        assert_eq!(tb.paragraphs[0].align, TextAlign::Right, "pPr 应覆盖 lstStyle");
    }

    #[test]
    fn level_out_of_range_is_clamped() {
        let styles = TextStyles::default();
        // 越界级别不应 panic
        let _ = styles.level(99);
        let tb = parse_text_box(
            &n(r#"<a:txBody><a:bodyPr/>
                   <a:p><a:pPr lvl="99"/><a:r><a:t>x</a:t></a:r></a:p>
                 </a:txBody>"#),
            &styles,
            &RunProps::default(),
            None,
            &ctx(),
        );
        assert_eq!(tb.paragraphs.len(), 1);
    }

    #[test]
    fn body_props_anchor_and_wrap() {
        let bp = parse_body_props(Some(&n(r#"<a:bodyPr anchor="ctr" wrap="none" anchorCtr="1"/>"#)));
        assert_eq!(bp.anchor, VerticalAnchor::Middle);
        assert_eq!(bp.wrap, ppt_core::scene::TextWrap::None);
        assert!(bp.anchor_center);
    }

    #[test]
    fn body_props_insets_converted() {
        // 91440 EMU = 0.1 英寸 = 7.2pt
        let bp = parse_body_props(Some(&n(
            r#"<a:bodyPr lIns="91440" tIns="45720" rIns="91440" bIns="45720"/>"#,
        )));
        assert!((bp.insets.left - 7.2).abs() < 1e-3);
        assert!((bp.insets.top - 3.6).abs() < 1e-3);
    }

    #[test]
    fn body_props_defaults_when_attrs_missing() {
        let bp = parse_body_props(Some(&n(r#"<a:bodyPr/>"#)));
        assert!((bp.insets.left - 7.2).abs() < 1e-4);
        assert_eq!(bp.anchor, VerticalAnchor::Top);
    }

    #[test]
    fn body_props_autofit_variants() {
        let norm = parse_body_props(Some(&n(
            r#"<a:bodyPr><a:normAutofit fontScale="80000" lnSpcReduction="20000"/></a:bodyPr>"#,
        )));
        match norm.auto_fit {
            AutoFit::NormAutofit {
                font_scale,
                line_space_reduction,
            } => {
                assert!((font_scale - 0.8).abs() < 1e-6);
                assert!((line_space_reduction - 0.2).abs() < 1e-6);
            }
            other => panic!("应为 normAutofit，实际 {other:?}"),
        }

        let sp = parse_body_props(Some(&n(r#"<a:bodyPr><a:spAutoFit/></a:bodyPr>"#)));
        assert_eq!(sp.auto_fit, AutoFit::SpAutoFit);

        let none = parse_body_props(Some(&n(r#"<a:bodyPr><a:noAutofit/></a:bodyPr>"#)));
        assert_eq!(none.auto_fit, AutoFit::NoAutofit);
    }

    #[test]
    fn body_props_norm_autofit_without_scale_needs_solving() {
        let bp = parse_body_props(Some(&n(r#"<a:bodyPr><a:normAutofit/></a:bodyPr>"#)));
        assert!(bp.auto_fit.needs_solving(), "缺 fontScale 时应由排版层求解");
    }

    #[test]
    fn body_props_vertical_directions() {
        for (v, expected) in [
            ("horz", TextDirection::Horizontal),
            ("vert", TextDirection::Rotate90),
            ("vert270", TextDirection::Rotate270),
            ("eaVert", TextDirection::Stacked),
        ] {
            let bp = parse_body_props(Some(&n(&format!(r#"<a:bodyPr vert="{v}"/>"#))));
            assert_eq!(bp.direction, expected, "竖排值 {v}");
        }
    }

    #[test]
    fn body_props_rotation_converted() {
        // 5400000 = 90 度
        let bp = parse_body_props(Some(&n(r#"<a:bodyPr rot="5400000"/>"#)));
        assert!((bp.rotation_deg.unwrap() - 90.0).abs() < 0.01);
    }

    #[test]
    fn body_props_columns() {
        let bp = parse_body_props(Some(&n(
            r#"<a:bodyPr numCol="3" spcCol="91440"/>"#,
        )));
        assert_eq!(bp.columns.len(), 3);
        assert!((bp.columns[0].spacing_pt - 7.2).abs() < 1e-3);
    }

    #[test]
    fn single_column_not_materialized() {
        let bp = parse_body_props(Some(&n(r#"<a:bodyPr numCol="1"/>"#)));
        assert!(bp.columns.is_empty(), "单栏无需记录分栏信息");
    }

    #[test]
    fn run_props_text_color_and_highlight() {
        let tb = parse_body(
            r#"<a:txBody><a:bodyPr/>
                 <a:p><a:r><a:rPr>
                   <a:solidFill><a:srgbClr val="0000FF"/></a:solidFill>
                   <a:highlight><a:srgbClr val="FFFF00"/></a:highlight>
                 </a:rPr><a:t>x</a:t></a:r></a:p>
               </a:txBody>"#,
        );
        let p = &tb.paragraphs[0].runs[0].props;
        assert_eq!(p.color, Some(Color::rgb(0, 0, 255)));
        assert_eq!(p.highlight, Some(Color::rgb(255, 255, 0)));
    }

    #[test]
    fn run_props_font_families_parsed_independently() {
        let tb = parse_body(
            r#"<a:txBody><a:bodyPr/>
                 <a:p><a:r><a:rPr>
                   <a:latin typeface="Arial"/><a:ea typeface="微软雅黑"/><a:cs typeface="Arial"/>
                 </a:rPr><a:t>x</a:t></a:r></a:p>
               </a:txBody>"#,
        );
        let f = &tb.paragraphs[0].runs[0].props.font;
        assert_eq!(f.latin.as_deref(), Some("Arial"));
        assert_eq!(f.ea.as_deref(), Some("微软雅黑"));
        assert_eq!(f.cs.as_deref(), Some("Arial"));
    }

    #[test]
    fn run_props_underline_and_strike() {
        let tb = parse_body(
            r#"<a:txBody><a:bodyPr/>
                 <a:p><a:r><a:rPr u="sng" strike="sngStrike"/><a:t>x</a:t></a:r></a:p>
               </a:txBody>"#,
        );
        let p = &tb.paragraphs[0].runs[0].props;
        assert_eq!(p.underline, UnderlineStyle::Single);
        assert_eq!(p.strike, StrikeStyle::Single);
        assert!(p.has_decoration());
    }

    #[test]
    fn underline_values() {
        assert_eq!(parse_underline("sng"), UnderlineStyle::Single);
        assert_eq!(parse_underline("dbl"), UnderlineStyle::Double);
        assert_eq!(parse_underline("wavy"), UnderlineStyle::Wavy);
        assert_eq!(parse_underline("wavyDbl"), UnderlineStyle::WavyDouble);
        assert_eq!(parse_underline("none"), UnderlineStyle::None);
        assert_eq!(parse_underline("bogus"), UnderlineStyle::None);
    }

    #[test]
    fn run_props_superscript_baseline() {
        let tb = parse_body(
            r#"<a:txBody><a:bodyPr/>
                 <a:p><a:r><a:rPr baseline="30000"/><a:t>2</a:t></a:r></a:p>
               </a:txBody>"#,
        );
        let p = &tb.paragraphs[0].runs[0].props;
        assert!((p.baseline_pct - 0.3).abs() < 1e-6, "上标基线应上移 30%");
    }

    #[test]
    fn run_props_subscript_baseline() {
        let tb = parse_body(
            r#"<a:txBody><a:bodyPr/>
                 <a:p><a:r><a:rPr baseline="-25000"/><a:t>2</a:t></a:r></a:p>
               </a:txBody>"#,
        );
        assert!(tb.paragraphs[0].runs[0].props.baseline_pct < 0.0);
    }

    #[test]
    fn run_props_letter_spacing() {
        let tb = parse_body(
            r#"<a:txBody><a:bodyPr/>
                 <a:p><a:r><a:rPr spc="300"/><a:t>x</a:t></a:r></a:p>
               </a:txBody>"#,
        );
        // 300/100 = 3pt
        assert!((tb.paragraphs[0].runs[0].props.spacing_pt - 3.0).abs() < 1e-4);
    }

    #[test]
    fn run_props_caps_and_language() {
        let tb = parse_body(
            r#"<a:txBody><a:bodyPr/>
                 <a:p><a:r><a:rPr cap="all" lang="en-US"/><a:t>x</a:t></a:r></a:p>
               </a:txBody>"#,
        );
        let p = &tb.paragraphs[0].runs[0].props;
        assert_eq!(p.caps, TextCaps::All);
        assert_eq!(p.language.as_deref(), Some("en-US"));
    }

    #[test]
    fn paragraph_default_run_applies_to_runs() {
        let tb = parse_body(
            r#"<a:txBody><a:bodyPr/>
                 <a:p>
                   <a:pPr><a:defRPr sz="3600"/></a:pPr>
                   <a:r><a:t>x</a:t></a:r>
                 </a:p>
               </a:txBody>"#,
        );
        assert!(
            (tb.paragraphs[0].runs[0].props.size_pt - 36.0).abs() < 1e-4,
            "run 应继承段落 defRPr"
        );
    }

    #[test]
    fn end_para_rpr_used_for_empty_paragraph() {
        let tb = parse_body(
            r#"<a:txBody><a:bodyPr/>
                 <a:p><a:endParaRPr sz="4800"/></a:p>
               </a:txBody>"#,
        );
        assert!(tb.paragraphs[0].runs.is_empty());
        assert!(
            (tb.paragraphs[0].default_run.size_pt - 48.0).abs() < 1e-4,
            "空段落应保留末段字符属性"
        );
    }

    #[test]
    fn tab_stops_parsed() {
        let tb = parse_body(
            r#"<a:txBody><a:bodyPr/>
                 <a:p><a:pPr>
                   <a:tabLst>
                     <a:tab pos="914400" algn="l"/>
                     <a:tab pos="1828800" algn="ctr"/>
                     <a:tab pos="2743200" algn="r"/>
                   </a:tabLst>
                 </a:pPr><a:r><a:t>x</a:t></a:r></a:p>
               </a:txBody>"#,
        );
        let tabs = &tb.paragraphs[0].tab_stops;
        assert_eq!(tabs.len(), 3);
        assert!((tabs[0].position_pt - 72.0).abs() < 1e-3);
        assert_eq!(tabs[1].align, TabAlign::Center);
        assert_eq!(tabs[2].align, TabAlign::Right);
    }

    #[test]
    fn rtl_and_font_align() {
        let tb = parse_body(
            r#"<a:txBody><a:bodyPr/>
                 <a:p><a:pPr rtl="1" fontAlgn="ctr" eaLnBrk="0"/><a:r><a:t>x</a:t></a:r></a:p>
               </a:txBody>"#,
        );
        let p = &tb.paragraphs[0];
        assert!(p.rtl);
        assert_eq!(p.font_align, FontAlign::Center);
        assert!(!p.ea_line_break);
    }

    #[test]
    fn hyperlink_external_url() {
        let ctx = LinkContext {
            resolve_rel: &|id: &str| {
                if id == "rId3" {
                    Some("https://example.com".to_string())
                } else {
                    None
                }
            },
        };
        let tb = parse_text_box(
            &n(r#"<a:txBody><a:bodyPr/>
                   <a:p><a:r><a:rPr><a:hlinkClick r:id="rId3" tooltip="提示"/></a:rPr><a:t>链接</a:t></a:r></a:p>
                 </a:txBody>"#),
            &TextStyles::default(),
            &RunProps::default(),
            None,
            &ctx,
        );
        let link = tb.paragraphs[0].runs[0].hyperlink.as_ref().unwrap();
        assert_eq!(
            link.target,
            HyperlinkTarget::Url("https://example.com".to_string())
        );
        assert_eq!(link.tooltip.as_deref(), Some("提示"));
    }

    #[test]
    fn hyperlink_show_jump_actions() {
        for (action, expected) in [
            ("ppaction://hlinkshowjump?jump=nextslide", HyperlinkTarget::NextSlide),
            (
                "ppaction://hlinkshowjump?jump=previousslide",
                HyperlinkTarget::PreviousSlide,
            ),
            ("ppaction://hlinkshowjump?jump=firstslide", HyperlinkTarget::FirstSlide),
            ("ppaction://hlinkshowjump?jump=lastslide", HyperlinkTarget::LastSlide),
            ("ppaction://hlinkshowjump?jump=endshow", HyperlinkTarget::EndShow),
        ] {
            let tb = parse_text_box(
                &n(&format!(
                    r#"<a:txBody><a:bodyPr/>
                         <a:p><a:r><a:rPr><a:hlinkClick action="{action}"/></a:rPr><a:t>x</a:t></a:r></a:p>
                       </a:txBody>"#
                )),
                &TextStyles::default(),
                &RunProps::default(),
                None,
                &ctx(),
            );
            assert_eq!(
                tb.paragraphs[0].runs[0].hyperlink.as_ref().unwrap().target,
                expected,
                "动作 {action}"
            );
        }
    }

    #[test]
    fn hyperlink_slide_jump_resolves_index() {
        let ctx = LinkContext {
            resolve_rel: &|_: &str| Some("ppt/slides/slide7.xml".to_string()),
        };
        let tb = parse_text_box(
            &n(r#"<a:txBody><a:bodyPr/>
                   <a:p><a:r><a:rPr><a:hlinkClick r:id="rId1" action="ppaction://hlinksldjump"/></a:rPr><a:t>x</a:t></a:r></a:p>
                 </a:txBody>"#),
            &TextStyles::default(),
            &RunProps::default(),
            None,
            &ctx,
        );
        assert_eq!(
            tb.paragraphs[0].runs[0].hyperlink.as_ref().unwrap().target,
            HyperlinkTarget::Slide(6),
            "slide7.xml 应对应索引 6"
        );
    }

    #[test]
    fn hyperlink_without_resolvable_relation_is_none() {
        let tb = parse_text_box(
            &n(r#"<a:txBody><a:bodyPr/>
                   <a:p><a:r><a:rPr><a:hlinkClick r:id="rId999"/></a:rPr><a:t>x</a:t></a:r></a:p>
                 </a:txBody>"#),
            &TextStyles::default(),
            &RunProps::default(),
            None,
            &ctx(),
        );
        assert!(tb.paragraphs[0].runs[0].hyperlink.is_none());
    }

    #[test]
    fn text_with_entities_decoded() {
        let tb = parse_body(
            r#"<a:txBody><a:bodyPr/>
                 <a:p><a:r><a:t>a &amp; b &lt;c&gt;</a:t></a:r></a:p>
               </a:txBody>"#,
        );
        assert_eq!(tb.paragraphs[0].runs[0].text, "a & b <c>");
    }

    #[test]
    fn text_with_cjk_and_punctuation_preserved() {
        let tb = parse_body(
            r#"<a:txBody><a:bodyPr/>
                 <a:p><a:r><a:t>注意：「引用」，括号（说明）。</a:t></a:r></a:p>
               </a:txBody>"#,
        );
        assert_eq!(
            tb.paragraphs[0].runs[0].text,
            "注意：「引用」，括号（说明）。"
        );
    }

    #[test]
    fn empty_tx_body_yields_no_paragraphs() {
        let tb = parse_body(r#"<a:txBody><a:bodyPr/></a:txBody>"#);
        assert!(tb.paragraphs.is_empty());
        assert!(tb.is_empty());
        assert!(is_blank_text_box(&tb));
    }

    #[test]
    fn whitespace_only_text_detected_as_blank() {
        let tb = parse_body(
            r#"<a:txBody><a:bodyPr/><a:p><a:r><a:t>   </a:t></a:r></a:p></a:txBody>"#,
        );
        assert!(is_blank_text_box(&tb));
    }

    #[test]
    fn unknown_child_elements_are_ignored() {
        // 课件里常见 `a:commentRangeStart` 等扩展元素
        let tb = parse_body(
            r#"<a:txBody><a:bodyPr/>
                 <a:p>
                   <a:unknownMarker/>
                   <a:r><a:t>正文</a:t></a:r>
                 </a:p>
               </a:txBody>"#,
        );
        assert_eq!(tb.plain_text(), "正文");
    }

    #[test]
    fn extract_plain_text_works_on_raw_node() {
        let tx = n(r#"<a:txBody>
            <a:bodyPr/>
            <a:p><a:r><a:t>第一行</a:t></a:r></a:p>
            <a:p><a:r><a:t>第二行</a:t></a:r></a:p>
          </a:txBody>"#);
        assert_eq!(extract_plain_text(&tx), "第一行\n第二行");
    }

    #[test]
    fn scheme_color_in_text_resolves() {
        let scheme = ColorScheme::default();
        let tb = parse_text_box(
            &n(r#"<a:txBody><a:bodyPr/>
                   <a:p><a:r><a:rPr><a:solidFill><a:schemeClr val="accent1"><a:lumMod val="75000"/></a:schemeClr></a:solidFill></a:rPr><a:t>x</a:t></a:r></a:p>
                 </a:txBody>"#),
            &TextStyles::default(),
            &RunProps::default(),
            Some(&scheme),
            &ctx(),
        );
        let c = tb.paragraphs[0].runs[0].props.color.unwrap();
        // lumMod 75% 后应比原色更暗
        assert!(c.r < scheme.accent1.r || c.g < scheme.accent1.g || c.b < scheme.accent1.b);
    }

    #[test]
    fn css_style_default_applies_to_all_levels() {
        let styles = styles(
            r#"<a:lstStyle>
                 <a:defPPr><a:defRPr sz="2000"/></a:defPPr>
                 <a:lvl1pPr algn="ctr"/>
               </a:lstStyle>"#,
        );
        // defPPr 的字符属性应落到所有级别
        assert!((styles.level(0).run.size_pt - 20.0).abs() < 1e-4);
        assert!((styles.level(5).run.size_pt - 20.0).abs() < 1e-4);
        // lvl1pPr 的段落属性只影响第 1 级
        assert_eq!(styles.level(0).align, Some(TextAlign::Center));
        assert_eq!(styles.level(1).align, None);
    }

    #[test]
    fn styles_default_is_usable() {
        let s = TextStyles::default();
        assert!(s.levels.is_empty());
        assert_eq!(s.level(0).align, None);
        assert_eq!(s.level(0).run.size_pt, 18.0);
        // 没有级别信息时，effective_run 应原样返回传入的继承值
        let inherited = RunProps {
            size_pt: 30.0,
            ..Default::default()
        };
        assert!((s.effective_run(0, &inherited).size_pt - 30.0).abs() < 1e-4);
    }
}
