//! 表格模型。

use serde::{Deserialize, Serialize};

use super::geom::Insets;
use super::paint::{Color, DashStyle, Fill, LineJoin};
use super::text::{TextBox, VerticalAnchor};

/// 表格。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Table {
    /// 列宽（pt），长度等于列数。
    pub columns: Vec<f32>,
    /// 行高（pt），长度等于行数。
    pub rows: Vec<f32>,
    /// 单元格网格，`cells[row][col]`。
    pub cells: Vec<Vec<TableCell>>,
    /// 是否首行作为表头。
    pub first_row_header: bool,
    /// 是否启用斑马纹。
    pub banded_rows: bool,
    /// 是否首列强调。
    pub first_col: bool,
    pub banded_columns: bool,
    pub last_row: bool,
    pub last_col: bool,
    /// 特殊列/行强调时使用的强调色，`None` 表示沿用单元格自身填充。
    pub style_id: Option<String>,
}

impl Table {
    #[inline]
    pub fn row_count(&self) -> usize {
        self.cells.len()
    }

    #[inline]
    pub fn col_count(&self) -> usize {
        self.cells.first().map(|r| r.len()).unwrap_or(0)
    }

    /// 是否为空表（无行列或全无尺寸）。
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
            || self.columns.is_empty()
            || self.rows.is_empty()
    }

    /// 总宽。
    #[inline]
    pub fn total_width_pt(&self) -> f32 {
        self.columns.iter().sum()
    }

    /// 总高。
    #[inline]
    pub fn total_height_pt(&self) -> f32 {
        self.rows.iter().sum()
    }

    /// 第 `row` 行的上边界 y 偏移。
    pub fn row_offset_pt(&self, row: usize) -> f32 {
        self.rows.iter().take(row).sum()
    }

    /// 第 `col` 列的左边界 x 偏移。
    pub fn col_offset_pt(&self, col: usize) -> f32 {
        self.columns.iter().take(col).sum()
    }

    /// 返回需要跳过的、被合并覆盖的单元格位置集合。
    ///
    /// 渲染时主单元格负责绘制自身区域（含被合并的空间），
    /// 从属单元格直接跳过，避免重复绘制边框与底纹。
    pub fn spanned_cells(&self) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for (r, row) in self.cells.iter().enumerate() {
            for (c, cell) in row.iter().enumerate() {
                if cell.h_merge || cell.v_merge {
                    out.push((r, c));
                }
            }
        }
        out
    }
}

/// 表格单元格。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableCell {
    /// 跨列数（≥1）。
    pub col_span: u32,
    /// 跨行数（≥1）。
    pub row_span: u32,
    /// 该单元格是被左侧单元格横向合并覆盖的从属格。
    pub h_merge: bool,
    /// 该单元格是被上方单元格纵向合并覆盖的从属格。
    pub v_merge: bool,
    pub fill: Fill,
    pub borders: TableBorders,
    pub text: Option<TextBox>,
    pub margins: Insets,
    pub anchor: VerticalAnchor,
}

impl Default for TableCell {
    fn default() -> Self {
        TableCell {
            col_span: 1,
            row_span: 1,
            h_merge: false,
            v_merge: false,
            fill: Fill::None,
            borders: TableBorders::default(),
            text: None,
            // OOXML 表格单元格默认内边距：左右 0.1 英寸，上下 0.05 英寸
            margins: Insets {
                left: 7.2,
                top: 3.6,
                right: 7.2,
                bottom: 3.6,
            },
            anchor: VerticalAnchor::Middle,
        }
    }
}

impl TableCell {
    /// 该单元格是否参与绘制（从属合并格不绘制）。
    #[inline]
    pub fn is_drawable(&self) -> bool {
        !self.h_merge && !self.v_merge
    }
}

