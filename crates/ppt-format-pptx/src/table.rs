//! 表格解析：`a:tbl` → [`Table`]。
//!
//! # 合并单元格的处理
//!
//! OOXML 用「主轴格 + 从属格」表示合并：
//!
//! ```xml
//! <!-- 跨两列的主单元格 -->
//! <a:tc gridSpan="2">…</a:tc>
//! <!-- 被覆盖的从属格，内容为空 -->
//! <a:tc hMerge="1"/>
//! ```
//!
//! 网格里**仍保留所有位置**（列数不变），因此解析时保持矩形网格结构，
//! 由渲染器跳过 `hMerge`/`vMerge` 的格、并让主格绘制合并后的整块区域。

use ppt_core::scene::{
    BorderLine, Color, DashStyle, Fill, Insets, LineJoin, Table, TableBorders, TableCell,
    VerticalAnchor,
};
use ppt_core::{units, XmlNode};

use crate::paint;
use crate::shape::NodeParser;

/// 表格在幻灯片上的默认边框宽度（0.75pt），当单元格未指定时使用。
const DEFAULT_BORDER_PT: f32 = 0.75;

/// 解析 `a:tbl`。
pub fn parse_table(tbl: &XmlNode, parser: &NodeParser<'_>) -> Table {
    let columns = parse_columns(tbl);
    let rows = parse_row_heights(tbl);

    let mut cells: Vec<Vec<TableCell>> = Vec::new();
    for tr in tbl.children_named("tr") {
        let mut row = Vec::new();
        for tc in tr.children_named("tc") {
            row.push(parse_cell(tc, parser));
        }
        cells.push(row);
    }

    let props = tbl.child("tblPr");

    Table {
        columns,
        rows,
        cells,
        first_row_header: props
            .and_then(|p| p.attr_bool("firstRow"))
            .unwrap_or(false),
        banded_rows: props
            .and_then(|p| p.attr_bool("bandRow"))
            .unwrap_or(false),
        first_col: props
            .and_then(|p| p.attr_bool("firstCol"))
            .unwrap_or(false),
        banded_columns: props
            .and_then(|p| p.attr_bool("bandCol"))
            .unwrap_or(false),
        last_row: props.and_then(|p| p.attr_bool("lastRow")).unwrap_or(false),
        last_col: props.and_then(|p| p.attr_bool("lastCol")).unwrap_or(false),
        style_id: props
            .and_then(|p| p.child("tableStyleId"))
            .map(|n| n.deep_text()),
    }
}

/// 解析列宽（`a:tblGrid`）。
fn parse_columns(tbl: &XmlNode) -> Vec<f32> {
    let Some(grid) = tbl.child("tblGrid") else {
        return Vec::new();
    };
    grid.children_named("gridCol")
        .map(|c| units::emu_to_pt(c.attr_f64("w").unwrap_or(0.0)))
        .collect()
}

/// 解析行高（`a:tr/@h`）。
fn parse_row_heights(tbl: &XmlNode) -> Vec<f32> {
    tbl.children_named("tr")
        .map(|tr| units::emu_to_pt(tr.attr_f64("h").unwrap_or(0.0)))
        .collect()
}

/// 解析单元格。
fn parse_cell(tc: &XmlNode, parser: &NodeParser<'_>) -> TableCell {
    let tc_pr = tc.child("tcPr");

    let margins = parse_cell_margins(tc_pr);
    let anchor = match tc_pr.and_then(|p| p.attr("anchor")) {
        Some("ctr") => VerticalAnchor::Middle,
        Some("b") => VerticalAnchor::Bottom,
        _ => VerticalAnchor::Top,
    };

    let fill = tc_pr
        .map(|p| paint::parse_fill(p, Some(parser.scheme_for_table()), parser.resolver()))
        .unwrap_or(Fill::None);

    let borders = parse_borders(tc_pr, parser);

    let text = tc
        .child("txBody")
        .map(|tx| parser.parse_cell_text(tx));

    TableCell {
        col_span: tc
            .attr_u32("gridSpan")
            .unwrap_or(1)
            .max(1),
        row_span: tc.attr_u32("rowSpan").unwrap_or(1).max(1),
        h_merge: tc.attr_bool_or("hMerge", false),
        v_merge: tc.attr_bool_or("vMerge", false),
        fill,
        borders,
        text,
        margins,
        anchor,
    }
}

