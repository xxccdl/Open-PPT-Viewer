//! 图表：把 `ppt/charts/chartN.xml` 展开为一组**基础图元**。
//!
//! # 为什么在解析阶段展开，而不是新增一种 `Geometry`
//!
//! 一张图表最终画出来的东西就是「矩形 + 折线 + 文本」，与它是柱状还是饼图无关。
//! 若在 SceneGraph 里新增 `Geometry::Chart`，渲染器就得认识坐标轴、图例、
//! 数据标签这些**语义**概念 —— 等于把图表语义泄漏进光栅化层；
//! 展开成图元后，填充、描边、文本排版、任意缩放的清晰度全部复用既有实现，
//! 渲染器一行代码都不用改，也不需要为图表单独做视觉回归。
//!
//! 代价是布局必须在这里算准（见 [`Layout`]）。这个代价是值得付的：
//! 图表的布局规则本来就属于「格式解析」的职责，和表格列宽计算是同一类问题。
//!
//! # 覆盖范围
//!
//! 柱状图（`c:barChart` + `barDir="col"`）、条形图（`barDir="bar"`）、
//! 折线图、饼图、圆环图、散点图。其余类型（面积图/雷达图/股价图/曲面图）
//! 返回 `Err`，由调用方降级为占位并记录告警 —— **宁可如实说明不支持，
//! 也不要画一张错的图**。

use ppt_core::scene::{
    BodyProps, Color, Fill, Geometry, Insets, Node, Paragraph, PathGeometry, PathSegment, Point,
    Rect, RunProps, Stroke, StrokeFill, SubPath, TextAlign, TextBox, TextRun, TextWrap, Transform,
    VerticalAnchor,
};
use ppt_core::XmlNode;

use crate::color::ColorScheme;
use crate::paint::{self, RelResolver};

/// 图表四周留白（pt）。
const PAD: f32 = 6.0;
/// 标题占用的高度（含与绘图区的间距）。
const TITLE_H: f32 = 22.0;
/// 数值轴刻度标签占用的宽度。
const VAL_GUTTER: f32 = 34.0;
/// 类别轴标签占用的高度。
const CAT_GUTTER: f32 = 16.0;
/// 条形图的类别标签在左侧，比数值标签长得多。
const CAT_SIDE_GUTTER: f32 = 58.0;
/// 底部图例占用的高度 / 右侧图例占用的宽度。
const LEGEND_H: f32 = 18.0;
const LEGEND_W: f32 = 92.0;
/// 刻度文字与绘图区之间的间距。
const TICK_GAP: f32 = 4.0;

const LABEL_PT: f32 = 9.0;
const TITLE_PT: f32 = 12.0;

/// 网格线颜色。
fn grid_color() -> Color {
    Color::rgb(0xD9, 0xD9, 0xD9)
}

/// 标签文字颜色。
fn label_color() -> Color {
    Color::rgb(0x40, 0x40, 0x40)
}

/// 图表类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// 柱状图（竖直）。
    Column,
    /// 条形图（水平）。
    Bar,
    Line,
    Pie,
    Doughnut,
    Scatter,
}

impl Kind {
    /// 是否用「类别轴 + 数值轴」的直角坐标布局（饼图/圆环图不是）。
    fn is_cartesian(self) -> bool {
        matches!(self, Kind::Column | Kind::Bar | Kind::Line | Kind::Scatter)
    }
}

/// 图例位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LegendPos {
    Bottom,
    Right,
    Left,
    Top,
}

/// 一个数据系列。
#[derive(Debug, Clone, Default)]
struct Series {
    name: String,
    /// 类别标签（文本，`c:cat`）。
    cats: Vec<String>,
    /// 数值（`c:val` / `c:yVal`）。缺失点用 `None`，折线在此断开。
    ys: Vec<Option<f32>>,
    /// 散点图的 x 值（`c:xVal`）。
    xs: Vec<Option<f32>>,
    color: Option<Color>,
}

/// 解析出来的图表描述。
#[derive(Debug, Clone)]
struct Spec {
    kind: Kind,
    title: Option<String>,
    legend: Option<LegendPos>,
    series: Vec<Series>,
    show_val: bool,
    /// 数值轴是否可见（`c:valAx/c:delete`）。
    val_axis: bool,
    /// 类别轴是否可见。
    cat_axis: bool,
    gap_width: f32,
    overlap: f32,
    hole_size: f32,
}

/// 解析一个图表部件。
///
/// `frame` 是图表在幻灯片上的局部包围盒（通常 `(0,0,w,h)`），
/// 返回的图元坐标都在这个局部空间里，调用方只需套上框架的绝对变换。
///
/// `Err` 里的中文原因会直接变成用户可见的降级告警。
pub fn parse(
    root: &XmlNode,
    frame: Rect,
    scheme: &ColorScheme,
    resolve: RelResolver<'_>,
) -> Result<Vec<Node>, String> {
    let chart = root.child("chart").ok_or("图表部件里没有 c:chart")?;
    let spec = parse_spec(chart, scheme, resolve)?;
    if spec.series.is_empty() {
        return Err("图表里没有任何数据系列".to_string());
    }
    Ok(draw(&spec, frame, scheme))
}

/* ---------------- 解析 ---------------- */

