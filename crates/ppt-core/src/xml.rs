//! OOXML 的轻量 XML 树。
//!
//! # 为什么用「建树」而不是纯流式状态机
//!
//! OOXML 的继承与嵌套极深（`spTree` → `sp` → `txBody` → `p` → `r` → `rPr`），
//! 纯流式解析需要维护庞大的状态栈，代码可读性差且极易出错。
//! 单页 slide XML 通常只有几十到几百 KB，建树的开销在毫秒级，
//! 且由于**按页惰性解析**，这部分成本不会落在打开课件的关键路径上。
//!
//! 为兼顾内存，这里做了两点取舍：
//! - 属性用 `Vec<(String, String)>` 而非 `HashMap`（幻灯片元素的属性通常 ≤ 10 个，
//!   线性查找比哈希更快且更省内存）；
//! - 元素名只保留**局部名**（剥离 `p:`/`a:`/`r:` 前缀），
//!   对第三方工具生成的任意前缀也天然兼容。

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::error::{Error, Result};

/// 一个 XML 元素节点。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct XmlNode {
    /// 局部名（已剥离命名空间前缀），如 `sp`、`txBody`。
    pub name: String,
    /// 属性：**限定名** → 值。
    ///
    /// 键保留原始前缀（如 `r:id`），这是必要的：
    /// `p:sldId` 同时有 `id`（幻灯片编号）与 `r:id`（关系 id）两个属性，
    /// 若只留局部名两者会撞成同一个键，导致幻灯片顺序解析错误。
    /// 用 [`XmlNode::attr`] 查询时按局部名匹配，因此绝大多数调用方无需关心前缀。
    pub attrs: Vec<(String, String)>,
    pub children: Vec<XmlNode>,
    /// 直接子文本（拼接本节点下的所有文本事件）。
    pub text: String,
}

impl XmlNode {
    /// 取属性值（按**局部名**匹配，忽略命名空间前缀）。
    ///
    /// 同名属性存在多个时返回第一个，这与「按文档顺序取首个」的直觉一致。
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name || local_name(k) == name)
            .map(|(_, v)| v.as_str())
    }

    /// 取属性值（按**限定名**精确匹配），用于区分 `id` 与 `r:id` 这类同名属性。
    pub fn attr_qualified(&self, qname: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == qname)
            .map(|(_, v)| v.as_str())
    }

    /// 所有局部名匹配的属性值。
    pub fn attrs_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.attrs
            .iter()
            .filter(move |(k, _)| k == name || local_name(k) == name)
            .map(|(_, v)| v.as_str())
    }

    /// 取属性并解析为 `f32`。
    pub fn attr_f32(&self, name: &str) -> Option<f32> {
        self.attr(name).and_then(|v| v.trim().parse::<f32>().ok())
    }

    /// 取属性并解析为 `f64`（EMU 值超出 f32 精度时用）。
    pub fn attr_f64(&self, name: &str) -> Option<f64> {
        self.attr(name).and_then(|v| v.trim().parse::<f64>().ok())
    }

    /// 取属性并解析为 `i64`。
    pub fn attr_i64(&self, name: &str) -> Option<i64> {
        self.attr(name).and_then(|v| v.trim().parse::<i64>().ok())
    }

    /// 取属性并解析为 `u32`。
    pub fn attr_u32(&self, name: &str) -> Option<u32> {
        self.attr(name).and_then(|v| v.trim().parse::<u32>().ok())
    }

    /// 取属性并解析为 `usize`。
    pub fn attr_usize(&self, name: &str) -> Option<usize> {
        self.attr(name).and_then(|v| v.trim().parse::<usize>().ok())
    }

    /// 取布尔属性（OOXML 用 `1`/`0`/`true`/`false`）。
    pub fn attr_bool(&self, name: &str) -> Option<bool> {
        self.attr(name).map(parse_ooxml_bool)
    }

    /// 取布尔属性，缺失时用默认值。
    pub fn attr_bool_or(&self, name: &str, default: bool) -> bool {
        self.attr_bool(name).unwrap_or(default)
    }

    /// 第一个指定局部名的子元素。
    pub fn child(&self, name: &str) -> Option<&XmlNode> {
        self.children.iter().find(|c| c.name == name)
    }

    /// 所有指定局部名的子元素。
    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a XmlNode> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// 是否存在指定名的子元素。
    pub fn has_child(&self, name: &str) -> bool {
        self.children.iter().any(|c| c.name == name)
    }

    /// 子元素数量。
    #[inline]
    pub fn child_count(&self) -> usize {
        self.children.len()
    }

    /// 递归查找第一个指定局部名的后代元素（含自身）。
    pub fn find_descendant(&self, name: &str) -> Option<&XmlNode> {
        if self.name == name {
            return Some(self);
        }
        for c in &self.children {
            if let Some(found) = c.find_descendant(name) {
                return Some(found);
            }
        }
        None
    }

    /// 沿路径逐级查找子元素，如 `["spPr", "xfrm"]`。
    pub fn path(&self, names: &[&str]) -> Option<&XmlNode> {
        let mut cur = self;
        for n in names {
            cur = cur.child(n)?;
        }
        Some(cur)
    }

    /// 该节点的直接文本。
    #[inline]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// 该节点及其后代的全部文本（按文档顺序拼接）。
    pub fn deep_text(&self) -> String {
        let mut out = String::new();
        self.collect_text(&mut out);
        out
    }

    fn collect_text(&self, out: &mut String) {
        out.push_str(&self.text);
        for c in &self.children {
            c.collect_text(out);
        }
    }

    /// 是否为空元素（无子元素、无文本）。
    #[inline]
    pub fn is_empty_element(&self) -> bool {
        self.children.is_empty() && self.text.is_empty()
    }

    /// 记录该节点及其后代的标签分布，用于诊断日志。
    pub fn tag_summary(&self) -> Vec<(String, usize)> {
        let mut map: Vec<(String, usize)> = Vec::new();
        self.accumulate_tags(&mut map);
        map
    }

    fn accumulate_tags(&self, out: &mut Vec<(String, usize)>) {
        match out.iter_mut().find(|(n, _)| *n == self.name) {
            Some((_, c)) => *c += 1,
            None => out.push((self.name.clone(), 1)),
        }
        for c in &self.children {
            c.accumulate_tags(out);
        }
    }
}

