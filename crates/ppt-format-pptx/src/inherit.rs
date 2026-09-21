//! 幻灯片继承链：母版 / 版式 / 主题 → 形状。
//!
//! # 为什么需要单独一层
//!
//! PowerPoint 里一张幻灯片通常只写「内容」，位置、字体、颜色大多来自
//! `slideLayout` 与 `slideMaster`。占位符（`p:ph`）是继承的载体：
//!
//! ```text
//! 幻灯片  <p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr>...
//!            │  自身没写 a:xfrm、没有 a:lstStyle
//!            ▼  按 idx/type 查找
//! 版式    <p:sp><p:ph type="title"/><p:spPr><a:xfrm>…</a:xfrm>
//!            │  给出了位置
//!            ▼  再向上
//! 母版    <p:txStyles><p:titleStyle><a:lvl1pPr>… 给出字号与字体
//! ```
//!
//! 本模块把这三级**预先展平**成一张查找表（[`SlideInheritance`]），
//! 形状解析时只需一次查表，不必逐层向上递归。

use std::collections::HashMap;

use ppt_core::scene::{Fill, RunProps, Size, Transform};
use ppt_core::{units, XmlNode};

use crate::color::ColorScheme;
use crate::paint::{self, RelResolver};
use crate::text::{TextStyles, MAX_LEVELS};
use crate::theme::Theme;

/// 占位符类型。OOXML 共定义 18 种。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlaceholderType {
    Title,
    CenterTitle,
    SubTitle,
    Body,
    Object,
    Chart,
    Table,
    ClipArt,
    Diagram,
    Media,
    SlideImage,
    Picture,
    Date,
    Footer,
    SlideNumber,
    Header,
    VerticalBody,
    VerticalTitle,
    VerticalObject,
}

impl PlaceholderType {
    pub fn parse(v: &str) -> Option<PlaceholderType> {
        Some(match v {
            "title" => PlaceholderType::Title,
            "ctrTitle" => PlaceholderType::CenterTitle,
            "subTitle" => PlaceholderType::SubTitle,
            "body" => PlaceholderType::Body,
            "obj" => PlaceholderType::Object,
            "chart" => PlaceholderType::Chart,
            "tbl" => PlaceholderType::Table,
            "clipArt" => PlaceholderType::ClipArt,
            "dgm" => PlaceholderType::Diagram,
            "media" => PlaceholderType::Media,
            "sldImg" => PlaceholderType::SlideImage,
            "pic" => PlaceholderType::Picture,
            "dt" => PlaceholderType::Date,
            "ftr" => PlaceholderType::Footer,
            "sldNum" => PlaceholderType::SlideNumber,
            "hdr" => PlaceholderType::Header,
            "vertBody" => PlaceholderType::VerticalBody,
            "vertTitle" => PlaceholderType::VerticalTitle,
            "vertObj" => PlaceholderType::VerticalObject,
            _ => return None,
        })
    }

    /// 从 `p:ph` 元素推断类型。
    ///
    /// ECMA-376 规定：`p:ph` 省略 `type` 时等价于 `type="obj"`。
    /// 这一点很关键 —— 省略 `type` 的占位符（只写 `idx`）在课件里非常常见，
    /// 若当作「无类型」处理，它会落到 `otherStyle`，
    /// 从而丢掉母版 `bodyStyle` 定义的字号与项目符号。
    pub fn from_ph_element(ph: &XmlNode) -> PlaceholderType {
        ph.attr("type")
            .and_then(PlaceholderType::parse)
            .unwrap_or(PlaceholderType::Object)
    }

    /// 该占位符是否属于「标题」类（决定用 `p:titleStyle`）。
    #[inline]
    pub fn is_title(self) -> bool {
        matches!(
            self,
            PlaceholderType::Title
                | PlaceholderType::CenterTitle
                | PlaceholderType::VerticalTitle
        )
    }

    /// 该占位符是否属于「正文」类（决定用 `p:bodyStyle`）。
    #[inline]
    pub fn is_body(self) -> bool {
        matches!(
            self,
            PlaceholderType::Body
                | PlaceholderType::SubTitle
                | PlaceholderType::Object
                | PlaceholderType::VerticalBody
                | PlaceholderType::VerticalObject
        )
    }