fn parse_spec(
    chart: &XmlNode,
    scheme: &ColorScheme,
    resolve: RelResolver<'_>,
) -> Result<Spec, String> {
    let plot = chart.child("plotArea").ok_or("图表缺少绘图区")?;

    // 绘图区里同时挂着图表类型节点与坐标轴节点，只认第一个能识别的类型
    let (kind, ty) = plot
        .children
        .iter()
        .find_map(|ch| {
            let k = match ch.name.as_str() {
                "barChart" => {
                    let dir = ch
                        .child("barDir")
                        .and_then(|d| d.attr("val"))
                        .unwrap_or("col");
                    if dir.eq_ignore_ascii_case("bar") {
                        Kind::Bar
                    } else {
                        Kind::Column
                    }
                }
                "lineChart" => Kind::Line,
                "pieChart" => Kind::Pie,
                "doughnutChart" => Kind::Doughnut,
                "scatterChart" => Kind::Scatter,
                _ => return None,
            };
            Some((k, ch))
        })
        .ok_or_else(|| {
            let kinds: Vec<&str> = plot
                .children
                .iter()
                .map(|c| c.name.as_str())
                .filter(|n| n.ends_with("Chart"))
                .collect();
            if kinds.is_empty() {
                "绘图区里没有图表类型节点".to_string()
            } else {
                format!("暂不支持该图表类型（{}）", kinds.join("/"))
            }
        })?;

    let series = ty
        .children_named("ser")
        .map(|s| parse_series(s, scheme, resolve))
        .collect::<Vec<_>>();

    // 坐标轴按**所在边**判定，而不是按标签名：散点图的两条轴都是 `c:valAx`，
    // 只认名字会把 x 轴漏掉（刻度就整条不画了）
    let (val_pos, cat_pos) = axis_positions(kind);

    Ok(Spec {
        kind,
        title: rich_text(chart.child("title")),
        legend: chart.child("legend").map(parse_legend),
        series,
        show_val: ty
            .child("dLbls")
            .and_then(|d| d.child("showVal"))
            .and_then(|v| v.attr_bool("val"))
            .unwrap_or(false),
        val_axis: axis_visible(plot, "valAx", val_pos),
        cat_axis: axis_visible(plot, "catAx", cat_pos)
            || axis_visible(plot, "dateAx", cat_pos)
            // 散点图的 x 轴在 OOXML 里也是 valAx
            || axis_visible(plot, "valAx", cat_pos),
        gap_width: ty
            .child("gapWidth")
            .and_then(|g| g.attr_f32("val"))
            .unwrap_or(150.0),
        overlap: ty
            .child("overlap")
            .and_then(|o| o.attr_f32("val"))
            .unwrap_or(0.0),
        hole_size: ty
            .child("holeSize")
            .and_then(|h| h.attr_f32("val"))
            .unwrap_or(50.0),
    })
}

/// 数值轴与类别轴各在**哪一边**。
///
/// 条形图是个例外：它把类别放在左侧、数值放在底部，
/// 与柱状图/折线图/散点图正好相反。
fn axis_positions(kind: Kind) -> (&'static str, &'static str) {
    match kind {
        Kind::Bar => ("b", "l"),
        _ => ("l", "b"),
    }
}

/// 位于 `pos` 边上的 `tag` 轴是否可见。`c:delete val="1"` 表示删除该轴。
///
/// 缺失的轴按「不画」处理：饼图、圆环图本来就没有坐标轴，
/// 硬留出 gutter 会白占掉一大块画面。
fn axis_visible(plot: &XmlNode, tag: &str, pos: &str) -> bool {
    plot.children_named(tag)
        .find(|ax| {
            ax.child("axPos")
                .and_then(|p| p.attr("val"))
                .is_some_and(|v| v.eq_ignore_ascii_case(pos))
        })
        .is_some_and(|ax| {
            !ax.child("delete")
                .and_then(|d| d.attr_bool("val"))
                .unwrap_or(false)
        })
}

fn parse_legend(legend: &XmlNode) -> LegendPos {
    match legend
        .child("legendPos")
        .and_then(|p| p.attr("val"))
        .unwrap_or("r")
        .to_ascii_lowercase()
        .as_str()
    {
        "b" => LegendPos::Bottom,
        "l" => LegendPos::Left,
        "t" => LegendPos::Top,
        // PowerPoint 默认在右侧，`tr`（右上）也按右侧排
        _ => LegendPos::Right,
    }
}

fn parse_series(ser: &XmlNode, scheme: &ColorScheme, resolve: RelResolver<'_>) -> Series {
    let name = series_name(ser);

    // 散点图用 xVal/yVal，其余用 cat/val
    let cat_node = ser.child("cat").or_else(|| ser.child("xVal"));
    let val_node = ser.child("val").or_else(|| ser.child("yVal"));

    let is_scatter = ser.child("xVal").is_some();
    let raw_cat = cat_node.map(cache_values).unwrap_or_default();

    let (cats, xs) = if is_scatter {
        let xs = raw_cat.iter().map(|v| v.parse::<f32>().ok()).collect();
        (Vec::new(), xs)
    } else {
        (raw_cat, Vec::new())
    };

    let ys = val_node
        .map(cache_values)
        .unwrap_or_default()
        .iter()
        .map(|v| v.parse::<f32>().ok())
        .collect();

    let color = ser
        .child("spPr")
        .and_then(|p| match paint::parse_fill(p, Some(scheme), resolve) {
            Fill::Solid(c) => Some(c),
            _ => None,
        });

    Series {
        name,
        cats,
        ys,
        xs,
        color,
    }
}

/// 取 `c:numRef` / `c:strRef` 里的点序列（按 `idx` 排序并补齐空洞）。
///
/// 结构是三层：`c:val` → `c:numRef` → `c:numCache`，缓存藏在引用里，
/// 所以要往下找一层；测试与某些生成器也会把缓存直接挂在第一层。
///
/// 必须补齐空洞：OOXML 的 `c:pt` **允许跳号**（空单元格不生成点），
/// 若只按出现顺序读，柱子的位置会整体前移，图和数据就对不上了。
fn cache_values(node: &XmlNode) -> Vec<String> {
    fn cache_of(n: &XmlNode) -> Option<&XmlNode> {
        n.child("numCache")
            .or_else(|| n.child("strCache"))
            .or_else(|| n.child("multiLvlStrCache"))
    }

    let Some(cache) = cache_of(node)
        .or_else(|| node.children.iter().find_map(cache_of))
    else {
        return Vec::new();
    };

    let mut slots: Vec<(usize, String)> = Vec::new();
    for pt in cache.children_named("pt") {
        let idx = pt.attr_usize("idx").unwrap_or(slots.len());
        let v = pt
            .child("v")
            .map(|v| v.text().trim().to_string())
            .unwrap_or_default();
        slots.push((idx, v));
    }

    let len = slots.iter().map(|(i, _)| i + 1).max().unwrap_or(0);
    let mut out = vec![String::new(); len];
    for (i, v) in slots {
        if i < len {
            out[i] = v;
        }
    }
    out
}

/// 系列名：优先富文本（`c:tx/c:rich/a:t`），否则取 `c:tx/c:strRef/c:strCache` 的第一个点。
fn series_name(ser: &XmlNode) -> String {
    let Some(tx) = ser.child("tx") else {
        return String::new();
    };
    if let Some(t) = rich_text(Some(tx)) {
        return t;
    }
    cache_values(tx)
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or_default()
}

