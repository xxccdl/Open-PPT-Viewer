//! `PptxSource`：把 `.pptx` 接入格式无关的 [`DocumentSource`] 抽象。
//!
//! # 惰性解析的落点
//!
//! - [`PptxSource::open`] 只解析**文档级**信息：内容类型表、`presentation.xml`、
//!   幻灯片顺序与尺寸。**不触碰任何 `slideN.xml`，更不读取媒体**。
//! - [`PptxSource::page_content`] 是唯一会解析页面的入口。
//!   它按需读取该页的 slide XML、其 `_rels`、引用的版式与母版、以及主题。
//!
//! # 版式与主题的缓存
//!
//! 一份课件里所有幻灯片通常只共用 1~3 个版式，而每个版式都要解析
//! 母版与主题（XML 体积远大于单页）。若每页都重新解析，翻页会明显变慢。
//! 因此这里按「版式部件名」缓存整条继承链，
//! 用 `Mutex` 保护以便多线程渲染时安全复用。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

use ppt_core::opc::{self, rel_type, ContentTypes, Package, Relationships};
use ppt_core::scene::{Color, Fill, Rect, Size};
use ppt_core::{units, DocFormat, DocumentSource, Error, MediaProvider, PageContent, Result};
use ppt_core::xml::{self, XmlNode};

use crate::color::ClrMap;
use crate::inherit::SlideInheritance;
use crate::shape;
use crate::theme::Theme;

/// 文档级解析结果（open 阶段一次算好）。
struct DocInfo {
    presentation_part: String,
    /// 按顺序排列的幻灯片部件名。
    slide_parts: Vec<String>,
    /// 每张幻灯片对应的版式部件名（与 `slide_parts` 等长）。
    layout_parts: Vec<Option<String>>,
    /// 每张幻灯片对应的备注页部件名。
    notes_parts: Vec<Option<String>>,
    slide_size_pt: Size,
    title: Option<String>,
}

/// 一个已打开的 PPTX 文档。
pub struct PptxSource {
    package: Arc<Package>,
    content_types: ContentTypes,
    info: DocInfo,
    /// 版式部件名 → 已展平的继承上下文。
    inherit_cache: Mutex<HashMap<String, Arc<SlideInheritance>>>,
    /// 演示文稿级的关系表（用于解析超链接等）。
    pres_rels: Arc<Relationships>,
    path: PathBuf,
    fingerprint: String,
}

impl std::fmt::Debug for PptxSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PptxSource")
            .field("path", &self.path)
            .field("slides", &self.info.slide_parts.len())
            .field("size_pt", &self.info.slide_size_pt)
            .finish()
    }
}

/// 16:9 的默认幻灯片尺寸（960×540 pt），在 `p:sldSz` 缺失时使用。
const DEFAULT_SLIDE_SIZE: Size = Size {
    w: 960.0,
    h: 540.0,
};