/// 解析 OOXML 布尔值。
///
/// 注意：OOXML 中「属性存在即为真」的语义由调用方处理，
/// 这里只负责把字面值翻译成布尔。
#[inline]
pub fn parse_ooxml_bool(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "on" | "yes"
    )
}

/// 剥离命名空间前缀，返回局部名。
#[inline]
pub fn local_name(qname: &str) -> &str {
    match qname.rfind(':') {
        Some(i) => &qname[i + 1..],
        None => qname,
    }
}

/// 解析 XML 为节点树。
///
/// `part` 仅用于错误信息，便于定位是哪个部件解析失败。
pub fn parse(part: &str, xml: &str) -> Result<XmlNode> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);

    // 虚拟根：OOXML 部件有且只有一个根元素，用它承载它
    let mut root = XmlNode {
        name: String::from("#root"),
        ..Default::default()
    };
    // 元素栈：栈顶是当前打开的节点
    let mut stack: Vec<XmlNode> = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                stack.push(build_node(&e, part)?);
            }
            Ok(Event::Empty(e)) => {
                let node = build_node(&e, part)?;
                attach(&mut root, &mut stack, node);
            }
            Ok(Event::End(_)) => {
                let node = stack
                    .pop()
                    .ok_or_else(|| Error::xml(part, "XML 标签不匹配：出现多余的结束标签".to_string()))?;
                attach(&mut root, &mut stack, node);
            }
            Ok(Event::Text(e)) => {
                if let Some(top) = stack.last_mut() {
                    // xml10_content() 顺带做 XML 1.0 的行尾归一化（\r\n → \n），
                    // 这对幻灯片文本的保真度是必要的。
                    top.text.push_str(&e.xml10_content());
                }
            }
            Ok(Event::CData(e)) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&e.xml10_content());
                }
            }
            Ok(Event::GeneralRef(e)) => {
                // 0.42 起实体引用（`&amp;`）不再混在文本里，而是独立的 GeneralRef 事件。
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&resolve_general_ref(&e));
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => {
                return Err(Error::xml(
                    part,
                    format!("解析失败（位置 {}）：{e}", reader.buffer_position()),
                ));
            }
        }
    }

    if let Some(orphan) = stack.pop() {
        return Err(Error::xml(
            part,
            format!("XML 未正常闭合：元素 <{}> 缺少结束标签", orphan.name),
        ));
    }

    Ok(root)
}