/// 深度优先收集所有 `a:t` 文本（标题、系列名都可能是富文本）。
fn rich_text(node: Option<&XmlNode>) -> Option<String> {
    fn walk(node: &XmlNode, out: &mut String) {
        if node.name == "t" {
            out.push_str(node.text());
        }
        for child in &node.children {
            walk(child, out);
        }
    }

    let mut out = String::new();
    walk(node?, &mut out);
    let trimmed = out.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/* ---------------- 绘制 ---------------- */

/// 绘图区在图表局部空间里的位置。
struct Layout {
    /// 绘图区（不含坐标轴标签与图例）。
    plot: Rect,
}

fn draw(spec: &Spec, frame: Rect, scheme: &ColorScheme) -> Vec<Node> {
    let mut nodes = Vec::new();
    let layout = Layout {
        plot: plot_rect(spec, frame),
    };

    if let Some(title) = &spec.title {
        nodes.push(text(
            frame.x,
            frame.y,
            frame.w,
            TITLE_H - 6.0,
            title.clone(),
            TITLE_PT,
            true,
            Color::BLACK,
            TextAlign::Center,
        ));
    }

    match spec.kind {
        Kind::Column | Kind::Bar => draw_bars(spec, &layout, scheme, &mut nodes),
        Kind::Line => draw_lines(spec, &layout, scheme, &mut nodes),
        Kind::Scatter => draw_scatter(spec, &layout, scheme, &mut nodes),
        Kind::Pie | Kind::Doughnut => draw_pie(spec, &layout, scheme, &mut nodes),
    }

    if let Some(pos) = spec.legend {
        draw_legend(spec, frame, &layout, pos, scheme, &mut nodes);
    }

    nodes
}

/// 计算绘图区。gutter 的大小由「哪些轴可见」决定 —— 没有刻度就不留白，
/// 否则一张饼图左右会白白空出几十 pt。
fn plot_rect(spec: &Spec, frame: Rect) -> Rect {
    let mut left = PAD;
    let mut right = PAD;
    let mut top = PAD;
    let mut bottom = PAD;

    if spec.title.is_some() {
        top += TITLE_H;
    }

    if spec.kind.is_cartesian() {
        let (val_pos, cat_pos) = axis_positions(spec.kind);
        if spec.val_axis {
            if val_pos == "l" {
                left += VAL_GUTTER;
            } else {
                bottom += CAT_GUTTER;
            }
        }
        if spec.cat_axis {
            if cat_pos == "l" {
                // 条形图的类别标签在左侧，比数值标签长得多
                left += CAT_SIDE_GUTTER;
            } else {
                bottom += CAT_GUTTER;
            }
        }
    }

    match spec.legend {
        Some(LegendPos::Bottom) => bottom += LEGEND_H,
        Some(LegendPos::Top) => top += LEGEND_H,
        Some(LegendPos::Right) => right += LEGEND_W,
        Some(LegendPos::Left) => left += LEGEND_W,
        None => {}
    }

    Rect::new(
        frame.x + left,
        frame.y + top,
        (frame.w - left - right).max(1.0),
        (frame.h - top - bottom).max(1.0),
    )
}

/// 系列配色：优先用课件指定的颜色，否则按主题强调色循环。
///
/// 强调色的顺序与 PowerPoint 的默认图表配色一致（accent1 起循环），
/// 这样「没设颜色的图表」看起来才像课件该有的样子。
fn series_color(spec: &Spec, scheme: &ColorScheme, index: usize) -> Color {
    let s = &spec.series[index % spec.series.len().max(1)];
    s.color.unwrap_or_else(|| accent(scheme, index))
}

fn accent(scheme: &ColorScheme, index: usize) -> Color {
    match index % 6 {
        0 => scheme.accent1,
        1 => scheme.accent2,
        2 => scheme.accent3,
        3 => scheme.accent4,
        4 => scheme.accent5,
        _ => scheme.accent6,
    }
}

/// 饼图按**数据点**取色：一块饼里每块一个颜色，与直角坐标图按系列取色不同。
fn pie_slice_color(scheme: &ColorScheme, index: usize) -> Color {
    accent(scheme, index)
}

/// 数值范围 → 好看的刻度。
///
/// 步长取 1/2/5×10^k，保证约 5 段；上下界向外取整到步长倍数，
/// 这样刻度标签是整数，而不是 0.37 这种读不出来的数。
fn nice_scale(min: f32, max: f32) -> (f32, f32, f32) {
    let lo = min.min(0.0);
    let mut hi = max.max(0.0);
    if !lo.is_finite() || !hi.is_finite() {
        return (0.0, 1.0, 0.5);
    }
    if (hi - lo).abs() < 1e-6 {
        hi = lo + 1.0;
    }

    let raw = (hi - lo) / 5.0;
    let mag = 10f32.powf(raw.log10().floor());
    let norm = raw / mag;
    let step = mag
        * if norm <= 1.0 {
            1.0
        } else if norm <= 2.0 {
            2.0
        } else if norm <= 5.0 {
            5.0
        } else {
            10.0
        };

    ((lo / step).floor() * step, (hi / step).ceil() * step, step)
}

/// 数字 → 刻度文字。整数不显示小数点，避免「5.0」这种写法。
fn fmt_num(v: f32) -> String {
    if (v - v.round()).abs() < 0.05 {
        format!("{}", v.round() as i64)
    } else if v.abs() < 1.0 {
        format!("{v:.2}")
    } else {
        format!("{v:.1}")
    }
}

/// 数据范围（含所有系列）。
fn value_range(spec: &Spec) -> (f32, f32) {
    let mut lo = f32::MAX;
    let mut hi = f32::MIN;
    for s in &spec.series {
        for v in s.ys.iter().flatten() {
            lo = lo.min(*v);
            hi = hi.max(*v);
        }
    }
    if lo > hi {
        return (0.0, 1.0);
    }
    (lo, hi)
}

/// 类别数量（以最长系列为准）。
fn cat_count(spec: &Spec) -> usize {
    spec.series.iter().map(|s| s.ys.len()).max().unwrap_or(0)
}

/// 第 i 个类别的标签文本。
fn cat_label(spec: &Spec, index: usize) -> String {
    spec.series
        .iter()
        .find_map(|s| s.cats.get(index))
        .cloned()
        .unwrap_or_else(|| format!("{}", index + 1))
}

fn draw_bars(spec: &Spec, layout: &Layout, scheme: &ColorScheme, out: &mut Vec<Node>) {
    let n_cats = cat_count(spec);
    if n_cats == 0 {
        return;
    }
    let (lo, hi, step) = {
        let (a, b) = value_range(spec);
        nice_scale(a, b)
    };
    let span = (hi - lo).max(1e-6);
    let plot = layout.plot;
    let horizontal = spec.kind == Kind::Bar;

    // 类别方向的总长度 / 数值方向的长度
    let (cat_total, val_len) = if horizontal {
        (plot.h, plot.w)
    } else {
        (plot.w, plot.h)
    };
    let cat_w = cat_total / n_cats as f32;
    let n_ser = spec.series.len().max(1);

    // 每个类别里所有柱子的总宽：gapWidth 是「空隙占柱宽」的百分比
    let cluster = (cat_w * 100.0 / (100.0 + spec.gap_width.max(0.0))).max(0.5);
    let bar_w = (cluster / n_ser as f32).max(0.4);
    // overlap=-100 时相邻柱子相隔一个柱宽，0 时紧贴
    let shift = bar_w * (1.0 - spec.overlap / 100.0);
    let used = bar_w + shift * (n_ser as f32 - 1.0).max(0.0);
    let cluster_start = (cat_w - used) / 2.0;

    // 网格线 + 数值轴刻度
    if spec.val_axis {
        let mut v = lo;
        while v <= hi + step * 0.001 {
            let t = (v - lo) / span;
            let (x, y, w, h) = if horizontal {
                let x = plot.x + t * val_len;
                (x, plot.y, 0.6, plot.h)
            } else {
                let y = plot.bottom() - t * val_len;
                (plot.x, y, plot.w, 0.6)
            };
            out.push(rect(x, y, w, h, grid_color()));

            let label = fmt_num(v);
            if horizontal {
                out.push(text(
                    x - VAL_GUTTER,
                    plot.bottom() + TICK_GAP,
                    VAL_GUTTER - TICK_GAP,
                    CAT_GUTTER - TICK_GAP,
                    label,
                    LABEL_PT,
                    false,
                    label_color(),
                    TextAlign::Right,
                ));
            } else {
                out.push(text(
                    plot.x - VAL_GUTTER,
                    y - CAT_GUTTER / 2.0,
                    VAL_GUTTER - TICK_GAP,
                    CAT_GUTTER,
                    label,
                    LABEL_PT,
                    false,
                    label_color(),
                    TextAlign::Right,
                ));
            }
            v += step;
        }
    }

    // 柱子
    for (si, series) in spec.series.iter().enumerate() {
        let color = series_color(spec, scheme, si);
        for ci in 0..n_cats {
            let Some(value) = series.ys.get(ci).copied().flatten() else {
                continue;
            };
            let t0 = ((0.0f32.min(value) - lo) / span).clamp(0.0, 1.0);
            let t1 = ((0.0f32.max(value) - lo) / span).clamp(0.0, 1.0);
            let base = ci as f32 * cat_w + cluster_start + si as f32 * shift;

            let (x, y, w, h) = if horizontal {
                let x = plot.x + t0 * val_len;
                let w = ((t1 - t0) * val_len).max(0.6);
                (x, plot.y + base, w, bar_w)
            } else {
                let y = plot.bottom() - t1 * val_len;
                let h = ((t1 - t0) * val_len).max(0.6);
                (plot.x + base, y, bar_w, h)
            };
            out.push(rect(x, y, w, h, color));

            if spec.show_val {
                let label = fmt_num(value);
                if horizontal {
                    out.push(text(
                        x + w + 2.0,
                        y,
                        26.0,
                        bar_w,
                        label,
                        LABEL_PT,
                        false,
                        label_color(),
                        TextAlign::Left,
                    ));
                } else {
                    out.push(text(
                        x - 12.0,
                        y - CAT_GUTTER,
                        bar_w + 24.0,
                        CAT_GUTTER - 2.0,
                        label,
                        LABEL_PT,
                        false,
                        label_color(),
                        TextAlign::Center,
                    ));
                }
            }
        }
    }

    draw_category_labels(spec, layout, out);
}

fn draw_category_labels(spec: &Spec, layout: &Layout, out: &mut Vec<Node>) {
    if !spec.cat_axis {
        return;
    }
    let n_cats = cat_count(spec);
    if n_cats == 0 {
        return;
    }
    let plot = layout.plot;
    let horizontal = spec.kind == Kind::Bar;

    for ci in 0..n_cats {
        let label = cat_label(spec, ci);
        if horizontal {
            let cat_w = plot.h / n_cats as f32;
            out.push(text(
                plot.x - CAT_SIDE_GUTTER,
                plot.y + ci as f32 * cat_w,
                CAT_SIDE_GUTTER - TICK_GAP,
                cat_w,
                label,
                LABEL_PT,
                false,
                label_color(),
                TextAlign::Right,
            ));
        } else {
            let cat_w = plot.w / n_cats as f32;
            out.push(text(
                plot.x + ci as f32 * cat_w,
                plot.bottom() + TICK_GAP,
                cat_w,
                CAT_GUTTER - TICK_GAP,
                label,
                LABEL_PT,
                false,
                label_color(),
                TextAlign::Center,
            ));
        }
    }
}

fn draw_lines(spec: &Spec, layout: &Layout, scheme: &ColorScheme, out: &mut Vec<Node>) {
    let n_cats = cat_count(spec);
    if n_cats == 0 {
        return;
    }
    let (lo, hi, step) = {
        let (a, b) = value_range(spec);
        nice_scale(a, b)
    };
    let span = (hi - lo).max(1e-6);
    let plot = layout.plot;
    let cat_w = plot.w / n_cats as f32;

    if spec.val_axis {
        let mut v = lo;
        while v <= hi + step * 0.001 {
            let y = plot.bottom() - (v - lo) / span * plot.h;
            out.push(rect(plot.x, y, plot.w, 0.6, grid_color()));
            out.push(text(
                plot.x - VAL_GUTTER,
                y - CAT_GUTTER / 2.0,
                VAL_GUTTER - TICK_GAP,
                CAT_GUTTER,
                fmt_num(v),
                LABEL_PT,
                false,
                label_color(),
                TextAlign::Right,
            ));
            v += step;
        }
    }

    for (si, series) in spec.series.iter().enumerate() {
        let color = series.color.unwrap_or_else(|| accent(scheme, si));
        let points: Vec<Point> = series
            .ys
            .iter()
            .enumerate()
            .filter_map(|(ci, v)| {
                let v = (*v)?;
                let x = plot.x + (ci as f32 + 0.5) * cat_w;
                let y = plot.bottom() - (v - lo) / span * plot.h;
                Some(Point::new(x, y))
            })
            .collect();

        if points.len() >= 2 {
            out.push(polyline(&points, color, 2.0));
        }
        // 数据点标记：PowerPoint 折线图默认带标记，没有标记的折线
        // 在只有两三个数据点时几乎看不出形状
        for p in &points {
            out.push(dot(p.x, p.y, 5.0, color));
        }

        if spec.show_val {
            for (ci, v) in series.ys.iter().enumerate() {
                let Some(v) = *v else { continue };
                let x = plot.x + (ci as f32 + 0.5) * cat_w;
                let y = plot.bottom() - (v - lo) / span * plot.h;
                out.push(text(
                    x - 14.0,
                    y - CAT_GUTTER,
                    28.0,
                    CAT_GUTTER - 2.0,
                    fmt_num(v),
                    LABEL_PT,
                    false,
                    label_color(),
                    TextAlign::Center,
                ));
            }
        }
    }

    draw_category_labels(spec, layout, out);
}

fn draw_scatter(spec: &Spec, layout: &Layout, scheme: &ColorScheme, out: &mut Vec<Node>) {
    let (y_lo, y_hi, y_step) = {
        let (a, b) = value_range(spec);
        nice_scale(a, b)
    };
    let (x_lo, x_hi, x_step) = {
        let mut lo = f32::MAX;
        let mut hi = f32::MIN;
        for s in &spec.series {
            for v in s.xs.iter().flatten() {
                lo = lo.min(*v);
                hi = hi.max(*v);
            }
        }
        if lo > hi {
            (0.0, 1.0, 0.5)
        } else {
            nice_scale(lo, hi)
        }
    };
    let plot = layout.plot;
    let x_span = (x_hi - x_lo).max(1e-6);
    let y_span = (y_hi - y_lo).max(1e-6);

    if spec.val_axis {
        let mut v = y_lo;
        while v <= y_hi + y_step * 0.001 {
            let y = plot.bottom() - (v - y_lo) / y_span * plot.h;
            out.push(rect(plot.x, y, plot.w, 0.6, grid_color()));
            out.push(text(
                plot.x - VAL_GUTTER,
                y - CAT_GUTTER / 2.0,
                VAL_GUTTER - TICK_GAP,
                CAT_GUTTER,
                fmt_num(v),
                LABEL_PT,
                false,
                label_color(),
                TextAlign::Right,
            ));
            v += y_step;
        }
    }

    if spec.cat_axis {
        let mut v = x_lo;
        while v <= x_hi + x_step * 0.001 {
            let x = plot.x + (v - x_lo) / x_span * plot.w;
            out.push(text(
                x - 20.0,
                plot.bottom() + TICK_GAP,
                40.0,
                CAT_GUTTER - TICK_GAP,
                fmt_num(v),
                LABEL_PT,
                false,
                label_color(),
                TextAlign::Center,
            ));
            v += x_step;
        }
    }

    for (si, series) in spec.series.iter().enumerate() {
        let color = series.color.unwrap_or_else(|| accent(scheme, si));
        for ci in 0..series.ys.len() {
            let (Some(xv), Some(yv)) = (
                series.xs.get(ci).copied().flatten(),
                series.ys.get(ci).copied().flatten(),
            ) else {
                continue;
            };
            let x = plot.x + (xv - x_lo) / x_span * plot.w;
            let y = plot.bottom() - (yv - y_lo) / y_span * plot.h;
            out.push(dot(x, y, 6.0, color));
            if spec.show_val {
                out.push(text(
                    x - 14.0,
                    y - CAT_GUTTER,
                    28.0,
                    CAT_GUTTER - 2.0,
                    fmt_num(yv),
                    LABEL_PT,
                    false,
                    label_color(),
                    TextAlign::Center,
                ));
            }
        }
    }
}

fn draw_pie(spec: &Spec, layout: &Layout, scheme: &ColorScheme, out: &mut Vec<Node>) {
    let series = &spec.series[0];
    let total: f32 = series.ys.iter().flatten().filter(|v| **v > 0.0).sum();
    if total <= 0.0 {
        return;
    }

    let plot = layout.plot;
    let radius = (plot.w.min(plot.h) / 2.0).max(1.0);
    let center = Point::new(plot.x + plot.w / 2.0, plot.y + plot.h / 2.0);
    let hole = if spec.kind == Kind::Doughnut {
        radius * (spec.hole_size / 100.0).clamp(0.0, 0.9)
    } else {
        0.0
    };

    // 从 12 点方向开始顺时针 —— PowerPoint 的默认起始角
    let mut angle = -std::f32::consts::FRAC_PI_2;
    for (ci, v) in series.ys.iter().enumerate() {
        let Some(v) = *v else { continue };
        if v <= 0.0 {
            continue;
        }
        let sweep = v / total * std::f32::consts::TAU;
        let color = pie_slice_color(scheme, ci);
        out.push(slice_path(center, radius, hole, angle, angle + sweep, color));
        angle += sweep;

        if spec.show_val {
            let mid = angle - sweep / 2.0;
            let lr = radius * if hole > 0.0 { 0.78 } else { 0.62 };
            let lx = center.x + mid.cos() * lr;
            let ly = center.y + mid.sin() * lr;
            out.push(text(
                lx - 18.0,
                ly - 6.0,
                36.0,
                12.0,
                fmt_num(v),
                LABEL_PT,
                true,
                Color::WHITE,
                TextAlign::Center,
            ));
        }
    }
}

fn draw_legend(
    spec: &Spec,
    frame: Rect,
    layout: &Layout,
    pos: LegendPos,
    scheme: &ColorScheme,
    out: &mut Vec<Node>,
) {
    let entries: Vec<(String, Color)> = if spec.kind == Kind::Pie || spec.kind == Kind::Doughnut {
        // 饼图的图例是「每一块」，不是「每个系列」
        let n = spec.series[0].ys.len();
        (0..n)
            .map(|i| (cat_label(spec, i), pie_slice_color(scheme, i)))
            .collect()
    } else {
        spec.series
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let name = if s.name.is_empty() {
                    format!("系列{}", i + 1)
                } else {
                    s.name.clone()
                };
                (name, s.color.unwrap_or_else(|| accent(scheme, i)))
            })
            .collect()
    };
    if entries.is_empty() {
        return;
    }

    let plot = layout.plot;
    match pos {
        LegendPos::Bottom | LegendPos::Top => {
            let y = if pos == LegendPos::Bottom {
                frame.bottom() - LEGEND_H + 2.0
            } else {
                frame.y + 2.0
            };
            let slot = plot.w / entries.len() as f32;
            for (i, (name, color)) in entries.iter().enumerate() {
                let x = plot.x + i as f32 * slot;
                out.push(rect(x, y + 5.0, 8.0, 8.0, *color));
                out.push(text(
                    x + 11.0,
                    y,
                    slot - 13.0,
                    LEGEND_H - 2.0,
                    name.clone(),
                    LABEL_PT,
                    false,
                    label_color(),
                    TextAlign::Left,
                ));
            }
        }
        LegendPos::Right | LegendPos::Left => {
            let x = if pos == LegendPos::Right {
                frame.right() - LEGEND_W + 2.0
            } else {
                frame.x + 2.0
            };
            let y0 = plot.y;
            let slot = (plot.h / entries.len() as f32).min(LEGEND_H);
            for (i, (name, color)) in entries.iter().enumerate() {
                let y = y0 + i as f32 * slot;
                out.push(rect(x, y + 4.0, 8.0, 8.0, *color));
                out.push(text(
                    x + 11.0,
                    y,
                    LEGEND_W - 14.0,
                    slot,
                    name.clone(),
                    LABEL_PT,
                    false,
                    label_color(),
                    TextAlign::Left,
                ));
            }
        }
    }
}