impl PptxSource {
    /// 打开一个 `.pptx` 文件。
    ///
    /// 只解析文档级信息，不解析任何页面。
    pub fn open(path: impl AsRef<Path>) -> Result<PptxSource> {
        let path = path.as_ref().to_path_buf();
        let package = Arc::new(Package::open(&path)?);

        // 1) 内容类型表
        let content_types = if package.contains("[Content_Types].xml") {
            ContentTypes::parse(&package.read_part_str("[Content_Types].xml")?)?
        } else {
            return Err(Error::MissingPart(
                "缺少 [Content_Types].xml，不是有效的 Office 文档".to_string(),
            ));
        };

        // 2) 主部件
        let presentation_part = opc::find_main_document_part(&package, &content_types)?;

        if !content_types.is_presentation(&presentation_part) {
            // 容器合法但不是演示文稿（如 docx / xlsx）
            return Err(Error::UnsupportedFormat(format!(
                "该文件是 {} 格式，当前版本仅支持 .pptx 与 .pdf",
                describe_non_presentation(&content_types, &presentation_part)
            )));
        }

        // 3) 演示文稿关系与内容
        let pres_rels = Arc::new(opc::load_relationships(&package, &presentation_part)?);
        let pres_xml = package.read_part_str(&presentation_part)?;
        let pres_root = xml::parse_root(&presentation_part, &pres_xml)?;

        let slide_parts = collect_slide_order(&pres_root, &pres_rels);

        if slide_parts.is_empty() {
            return Err(Error::Container(
                "演示文稿中没有任何幻灯片".to_string(),
            ));
        }

        // 4) 每页的版式与备注关系（只读各自的 .rels，不读 slide XML）
        let mut layout_parts = Vec::with_capacity(slide_parts.len());
        let mut notes_parts = Vec::with_capacity(slide_parts.len());
        for part in &slide_parts {
            let rels = opc::load_relationships(&package, part).unwrap_or_else(|_| Relationships::empty());
            layout_parts.push(
                rels.first_of_type(rel_type::SLIDE_LAYOUT)
                    .and_then(|r| r.part())
                    .map(|s| s.to_string()),
            );
            notes_parts.push(
                rels.first_of_type(rel_type::NOTES_SLIDE)
                    .and_then(|r| r.part())
                    .map(|s| s.to_string()),
            );
        }

        // 5) 尺寸与标题
        let slide_size_pt = parse_slide_size(&pres_root);
        let title = read_core_title(&package);

        let fingerprint = compute_fingerprint(&package, &presentation_part);

        Ok(PptxSource {
            package,
            content_types,
            info: DocInfo {
                presentation_part,
                slide_parts,
                layout_parts,
                notes_parts,
                slide_size_pt,
                title,
            },
            inherit_cache: Mutex::new(HashMap::new()),
            pres_rels,
            path,
            fingerprint,
        })
    }

    /// 内部包句柄（供诊断与测试使用）。
    pub fn package(&self) -> &Arc<Package> {
        &self.package
    }

    /// 打开阶段读取的字节数（仅中央目录 + 文档级部件）。
    pub fn open_read_bytes(&self) -> u64 {
        self.package.open_read_bytes()
    }

    /// 累计读取字节数。
    pub fn bytes_read(&self) -> u64 {
        self.package.bytes_read()
    }

    /// 主部件名。
    pub fn presentation_part(&self) -> &str {
        &self.info.presentation_part
    }

    /// 全部幻灯片部件名（按放映顺序）。
    pub fn slide_parts(&self) -> &[String] {
        &self.info.slide_parts
    }

    /// 取某页的版式部件名。
    pub fn layout_part(&self, index: usize) -> Option<&str> {
        self.info.layout_parts.get(index).and_then(|v| v.as_deref())
    }

    /// 已缓存的继承链数量。
    ///
    /// 用于验证「同一版式的多页只解析一次主题与母版」这一性能约定。
    pub fn cached_inheritance_count(&self) -> usize {
        self.inherit_cache
            .lock()
            .map(|c| c.len())
            .unwrap_or(0)
    }

    /// 建立（或从缓存取）某页的继承上下文。
    fn inheritance_for(&self, index: usize) -> Result<Arc<SlideInheritance>> {
        let layout_part = self
            .info
            .layout_parts
            .get(index)
            .and_then(|v| v.clone());

        // 没有版式的幻灯片（极少数工具生成）直接用主题默认值
        let cache_key = layout_part.clone().unwrap_or_else(|| "<none>".to_string());

        if let Ok(cache) = self.inherit_cache.lock() {
            if let Some(hit) = cache.get(&cache_key) {
                return Ok(Arc::clone(hit));
            }
        }

        let ctx = self.build_inheritance(layout_part.as_deref())?;
        let ctx = Arc::new(ctx);

        if let Ok(mut cache) = self.inherit_cache.lock() {
            cache.insert(cache_key, Arc::clone(&ctx));
        }
        Ok(ctx)
    }