/// 解析并返回根元素（跳过虚拟根）。
pub fn parse_root(part: &str, xml: &str) -> Result<XmlNode> {
    let mut root = parse(part, xml)?;
    if root.children.len() == 1 {
        Ok(root.children.remove(0))
    } else if root.children.is_empty() {
        Err(Error::xml(part, "部件中没有根元素".to_string()))
    } else {
        // 多根元素属于异常，但取第一个比直接失败更符合「容错读取」的目标
        Ok(root.children.remove(0))
    }
}

fn build_node(e: &quick_xml::events::BytesStart<'_>, part: &str) -> Result<XmlNode> {
    let qname = e.name();
    let qname: &str = qname.as_ref();

    let mut attrs = Vec::new();
    for a in e.attributes() {
        let a = a.map_err(|err| Error::xml(part, format!("属性解析失败：{err}")))?;
        // 保留限定名（含前缀），供 attr_qualified 区分同名属性
        let key = a.key.as_ref().to_string();
        // 属性值可能含实体（如形状名里的 `&amp;`）。
        // 解析失败时保留原样，总好过让整页解析失败。
        let value = match quick_xml::escape::unescape(&a.value) {
            Ok(v) => v.into_owned(),
            Err(_) => a.value.clone().into_owned(),
        };
        attrs.push((key, value));
    }

    Ok(XmlNode {
        name: local_name(qname).to_string(),
        attrs,
        children: Vec::new(),
        text: String::new(),
    })
}

/// 把 `GeneralRef` 事件还原为字符。
///
/// 支持字符引用（`&#65;` / `&#x41;`）与预定义实体（`&amp;` `&lt;` `&gt;` `&apos;` `&quot;`）。
/// 未知实体会原样保留 `&name;`，避免静默丢内容。
fn resolve_general_ref(e: &quick_xml::events::BytesRef<'_>) -> String {
    let name: &str = e;
    if e.is_char_ref() {
        if let Ok(Some(ch)) = e.resolve_char_ref() {
            return ch.to_string();
        }
    } else if let Some(resolved) = quick_xml::escape::resolve_predefined_entity(name) {
        return resolved.to_string();
    }
    format!("&{name};")
}