    /// 该占位符是否由幻灯片自身提供内容（而非自动生成）。
    ///
    /// 日期/页脚/页码这三类即使幻灯片上没画，放映时也应能显示，
    /// 因此解析时需要从版式补齐位置。
    #[inline]
    pub fn is_auto_generated(self) -> bool {
        matches!(
            self,
            PlaceholderType::Date | PlaceholderType::Footer | PlaceholderType::SlideNumber
        )
    }
}

/// 占位符查找键。
///
/// OOXML 的匹配规则：`idx` 存在时优先按 `idx` 匹配；
/// 否则按 `type` 匹配。两者都没有时用默认键。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PhKey {
    Idx(u32),
    Type(PlaceholderType),
    Default,
}

impl PhKey {
    /// 从 `p:ph` 元素解析。
    pub fn parse(ph: &XmlNode) -> PhKey {
        if let Some(idx) = ph.attr_u32("idx") {
            return PhKey::Idx(idx);
        }
        if let Some(t) = ph.attr("type").and_then(PlaceholderType::parse) {
            return PhKey::Type(t);
        }
        // 既无 idx 也无 type：按规范等价于 `type="obj"`
        PhKey::Type(PlaceholderType::Object)
    }
}

/// 占位符继承来的属性。
#[derive(Debug, Clone, Default)]
pub struct Placeholder {
    pub ph_type: Option<PlaceholderType>,
    pub idx: Option<u32>,
    /// 继承来的位置与尺寸。
    pub transform: Option<Transform>,
    pub extent: Option<Size>,
    /// 继承来的 `a:lstStyle`。
    pub lst_style: Option<XmlNode>,
    /// 继承来的角色（决定用哪套 `txStyles`）。
    pub role: StyleRole,
    /// 是否可见（`p:ph` 上 `hasCustomPrompt` 等属性不影响，这里指形状是否被隐藏）。
    pub hidden: bool,
}

/// 文本样式角色，对应母版 `p:txStyles` 的三个子节点。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StyleRole {
    Title,
    Body,
    #[default]
    Other,
}

impl Placeholder {
    /// 由 `p:ph` 元素推断样式角色。
    fn role_of(ph_type: Option<PlaceholderType>) -> StyleRole {
        match ph_type {
            Some(t) if t.is_title() => StyleRole::Title,
            Some(t) if t.is_body() => StyleRole::Body,
            _ => StyleRole::Other,
        }
    }
}

/// 一张幻灯片完整的继承上下文。
#[derive(Debug, Clone)]
pub struct SlideInheritance {
    pub theme: Theme,
    pub color_scheme: ColorScheme,
    /// 母版 `p:txStyles` 的三套文本样式。
    pub title_style: TextStyles,
    pub body_style: TextStyles,
    pub other_style: TextStyles,
    /// 占位符查找表（已把版式与母版展平）。
    placeholders: HashMap<PhKey, Placeholder>,
    /// 继承来的背景填充（版式或母版的 `p:bg`）。
    pub background: Option<Fill>,
    /// 幻灯片尺寸（pt），用于相对尺寸的占位符。
    pub slide_size: Size,
}

impl Default for SlideInheritance {
    fn default() -> Self {
        SlideInheritance {
            theme: Theme::default(),
            color_scheme: ColorScheme::default(),
            title_style: TextStyles::default(),
            body_style: TextStyles::default(),
            other_style: TextStyles::default(),
            placeholders: HashMap::new(),
            background: None,
            slide_size: Size::new(960.0, 540.0),
        }
    }
}