/// 解析单元格内边距；未指定时沿用 OOXML 默认值。
fn parse_cell_margins(tc_pr: Option<&XmlNode>) -> Insets {
    // OOXML 表格单元格默认内边距：左右 0.1 英寸 = 7.2pt，上下 0.05 英寸 = 3.6pt
    let mut insets = Insets {
        left: 7.2,
        top: 3.6,
        right: 7.2,
        bottom: 3.6,
    };
    if let Some(p) = tc_pr {
        if let Some(v) = p.attr_f64("marL") {
            insets.left = units::emu_to_pt(v);
        }
        if let Some(v) = p.attr_f64("marR") {
            insets.right = units::emu_to_pt(v);
        }
        if let Some(v) = p.attr_f64("marT") {
            insets.top = units::emu_to_pt(v);
        }
        if let Some(v) = p.attr_f64("marB") {
            insets.bottom = units::emu_to_pt(v);
        }
    }
    insets
}

/// 解析六条边框。
fn parse_borders(tc_pr: Option<&XmlNode>, parser: &NodeParser<'_>) -> TableBorders {
    let Some(p) = tc_pr else {
        return TableBorders::default();
    };
    let one = |name: &str| -> Option<BorderLine> {
        parse_border_line(p.child(name), parser)
    };
    TableBorders {
        left: one("lnL"),
        top: one("lnT"),
        right: one("lnR"),
        bottom: one("lnB"),
        tl_to_br: one("lnTlToBr"),
        tr_to_bl: one("lnBlToTr"),
    }
}

