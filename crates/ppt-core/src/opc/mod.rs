//! OPC 容器层：包读取、内容类型与关系图。

pub mod package;

pub use package::{
    is_external_target, normalize_part_name, part_dir, part_extension, part_file_name,
    resolve_part_name, MmapReader, Package,
};

use std::collections::HashMap;

use crate::error::{Error, Result};
use crate::xml;

/// `[Content_Types].xml` 的内容类型表。
#[derive(Debug, Clone, Default)]
pub struct ContentTypes {
    /// 扩展名（小写，不含点） → 内容类型。
    defaults: HashMap<String, String>,
    /// 部件名（归一化，含前导 `/`） → 内容类型。
    overrides: HashMap<String, String>,
}

/// 根部件的内容类型。
pub const CT_PRESENTATION: &str =
    "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml";
pub const CT_PRESENTATION_MACRO: &str =
    "application/vnd.ms-powerpoint.presentation.macroEnabled.main+xml";
pub const CT_SLIDE_SHOW: &str =
    "application/vnd.openxmlformats-officedocument.presentationml.slideshow.main+xml";
pub const CT_TEMPLATE: &str =
    "application/vnd.openxmlformats-officedocument.presentationml.template.main+xml";
/// 启用宏的放映（`.ppsm`）。
pub const CT_SLIDE_SHOW_MACRO: &str =
    "application/vnd.ms-powerpoint.slideshow.macroEnabled.main+xml";
/// 启用宏的模板（`.potm`）。
pub const CT_TEMPLATE_MACRO: &str =
    "application/vnd.ms-powerpoint.template.macroEnabled.main+xml";

impl ContentTypes {
    /// 解析 `[Content_Types].xml`。
    pub fn parse(xml_text: &str) -> Result<ContentTypes> {
        let root = xml::parse_root("[Content_Types].xml", xml_text)?;
        let mut ct = ContentTypes::default();

        for child in &root.children {
            match child.name.as_str() {
                "Default" => {
                    if let (Some(ext), Some(ctype)) = (child.attr("Extension"), child.attr("ContentType"))
                    {
                        ct.defaults
                            .insert(ext.trim_start_matches('.').to_ascii_lowercase(), ctype.to_string());
                    }
                }
                "Override" => {
                    if let (Some(part), Some(ctype)) =
                        (child.attr("PartName"), child.attr("ContentType"))
                    {
                        ct.overrides
                            .insert(normalize_part_name(part), ctype.to_string());
                    }
                }
                _ => {}
            }
        }

        Ok(ct)
    }

    /// 查询部件的内容类型：优先 `Override`，回退到按扩展名的 `Default`。
    pub fn content_type_of(&self, part: &str) -> Option<&str> {
        let key = normalize_part_name(part);
        if let Some(ct) = self.overrides.get(&key) {
            return Some(ct.as_str());
        }
        let ext = part_extension(&key)?;
        self.defaults.get(&ext).map(|s| s.as_str())
    }

    /// 该部件是否为演示文稿主部件。
    ///
    /// OOXML 里「一份演示文稿」有六种根部件内容类型：普通演示文稿、放映、
    /// 模板，各自还有一个「启用宏」的变体。**六种都要认** ——
    /// 漏掉任何一种，老师手里那种格式的课件就会打不开，而且报的是
    /// 「该文件是 X 格式」这种让人以为格式不对的错。
    pub fn is_presentation(&self, part: &str) -> bool {
        matches!(
            self.content_type_of(part),
            Some(
                CT_PRESENTATION
                    | CT_PRESENTATION_MACRO
                    | CT_SLIDE_SHOW
                    | CT_SLIDE_SHOW_MACRO
                    | CT_TEMPLATE
                    | CT_TEMPLATE_MACRO
            )
        )
    }