fn attach(root: &mut XmlNode, stack: &mut [XmlNode], node: XmlNode) {
    match stack.last_mut() {
        Some(parent) => parent.children.push(node),
        None => root.children.push(node),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_elements_and_attrs() {
        let xml = r#"<?xml version="1.0"?>
            <p:spTree xmlns:p="urn:p" xmlns:a="urn:a">
              <p:sp>
                <p:nvSpPr><p:cNvPr id="2" name="标题 1"/></p:nvSpPr>
                <p:spPr><a:off x="100" y="200"/><a:ext cx="300" cy="400"/></p:spPr>
              </p:sp>
            </p:spTree>"#;
        let root = parse_root("test.xml", xml).unwrap();
        assert_eq!(root.name, "spTree");
        assert_eq!(root.child_count(), 1);

        let sp = root.child("sp").unwrap();
        let cnv = sp.path(&["nvSpPr", "cNvPr"]).unwrap();
        assert_eq!(cnv.attr_u32("id"), Some(2));
        assert_eq!(cnv.attr("name"), Some("标题 1"));

        let off = sp.path(&["spPr", "off"]).unwrap();
        assert_eq!(off.attr_f64("x"), Some(100.0));
    }

    #[test]
    fn strips_namespace_prefixes_so_any_prefix_works() {
        let xml_a = r#"<p:sp><p:spPr><a:solidFill/></p:spPr></p:sp>"#;
        let xml_b = r#"<sp><spPr><solidFill/></spPr></sp>"#;
        let a = parse_root("a", xml_a).unwrap();
        let b = parse_root("b", xml_b).unwrap();
        assert_eq!(a.name, b.name);
        assert_eq!(a.child("spPr").unwrap().name, b.child("spPr").unwrap().name);
        assert!(a.path(&["spPr", "solidFill"]).is_some());
        assert!(b.path(&["spPr", "solidFill"]).is_some());
    }

    #[test]
    fn self_closing_elements_are_attached() {
        let xml = r#"<root><a/><b/><c/></root>"#;
        let root = parse_root("t", xml).unwrap();
        assert_eq!(root.child_count(), 3);
        assert!(root.children.iter().all(|c| c.is_empty_element() || c.children.is_empty()));
    }

    #[test]
    fn captures_text_content() {
        let xml = r#"<a:t>你好 World</a:t>"#;
        let root = parse_root("t", xml).unwrap();
        assert_eq!(root.name, "t");
        assert_eq!(root.text(), "你好 World");
    }

    #[test]
    fn captures_text_with_entities() {
        let xml = r#"<a:t>a &amp; b &lt;c&gt;</a:t>"#;
        let root = parse_root("t", xml).unwrap();
        assert_eq!(root.text(), "a & b <c>");
    }

    #[test]
    fn deep_text_concatenates_in_order() {
        let xml = r#"<p><r><t>Hello</t></r><r><t>世界</t></r></p>"#;
        let root = parse_root("t", xml).unwrap();
        assert_eq!(root.deep_text(), "Hello世界");
    }

    #[test]
    fn boolean_parsing_accepts_ooxml_forms() {
        assert!(parse_ooxml_bool("1"));
        assert!(parse_ooxml_bool("true"));
        assert!(parse_ooxml_bool("TRUE"));
        assert!(!parse_ooxml_bool("0"));
        assert!(!parse_ooxml_bool("false"));
        assert!(parse_ooxml_bool(" 1 "));

        let xml = r#"<a:grpSpPr flipH="1" flipV="0"/>"#;
        let n = parse_root("t", xml).unwrap();
        assert!(n.attr_bool("flipH").unwrap());
        assert!(!n.attr_bool("flipV").unwrap());
        assert!(n.attr_bool_or("missing", true));
    }

    #[test]
    fn missing_attribute_returns_none() {
        let xml = r#"<a:off x="1"/>"#;
        let n = parse_root("t", xml).unwrap();
        assert_eq!(n.attr("y"), None);
        assert_eq!(n.attr_f64("y"), None);
        assert_eq!(n.attr_u32("y"), None);
    }

    #[test]
    fn children_named_filters_correctly() {
        let xml = r#"<p:pPr><a:buChar char="•"/><a:buChar char="-"/><a:tab/></p:pPr>"#;
        let n = parse_root("t", xml).unwrap();
        let bullets: Vec<_> = n.children_named("buChar").collect();
        assert_eq!(bullets.len(), 2);
        assert_eq!(bullets[0].attr("char"), Some("•"));
        assert!(n.has_child("tab"));
        assert!(!n.has_child("buNone"));
    }

    #[test]
    fn find_descendant_searches_deeply() {
        let xml = r#"<root><a><b><c><target v="1"/></c></b></a></root>"#;
        let n = parse_root("t", xml).unwrap();
        let t = n.find_descendant("target").unwrap();
        assert_eq!(t.attr("v"), Some("1"));
        assert!(n.find_descendant("nope").is_none());
    }

    #[test]
    fn unclosed_tag_is_reported_as_error() {
        let xml = r#"<root><a></root>"#;
        let err = parse("t", xml).unwrap_err();
        assert!(matches!(err, Error::Xml { .. }));
    }

    #[test]
    fn cdata_is_captured() {
        let xml = r#"<root><![CDATA[<not xml>]]></root>"#;
        let n = parse_root("t", xml).unwrap();
        assert_eq!(n.text(), "<not xml>");
    }

    #[test]
    fn empty_document_reports_error() {
        assert!(parse_root("t", "").is_err());
        assert!(parse_root("t", "   ").is_err());
    }

    #[test]
    fn tag_summary_counts_repeated_tags() {
        let xml = r#"<root><sp/><sp/><pic/></root>"#;
        let n = parse_root("t", xml).unwrap();
        let summary = n.tag_summary();
        assert!(summary.contains(&("sp".to_string(), 2)));
        assert!(summary.contains(&("pic".to_string(), 1)));
    }

    #[test]
    fn local_name_helper() {
        assert_eq!(local_name("p:sp"), "sp");
        assert_eq!(local_name("sp"), "sp");
        assert_eq!(local_name("a:rPr"), "rPr");
        assert_eq!(local_name("r:embed"), "embed");
    }
}