impl SlideInheritance {
    /// 建立继承上下文。
    ///
    /// 参数按「由远及近」排列：主题 → 母版 → 版式。
    /// 越靠后的来源优先级越高，与 OOXML 的继承方向一致。
    ///
    /// `clr_map` 参数已无用 —— 颜色映射在 [`crate::theme::Theme::parse`]
    /// 阶段就应用完毕，这里保留形参只为调用方语义完整。
    pub fn build(
        theme: &Theme,
        master: Option<&XmlNode>,
        layout: Option<&XmlNode>,
        _clr_map: Option<&crate::color::ClrMap>,
        slide_size: Size,
        resolve: RelResolver<'_>,
    ) -> SlideInheritance {
        let mut ctx = SlideInheritance {
            theme: theme.clone(),
            color_scheme: theme.color_scheme,
            slide_size,
            ..Default::default()
        };

        // 1) 母版：文本样式、占位符、背景
        if let Some(m) = master {
            ctx.title_style = parse_tx_style(m, "titleStyle", theme, &ctx.color_scheme);
            ctx.body_style = parse_tx_style(m, "bodyStyle", theme, &ctx.color_scheme);
            ctx.other_style = parse_tx_style(m, "otherStyle", theme, &ctx.color_scheme);

            ctx.collect_placeholders(m, 0);
            ctx.collect_background(m, resolve);
        }

        // 2) 版式：优先级更高，覆盖同名占位符并补充背景
        if let Some(l) = layout {
            ctx.collect_placeholders(l, 1);
            // 版式背景优先于母版
            if let Some(bg) = parse_background(l, &ctx.color_scheme, resolve) {
                ctx.background = Some(bg);
            }
        }

        ctx
    }

    /// 收集一个部件（母版或版式）里的占位符。
    ///
    /// `priority` 越大越优先：版式传 1、母版传 0。
    /// 已存在且优先级不低时不覆盖，从而保证版式优先。
    fn collect_placeholders(&mut self, part: &XmlNode, priority: u8) {
        let Some(tree) = part.path(&["cSld", "spTree"]) else {
            return;
        };

        for sp in tree.children_named("sp") {
            let Some(ph) = sp.path(&["nvSpPr", "nvPr", "ph"]) else {
                continue;
            };
            let key = PhKey::parse(ph);

            let ph_type = ph.attr("type").and_then(PlaceholderType::parse);

            // 版式优先于母版
            if priority == 0 && self.placeholders.contains_key(&key) {
                continue;
            }

            let (transform, extent) = parse_transform(sp.child("spPr"));
            let lst_style = sp.child("txBody").and_then(|t| t.child("lstStyle")).cloned();

            self.placeholders.insert(
                key,
                Placeholder {
                    ph_type,
                    idx: ph.attr_u32("idx"),
                    transform,
                    extent,
                    lst_style,
                    role: Placeholder::role_of(ph_type),
                    hidden: sp
                        .path(&["nvSpPr", "cNvPr"])
                        .and_then(|n| n.attr_bool("hidden"))
                        .unwrap_or(false),
                },
            );
        }

        // 母版/版式也可能用 `p:pic` 形式的占位符（极少见）
        for pic in tree.children_named("pic") {
            let Some(ph) = pic.path(&["nvPicPr", "nvPr", "ph"]) else {
                continue;
            };
            let key = PhKey::parse(ph);
            if priority == 0 && self.placeholders.contains_key(&key) {
                continue;
            }
            let (transform, extent) = parse_transform(pic.child("spPr"));
            self.placeholders.insert(
                key,
                Placeholder {
                    ph_type: ph.attr("type").and_then(PlaceholderType::parse),
                    idx: ph.attr_u32("idx"),
                    transform,
                    extent,
                    lst_style: None,
                    role: StyleRole::Other,
                    hidden: false,
                },
            );
        }
    }

    fn collect_background(&mut self, part: &XmlNode, resolve: RelResolver<'_>) {
        if self.background.is_some() {
            return;
        }
        if let Some(fill) = parse_background(part, &self.color_scheme, resolve) {
            self.background = Some(fill);
        }
    }

    /// 按 `p:ph` 查占位符。
    pub fn placeholder(&self, key: PhKey) -> Option<&Placeholder> {
        self.placeholders.get(&key)
    }

    /// 按占位符角色取对应的文本样式。
    pub fn tx_style(&self, role: StyleRole) -> &TextStyles {
        match role {
            StyleRole::Title => &self.title_style,
            StyleRole::Body => &self.body_style,
            StyleRole::Other => &self.other_style,
        }
    }

    /// 按占位符类型取文本样式。
    pub fn tx_style_for(&self, ph_type: Option<PlaceholderType>) -> &TextStyles {
        self.tx_style(Placeholder::role_of(ph_type))
    }

    #[inline]
    pub fn placeholder_count(&self) -> usize {
        self.placeholders.len()
    }
}