/// 单元格四边边框，另含两条对角线。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TableBorders {
    pub left: Option<BorderLine>,
    pub top: Option<BorderLine>,
    pub right: Option<BorderLine>,
    pub bottom: Option<BorderLine>,
    /// 左上→右下对角线。
    pub tl_to_br: Option<BorderLine>,
    /// 左下→右上对角线。
    pub tr_to_bl: Option<BorderLine>,
}

impl TableBorders {
    /// 是否所有边框都缺失（用于渲染时判断是否回退到表格级默认边框）。
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.left.is_none()
            && self.top.is_none()
            && self.right.is_none()
            && self.bottom.is_none()
            && self.tl_to_br.is_none()
            && self.tr_to_bl.is_none()
    }
}

/// 单条边框线。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BorderLine {
    pub color: Color,
    pub width_pt: f32,
    pub dash: DashStyle,
    /// 复合线类型（双线等），`true` 表示双线。
    pub double: bool,
    pub join: LineJoin,
}

impl BorderLine {
    pub fn new(color: Color, width_pt: f32) -> BorderLine {
        BorderLine {
            color,
            width_pt,
            dash: DashStyle::Solid,
            double: false,
            join: LineJoin::Miter,
        }
    }

    #[inline]
    pub fn is_visible(&self) -> bool {
        self.width_pt > 0.0 && !self.color.is_transparent()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_table() -> Table {
        let cols = vec![100.0, 150.0];
        let rows = vec![40.0, 60.0, 30.0];
        let cells = rows
            .iter()
            .map(|_| {
                cols.iter()
                    .map(|_| TableCell::default())
                    .collect::<Vec<_>>()
            })
            .collect();
        Table {
            columns: cols,
            rows,
            cells,
            ..Default::default()
        }
    }

    #[test]
    fn table_dimensions() {
        let t = sample_table();
        assert_eq!(t.row_count(), 3);
        assert_eq!(t.col_count(), 2);
        assert_eq!(t.total_width_pt(), 250.0);
        assert_eq!(t.total_height_pt(), 130.0);
        assert!(!t.is_empty());
    }

    #[test]
    fn row_and_col_offsets() {
        let t = sample_table();
        assert_eq!(t.row_offset_pt(0), 0.0);
        assert_eq!(t.row_offset_pt(1), 40.0);
        assert_eq!(t.row_offset_pt(2), 100.0);
        assert_eq!(t.col_offset_pt(1), 100.0);
    }

    #[test]
    fn empty_table_detection() {
        assert!(Table::default().is_empty());
    }

    #[test]
    fn spanned_cells_lists_merged_only() {
        let mut t = sample_table();
        t.cells[0][0].col_span = 2;
        t.cells[0][1].h_merge = true;
        let spanned = t.spanned_cells();
        assert_eq!(spanned, vec![(0, 1)]);
        // 跨列的主单元格负责绘制合并后的整块区域，因此是可绘制的
        assert!(t.cells[0][0].is_drawable());
        // 被覆盖的从属格跳过绘制，避免边框与底纹重复
        assert!(!t.cells[0][1].is_drawable());
        assert!(t.cells[1][0].is_drawable());
    }

    #[test]
    fn default_cell_margins_match_ooxml() {
        let c = TableCell::default();
        assert!((c.margins.left - 7.2).abs() < 1e-4);
        assert!((c.margins.top - 3.6).abs() < 1e-4);
    }

    #[test]
    fn border_visibility() {
        assert!(BorderLine::new(Color::BLACK, 1.0).is_visible());
        assert!(!BorderLine::new(Color::BLACK, 0.0).is_visible());
        assert!(!BorderLine::new(Color::TRANSPARENT, 1.0).is_visible());
    }

    #[test]
    fn empty_borders_detection() {
        assert!(TableBorders::default().is_empty());
        let b = TableBorders {
            top: Some(BorderLine::new(Color::BLACK, 1.0)),
            ..Default::default()
        };
        assert!(!b.is_empty());
    }
}