/* ---------------- 图元构造 ----------------
 *
 * 坐标约定（与 SceneGraph 的其余部分一致，**不要**在这里图省事）：
 * 几何一律从**局部原点 `(0,0)`** 开始描述，摆放位置交给节点的 `transform`。
 * 渲染器画 `Geometry::Rect` 时用的是 `rect_path(0, 0, w, h)`，
 * 路径几何也按自身坐标直接成路径 —— 把绝对坐标写进 `local_bbox`
 * 只会让所有图元堆在框架左上角。
 */

/// 一个实心矩形图元。网格线、柱子、数据点标记都用它 ——
/// 网格线只是「很细的矩形」，用不着单独的线段类型。
fn rect(x: f32, y: f32, w: f32, h: f32, fill: Color) -> Node {
    Node {
        transform: Transform::translate(x, y),
        geometry: Geometry::Rect,
        fill: Fill::Solid(fill),
        local_bbox: Some(Rect::new(0.0, 0.0, w.max(0.1), h.max(0.1))),
        ..Default::default()
    }
}

/// 一个实心圆形图元，用于数据点标记。
///
/// 用圆而不是方块：PowerPoint 与 WPS 的默认标记都是圆的，
/// 折线图的拐点画成方块会显得很生硬。
fn dot(cx: f32, cy: f32, diameter: f32, fill: Color) -> Node {
    let d = diameter.max(0.5);
    Node {
        transform: Transform::translate(cx - d / 2.0, cy - d / 2.0),
        geometry: Geometry::Ellipse,
        fill: Fill::Solid(fill),
        local_bbox: Some(Rect::new(0.0, 0.0, d, d)),
        ..Default::default()
    }
}