/// 从 `p:spPr` 解析位置与尺寸。
///
/// 返回 `(绝对变换, 尺寸)`；`a:xfrm` 缺失时返回 `(None, None)`。
pub fn parse_transform(sp_pr: Option<&XmlNode>) -> (Option<Transform>, Option<Size>) {
    parse_xfrm(sp_pr.and_then(|p| p.child("xfrm")))
}

/// 形状自身 `a:xfrm` 的翻转标记（`flipH` / `flipV`），没有就是 `(false, false)`。
///
/// 单独拎出来是因为**文字不跟着形状一起翻转**：拿这两个标记做个
/// 关于形状中心的镜像，就能把文字那份变换抵消回可读状态，
/// 见 [`ppt_core::scene::Transform::mirror_about_center`]。
pub fn parse_flip(sp_pr: Option<&XmlNode>) -> (bool, bool) {
    let Some(xfrm) = sp_pr.and_then(|p| p.child("xfrm")) else {
        return (false, false);
    };
    (
        xfrm.attr_bool_or("flipH", false),
        xfrm.attr_bool_or("flipV", false),
    )
}

/// 直接从一个变换节点解析位置与尺寸。
///
/// 与 [`parse_transform`] 的区别：`p:graphicFrame` 的变换就是 `p:xfrm` 本身，
/// 不需要再向下找一层。
pub fn parse_xfrm(xfrm: Option<&XmlNode>) -> (Option<Transform>, Option<Size>) {
    let Some(xfrm) = xfrm else {
        return (None, None);
    };

    let off = xfrm.child("off");
    let ext = xfrm.child("ext");

    let extent = ext.map(|e| {
        Size::new(
            units::emu_to_pt(e.attr_f64("cx").unwrap_or(0.0)),
            units::emu_to_pt(e.attr_f64("cy").unwrap_or(0.0)),
        )
    });

    let Some(extent) = extent else {
        return (None, None);
    };

    let offset = off
        .map(|o| {
            ppt_core::scene::Point::new(
                units::emu_to_pt(o.attr_f64("x").unwrap_or(0.0)),
                units::emu_to_pt(o.attr_f64("y").unwrap_or(0.0)),
            )
        })
        .unwrap_or(ppt_core::scene::Point::ZERO);

    let rot = units::ooxml_angle_to_deg(xfrm.attr_f64("rot").unwrap_or(0.0));
    let flip_h = xfrm.attr_bool_or("flipH", false);
    let flip_v = xfrm.attr_bool_or("flipV", false);

    (
        Some(Transform::from_ooxml(offset, extent, rot, flip_h, flip_v)),
        Some(extent),
    )
}

/// 从母版解析 `p:txStyles` 中的一套样式。
fn parse_tx_style(
    master: &XmlNode,
    style_name: &str,
    theme: &Theme,
    scheme: &ColorScheme,
) -> TextStyles {
    let Some(node) = master.path(&["txStyles", style_name]) else {
        return TextStyles::default();
    };
    // 以主题的正文字体作为基线，符合 PowerPoint 的实际行为
    let seed = theme_seed_run(theme, scheme);
    TextStyles::parse(Some(node), Some(scheme), &seed)
}

/// 主题提供的默认字符属性（正文字体 + 正文色）。
///
/// 在母版 `p:txStyles` 缺失时作为文本样式的基线，
/// 保证即使遇到残缺的课件也不会退化成「18pt 黑色无字体」。
pub fn theme_seed_run(theme: &Theme, scheme: &ColorScheme) -> RunProps {
    let mut run = RunProps::default();
    if let Some(f) = theme.font_scheme.minor_latin.as_ref() {
        run.font.latin = Some(f.clone());
    }
    if let Some(f) = theme.font_scheme.minor_ea.as_ref() {
        run.font.ea = Some(f.clone());
    }
    run.color = Some(scheme.tx1);
    run
}

