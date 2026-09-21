//! 主题解析：`ppt/theme/themeN.xml`。
//!
//! 主题提供三类信息：
//!
//! - **颜色方案**（`a:clrScheme`）→ 见 [`crate::color::ColorScheme`]
//! - **字体方案**（`a:fontScheme`）→ 本文档的 [`FontScheme`]
//! - **格式方案**（`a:fmtScheme`）→ 填充/线条/效果的样式列表，由 `a:fillRef`
//!   / `a:lnRef` / `a:effectRef` 按索引引用
//!
//! 格式方案的三个列表（`fillStyleLst`、`lnStyleLst`、`effectStyleLst`）
//! 在 OOXML 中是**按位置索引**引用的：`a:fillRef idx="3"` 表示
//! 使用第 3 个填充样式。因此这里保留原始 XML 节点，由引用方按需解析 ——
//! 避免为了少数几个形状把所有样式都解析成结构体。

use ppt_core::XmlNode;
use ppt_text::fonts::ThemeFontRef;

use crate::color::{ClrMap, ColorScheme};

/// 字体方案。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FontScheme {
    pub name: Option<String>,
    /// 标题字体（`a:majorFont`）。
    pub major_latin: Option<String>,
    pub major_ea: Option<String>,
    pub major_cs: Option<String>,
    /// 正文字体（`a:minorFont`）。
    pub minor_latin: Option<String>,
    pub minor_ea: Option<String>,
    pub minor_cs: Option<String>,
}

impl FontScheme {
    /// 从主题 XML 的 `a:fontScheme` 解析。
    pub fn parse(font_scheme: &XmlNode) -> FontScheme {
        let read = |container: &str| -> (Option<String>, Option<String>, Option<String>) {
            let Some(node) = font_scheme.child(container) else {
                return (None, None, None);
            };
            let get = |tag: &str| -> Option<String> {
                node.child(tag)
                    .and_then(|n| n.attr("typeface"))
                    .filter(|t| !t.is_empty())
                    .map(|t| t.to_string())
            };
            (get("latin"), get("ea"), get("cs"))
        };

        let (major_latin, major_ea, major_cs) = read("majorFont");
        let (minor_latin, minor_ea, minor_cs) = read("minorFont");

        FontScheme {
            name: font_scheme.attr("name").map(|s| s.to_string()),
            major_latin,
            major_ea,
            major_cs,
            minor_latin,
            minor_ea,
            minor_cs,
        }
    }

    /// 按 `+mj-lt` / `+mn-ea` 这类主题字体引用取值。
    ///
    /// 课件的 `a:latin typeface="+mn-lt"` 必须在这里解析成具体字体名，
    /// 后续的排版引擎只认具体名，不再理解主题引用。
    pub fn resolve_ref(&self, r: ThemeFontRef) -> Option<&str> {
        use ThemeFontRef as R;
        match r {
            R::MajorLatin => self.major_latin.as_deref(),
            R::MajorEa => self.major_ea.as_deref(),
            R::MajorCs => self.major_cs.as_deref(),
            R::MinorLatin => self.minor_latin.as_deref(),
            R::MinorEa => self.minor_ea.as_deref(),
            R::MinorCs => self.minor_cs.as_deref(),
        }
    }

    /// 按 `typeface` 字符串解析主题字体引用；非 `+` 开头的名字返回 `None`。
    pub fn resolve_typeface(&self, typeface: &str) -> Option<&str> {
        ThemeFontRef::parse(typeface).and_then(|r| self.resolve_ref(r))
    }

    /// 是否所有字体都未指定。
    pub fn is_empty(&self) -> bool {
        self.major_latin.is_none()
            && self.major_ea.is_none()
            && self.minor_latin.is_none()
            && self.minor_ea.is_none()
    }
}

/// 一个完整的主题。
#[derive(Debug, Clone)]
pub struct Theme {
    pub color_scheme: ColorScheme,
    pub font_scheme: FontScheme,
    /// `a:fmtScheme` 的原始节点，供 `a:fillRef` 等按索引引用。
    fmt_scheme: Option<XmlNode>,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            color_scheme: ColorScheme::default(),
            font_scheme: FontScheme::default(),
            fmt_scheme: None,
        }
    }
}

impl Theme {
    /// 从 `ppt/theme/themeN.xml` 的根节点解析。
    ///
    /// `clr_map` 来自母版的 `p:clrMap`，决定 `bg1`/`tx1` 等语义槽
    /// 最终指向主题里的哪个颜色。
    pub fn parse(root: &XmlNode, clr_map: Option<&ClrMap>) -> Theme {
        let elements = root.find_descendant("themeElements");

        let Some(elements) = elements else {
            return Theme::default();
        };

        let color_scheme = elements
            .child("clrScheme")
            .map(|cs| ColorScheme::from_theme_xml(cs, clr_map))
            .unwrap_or_default();

        let font_scheme = elements
            .child("fontScheme")
            .map(FontScheme::parse)
            .unwrap_or_default();

        Theme {
            color_scheme,
            font_scheme,
            fmt_scheme: elements.child("fmtScheme").cloned(),
        }
    }