    /// 按内容类型后缀筛选部件（如 `slideLayout`）。
    pub fn parts_with_type_suffix<'a>(
        &'a self,
        suffix: &'a str,
        all_parts: &'a [String],
    ) -> Vec<&'a str> {
        all_parts
            .iter()
            .filter(|p| {
                self.content_type_of(p)
                    .is_some_and(|ct| ct.ends_with(suffix))
            })
            .map(|s| s.as_str())
            .collect()
    }

    #[inline]
    pub fn override_count(&self) -> usize {
        self.overrides.len()
    }

    #[inline]
    pub fn default_count(&self) -> usize {
        self.defaults.len()
    }
}

/// 关系模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RelMode {
    #[default]
    Internal,
    External,
}

/// 一条关系。
#[derive(Debug, Clone)]
pub struct Relationship {
    pub id: String,
    /// 完整的关系类型 URI。
    pub rel_type: String,
    /// 原始 Target 值。
    pub target: String,
    /// 解析为包内部件名；外部资源为 `None`。
    pub resolved: Option<String>,
    pub mode: RelMode,
}

impl Relationship {
    /// 关系类型的末段短名，如 `slide`、`slideLayout`、`image`。
    pub fn short_type(&self) -> &str {
        part_file_name(&self.rel_type)
    }

    #[inline]
    pub fn is_external(&self) -> bool {
        self.mode == RelMode::External
    }

    /// 内部关系的部件名。
    pub fn part(&self) -> Option<&str> {
        self.resolved.as_deref()
    }
}

/// 一个部件的 `_rels` 关系集合。
#[derive(Debug, Clone, Default)]
pub struct Relationships {
    by_id: HashMap<String, Relationship>,
}

impl Relationships {
    /// 空集合（很多部件没有 `_rels`，这属正常情况）。
    pub fn empty() -> Relationships {
        Relationships::default()
    }

    /// 解析 `.rels` 部件内容。
    ///
    /// `source_part` 是**拥有这些关系的部件**，用于把相对 Target 解析成绝对部件名。
    pub fn parse(source_part: &str, xml_text: &str) -> Result<Relationships> {
        let root = xml::parse_root("<Relationships>", xml_text)?;
        let mut rels = Relationships::default();

        for child in root.children_named("Relationship") {
            let id = match child.attr("Id") {
                Some(v) => v.to_string(),
                None => continue,
            };
            let rel_type = child.attr("Type").unwrap_or_default().to_string();
            let target = child.attr("Target").unwrap_or_default().to_string();
            let mode = match child.attr("TargetMode") {
                Some(m) if m.eq_ignore_ascii_case("External") => RelMode::External,
                _ => RelMode::Internal,
            };

            let resolved = if mode == RelMode::Internal {
                resolve_part_name(source_part, &target)
            } else {
                None
            };

            rels.by_id.insert(
                id.clone(),
                Relationship {
                    id,
                    rel_type,
                    target,
                    resolved,
                    mode,
                },
            );
        }

        Ok(rels)
    }

    pub fn get(&self, id: &str) -> Option<&Relationship> {
        self.by_id.get(id)
    }

    /// 按关系类型短名查找第一条（如 `slideLayout`）。
    pub fn first_of_type(&self, short_type: &str) -> Option<&Relationship> {
        self.by_id
            .values()
            .find(|r| r.short_type() == short_type)
    }

    /// 按关系类型短名查找全部。
    pub fn all_of_type<'a>(&'a self, short_type: &'a str) -> impl Iterator<Item = &'a Relationship> {
        self.by_id.values().filter(move |r| r.short_type() == short_type)
    }