/// 解析背景（`p:cSld/p:bg`）。
pub fn parse_background(
    part: &XmlNode,
    scheme: &ColorScheme,
    resolve: RelResolver<'_>,
) -> Option<Fill> {
    let bg = part.path(&["cSld", "bg"])?;

    // 直接指定填充
    if let Some(bg_pr) = bg.child("bgPr") {
        let fill = paint::parse_fill(bg_pr, Some(scheme), resolve);
        return match fill {
            Fill::None => None,
            other => Some(other),
        };
    }

    // 引用主题背景样式：`<p:bgRef idx="1001"><a:schemeClr val="bg1"/></p:bgRef>`
    // idx >= 1001 表示 `bgFillStyleLst` 中的第 (idx - 1000) 项
    let bg_ref = bg.child("bgRef")?;
    let idx = bg_ref.attr_u32("idx").unwrap_or(0);
    if idx == 0 {
        // idx=0 且带颜色子元素：直接用该颜色
        return bg_ref
            .children
            .first()
            .and_then(|c| crate::color::resolve_color_node(c, Some(scheme)))
            .map(Fill::Solid);
    }
    // 主题引用需要 Theme 才能解析，这里只能交给调用方；
    // 缺少主题时退化为「引用色」，至少保证底色不为空
    bg_ref
        .children
        .first()
        .and_then(|c| crate::color::resolve_color_node(c, Some(scheme)))
        .map(Fill::Solid)
}

/// 判断继承上下文中是否存在某个占位符类型。
impl SlideInheritance {
    pub fn has_placeholder_type(&self, t: PlaceholderType) -> bool {
        self.placeholders
            .values()
            .any(|p| p.ph_type == Some(t))
    }

    /// 全部占位符键（用于诊断「版式里有几个占位符」）。
    pub fn placeholder_keys(&self) -> Vec<PhKey> {
        self.placeholders.keys().copied().collect()
    }
}

/// 文本级别的上限（与 [`MAX_LEVELS`] 一致，供外部引用）。
pub const LEVELS: usize = MAX_LEVELS;

#[cfg(test)]
mod tests {
    use super::*;
    use ppt_core::scene::Color;
    use ppt_core::xml;

    fn n(xml_text: &str) -> XmlNode {
        xml::parse_root("t", xml_text).unwrap()
    }