    /// 从版式向上追踪母版与主题，构建继承上下文。
    fn build_inheritance(&self, layout_part: Option<&str>) -> Result<SlideInheritance> {
        // 解析器：把关系 id 解析为部件名
        let resolve_owned = |part: &str| -> Relationships {
            opc::load_relationships(&self.package, part).unwrap_or_else(|_| Relationships::empty())
        };

        // --- 版式 ---
        let layout_node = match layout_part {
            Some(p) => {
                let text = self.package.read_part_str(p)?;
                Some(xml::parse_root(p, &text)?)
            }
            None => None,
        };
        let layout_rels = layout_part.map(resolve_owned).unwrap_or_default();

        // --- 母版 ---
        let master_part = layout_rels
            .first_of_type(rel_type::SLIDE_MASTER)
            .and_then(|r| r.part())
            .map(|s| s.to_string());

        let master_node = match &master_part {
            Some(p) => {
                let text = self.package.read_part_str(p)?;
                Some(xml::parse_root(p, &text)?)
            }
            None => None,
        };
        let master_rels = master_part
            .as_deref()
            .map(resolve_owned)
            .unwrap_or_default();

        // 颜色映射来自母版
        let clr_map = master_node
            .as_ref()
            .and_then(|m| m.child("clrMap"))
            .map(ClrMap::parse);

        // --- 主题 ---
        // 优先用母版引用的主题；母版缺失时回退到版式引用的
        let theme_part = master_rels
            .first_of_type(rel_type::THEME)
            .and_then(|r| r.part())
            .map(|s| s.to_string())
            .or_else(|| {
                layout_rels
                    .first_of_type(rel_type::THEME)
                    .and_then(|r| r.part())
                    .map(|s| s.to_string())
            });

        let theme = match &theme_part {
            Some(p) => {
                let text = self.package.read_part_str(p)?;
                let root = xml::parse_root(p, &text)?;
                Theme::parse(&root, clr_map.as_ref())
            }
            None => Theme::default(),
        };

        // 关系解析闭包：当前页的关系表在 page_content 里才需要，
        // 这里解析版式/母版层级的关系（如 `a:blip` 指向的媒体）
        let resolve_ctx = MoveRelResolver::new(&layout_rels, &master_rels, &self.pres_rels);
        // 必须用具名绑定：闭包借用了 resolve_ctx，
        // 若直接写在尾表达式中会成为临时值，在 resolve_ctx 之前被释放
        let resolve_fn = |id: &str| resolve_ctx.resolve(id);

        let ctx = SlideInheritance::build(
            &theme,
            master_node.as_ref(),
            layout_node.as_ref(),
            clr_map.as_ref(),
            self.info.slide_size_pt,
            &resolve_fn,
        );
        Ok(ctx)
    }

    /// 解析某页备注。
    fn notes_text(&self, index: usize) -> Result<Option<String>> {
        let Some(part) = self.info.notes_parts.get(index).and_then(|v| v.as_deref()) else {
            return Ok(None);
        };
        let text = self.package.read_part_str(part)?;
        let root = xml::parse_root(part, &text)?;
        Ok(shape::parse_notes(&root))
    }
}

/// 在多个关系表之间做「首个命中」查询的解析器。
///
/// 幻灯片的关系分散在 slide / layout / master / presentation 四张表里，
/// 而 `a:blip`、`a:hlinkClick` 只写 `r:id`，不指明属于哪张表。
/// OOXML 规定 id 在各自部件内唯一，因此按「由近及远」的顺序查找即可。
struct MoveRelResolver<'a> {
    tables: Vec<&'a Relationships>,
}

impl<'a> MoveRelResolver<'a> {
    fn new(
        layout: &'a Relationships,
        master: &'a Relationships,
        pres: &'a Relationships,
    ) -> MoveRelResolver<'a> {
        MoveRelResolver {
            tables: vec![layout, master, pres],
        }
    }

    fn resolve(&self, id: &str) -> Option<String> {
        for t in &self.tables {
            if let Some(r) = t.get(id) {
                if let Some(p) = r.part() {
                    return Some(p.to_string());
                }
                // 外部链接（超链接）直接用 Target
                if r.is_external() {
                    return Some(r.target.clone());
                }
            }
        }
        None
    }

    fn as_fn(&self) -> impl Fn(&str) -> Option<String> + '_ {
        move |id: &str| self.resolve(id)
    }
}