    /// 按关系类型短名收集内部部件名（保持 id 顺序以稳定输出）。
    pub fn parts_of_type(&self, short_type: &str) -> Vec<&str> {
        let mut items: Vec<&Relationship> = self
            .by_id
            .values()
            .filter(|r| r.short_type() == short_type)
            .collect();
        items.sort_by(|a, b| compare_rel_id(&a.id, &b.id));
        items.into_iter().filter_map(|r| r.part()).collect()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// 合并另一组关系（用于把布局/母版的关系并入场景图的查找表）。
    pub fn extend(&mut self, other: Relationships) {
        self.by_id.extend(other.by_id);
    }
}

/// 关系 id 排序：`rId2` 应排在 `rId10` 之前。
fn compare_rel_id(a: &str, b: &str) -> std::cmp::Ordering {
    let num = |s: &str| -> Option<u64> {
        let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
        digits.parse().ok()
    };
    match (num(a), num(b)) {
        (Some(x), Some(y)) => x.cmp(&y).then_with(|| a.cmp(b)),
        _ => a.cmp(b),
    }
}

/// 计算某个部件的 `.rels` 部件名。
///
/// 规则（OPC 规范）：
/// - 包根关系表为 `_rels/.rels`；
/// - 部件 `dir/name.ext` 的关系表为 `dir/_rels/name.ext.rels`。
pub fn rels_part_for(part: &str) -> String {
    let normalized = normalize_part_name(part);
    if normalized.is_empty() {
        return "_rels/.rels".to_string();
    }
    let dir = part_dir(&normalized);
    let file = part_file_name(&normalized);
    if dir.is_empty() {
        format!("_rels/{file}.rels")
    } else {
        format!("{dir}_rels/{file}.rels")
    }
}

/// 常用关系类型短名。
pub mod rel_type {
    pub const OFFICE_DOCUMENT: &str = "officeDocument";
    pub const SLIDE: &str = "slide";
    pub const SLIDE_LAYOUT: &str = "slideLayout";
    pub const SLIDE_MASTER: &str = "slideMaster";
    pub const NOTES_SLIDE: &str = "notesSlide";
    pub const NOTES_MASTER: &str = "notesMaster";
    pub const HANDOUT_MASTER: &str = "handoutMaster";
    pub const THEME: &str = "theme";
    pub const IMAGE: &str = "image";
    pub const HYPERLINK: &str = "hyperlink";
    pub const CHART: &str = "chart";
    pub const OLE_OBJECT: &str = "oleObject";
    pub const MEDIA: &str = "media";
    pub const VIDEO: &str = "video";
    pub const AUDIO: &str = "audio";
    pub const TABLE_STYLES: &str = "tableStyles";
    pub const PRES_PROPS: &str = "presProps";
    pub const VIEW_PROPS: &str = "viewProps";
}

/// 读取一个部件的关系集合；部件没有 `.rels` 时返回空集合。
pub fn load_relationships(pkg: &Package, part: &str) -> Result<Relationships> {
    let rels_part = rels_part_for(part);
    if !pkg.contains(&rels_part) {
        return Ok(Relationships::empty());
    }
    let xml_text = pkg.read_part_str(&rels_part)?;
    Relationships::parse(part, &xml_text)
}

/// 读取包根关系，并定位演示文稿主部件。
pub fn find_main_document_part(pkg: &Package, content_types: &ContentTypes) -> Result<String> {
    // 首选：包根关系中的 officeDocument
    if pkg.contains("_rels/.rels") {
        let rels = load_relationships(pkg, "")?;
        if let Some(rel) = rels.first_of_type(rel_type::OFFICE_DOCUMENT) {
            if let Some(part) = rel.part() {
                return Ok(part.to_string());
            }
        }
    }

    // 兜底：按内容类型扫描（部分第三方工具生成的包缺少根关系）
    for name in pkg.entry_names() {
        if content_types.is_presentation(name) {
            return Ok(name.clone());
        }
    }

    Err(Error::MissingPart(
        "找不到演示文稿主部件（presentation.xml）".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CT_XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Default Extension="png" ContentType="image/png"/>
  <Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
  <Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
</Types>"#;

    fn sample_ct() -> ContentTypes {
        ContentTypes::parse(CT_XML).unwrap()
    }

    #[test]
    fn content_types_defaults_and_overrides() {
        let ct = sample_ct();
        assert_eq!(ct.default_count(), 3);
        assert_eq!(ct.override_count(), 2);
        assert_eq!(
            ct.content_type_of("ppt/media/image1.png"),
            Some("image/png")
        );
        assert_eq!(
            ct.content_type_of("/ppt/slides/slide1.xml"),
            Some("application/vnd.openxmlformats-officedocument.presentationml.slide+xml")
        );
        // 未声明的扩展名无从推断
        assert_eq!(ct.content_type_of("ppt/media/weird.bin"), None);
    }

    #[test]
    fn content_type_lookup_is_case_insensitive_on_part_name() {
        let ct = sample_ct();
        assert!(ct.content_type_of("PPT/Slides/Slide1.XML").is_some());
    }

    #[test]
    fn presentation_main_part_recognized() {
        let ct = sample_ct();
        assert!(ct.is_presentation("ppt/presentation.xml"));
        assert!(!ct.is_presentation("ppt/slides/slide1.xml"));
    }

    #[test]
    fn rels_part_name_computation() {
        assert_eq!(rels_part_for("ppt/slides/slide1.xml"), "ppt/slides/_rels/slide1.xml.rels");
        assert_eq!(rels_part_for("presentation.xml"), "_rels/presentation.xml.rels");
        assert_eq!(rels_part_for(""), "_rels/.rels");
        assert_eq!(rels_part_for("/ppt/presentation.xml"), "ppt/_rels/presentation.xml.rels");
    }

    #[test]
    fn parse_relationships_resolves_relative_targets() {
        let rels_xml = r#"<?xml version="1.0"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image1.png"/>
</Relationships>"#;
        let rels = Relationships::parse("ppt/slides/slide1.xml", rels_xml).unwrap();
        assert_eq!(rels.len(), 2);

        let layout = rels.get("rId1").unwrap();
        assert_eq!(layout.short_type(), "slideLayout");
        assert_eq!(layout.part(), Some("ppt/slidelayouts/slidelayout1.xml"));
        assert!(!layout.is_external());

        assert_eq!(
            rels.first_of_type("image").unwrap().part(),
            Some("ppt/media/image1.png")
        );
    }

    #[test]
    fn external_relationship_has_no_part() {
        let rels_xml = r#"<Relationships>
  <Relationship Id="rId5" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.com" TargetMode="External"/>
</Relationships>"#;
        let rels = Relationships::parse("ppt/slides/slide1.xml", rels_xml).unwrap();
        let r = rels.get("rId5").unwrap();
        assert!(r.is_external());
        assert_eq!(r.part(), None);
        assert_eq!(r.target, "https://example.com");
    }

    #[test]
    fn parts_of_type_sorted_by_rel_id_numerically() {
        let mut xml = String::from("<Relationships>");
        for i in [10, 2, 1] {
            xml.push_str(&format!(
                r#"<Relationship Id="rId{i}" Type="http://x/slide" Target="slide{i}.xml"/>"#
            ));
        }
        xml.push_str("</Relationships>");
        let rels = Relationships::parse("ppt/presentation.xml", &xml).unwrap();
        let parts = rels.parts_of_type("slide");
        assert_eq!(
            parts,
            vec![
                "ppt/slide1.xml",
                "ppt/slide2.xml",
                "ppt/slide10.xml"
            ]
        );
    }

    #[test]
    fn empty_relationships_is_ok() {
        let rels = Relationships::parse("x.xml", r#"<Relationships/>"#).unwrap();
        assert!(rels.is_empty());
        assert_eq!(rels.len(), 0);
    }

    #[test]
    fn rel_id_comparison_handles_non_numeric() {
        assert_eq!(
            compare_rel_id("rId2", "rId10"),
            std::cmp::Ordering::Less
        );
        // 一方不含数字时退化为字典序
        assert_eq!(compare_rel_id("abc", "rId1"), std::cmp::Ordering::Less);
        assert_eq!(compare_rel_id("rId1", "abc"), std::cmp::Ordering::Greater);
    }
}