    fn no_rel(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn placeholder_type_parsing() {
        assert_eq!(PlaceholderType::parse("title"), Some(PlaceholderType::Title));
        assert_eq!(
            PlaceholderType::parse("ctrTitle"),
            Some(PlaceholderType::CenterTitle)
        );
        assert_eq!(PlaceholderType::parse("body"), Some(PlaceholderType::Body));
        assert_eq!(
            PlaceholderType::parse("sldNum"),
            Some(PlaceholderType::SlideNumber)
        );
        assert_eq!(PlaceholderType::parse("nope"), None);
    }

    #[test]
    fn placeholder_type_classification() {
        assert!(PlaceholderType::Title.is_title());
        assert!(PlaceholderType::CenterTitle.is_title());
        assert!(!PlaceholderType::Body.is_title());
        assert!(PlaceholderType::Body.is_body());
        assert!(PlaceholderType::Object.is_body());
        assert!(!PlaceholderType::Title.is_body());

        // 日期/页脚/页码属于自动生成
        assert!(PlaceholderType::Date.is_auto_generated());
        assert!(PlaceholderType::Footer.is_auto_generated());
        assert!(PlaceholderType::SlideNumber.is_auto_generated());
        assert!(!PlaceholderType::Body.is_auto_generated());
    }

    #[test]
    fn ph_key_prefers_idx_over_type() {
        let k = PhKey::parse(&n(r#"<p:ph type="body" idx="12"/>"#));
        assert_eq!(k, PhKey::Idx(12));
    }

    #[test]
    fn ph_key_falls_back_to_type() {
        let k = PhKey::parse(&n(r#"<p:ph type="title"/>"#));
        assert_eq!(k, PhKey::Type(PlaceholderType::Title));
    }

    #[test]
    fn ph_key_without_attrs_defaults_to_object() {
        let k = PhKey::parse(&n(r#"<p:ph/>"#));
        assert_eq!(k, PhKey::Type(PlaceholderType::Object));
    }

    #[test]
    fn parse_transform_converts_emu_to_points() {
        let sp_pr = n(
            r#"<p:spPr><a:xfrm>
                 <a:off x="914400" y="457200"/>
                 <a:ext cx="1828800" cy="914400"/>
               </a:xfrm></p:spPr>"#,
        );
        let (t, ext) = parse_transform(Some(&sp_pr));
        let ext = ext.expect("应解析出尺寸");
        assert!((ext.w - 144.0).abs() < 1e-3, "实际 {}", ext.w);
        assert!((ext.h - 72.0).abs() < 1e-3);
        let t = t.expect("应解析出变换");
        let p = t.apply(ppt_core::scene::Point::ZERO);
        assert!((p.x - 72.0).abs() < 1e-3);
        assert!((p.y - 36.0).abs() < 1e-3);
    }

    #[test]
    fn parse_transform_handles_rotation_and_flip() {
        let sp_pr = n(
            r#"<p:spPr><a:xfrm rot="5400000" flipH="1">
                 <a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/>
               </a:xfrm></p:spPr>"#,
        );
        let (t, _) = parse_transform(Some(&sp_pr));
        let t = t.unwrap();
        // 100x100 旋转 90 度后中心仍在 (36,36)
        let center = t.apply(ppt_core::scene::Point::new(36.0, 36.0));
        assert!((center.x - 36.0).abs() < 1e-3);
        assert!((center.y - 36.0).abs() < 1e-3);
    }

    #[test]
    fn parse_transform_without_xfrm_returns_none() {
        let (t, e) = parse_transform(Some(&n(r#"<p:spPr/>"#)));
        assert!(t.is_none() && e.is_none());
        let (t, e) = parse_transform(None);
        assert!(t.is_none() && e.is_none());
    }

    #[test]
    fn parse_transform_without_off_defaults_to_origin() {
        let sp_pr = n(r#"<p:spPr><a:xfrm><a:ext cx="914400" cy="914400"/></a:xfrm></p:spPr>"#);
        let (t, e) = parse_transform(Some(&sp_pr));
        assert!(t.is_some());
        assert!(e.is_some());
        let p = t.unwrap().apply(ppt_core::scene::Point::ZERO);
        assert!(p.x.abs() < 1e-6 && p.y.abs() < 1e-6);
    }

    const MASTER_XML: &str = r#"<p:sldMaster xmlns:p="urn:p" xmlns:a="urn:a">
      <p:cSld>
        <p:bg><p:bgPr><a:solidFill><a:srgbClr val="F0F0F0"/></a:solidFill></p:bgPr></p:bg>
        <p:spTree>
          <p:sp>
            <p:nvSpPr><p:cNvPr id="1" name="标题占位符"/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>
            <p:spPr><a:xfrm>
              <a:off x="457200" y="274638"/><a:ext cx="8229600" cy="1143000"/>
            </a:xfrm></p:spPr>
          </p:sp>
          <p:sp>
            <p:nvSpPr><p:cNvPr id="2" name="内容占位符"/><p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr>
            <p:spPr><a:xfrm>
              <a:off x="457200" y="1600200"/><a:ext cx="8229600" cy="4525963"/>
            </a:xfrm></p:spPr>
          </p:sp>
        </p:spTree>
      </p:cSld>
      <p:txStyles>
        <p:titleStyle>
          <a:lvl1pPr algn="ctr"><a:defRPr sz="4400" b="1">
            <a:solidFill><a:srgbClr val="1F3864"/></a:solidFill>
          </a:defRPr></a:lvl1pPr>
        </p:titleStyle>
        <p:bodyStyle>
          <a:lvl1pPr><a:defRPr sz="2800"/></a:lvl1pPr>
          <a:lvl2pPr><a:defRPr sz="2400"/></a:lvl2pPr>
        </p:bodyStyle>
        <p:otherStyle><a:defPPr><a:defRPr sz="1800"/></a:defPPr></p:otherStyle>
      </p:txStyles>
    </p:sldMaster>"#;

    const LAYOUT_XML: &str = r#"<p:sldLayout xmlns:p="urn:p" xmlns:a="urn:a">
      <p:cSld>
        <p:spTree>
          <p:sp>
            <p:nvSpPr><p:cNvPr id="1" name="标题占位符"/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>
            <p:spPr><a:xfrm>
              <a:off x="609600" y="365125"/><a:ext cx="7772400" cy="1325563"/>
            </a:xfrm></p:spPr>
            <p:txBody><a:lstStyle>
              <a:lvl1pPr algn="l"><a:defRPr sz="4000"/></a:lvl1pPr>
            </a:lstStyle><a:bodyPr/></p:txBody>
          </p:sp>
        </p:spTree>
      </p:cSld>
    </p:sldLayout>"#;

    fn build() -> SlideInheritance {
        let theme = Theme::default();
        let master = n(MASTER_XML);
        let layout = n(LAYOUT_XML);
        SlideInheritance::build(
            &theme,
            Some(&master),
            Some(&layout),
            None,
            Size::new(960.0, 540.0),
            &no_rel,
        )
    }

    #[test]
    fn collects_placeholders_from_both_master_and_layout() {
        let ctx = build();
        // 母版 2 个（title, idx=1）+ 版式 1 个（title，覆盖母版的 title）
        assert!(ctx.has_placeholder_type(PlaceholderType::Title));
        assert!(ctx.placeholder(PhKey::Idx(1)).is_some());
    }

    #[test]
    fn layout_placeholder_overrides_master() {
        let ctx = build();
        // 版式的 title 位置是 609600 EMU = 48pt，母版是 457200 EMU = 36pt
        let title = ctx.placeholder(PhKey::Type(PlaceholderType::Title)).unwrap();
        let t = title.transform.expect("应有继承位置");
        let p = t.apply(ppt_core::scene::Point::ZERO);
        assert!(
            (p.x - 48.0).abs() < 1e-3,
            "应采用版式的位置（48pt），实际 {}",
            p.x
        );
    }

    #[test]
    fn master_placeholder_used_when_layout_lacks_it() {
        let ctx = build();
        // idx=1 只在母版里有
        let ph = ctx.placeholder(PhKey::Idx(1)).unwrap();
        assert_eq!(ph.idx, Some(1));
        assert!(ph.transform.is_some());
    }

    #[test]
    fn tx_styles_parsed_from_master() {
        let ctx = build();
        // 标题：44pt 粗体、居中、深蓝
        let title = &ctx.title_style;
        assert!((title.level(0).run.size_pt - 44.0).abs() < 1e-4);
        assert!(title.level(0).run.bold);
        assert_eq!(title.level(0).align, Some(ppt_core::scene::TextAlign::Center));
        assert_eq!(
            title.level(0).run.color,
            Some(Color::rgb(0x1F, 0x38, 0x64))
        );

        // 正文：一级 28pt、二级 24pt
        assert!((ctx.body_style.level(0).run.size_pt - 28.0).abs() < 1e-4);
        assert!((ctx.body_style.level(1).run.size_pt - 24.0).abs() < 1e-4);

        // 其他：18pt
        assert!((ctx.other_style.level(0).run.size_pt - 18.0).abs() < 1e-4);
    }

    #[test]
    fn tx_style_lookup_by_role_and_type() {
        let ctx = build();
        assert!((ctx.tx_style(StyleRole::Title).level(0).run.size_pt - 44.0).abs() < 1e-4);
        assert!((ctx.tx_style(StyleRole::Body).level(0).run.size_pt - 28.0).abs() < 1e-4);
        assert!((ctx.tx_style(StyleRole::Other).level(0).run.size_pt - 18.0).abs() < 1e-4);

        assert!(
            (ctx.tx_style_for(Some(PlaceholderType::Title))
                .level(0)
                .run
                .size_pt
                - 44.0)
                .abs()
                < 1e-4
        );
        assert!(
            (ctx.tx_style_for(Some(PlaceholderType::Body))
                .level(0)
                .run
                .size_pt
                - 28.0)
                .abs()
                < 1e-4
        );
        // 非占位符形状用 otherStyle
        assert!(
            (ctx.tx_style_for(None).level(0).run.size_pt - 18.0).abs() < 1e-4
        );
    }

    #[test]
    fn layout_lst_style_is_captured_for_shape() {
        let ctx = build();
        let ph = ctx.placeholder(PhKey::Type(PlaceholderType::Title)).unwrap();
        assert!(
            ph.lst_style.is_some(),
            "版式占位符的 a:lstStyle 应被继承下来"
        );
    }

    #[test]
    fn background_inherited_from_master() {
        let ctx = build();
        match ctx.background {
            Some(Fill::Solid(c)) => {
                assert_eq!(c, Color::rgb(0xF0, 0xF0, 0xF0));
            }
            other => panic!("应继承母版背景，实际 {other:?}"),
        }
    }

    #[test]
    fn layout_background_overrides_master() {
        let theme = Theme::default();
        let master = n(MASTER_XML);
        let layout = n(
            r#"<p:sldLayout xmlns:p="urn:p" xmlns:a="urn:a">
                 <p:cSld>
                   <p:bg><p:bgPr><a:solidFill><a:srgbClr val="112233"/></a:solidFill></p:bgPr></p:bg>
                   <p:spTree/>
                 </p:cSld>
               </p:sldLayout>"#,
        );
        let ctx = SlideInheritance::build(
            &theme,
            Some(&master),
            Some(&layout),
            None,
            Size::new(960.0, 540.0),
            &no_rel,
        );
        match ctx.background {
            Some(Fill::Solid(c)) => assert_eq!(c, Color::rgb(0x11, 0x22, 0x33)),
            other => panic!("版式背景应优先，实际 {other:?}"),
        }
    }

    #[test]
    fn build_without_master_or_layout_is_safe() {
        let ctx = SlideInheritance::build(
            &Theme::default(),
            None,
            None,
            None,
            Size::new(960.0, 540.0),
            &no_rel,
        );
        assert_eq!(ctx.placeholder_count(), 0);
        assert!(ctx.background.is_none());
        // 样式回退到默认值而非 panic
        assert!((ctx.title_style.level(0).run.size_pt - 18.0).abs() < 1e-4);
    }

    #[test]
    fn malformed_master_does_not_panic() {
        let ctx = SlideInheritance::build(
            &Theme::default(),
            Some(&n(r#"<p:sldMaster/>"#)),
            Some(&n(r#"<p:sldLayout/>"#)),
            None,
            Size::new(960.0, 540.0),
            &no_rel,
        );
        assert_eq!(ctx.placeholder_count(), 0);
    }

    #[test]
    fn background_bg_ref_uses_color_child() {
        let part = n(
            r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a">
                 <p:cSld>
                   <p:bg><p:bgRef idx="1001"><a:schemeClr val="accent1"/></p:bgRef></p:bg>
                   <p:spTree/>
                 </p:cSld>
               </p:sld>"#,
        );
        let scheme = ColorScheme::default();
        let fill = parse_background(&part, &scheme, &no_rel).unwrap();
        match fill {
            Fill::Solid(c) => assert_eq!(c, scheme.accent1),
            other => panic!("应解析出实心填充，实际 {other:?}"),
        }
    }

    #[test]
    fn background_without_bg_element_is_none() {
        let part = n(r#"<p:sld xmlns:p="urn:p"><p:cSld><p:spTree/></p:cSld></p:sld>"#);
        assert!(parse_background(&part, &ColorScheme::default(), &no_rel).is_none());
    }

    #[test]
    fn background_with_no_fill_is_none() {
        let part = n(
            r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a">
                 <p:cSld>
                   <p:bg><p:bgPr><a:noFill/></p:bgPr></p:bg>
                   <p:spTree/>
                 </p:cSld>
               </p:sld>"#,
        );
        assert!(parse_background(&part, &ColorScheme::default(), &no_rel).is_none());
    }

    #[test]
    fn placeholder_keys_listed_for_diagnostics() {
        let ctx = build();
        let keys = ctx.placeholder_keys();
        assert!(!keys.is_empty());
        assert!(keys.contains(&PhKey::Idx(1)));
    }

    #[test]
    fn theme_seed_uses_minor_font_and_text_color() {
        let mut theme = Theme::default();
        theme.font_scheme.minor_latin = Some("Calibri".into());
        theme.font_scheme.minor_ea = Some("等线".into());
        let scheme = ColorScheme::default();
        let seed = theme_seed_run(&theme, &scheme);
        assert_eq!(seed.font.latin.as_deref(), Some("Calibri"));
        assert_eq!(seed.font.ea.as_deref(), Some("等线"));
        assert_eq!(seed.color, Some(scheme.tx1));
    }

    #[test]
    fn ph_key_is_hashable_and_distinct() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(PhKey::Idx(1));
        set.insert(PhKey::Type(PlaceholderType::Title));
        set.insert(PhKey::Default);
        set.insert(PhKey::Idx(1));
        assert_eq!(set.len(), 3);
    }
}