/// 解析单条边框线。
fn parse_border_line(ln: Option<&XmlNode>, parser: &NodeParser<'_>) -> Option<BorderLine> {
    let ln = ln?;

    // `a:ln` 内嵌在单元格里，宽度单位与形状描边一致（EMU）
    let width_pt = ln
        .attr_f64("w")
        .map(units::emu_to_pt)
        .unwrap_or(DEFAULT_BORDER_PT);

    // 无填充 = 无边框
    if ln.has_child("noFill") {
        return None;
    }

    let color = ln
        .child("solidFill")
        .and_then(|f| {
            crate::color::parse_solid_fill(f, Some(parser.scheme_for_table()))
        })
        .unwrap_or(Color::BLACK);

    let dash = match ln.child("prstDash").and_then(|d| d.attr("val")) {
        Some("dot") | Some("sysDot") => DashStyle::Dot,
        Some("dash") => DashStyle::Dash,
        Some("lgDash") => DashStyle::LgDash,
        Some("dashDot") => DashStyle::DashDot,
        Some("sysDash") => DashStyle::SysDash,
        _ => DashStyle::Solid,
    };

    let join = if ln.has_child("round") {
        LineJoin::Round
    } else if ln.has_child("bevel") {
        LineJoin::Bevel
    } else if ln.has_child("miter") {
        LineJoin::Miter
    } else {
        LineJoin::Miter
    };

    let line = BorderLine {
        color,
        width_pt,
        dash,
        // 复合线（双线）在本版本按单线绘制，避免视觉上失真
        double: false,
        join,
    };

    if line.is_visible() {
        Some(line)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inherit::SlideInheritance;
    use crate::theme::Theme;
    use ppt_core::scene::Size;
    use ppt_core::xml;

    fn n(xml_text: &str) -> XmlNode {
        xml::parse_root("t", xml_text).unwrap()
    }

    fn no_rel(_: &str) -> Option<String> {
        None
    }

    fn with_parser<T>(f: impl FnOnce(&NodeParser<'_>) -> T) -> T {
        let inh = SlideInheritance::build(
            &Theme::default(),
            None,
            None,
            None,
            Size::new(960.0, 540.0),
            &no_rel,
        );
        let parser = NodeParser::new(&inh, &no_rel);
        f(&parser)
    }

    #[test]
    fn parses_grid_and_row_dimensions() {
        let tbl = n(
            r#"<a:tbl>
                 <a:tblGrid>
                   <a:gridCol w="914400"/><a:gridCol w="1828800"/>
                 </a:tblGrid>
                 <a:tr h="457200"><a:tc/><a:tc/></a:tr>
                 <a:tr h="914400"><a:tc/><a:tc/></a:tr>
               </a:tbl>"#,
        );
        let t = with_parser(|p| parse_table(&tbl, p));
        assert_eq!(t.columns.len(), 2);
        assert!((t.columns[0] - 72.0).abs() < 1e-3);
        assert!((t.columns[1] - 144.0).abs() < 1e-3);
        assert_eq!(t.rows.len(), 2);
        assert!((t.rows[0] - 36.0).abs() < 1e-3);
        assert_eq!(t.row_count(), 2);
        assert_eq!(t.col_count(), 2);
    }

    #[test]
    fn cell_margins_default_to_ooxml_values() {
        let tbl = n(
            r#"<a:tbl>
                 <a:tblGrid><a:gridCol w="914400"/></a:tblGrid>
                 <a:tr h="457200"><a:tc/></a:tr>
               </a:tbl>"#,
        );
        let t = with_parser(|p| parse_table(&tbl, p));
        let c = &t.cells[0][0];
        assert!((c.margins.left - 7.2).abs() < 1e-4);
        assert!((c.margins.top - 3.6).abs() < 1e-4);
    }

    #[test]
    fn cell_margins_override() {
        let tbl = n(
            r#"<a:tbl>
                 <a:tblGrid><a:gridCol w="914400"/></a:tblGrid>
                 <a:tr h="457200"><a:tc>
                   <a:tcPr marL="182880" marR="91440" marT="45720" marB="137160"/>
                 </a:tc></a:tr>
               </a:tbl>"#,
        );
        let t = with_parser(|p| parse_table(&tbl, p));
        let c = &t.cells[0][0];
        assert!((c.margins.left - 14.4).abs() < 1e-3);
        assert!((c.margins.right - 7.2).abs() < 1e-3);
        assert!((c.margins.top - 3.6).abs() < 1e-3);
        assert!((c.margins.bottom - 10.8).abs() < 1e-3);
    }

    #[test]
    fn cell_anchor_parsed() {
        let tbl = n(
            r#"<a:tbl>
                 <a:tblGrid><a:gridCol w="914400"/></a:tblGrid>
                 <a:tr h="457200">
                   <a:tc><a:tcPr anchor="ctr"/></a:tc>
                 </a:tr>
               </a:tbl>"#,
        );
        let t = with_parser(|p| parse_table(&tbl, p));
        assert_eq!(t.cells[0][0].anchor, VerticalAnchor::Middle);
    }

    #[test]
    fn cell_fill_parsed() {
        let tbl = n(
            r#"<a:tbl>
                 <a:tblGrid><a:gridCol w="914400"/></a:tblGrid>
                 <a:tr h="457200">
                   <a:tc><a:tcPr>
                     <a:solidFill><a:srgbClr val="DEEAF6"/></a:solidFill>
                   </a:tcPr></a:tc>
                 </a:tr>
               </a:tbl>"#,
        );
        let t = with_parser(|p| parse_table(&tbl, p));
        assert_eq!(
            t.cells[0][0].fill,
            Fill::Solid(Color::rgb(0xDE, 0xEA, 0xF6))
        );
    }

    #[test]
    fn merged_cells_flagged_correctly() {
        let tbl = n(
            r#"<a:tbl>
                 <a:tblGrid><a:gridCol w="914400"/><a:gridCol w="914400"/></a:tblGrid>
                 <a:tr h="457200">
                   <a:tc gridSpan="2"><a:txBody/></a:tc>
                   <a:tc hMerge="1"/>
                 </a:tr>
                 <a:tr h="457200">
                   <a:tc rowSpan="2" vMerge="0"/>
                   <a:tc vMerge="1"/>
                 </a:tr>
               </a:tbl>"#,
        );
        let t = with_parser(|p| parse_table(&tbl, p));

        assert_eq!(t.cells[0][0].col_span, 2);
        assert!(t.cells[0][1].h_merge);
        assert!(!t.cells[0][1].is_drawable(), "从属格不应参与绘制");

        assert_eq!(t.cells[1][0].row_span, 2);
        assert!(t.cells[1][1].v_merge);

        let spanned = t.spanned_cells();
        assert!(spanned.contains(&(0, 1)));
        assert!(spanned.contains(&(1, 1)));
    }

    #[test]
    fn borders_parsed_from_tcpr() {
        let tbl = n(
            r#"<a:tbl>
                 <a:tblGrid><a:gridCol w="914400"/></a:tblGrid>
                 <a:tr h="457200"><a:tc><a:tcPr>
                   <a:lnT w="12700"><a:solidFill><a:srgbClr val="000000"/></a:solidFill></a:lnT>
                   <a:lnB w="6350"><a:solidFill><a:srgbClr val="FF0000"/></a:solidFill></a:lnB>
                 </a:tcPr></a:tc></a:tr>
               </a:tbl>"#,
        );
        let t = with_parser(|p| parse_table(&tbl, p));
        let b = &t.cells[0][0].borders;
        assert!(b.top.is_some());
        assert!((b.top.unwrap().width_pt - 1.0).abs() < 1e-3);
        assert!(b.bottom.is_some());
        assert_eq!(b.bottom.unwrap().color, Color::rgb(255, 0, 0));
        // 未指定的边应为无边框
        assert!(b.left.is_none());
        assert!(b.right.is_none());
    }

    #[test]
    fn border_with_no_fill_is_absent() {
        let tbl = n(
            r#"<a:tbl>
                 <a:tblGrid><a:gridCol w="914400"/></a:tblGrid>
                 <a:tr h="457200"><a:tc><a:tcPr>
                   <a:lnL w="12700"><a:noFill/></a:lnL>
                 </a:tcPr></a:tc></a:tr>
               </a:tbl>"#,
        );
        let t = with_parser(|p| parse_table(&tbl, p));
        assert!(t.cells[0][0].borders.left.is_none(), "noFill 边框应视为不存在");
    }

    #[test]
    fn border_dash_styles() {
        for (val, expected) in [
            ("dash", DashStyle::Dash),
            ("dot", DashStyle::Dot),
            ("lgDash", DashStyle::LgDash),
            ("sysDash", DashStyle::SysDash),
        ] {
            let tbl = n(&format!(
                r#"<a:tbl>
                     <a:tblGrid><a:gridCol w="914400"/></a:tblGrid>
                     <a:tr h="457200"><a:tc><a:tcPr>
                       <a:lnT w="12700">
                         <a:solidFill><a:srgbClr val="000000"/></a:solidFill>
                         <a:prstDash val="{val}"/>
                       </a:lnT>
                     </a:tcPr></a:tc></a:tr>
                   </a:tbl>"#
            ));
            let t = with_parser(|p| parse_table(&tbl, p));
            assert_eq!(t.cells[0][0].borders.top.unwrap().dash, expected, "虚线 {val}");
        }
    }

    #[test]
    fn diagonal_borders_parsed() {
        let tbl = n(
            r#"<a:tbl>
                 <a:tblGrid><a:gridCol w="914400"/></a:tblGrid>
                 <a:tr h="457200"><a:tc><a:tcPr>
                   <a:lnTlToBr w="12700"><a:solidFill><a:srgbClr val="000000"/></a:solidFill></a:lnTlToBr>
                   <a:lnBlToTr w="12700"><a:solidFill><a:srgbClr val="000000"/></a:solidFill></a:lnBlToTr>
                 </a:tcPr></a:tc></a:tr>
               </a:tbl>"#,
        );
        let t = with_parser(|p| parse_table(&tbl, p));
        let b = &t.cells[0][0].borders;
        assert!(b.tl_to_br.is_some());
        assert!(b.tr_to_bl.is_some());
    }

    #[test]
    fn table_properties_parsed() {
        let tbl = n(
            r#"<a:tbl>
                 <a:tblPr firstRow="1" bandRow="1" firstCol="0">
                   <a:tableStyleId>{5C22544A-7EE6-4342-B048-85BDC9FD1C3A}</a:tableStyleId>
                 </a:tblPr>
                 <a:tblGrid><a:gridCol w="914400"/></a:tblGrid>
                 <a:tr h="457200"><a:tc/></a:tr>
               </a:tbl>"#,
        );
        let t = with_parser(|p| parse_table(&tbl, p));
        assert!(t.first_row_header);
        assert!(t.banded_rows);
        assert!(!t.first_col);
        assert!(t.style_id.is_some());
        assert!(t.style_id.unwrap().contains("5C22544A"));
    }

    #[test]
    fn cell_text_parsed() {
        let tbl = n(
            r#"<a:tbl>
                 <a:tblGrid><a:gridCol w="1828800"/></a:tblGrid>
                 <a:tr h="457200"><a:tc>
                   <a:txBody><a:bodyPr/><a:lstStyle/>
                     <a:p><a:r><a:rPr sz="1800"/><a:t>第一列</a:t></a:r></a:p>
                   </a:txBody>
                 </a:tc></a:tr>
               </a:tbl>"#,
        );
        let t = with_parser(|p| parse_table(&tbl, p));
        let text = t.cells[0][0].text.as_ref().expect("单元格应有文本");
        assert_eq!(text.plain_text(), "第一列");
        assert!((text.paragraphs[0].runs[0].props.size_pt - 18.0).abs() < 1e-4);
    }

    #[test]
    fn empty_table_is_detected() {
        let tbl = n(r#"<a:tbl/>"#);
        let t = with_parser(|p| parse_table(&tbl, p));
        assert!(t.is_empty());
    }

    #[test]
    fn table_without_grid_still_parses_cells() {
        let tbl = n(
            r#"<a:tbl>
                 <a:tr h="457200"><a:tc><a:txBody><a:p><a:r><a:t>x</a:t></a:r></a:p></a:txBody></a:tc></a:tr>
               </a:tbl>"#,
        );
        let t = with_parser(|p| parse_table(&tbl, p));
        // 没有 tblGrid 时列宽为空，但单元格数据仍在
        assert!(t.columns.is_empty());
        assert_eq!(t.row_count(), 1);
    }

    #[test]
    fn zero_span_is_clamped_to_one() {
        let tbl = n(
            r#"<a:tbl>
                 <a:tblGrid><a:gridCol w="914400"/></a:tblGrid>
                 <a:tr h="457200"><a:tc gridSpan="0" rowSpan="0"/></a:tr>
               </a:tbl>"#,
        );
        let t = with_parser(|p| parse_table(&tbl, p));
        assert_eq!(t.cells[0][0].col_span, 1, "非法 span 应被夹紧为 1");
        assert_eq!(t.cells[0][0].row_span, 1);
    }
}