    /// 主题是否成功加载（用于诊断日志）。
    pub fn is_loaded(&self) -> bool {
        self.fmt_scheme.is_some()
    }

    /// 解析 `a:fillRef`，返回填充样式列表中的第 `idx` 项。
    ///
    /// OOXML 的索引从 1 开始；`idx="0"` 表示「不引用主题样式」。
    ///
    /// 返回的节点**本身就是填充元素**（`a:solidFill` / `a:gradFill` 等），
    /// 可以直接喂给 [`crate::paint::parse_fill`] 的同级解析逻辑。
    pub fn fill_style(&self, idx: u32) -> Option<&XmlNode> {
        self.style_at("fillStyleLst", idx)
    }

    /// 解析 `a:lnRef`。返回的节点是 `a:ln`。
    pub fn line_style(&self, idx: u32) -> Option<&XmlNode> {
        self.style_at("lnStyleLst", idx)
    }

    /// 解析 `a:effectRef`。
    ///
    /// 注意与填充/线条不同：这里返回的是 `a:effectStyle` **包装节点**，
    /// 其中的 `a:effectLst` 才是实际效果列表。
    pub fn effect_style(&self, idx: u32) -> Option<&XmlNode> {
        self.style_at("effectStyleLst", idx)
    }

    /// 背景填充样式（`a:bgFillStyleLst`）。返回的节点是填充元素本身。
    pub fn background_fill_style(&self, idx: u32) -> Option<&XmlNode> {
        self.style_at("bgFillStyleLst", idx)
    }

    fn style_at(&self, list_name: &str, idx: u32) -> Option<&XmlNode> {
        if idx == 0 {
            return None;
        }
        let list = self.fmt_scheme.as_ref()?.child(list_name)?;
        list.children.get(idx as usize - 1)
    }