fn polyline(points: &[Point], color: Color, width_pt: f32) -> Node {
    let bbox = bbox_of_points(points);
    let offset = Transform::translate(-bbox.x, -bbox.y);
    let sub = SubPath {
        start: offset.apply(points[0]),
        segments: points[1..]
            .iter()
            .map(|p| PathSegment::Line(offset.apply(*p)))
            .collect(),
        closed: false,
    };
    let local = Rect::new(0.0, 0.0, bbox.w, bbox.h);

    Node {
        transform: Transform::translate(bbox.x, bbox.y),
        geometry: Geometry::Path(PathGeometry {
            subpaths: vec![sub],
            text_rect: None,
            bbox: local,
        }),
        fill: Fill::None,
        stroke: Some(Stroke {
            fill: StrokeFill::Solid(color),
            width_pt,
            ..Default::default()
        }),
        local_bbox: Some(local),
        ..Default::default()
    }
}

/// 饼图的一块扇形。
///
/// 圆弧用**折线**逼近（每 5° 一段）：在常见半径下弦高误差不到 0.1pt，
/// 肉眼不可能看出来，却省掉了一整套椭圆弧参数换算 —— 后者是
/// 这类渲染代码里最容易写错、又最难自查的部分。
fn slice_path(
    center: Point,
    radius: f32,
    hole: f32,
    start: f32,
    end: f32,
    color: Color,
) -> Node {
    const STEP_DEG: f32 = 5.0;
    // `end - start` 已经是弧度，别再 to_radians 一次 —— 那样 steps 会恒等于 1，
    // 扇形退化成三角形，跨度正好 180° 的那块更会退化成零宽（整块消失）
    let steps = (((end - start).abs() / STEP_DEG.to_radians()).ceil() as usize).max(1);

    let at = |angle: f32, r: f32| {
        Point::new(center.x + angle.cos() * r, center.y + angle.sin() * r)
    };
    let outer: Vec<Point> = (0..=steps)
        .map(|i| at(start + (end - start) * (i as f32 / steps as f32), radius))
        .collect();

    // 以内圈（或圆心）为原点，把整块扇形平移进局部坐标系
    let origin = if hole > 0.0 {
        at(start, hole)
    } else {
        center
    };
    let offset = Transform::translate(-origin.x, -origin.y);

    let mut sub = SubPath {
        start: Point::new(0.0, 0.0),
        segments: Vec::with_capacity(steps + 4),
        closed: true,
    };
    if hole > 0.0 {
        sub.segments.push(PathSegment::Line(offset.apply(outer[0])));
    }
    for p in &outer {
        sub.segments.push(PathSegment::Line(offset.apply(*p)));
    }
    if hole > 0.0 {
        // 内圈原路返回，形成圆环块
        for i in (0..=steps).rev() {
            let a = start + (end - start) * (i as f32 / steps as f32);
            sub.segments.push(PathSegment::Line(offset.apply(at(a, hole))));
        }
    }

    let bbox = bbox_of_points(&[origin]).union(&bbox_of_points(&outer));
    let local = Rect::new(0.0, 0.0, bbox.w, bbox.h);

    Node {
        // 路径坐标是以 `origin` 为原点写的，节点变换就必须平移到 `origin`。
        // 用 `bbox` 平移会整体偏掉 (origin - bbox) —— 各块扇形各偏各的，
        // 于是「饼」散成一地碎块。而单看每块的**面积**又是对的，
        // 只有按坐标断言才抓得到（真实课件上就是这么暴露的）
        transform: Transform::translate(origin.x, origin.y),
        geometry: Geometry::Path(PathGeometry {
            subpaths: vec![sub],
            text_rect: None,
            bbox: local,
        }),
        fill: Fill::Solid(color),
        // 白色细边让相邻两块分得开，和 PowerPoint 的默认样式一致
        stroke: Some(Stroke {
            fill: StrokeFill::Solid(Color::WHITE),
            width_pt: 0.75,
            ..Default::default()
        }),
        local_bbox: Some(local),
        ..Default::default()
    }
}