/// 按 `p:sldIdLst` 的顺序收集幻灯片部件名。
///
/// 顺序**必须**以 `p:sldIdLst` 为准，而不是按文件名数字排序 ——
/// 演示时可以调整幻灯片顺序，此时 `p:sldIdLst` 的顺序与文件名顺序不一致。
fn collect_slide_order(pres_root: &XmlNode, pres_rels: &Relationships) -> Vec<String> {
    let mut out = Vec::new();

    if let Some(lst) = pres_root.child("sldIdLst") {
        for sld_id in lst.children_named("sldId") {
            // 必须用限定名 `r:id`：`p:sldId` 的 `id` 是幻灯片编号（如 256），
            // 与关系 id 完全不同名不同义
            let Some(rel_id) = sld_id.attr_qualified("r:id") else {
                continue;
            };
            let Some(rel) = pres_rels.get(rel_id) else {
                continue;
            };
            if let Some(part) = rel.part() {
                out.push(part.to_string());
            }
        }
    }

    // 兜底：`p:sldIdLst` 缺失或全部解析失败时，退回按关系表的 slide 类型收集
    if out.is_empty() {
        out = pres_rels
            .parts_of_type(rel_type::SLIDE)
            .into_iter()
            .map(|s| s.to_string())
            .collect();
    }

    out
}

/// 解析 `p:sldSz`。
fn parse_slide_size(pres_root: &XmlNode) -> Size {
    let Some(sz) = pres_root.child("sldSz") else {
        return DEFAULT_SLIDE_SIZE;
    };
    let w = units::emu_to_pt(sz.attr_f64("cx").unwrap_or(0.0));
    let h = units::emu_to_pt(sz.attr_f64("cy").unwrap_or(0.0));
    if w <= 1.0 || h <= 1.0 {
        // 尺寸非法（如 0）时用默认值，避免后续除零
        return DEFAULT_SLIDE_SIZE;
    }
    Size::new(w, h)
}