    /// 字体方案非空时返回其引用。
    pub fn fonts(&self) -> &FontScheme {
        &self.font_scheme
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ppt_core::xml;

    fn n(xml_text: &str) -> XmlNode {
        xml::parse_root("t", xml_text).unwrap()
    }

    const THEME_XML: &str = r#"<a:theme xmlns:a="urn:a" name="Office 主题">
      <a:themeElements>
        <a:clrScheme name="Office">
          <a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1>
          <a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1>
          <a:dk2><a:srgbClr val="44546A"/></a:dk2>
          <a:lt2><a:srgbClr val="E7E6E6"/></a:lt2>
          <a:accent1><a:srgbClr val="4472C4"/></a:accent1>
          <a:accent2><a:srgbClr val="ED7D31"/></a:accent2>
          <a:accent3><a:srgbClr val="A5A5A5"/></a:accent3>
          <a:accent4><a:srgbClr val="FFC000"/></a:accent4>
          <a:accent5><a:srgbClr val="5B9BD5"/></a:accent5>
          <a:accent6><a:srgbClr val="70AD47"/></a:accent6>
          <a:hlink><a:srgbClr val="0563C1"/></a:hlink>
          <a:folHlink><a:srgbClr val="954F72"/></a:folHlink>
        </a:clrScheme>
        <a:fontScheme name="Office">
          <a:majorFont>
            <a:latin typeface="Calibri Light"/>
            <a:ea typeface=""/>
            <a:cs typeface=""/>
          </a:majorFont>
          <a:minorFont>
            <a:latin typeface="Calibri"/>
            <a:ea typeface="等线"/>
            <a:cs typeface=""/>
          </a:minorFont>
        </a:fontScheme>
        <a:fmtScheme name="Office">
          <a:fillStyleLst>
            <a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
            <a:solidFill><a:schemeClr val="phClr"><a:tint val="50000"/></a:schemeClr></a:solidFill>
            <a:gradFill><a:gsLst><a:gs pos="0"><a:schemeClr val="phClr"/></a:gs></a:gsLst></a:gradFill>
          </a:fillStyleLst>
          <a:lnStyleLst>
            <a:ln w="6350"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln>
            <a:ln w="12700"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln>
            <a:ln w="19050"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln>
          </a:lnStyleLst>
          <a:effectStyleLst>
            <a:effectStyle><a:effectLst/></a:effectStyle>
            <a:effectStyle><a:effectLst>
              <a:outerShdw blurRad="40000" dist="20000" dir="5400000"/>
            </a:effectLst></a:effectStyle>
          </a:effectStyleLst>
          <a:bgFillStyleLst>
            <a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
          </a:bgFillStyleLst>
        </a:fmtScheme>
      </a:themeElements>
    </a:theme>"#;

    fn theme() -> Theme {
        Theme::parse(&n(THEME_XML), None)
    }

    #[test]
    fn parses_color_scheme() {
        let t = theme();
        assert_eq!(t.color_scheme.bg1, ppt_core::scene::Color::WHITE);
        assert_eq!(t.color_scheme.tx1, ppt_core::scene::Color::BLACK);
        assert_eq!(
            t.color_scheme.accent1,
            ppt_core::scene::Color::rgb(0x44, 0x72, 0xC4)
        );
    }

    #[test]
    fn parses_font_scheme() {
        let t = theme();
        assert_eq!(t.font_scheme.major_latin.as_deref(), Some("Calibri Light"));
        assert_eq!(t.font_scheme.minor_latin.as_deref(), Some("Calibri"));
        assert_eq!(t.font_scheme.minor_ea.as_deref(), Some("等线"));
        // 空 typeface 应视为未指定，否则会去查一个不存在的字体名
        assert_eq!(t.font_scheme.major_ea, None);
        assert_eq!(t.font_scheme.major_cs, None);
        assert_eq!(t.font_scheme.name.as_deref(), Some("Office"));
    }

    #[test]
    fn theme_is_loaded_when_fmt_scheme_present() {
        assert!(theme().is_loaded());
    }

    #[test]
    fn fill_style_indexing_is_one_based() {
        let t = theme();
        // idx=0 表示不引用
        assert!(t.fill_style(0).is_none());
        assert!(t.fill_style(1).is_some());
        assert!(t.fill_style(2).is_some());
        assert!(t.fill_style(3).is_some());
        // 越界返回 None 而非 panic
        assert!(t.fill_style(99).is_none());
    }

    #[test]
    fn fill_style_content_matches_index() {
        let t = theme();
        // 列表项本身就是填充元素，没有包装层
        let first = t.fill_style(1).unwrap();
        assert_eq!(first.name, "solidFill");

        // 第 2 项是带 tint 的实心填充
        let second = t.fill_style(2).unwrap();
        assert_eq!(second.name, "solidFill");
        assert!(
            second.child("schemeClr").unwrap().has_child("tint"),
            "第 2 项应带 tint 变换"
        );

        // 第 3 项是渐变填充
        assert_eq!(t.fill_style(3).unwrap().name, "gradFill");
    }

    #[test]
    fn effect_style_is_wrapped_unlike_fill_and_line() {
        let t = theme();
        // 效果样式有 effectStyle 包装层，填充与线条没有
        assert_eq!(t.effect_style(1).unwrap().name, "effectStyle");
        assert_eq!(t.line_style(1).unwrap().name, "ln");
        assert_eq!(t.background_fill_style(1).unwrap().name, "solidFill");
    }

    #[test]
    fn line_style_widths_are_ordered() {
        let t = theme();
        let w = |i: u32| t.line_style(i).unwrap().attr_f64("w").unwrap();
        assert!(w(1) < w(2), "线宽应随索引递增");
        assert!(w(2) < w(3));
        assert_eq!(w(1), 6350.0);
    }

    #[test]
    fn effect_style_includes_empty_first_entry() {
        let t = theme();
        // 主题的第一个效果样式通常是空的，这是规范惯例
        assert!(t.effect_style(1).unwrap().child("effectLst").unwrap().is_empty_element());
        // 第二个包含外阴影
        assert!(
            t.effect_style(2)
                .unwrap()
                .child("effectLst")
                .unwrap()
                .has_child("outerShdw")
        );
    }

    #[test]
    fn background_fill_style_available() {
        let t = theme();
        assert!(t.background_fill_style(1).is_some());
        assert!(t.background_fill_style(0).is_none());
    }

    #[test]
    fn clr_map_affects_color_scheme() {
        let map = ClrMap {
            bg1: "dk1".into(),
            tx1: "lt1".into(),
            ..ClrMap::default()
        };
        let t = Theme::parse(&n(THEME_XML), Some(&map));
        // 反转映射后：bg1 取 dk1（黑），tx1 取 lt1（白）
        assert_eq!(t.color_scheme.bg1, ppt_core::scene::Color::BLACK);
        assert_eq!(t.color_scheme.tx1, ppt_core::scene::Color::WHITE);
    }

    #[test]
    fn missing_theme_elements_yields_default_theme() {
        let t = Theme::parse(&n(r#"<a:theme/>"#), None);
        assert!(!t.is_loaded());
        // 回退到 Office 默认配色，而不是全黑
        assert_eq!(t.color_scheme.bg1, ppt_core::scene::Color::WHITE);
        assert!(t.fill_style(1).is_none());
    }

    #[test]
    fn theme_without_fmt_scheme_still_has_colors() {
        let t = Theme::parse(
            &n(
                r#"<a:theme><a:themeElements>
                     <a:clrScheme name="x">
                       <a:dk1><a:srgbClr val="000000"/></a:dk1>
                       <a:lt1><a:srgbClr val="FFFFFF"/></a:lt1>
                       <a:dk2><a:srgbClr val="111111"/></a:dk2>
                       <a:lt2><a:srgbClr val="EEEEEE"/></a:lt2>
                       <a:accent1><a:srgbClr val="FF0000"/></a:accent1>
                       <a:accent2><a:srgbClr val="00FF00"/></a:accent2>
                       <a:accent3><a:srgbClr val="0000FF"/></a:accent3>
                       <a:accent4><a:srgbClr val="FFFF00"/></a:accent4>
                       <a:accent5><a:srgbClr val="FF00FF"/></a:accent5>
                       <a:accent6><a:srgbClr val="00FFFF"/></a:accent6>
                       <a:hlink><a:srgbClr val="0000EE"/></a:hlink>
                       <a:folHlink><a:srgbClr val="551A8B"/></a:folHlink>
                     </a:clrScheme>
                   </a:themeElements></a:theme>"#,
            ),
            None,
        );
        assert!(!t.is_loaded(), "缺少 fmtScheme 时不应报告已加载");
        assert_eq!(
            t.color_scheme.accent1,
            ppt_core::scene::Color::rgb(255, 0, 0)
        );
    }

    #[test]
    fn font_scheme_resolve_ref() {
        let t = theme();
        assert_eq!(t.font_scheme.resolve_ref(ThemeFontRef::MajorLatin), Some("Calibri Light"));
        assert_eq!(t.font_scheme.resolve_ref(ThemeFontRef::MinorEa), Some("等线"));
        // 空字体名已被过滤，解析 ref 返回 None
        assert_eq!(t.font_scheme.resolve_ref(ThemeFontRef::MajorEa), None);
    }

    #[test]
    fn resolve_typeface_by_string() {
        let t = theme();
        assert_eq!(t.font_scheme.resolve_typeface("+mj-lt"), Some("Calibri Light"));
        assert_eq!(t.font_scheme.resolve_typeface("+mn-ea"), Some("等线"));
        // 非主题引用不参与解析
        assert_eq!(t.font_scheme.resolve_typeface("Arial"), None);
        assert_eq!(t.font_scheme.resolve_typeface("+mj-ea"), None);
    }

    #[test]
    fn font_scheme_without_containers_is_empty() {
        let fs = FontScheme::parse(&n(r#"<a:fontScheme name="empty"/>"#));
        assert!(fs.is_empty());
        assert_eq!(fs.name.as_deref(), Some("empty"));
    }

    #[test]
    fn default_theme_has_office_colors() {
        let t = Theme::default();
        assert_eq!(t.color_scheme.accent1, ppt_core::scene::Color::rgb(0x44, 0x72, 0xC4));
        assert!(t.font_scheme.is_empty());
        assert!(!t.is_loaded());
    }

    #[test]
    fn finds_theme_elements_deeply_nested() {
        // 有些工具会在 themeElements 外面包一层
        let t = Theme::parse(
            &n(
                r#"<a:theme><a:extra><a:themeElements>
                     <a:clrScheme name="x">
                       <a:dk1><a:srgbClr val="000000"/></a:dk1>
                       <a:lt1><a:srgbClr val="FFFFFF"/></a:lt1>
                       <a:dk2><a:srgbClr val="111111"/></a:dk2>
                       <a:lt2><a:srgbClr val="222222"/></a:lt2>
                       <a:accent1><a:srgbClr val="ABCDEF"/></a:accent1>
                       <a:accent2><a:srgbClr val="111111"/></a:accent2>
                       <a:accent3><a:srgbClr val="222222"/></a:accent3>
                       <a:accent4><a:srgbClr val="333333"/></a:accent4>
                       <a:accent5><a:srgbClr val="444444"/></a:accent5>
                       <a:accent6><a:srgbClr val="555555"/></a:accent6>
                       <a:hlink><a:srgbClr val="0000EE"/></a:hlink>
                       <a:folHlink><a:srgbClr val="551A8B"/></a:folHlink>
                     </a:clrScheme>
                   </a:themeElements></a:extra></a:theme>"#,
            ),
            None,
        );
        assert_eq!(
            t.color_scheme.accent1,
            ppt_core::scene::Color::rgb(0xAB, 0xCD, 0xEF)
        );
    }
}