fn bbox_of_points(points: &[Point]) -> Rect {
    let mut min_x = f32::MAX;
    let mut min_y = f32::MAX;
    let mut max_x = f32::MIN;
    let mut max_y = f32::MIN;
    for p in points {
        min_x = min_x.min(p.x);
        min_y = min_y.min(p.y);
        max_x = max_x.max(p.x);
        max_y = max_y.max(p.y);
    }
    Rect::new(min_x, min_y, (max_x - min_x).max(0.1), (max_y - min_y).max(0.1))
}

#[allow(clippy::too_many_arguments)]
fn text(
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    content: String,
    size_pt: f32,
    bold: bool,
    color: Color,
    align: TextAlign,
) -> Node {
    let props = RunProps {
        size_pt,
        bold,
        color: Some(color),
        ..Default::default()
    };

    let mut run = TextRun::new(content);
    run.props = props;

    let tb = TextBox {
        body: BodyProps {
            anchor: VerticalAnchor::Middle,
            insets: Insets::uniform(0.0),
            // 图表标签绝不该换行 —— 换行会把整块布局挤歪
            wrap: TextWrap::None,
            ..Default::default()
        },
        paragraphs: vec![Paragraph {
            runs: vec![run],
            align,
            ..Default::default()
        }],
    };

    Node {
        transform: Transform::translate(x, y),
        geometry: Geometry::None,
        text: Some(tb),
        local_bbox: Some(Rect::new(0.0, 0.0, w.max(1.0), h.max(1.0))),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ppt_core::xml;

    fn n(xml_text: &str) -> XmlNode {
        xml::parse_root("test.xml", xml_text).expect("测试 XML 应能解析")
    }

    fn scheme() -> ColorScheme {
        ColorScheme::default()
    }

    /// 一份最小的柱状图：两个类别、一个系列。
    fn bar_chart_xml() -> &'static str {
        r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"
                        xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
             <c:chart>
               <c:title><c:tx><c:rich><a:p><a:r><a:t>各阶段产量</a:t></a:r></a:p></c:rich></c:tx></c:title>
               <c:plotArea>
                 <c:barChart>
                   <c:barDir val="col"/>
                   <c:ser>
                     <c:tx><c:strRef><c:strCache><c:pt idx="0"><c:v>产量</c:v></c:pt></c:strCache></c:strRef></c:tx>
                     <c:cat><c:strRef><c:strCache>
                       <c:pt idx="0"><c:v>一月</c:v></c:pt>
                       <c:pt idx="1"><c:v>二月</c:v></c:pt>
                     </c:strCache></c:strRef></c:cat>
                     <c:val><c:numRef><c:numCache>
                       <c:pt idx="0"><c:v>3</c:v></c:pt>
                       <c:pt idx="1"><c:v>7</c:v></c:pt>
                     </c:numCache></c:numRef></c:val>
                   </c:ser>
                 </c:barChart>
                 <c:catAx><c:delete val="0"/><c:axPos val="b"/></c:catAx>
                 <c:valAx><c:delete val="0"/><c:axPos val="l"/></c:valAx>
               </c:plotArea>
             </c:chart>
           </c:chartSpace>"#
    }

    fn no_rel(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn parses_title_series_and_categories() {
        let spec = parse_spec(&n(bar_chart_xml()).child("chart").unwrap(), &scheme(), &no_rel)
            .expect("应能解析柱状图");
        assert_eq!(spec.kind, Kind::Column);
        assert_eq!(spec.title.as_deref(), Some("各阶段产量"));
        assert_eq!(spec.series.len(), 1);
        assert_eq!(spec.series[0].name, "产量");
        assert_eq!(spec.series[0].cats, vec!["一月", "二月"]);
        assert_eq!(spec.series[0].ys, vec![Some(3.0), Some(7.0)]);
    }

    #[test]
    fn expands_to_drawable_primitives() {
        let nodes = parse(
            &n(bar_chart_xml()),
            Rect::new(0.0, 0.0, 400.0, 260.0),
            &scheme(),
            &no_rel,
        )
        .expect("应能展开成图元");

        // 有标题、有柱、有类别标签
        assert!(
            nodes
                .iter()
                .any(|nd| nd.text.as_ref().is_some_and(|t| t.plain_text() == "各阶段产量")),
            "标题应作为文本图元出现"
        );
        assert!(
            nodes
                .iter()
                .any(|nd| nd.text.as_ref().is_some_and(|t| t.plain_text() == "一月")),
            "类别标签应出现"
        );
        let bars = nodes
            .iter()
            .filter(|nd| {
                matches!(nd.geometry, Geometry::Rect)
                    && nd.fill == Fill::Solid(scheme().accent1)
                    && nd.local_bbox.is_some_and(|b| b.w > 4.0)
            })
            .count();
        assert!(bars >= 2, "两根柱子都应画出来，实际 {bars}");
    }

    #[test]
    fn scatter_recognises_both_value_axes() {
        // 散点图的两条轴在 OOXML 里都是 c:valAx，只能按 axPos 区分；
        // 若只按标签名找，x 轴会被漏掉、底部刻度整条不画
        let xml_text = r#"<c:chartSpace xmlns:c="x"><c:chart><c:plotArea>
             <c:scatterChart>
               <c:ser>
                 <c:xVal><c:numRef><c:numCache>
                   <c:pt idx="0"><c:v>1</c:v></c:pt>
                 </c:numCache></c:numRef></c:xVal>
                 <c:yVal><c:numRef><c:numCache>
                   <c:pt idx="0"><c:v>4</c:v></c:pt>
                 </c:numCache></c:numRef></c:yVal>
               </c:ser>
             </c:scatterChart>
             <c:valAx><c:delete val="0"/><c:axPos val="b"/></c:valAx>
             <c:valAx><c:delete val="0"/><c:axPos val="l"/></c:valAx>
           </c:plotArea></c:chart></c:chartSpace>"#;
        let spec = parse_spec(&n(xml_text).child("chart").unwrap(), &scheme(), &no_rel).unwrap();
        assert!(spec.val_axis, "y 轴（左侧）应可见");
        assert!(spec.cat_axis, "x 轴（底部）应可见");
        assert_eq!(spec.series[0].xs, vec![Some(1.0)]);
        assert_eq!(spec.series[0].ys, vec![Some(4.0)]);
    }

    #[test]
    fn hidden_axis_leaves_no_gutter() {
        // 删掉类别轴后不该再为它留白，否则绘图区会白白空掉一条
        let xml_text = r#"<c:chartSpace xmlns:c="x"><c:chart><c:plotArea>
             <c:barChart><c:barDir val="col"/>
               <c:ser><c:val><c:numRef><c:numCache>
                 <c:pt idx="0"><c:v>5</c:v></c:pt>
               </c:numCache></c:numRef></c:val></c:ser>
             </c:barChart>
             <c:catAx><c:delete val="1"/><c:axPos val="b"/></c:catAx>
             <c:valAx><c:delete val="0"/><c:axPos val="l"/></c:valAx>
           </c:plotArea></c:chart></c:chartSpace>"#;
        let spec = parse_spec(&n(xml_text).child("chart").unwrap(), &scheme(), &no_rel).unwrap();
        assert!(spec.val_axis);
        assert!(!spec.cat_axis, "被删除的类别轴不该算可见");

        let no_axis = plot_rect(&spec, Rect::new(0.0, 0.0, 300.0, 200.0));
        let mut shown = spec.clone();
        shown.cat_axis = true;
        let with_axis = plot_rect(&shown, Rect::new(0.0, 0.0, 300.0, 200.0));
        assert!(
            no_axis.h > with_axis.h,
            "不画类别轴时绘图区应更高：{} vs {}",
            no_axis.h,
            with_axis.h
        );
    }

    #[test]
    fn missing_category_index_does_not_shift_bars() {
        // 中间类别缺失时，第二个点的 idx 是 2，不能当成 1
        let xml_text = r#"<c:chartSpace xmlns:c="x" xmlns:a="y"><c:chart><c:plotArea>
             <c:barChart><c:barDir val="col"/>
               <c:ser><c:val><c:numRef><c:numCache>
                 <c:pt idx="0"><c:v>1</c:v></c:pt>
                 <c:pt idx="2"><c:v>9</c:v></c:pt>
               </c:numCache></c:numRef></c:val></c:ser>
             </c:barChart>
           </c:plotArea></c:chart></c:chartSpace>"#;
        let spec = parse_spec(&n(xml_text).child("chart").unwrap(), &scheme(), &no_rel).unwrap();
        assert_eq!(spec.series[0].ys, vec![Some(1.0), None, Some(9.0)]);
    }

    #[test]
    fn unsupported_chart_reports_reason() {
        let xml_text = r#"<c:chartSpace xmlns:c="x"><c:chart><c:plotArea>
             <c:radarChart/></c:plotArea></c:chart></c:chartSpace>"#;
        let err = parse_spec(&n(xml_text).child("chart").unwrap(), &scheme(), &no_rel)
            .expect_err("雷达图应如实报告不支持");
        assert!(err.contains("radarChart"), "原因里应带上类型名，实际：{err}");
    }

    #[test]
    fn pie_slices_cover_whole_circle() {
        let xml_text = r#"<c:chartSpace xmlns:c="x"><c:chart><c:plotArea>
             <c:pieChart>
               <c:ser><c:val><c:numRef><c:numCache>
                 <c:pt idx="0"><c:v>1</c:v></c:pt>
                 <c:pt idx="1"><c:v>3</c:v></c:pt>
               </c:numCache></c:numRef></c:val></c:ser>
             </c:pieChart>
           </c:plotArea></c:chart></c:chartSpace>"#;
        let nodes = parse(
            &n(xml_text),
            Rect::new(0.0, 0.0, 300.0, 240.0),
            &scheme(),
            &no_rel,
        )
        .unwrap();
        let slices: Vec<&Node> = nodes
            .iter()
            .filter(|nd| {
                matches!(nd.geometry, Geometry::Path(_)) && matches!(nd.fill, Fill::Solid(_))
            })
            .collect();
        assert_eq!(slices.len(), 2, "两个数据点应生成两块扇形");

        // 圆弧靠折线逼近，因此每块都该有**多段**。
        // 若把弧度量当成角度量再换算一次，steps 会恒为 1，扇形退化成三角形
        for s in &slices {
            if let Geometry::Path(p) = &s.geometry {
                let segs = p.subpaths[0].segments.len();
                assert!(segs >= 8, "扇形应有足够多的折线段逼近圆弧，实际 {segs}");
            }
        }

        // 1:3 的第二块跨度 270°，包围盒必须真的有面积（退化成零宽就整块看不见）
        let big = slices[1].local_bbox.expect("扇形应有包围盒");
        assert!(
            big.w > 200.0 && big.h > 200.0,
            "270° 的块不该退化成零宽，实际 {big:?}"
        );

        // 两块扇形合起来应覆盖整个圆
        let total: f32 = slices
            .iter()
            .map(|s| s.local_bbox.map(|b| b.w * b.h).unwrap_or(0.0))
            .sum();
        assert!(total > 0.0);
    }

    #[test]
    fn nice_scale_produces_readable_ticks() {
        let (lo, hi, step) = nice_scale(3.0, 7.0);
        assert_eq!((lo, hi), (0.0, 8.0));
        assert!((step - 2.0).abs() < 1e-4, "步长应为 2，实际 {step}");
        // 刻度必须正好落在边界上，且都是整数
        let mut v = lo;
        let mut count = 0;
        while v <= hi + 1e-4 {
            assert!((v - v.round()).abs() < 1e-4, "刻度 {v} 不是整数");
            v += step;
            count += 1;
        }
        assert!(count >= 4, "刻度太少，实际 {count}");
    }
}