/// 读取 `docProps/core.xml` 的 `dc:title`。
fn read_core_title(package: &Package) -> Option<String> {
    let text = package.read_part_str("docprops/core.xml").ok()?;
    let root = xml::parse_root("docprops/core.xml", &text).ok()?;
    let title = root.child("title")?.deep_text();
    let trimmed = title.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// 计算内容指纹，用于磁盘缓存目录命名。
///
/// 取「全部条目名 + 主部件内容」的 SHA-256：
/// 条目名集合决定了包结构，主部件内容决定了页面顺序与尺寸。
/// 两者结合足以区分不同课件，同时计算量很小（不读全部页面与媒体）。
fn compute_fingerprint(package: &Package, presentation_part: &str) -> String {
    let mut hasher = Sha256::new();

    hasher.update(b"openpptview-pptx-v1");
    hasher.update((package.entry_count() as u64).to_le_bytes());

    for name in package.entry_names() {
        hasher.update(name.as_bytes());
        hasher.update([0]);
    }

    hasher.update(presentation_part.as_bytes());
    if let Ok(bytes) = package.read_part(presentation_part) {
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }

    // 文件大小参与哈希，避免「结构相同但内容不同」的极小概率碰撞
    hasher.update(package.file_len().to_le_bytes());

    let digest = hasher.finalize();
    // 取前 16 字节十六进制，足够区分且目录名不过长
    digest[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 判断非演示文稿的实际类型，用于给出准确的中文提示。
fn describe_non_presentation(ct: &ContentTypes, part: &str) -> &'static str {
    let Some(content_type) = ct.content_type_of(part) else {
        return "未知";
    };
    if content_type.contains("wordprocessingml") {
        "Word 文档（.docx）"
    } else if content_type.contains("spreadsheetml") {
        "Excel 工作簿（.xlsx）"
    } else if content_type.contains("drawingml") {
        "Visio 绘图"
    } else {
        "其它 Office 文档"
    }
}

impl MediaProvider for PptxSource {
    fn read_media(&self, part: &str) -> Result<Vec<u8>> {
        self.package.read_part(part)
    }

    fn has_media(&self, part: &str) -> bool {
        self.package.contains(part)
    }
}

impl DocumentSource for PptxSource {
    fn page_count(&self) -> usize {
        self.info.slide_parts.len()
    }

    fn default_page_size_pt(&self) -> Size {
        self.info.slide_size_pt
    }

    fn page_content(&self, index: usize) -> Result<PageContent> {
        self.check_index(index)?;

        let part = &self.info.slide_parts[index];
        let text = self.package.read_part_str(part).map_err(|e| {
            // 单页读取失败降级为该页错误，不影响整本课件
            Error::page_parse(index, format!("无法读取幻灯片 XML：{e}"))
        })?;
        let root = xml::parse_root(part, &text)
            .map_err(|e| Error::page_parse(index, format!("幻灯片 XML 解析失败：{e}")))?;

        let inherit = self.inheritance_for(index)?;

        // 该页自己的关系表（图片、超链接、跳转）
        let slide_rels = opc::load_relationships(&self.package, part)
            .unwrap_or_else(|_| Relationships::empty());

        // 幻灯片级关系已经覆盖图片与超链接；其余 id 兜底到演示文稿级关系。
        // 这里需要具名绑定，否则临时值会在借用结束前被释放。
        let empty_rels = Relationships::empty();
        let resolver = SlideRelResolver {
            slide: &slide_rels,
            fallback: MoveRelResolver::new(&empty_rels, &empty_rels, &self.pres_rels),
        };
        let resolve_fn = |id: &str| resolver.resolve(id);

        // 图表这类对象的正文在**别的部件**里（`ppt/charts/chartN.xml`），
        // 幻灯片只留一个 r:id 指过去。按名读取由包缓存兜底，
        // 因此没有图表的页面不会因此多读任何部件。
        let read_part = |name: &str| self.package.read_part_str(name).ok();

        let scene = shape::parse_slide_with_parts(&root, &inherit, &resolve_fn, Some(&read_part));
        Ok(PageContent::Scene(Box::new(scene)))
    }

    fn notes(&self, index: usize) -> Result<Option<String>> {
        self.check_index(index)?;
        // 备注失败不应影响正文浏览
        Ok(self.notes_text(index).unwrap_or(None))
    }

    fn title(&self) -> Option<String> {
        self.info.title.clone()
    }

    fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    fn source_path(&self) -> Option<&Path> {
        Some(&self.path)
    }

    fn format(&self) -> DocFormat {
        DocFormat::Pptx
    }
}

/// 幻灯片级的关系解析器：本页关系优先，其次是演示文稿级。
struct SlideRelResolver<'a> {
    slide: &'a Relationships,
    fallback: MoveRelResolver<'a>,
}

impl<'a> SlideRelResolver<'a> {
    fn resolve(&self, id: &str) -> Option<String> {
        if let Some(r) = self.slide.get(id) {
            if let Some(p) = r.part() {
                return Some(p.to_string());
            }
            if r.is_external() {
                return Some(r.target.clone());
            }
        }
        self.fallback.resolve(id)
    }

    fn as_fn(&self) -> impl Fn(&str) -> Option<String> + '_ {
        move |id: &str| self.resolve(id)
    }
}

/// 未使用导入守卫：这些类型经 `Scene` 字段间接使用。
#[allow(dead_code)]
fn _assert_types(_: Color, _: Fill, _: Rect) {}

#[cfg(test)]
mod tests {
    use super::*;
    use ppt_core::xml;

    fn n(xml_text: &str) -> XmlNode {
        xml::parse_root("t", xml_text).unwrap()
    }

    #[test]
    fn slide_size_parsed_from_emu() {
        // 16:9 标准尺寸
        let pres = n(r#"<p:presentation><p:sldSz cx="12192000" cy="6858000"/></p:presentation>"#);
        let size = parse_slide_size(&pres);
        assert!((size.w - 960.0).abs() < 0.01, "实际 {}", size.w);
        assert!((size.h - 540.0).abs() < 0.01, "实际 {}", size.h);
    }

    #[test]
    fn slide_size_parsed_for_4_3() {
        let pres = n(r#"<p:presentation><p:sldSz cx="9144000" cy="6858000"/></p:presentation>"#);
        let size = parse_slide_size(&pres);
        assert!((size.w - 720.0).abs() < 0.01);
        assert!((size.h - 540.0).abs() < 0.01);
    }

    #[test]
    fn missing_slide_size_falls_back_to_default() {
        let pres = n(r#"<p:presentation/>"#);
        assert_eq!(parse_slide_size(&pres), DEFAULT_SLIDE_SIZE);
    }

    #[test]
    fn zero_slide_size_falls_back_to_default() {
        let pres = n(r#"<p:presentation><p:sldSz cx="0" cy="0"/></p:presentation>"#);
        assert_eq!(parse_slide_size(&pres), DEFAULT_SLIDE_SIZE);
    }

    #[test]
    fn slide_order_follows_sld_id_lst() {
        let pres = n(
            r#"<p:presentation>
                 <p:sldIdLst>
                   <p:sldId id="256" r:id="rId3"/>
                   <p:sldId id="257" r:id="rId2"/>
                 </p:sldIdLst>
               </p:presentation>"#,
        );
        let rels = Relationships::parse(
            "ppt/presentation.xml",
            r#"<Relationships>
                 <Relationship Id="rId2" Type="http://x/slide" Target="slides/slide1.xml"/>
                 <Relationship Id="rId3" Type="http://x/slide" Target="slides/slide2.xml"/>
               </Relationships>"#,
        )
        .unwrap();

        let order = collect_slide_order(&pres, &rels);
        // 顺序由 sldIdLst 决定：rId3 在前 → slide2.xml 在前
        assert_eq!(order, vec!["ppt/slides/slide2.xml", "ppt/slides/slide1.xml"]);
    }

    #[test]
    fn slide_order_falls_back_when_lst_missing() {
        let pres = n(r#"<p:presentation/>"#);
        let rels = Relationships::parse(
            "ppt/presentation.xml",
            r#"<Relationships>
                 <Relationship Id="rId1" Type="http://x/slide" Target="slides/slide1.xml"/>
                 <Relationship Id="rId2" Type="http://x/slide" Target="slides/slide2.xml"/>
               </Relationships>"#,
        )
        .unwrap();
        let order = collect_slide_order(&pres, &rels);
        assert_eq!(order.len(), 2);
        assert_eq!(order[0], "ppt/slides/slide1.xml");
    }

    #[test]
    fn slide_order_ignores_unresolvable_ids() {
        let pres = n(
            r#"<p:presentation><p:sldIdLst>
                 <p:sldId id="256" r:id="rIdMissing"/>
                 <p:sldId id="257" r:id="rId1"/>
               </p:sldIdLst></p:presentation>"#,
        );
        let rels = Relationships::parse(
            "ppt/presentation.xml",
            r#"<Relationships>
                 <Relationship Id="rId1" Type="http://x/slide" Target="slides/slide1.xml"/>
               </Relationships>"#,
        )
        .unwrap();
        let order = collect_slide_order(&pres, &rels);
        assert_eq!(order, vec!["ppt/slides/slide1.xml"]);
    }

    #[test]
    fn non_presentation_content_types_are_described() {
        let ct = ContentTypes::parse(
            r#"<Types>
                 <Override PartName="/word/document.xml"
                   ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
               </Types>"#,
        )
        .unwrap();
        assert_eq!(
            describe_non_presentation(&ct, "word/document.xml"),
            "Word 文档（.docx）"
        );
    }

    #[test]
    fn spreadsheet_content_type_described() {
        let ct = ContentTypes::parse(
            r#"<Types>
                 <Override PartName="/xl/workbook.xml"
                   ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
               </Types>"#,
        )
        .unwrap();
        assert_eq!(
            describe_non_presentation(&ct, "xl/workbook.xml"),
            "Excel 工作簿（.xlsx）"
        );
    }

    #[test]
    fn move_rel_resolver_prefers_nearer_table() {
        let layout = Relationships::parse(
            "ppt/slideLayouts/slideLayout1.xml",
            r#"<Relationships>
                 <Relationship Id="rId1" Type="http://x/image" Target="../media/a.png"/>
               </Relationships>"#,
        )
        .unwrap();
        let master = Relationships::parse(
            "ppt/slideMasters/slideMaster1.xml",
            r#"<Relationships>
                 <Relationship Id="rId1" Type="http://x/image" Target="../media/b.png"/>
               </Relationships>"#,
        )
        .unwrap();
        let pres = Relationships::empty();

        let r = MoveRelResolver::new(&layout, &master, &pres);
        // 版式更近，应优先命中
        assert_eq!(r.resolve("rId1").as_deref(), Some("ppt/media/a.png"));
        assert_eq!(r.resolve("nope"), None);
    }

    #[test]
    fn move_rel_resolver_returns_external_target() {
        let layout = Relationships::empty();
        let master = Relationships::empty();
        let pres = Relationships::parse(
            "ppt/presentation.xml",
            r#"<Relationships>
                 <Relationship Id="rId9" Type="http://x/hyperlink"
                   Target="https://example.com" TargetMode="External"/>
               </Relationships>"#,
        )
        .unwrap();
        let r = MoveRelResolver::new(&layout, &master, &pres);
        assert_eq!(
            r.resolve("rId9").as_deref(),
            Some("https://example.com")
        );
    }

    #[test]
    fn read_core_title_extracts_text() {
        // 用一个不含该部件的包验证「缺失即 None」的路径
        let tmp = std::env::temp_dir().join("openpptview-core-title-test.zip");
        let _ = std::fs::remove_file(&tmp);
        {
            let file = std::fs::File::create(&tmp).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file::<_, ()>("dummy.txt", Default::default())
                .unwrap();
            use std::io::Write;
            zip.write_all(b"x").unwrap();
            zip.finish().unwrap();
        }
        let pkg = Package::open(&tmp).unwrap();
        assert!(read_core_title(&pkg).is_none());
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn fingerprint_is_stable_for_same_content() {
        let tmp = std::env::temp_dir().join("openpptview-fp-test.zip");
        let _ = std::fs::remove_file(&tmp);
        {
            use std::io::Write;
            let file = std::fs::File::create(&tmp).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file::<_, ()>("ppt/presentation.xml", Default::default())
                .unwrap();
            zip.write_all(b"<p:presentation/>").unwrap();
            zip.start_file::<_, ()>("other.txt", Default::default())
                .unwrap();
            zip.write_all(b"hello").unwrap();
            zip.finish().unwrap();
        }

        let pkg = Package::open(&tmp).unwrap();
        let a = compute_fingerprint(&pkg, "ppt/presentation.xml");
        let b = compute_fingerprint(&pkg, "ppt/presentation.xml");
        assert_eq!(a, b, "同一内容指纹应稳定");
        assert_eq!(a.len(), 32, "指纹应为 16 字节的十六进制");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn fingerprint_differs_for_different_content() {
        use std::io::Write;

        let make = |name: &str, body: &[u8]| -> String {
            let tmp = std::env::temp_dir().join(name);
            let _ = std::fs::remove_file(&tmp);
            {
                let file = std::fs::File::create(&tmp).unwrap();
                let mut zip = zip::ZipWriter::new(file);
                zip.start_file::<_, ()>("ppt/presentation.xml", Default::default())
                    .unwrap();
                zip.write_all(body).unwrap();
                zip.finish().unwrap();
            }
            let pkg = Package::open(&tmp).unwrap();
            let fp = compute_fingerprint(&pkg, "ppt/presentation.xml");
            let _ = std::fs::remove_file(&tmp);
            fp
        };

        let a = make("openpptview-fp-a.zip", b"<p:presentation/>");
        let b = make("openpptview-fp-b.zip", b"<p:presentation><p:sldSz cx=\"1\"/></p:presentation>");
        assert_ne!(a, b, "不同内容应得到不同指纹");
    }
}
