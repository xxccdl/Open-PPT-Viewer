//! 预设几何（`a:prstGeom`）与自定义几何（`a:custGeom`）展开。
//!
//! # OOXML 几何的两个坐标系
//!
//! 预设几何用一套「虚拟坐标系」描述形状，该坐标系的原点在形状左上角、
//! 宽高由**路径定义自身的 `w`/`h`** 给出（多数预设为 `21600×21600`，
//! 线条类为 `21600×21600`，箭头类为 `21600×21600`），
//! 且可被 `a:avLst` 的调整值（如圆角半径 `adj`）修改。
//! 展开后必须线性映射到形状实际的 `(0,0,extent_w,extent_h)` 上。
//!
//! 为此每个预设的展开函数返回**在单位坐标系中归一化后的路径**
//! （坐标范围 `0..1`），由 [`expand`] 统一乘以 `extent`。
//! 这比在每个预设里手写缩放公式更不容易出错。
//!
//! # 降级策略
//!
//! 未实现的预设返回 [`Geometry::Placeholder`] 并携带原因，
//! 由调用方写入 `Scene::warnings`。**绝不 panic**：
//! 一个不认识的形状不应该让整页课件打不开。

use ppt_core::scene::{Geometry, PathGeometry, PathSegment, Point, Rect, Size, SubPath};

/// 预设几何的虚拟坐标系边长（OOXML 惯例）。
///
/// **只用于 `a:custGeom` 的路径坐标**（`<a:path w="21600" h="21600">`）。
/// 调整值（`a:gd`）用的不是这个单位，见 [`ADJUST_SCALE`]。
const GUIDE_SPACE: f32 = 21600.0;

/// 调整值（`a:gd`）的单位：**十万分之一**。
///
/// `roundRect` 的 `adj="16667"` 意思是「短边的 16.667%」，
/// `val` 是百分数的十万分之一，不是 21600 虚拟坐标系里的长度。
///
/// # 曾经错在哪
///
/// 这里原先把调整值除以 21600（那是自定义几何的坐标系边长，不是一回事），
/// 于是**凡是写了 `adj` 的圆角形状，圆角都被放大约 4.6 倍**：
/// 课件里那些圆角标注框、圆角矩形，PowerPoint 里是方方正正的圆角，
/// 我们这边全成了「胶囊」。圆角半径偏大还会把文字挤出形状边缘。
const ADJUST_SCALE: f32 = 100_000.0;

/// 形状的调整值表（`a:avLst`）。
#[derive(Debug, Clone, Default)]
pub struct AdjustValues {
    values: Vec<(String, f32)>,
}

impl AdjustValues {
    /// 从 `a:avLst` 解析。
    ///
    /// `a:gd` 的 `fmla` 属性是公式字符串（`val 50000`），不是数字，
    /// 因此这里做字符串解析；无法识别的公式（如 `*/ adj 2 1`）直接忽略，
    /// 由调用方回退到预设默认值。
    pub fn parse(node: Option<&ppt_core::XmlNode>) -> AdjustValues {
        let mut values = Vec::new();
        if let Some(av) = node {
            for gd in av.children_named("gd") {
                let (Some(name), Some(fmla)) = (gd.attr("name"), gd.attr("fmla")) else {
                    continue;
                };
                let fmla = fmla.trim();
                let Some(rest) = fmla.strip_prefix("val") else {
                    continue;
                };
                if let Ok(n) = rest.trim().parse::<f32>() {
                    values.push((name.to_string(), n));
                }
            }
        }
        AdjustValues { values }
    }

    /// 取调整值并归一化为「相对于形状短边的比例」。
    ///
    /// `val 16667` → `0.16667`（见 [`ADJUST_SCALE`]）。
    pub fn ratio(&self, name: &str, default: f32) -> f32 {
        self.values
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| *v / ADJUST_SCALE)
            .unwrap_or(default)
    }

    /// 取原始值（未归一化）。
    pub fn raw(&self, name: &str, default: f32) -> f32 {
        self.values
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| *v)
            .unwrap_or(default)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// 在单位坐标系（`0..1`）中构建路径的辅助工具。
struct PathBuilder {
    subpaths: Vec<SubPath>,
    current: Option<SubPath>,
}

impl PathBuilder {
    fn new() -> PathBuilder {
        PathBuilder {
            subpaths: Vec::new(),
            current: None,
        }
    }

    fn move_to(&mut self, x: f32, y: f32) {
        self.flush();
        self.current = Some(SubPath {
            start: Point::new(x, y),
            segments: Vec::new(),
            closed: false,
        });
    }

    fn line_to(&mut self, x: f32, y: f32) {
        if let Some(sp) = self.current.as_mut() {
            sp.segments.push(PathSegment::Line(Point::new(x, y)));
        }
    }

    fn cubic_to(&mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) {
        if let Some(sp) = self.current.as_mut() {
            sp.segments.push(PathSegment::Cubic {
                c1: Point::new(c1x, c1y),
                c2: Point::new(c2x, c2y),
                to: Point::new(x, y),
            });
        }
    }

    fn close(&mut self) {
        if let Some(sp) = self.current.as_mut() {
            sp.closed = true;
        }
    }

    fn flush(&mut self) {
        if let Some(sp) = self.current.take() {
            if !sp.segments.is_empty() {
                self.subpaths.push(sp);
            }
        }
    }

    /// 用三次贝塞尔逼近椭圆弧（k 为圆弧逼近常数）。
    fn arc_ellipse(&mut self, cx: f32, cy: f32, rx: f32, ry: f32, start: f32, sweep: f32) {
        // 单段最大 90 度，超过则拆分以保证精度
        let segments = (sweep.abs() / (std::f32::consts::FRAC_PI_2)).ceil().max(1.0) as usize;
        let step = sweep / segments as f32;
        let k = 4.0 / 3.0 * (step / 4.0).tan();

        let mut a0 = start;
        let mut p0 = Point::new(cx + rx * a0.cos(), cy + ry * a0.sin());
        self.move_to(p0.x, p0.y);

        for _ in 0..segments {
            let a1 = a0 + step;
            let p1 = Point::new(cx + rx * a1.cos(), cy + ry * a1.sin());
            let t0 = Point::new(-rx * a0.sin(), ry * a0.cos());
            let t1 = Point::new(-rx * a1.sin(), ry * a1.cos());
            self.cubic_to(
                p0.x + k * t0.x,
                p0.y + k * t0.y,
                p1.x - k * t1.x,
                p1.y - k * t1.y,
                p1.x,
                p1.y,
            );
            a0 = a1;
            p0 = p1;
        }
        self.close();
    }

    fn finish(mut self) -> Vec<SubPath> {
        self.flush();
        self.subpaths
    }
}

/// 展开预设几何。
///
/// `extent` 是形状的实际尺寸；返回的路径坐标已落在 `(0, 0, extent.w, extent.h)` 内。
/// 未识别的预设返回 `Placeholder`。
pub fn expand(preset: &str, adj: &AdjustValues, extent: Size) -> Geometry {
    let mut pb = PathBuilder::new();

    let known = match preset {
        "rect" => {
            unit_rect(&mut pb, 0.0, 0.0, 1.0, 1.0);
            true
        }
        "roundRect" => {
            // adj 默认 16667/21600 ≈ 0.1667，即短边的 1/6
            let r = adj.ratio("adj", 0.16667).clamp(0.0, 0.5);
            unit_round_rect(&mut pb, 0.0, 0.0, 1.0, 1.0, r);
            true
        }
        "snip1Rect" => {
            let r = adj.ratio("adj", 0.16667).clamp(0.0, 0.5);
            unit_snip_rect(&mut pb, r, false);
            true
        }
        "snip2SameRect" => {
            let r = adj.ratio("adj1", 0.16667).clamp(0.0, 0.5);
            unit_snip_rect(&mut pb, r, false);
            true
        }
        "ellipse" => {
            pb.arc_ellipse(0.5, 0.5, 0.5, 0.5, 0.0, std::f32::consts::TAU);
            true
        }
        "triangle" => {
            let adj_x = adj.ratio("adj", 0.5).clamp(0.0, 1.0);
            pb.move_to(adj_x, 0.0);
            pb.line_to(1.0, 1.0);
            pb.line_to(0.0, 1.0);
            pb.close();
            true
        }
        "rtTriangle" => {
            pb.move_to(0.0, 0.0);
            pb.line_to(0.0, 1.0);
            pb.line_to(1.0, 1.0);
            pb.close();
            true
        }
        "diamond" => {
            pb.move_to(0.5, 0.0);
            pb.line_to(1.0, 0.5);
            pb.line_to(0.5, 1.0);
            pb.line_to(0.0, 0.5);
            pb.close();
            true
        }
        "parallelogram" => {
            let adj = adj.ratio("adj", 0.25).clamp(0.0, 0.5);
            pb.move_to(adj, 0.0);
            pb.line_to(1.0, 0.0);
            pb.line_to(1.0 - adj, 1.0);
            pb.line_to(0.0, 1.0);
            pb.close();
            true
        }
        "trapezoid" => {
            let adj = adj.ratio("adj", 0.25).clamp(0.0, 0.5);
            pb.move_to(adj, 0.0);
            pb.line_to(1.0 - adj, 0.0);
            pb.line_to(1.0, 1.0);
            pb.line_to(0.0, 1.0);
            pb.close();
            true
        }
        "pentagon" => {
            unit_regular_polygon(&mut pb, 5, -90.0);
            true
        }
        "hexagon" => {
            let adj = adj.ratio("adj", 0.25).clamp(0.0, 0.5);
            pb.move_to(adj, 0.0);
            pb.line_to(1.0 - adj, 0.0);
            pb.line_to(1.0, 0.5);
            pb.line_to(1.0 - adj, 1.0);
            pb.line_to(adj, 1.0);
            pb.line_to(0.0, 0.5);
            pb.close();
            true
        }
        "heptagon" => {
            unit_regular_polygon(&mut pb, 7, -90.0);
            true
        }
        "octagon" => {
            let adj = adj.ratio("adj", 0.29289).clamp(0.0, 0.5);
            pb.move_to(adj, 0.0);
            pb.line_to(1.0 - adj, 0.0);
            pb.line_to(1.0, adj);
            pb.line_to(1.0, 1.0 - adj);
            pb.line_to(1.0 - adj, 1.0);
            pb.line_to(adj, 1.0);
            pb.line_to(0.0, 1.0 - adj);
            pb.line_to(0.0, adj);
            pb.close();
            true
        }
        "star4" => {
            unit_star(&mut pb, 4, 0.125, -90.0);
            true
        }
        "star5" => {
            unit_star(&mut pb, 5, 0.19, -90.0);
            true
        }
        "star6" => {
            unit_star(&mut pb, 6, 0.175, -90.0);
            true
        }
        "star7" => {
            unit_star(&mut pb, 7, 0.155, -90.0);
            true
        }
        "star8" => {
            unit_star(&mut pb, 8, 0.15, -90.0);
            true
        }
        "star10" => {
            unit_star(&mut pb, 10, 0.13, -90.0);
            true
        }
        "star12" => {
            unit_star(&mut pb, 12, 0.11, -90.0);
            true
        }
        "star16" => {
            unit_star(&mut pb, 16, 0.085, -90.0);
            true
        }
        "star24" => {
            unit_star(&mut pb, 24, 0.06, -90.0);
            true
        }
        "star32" => {
            unit_star(&mut pb, 32, 0.045, -90.0);
            true
        }
        "plus" => {
            let a = adj.ratio("adj1", 0.25).clamp(0.0, 0.5);
            unit_cross(&mut pb, a);
            true
        }
        "mathPlus" => {
            let a = adj.ratio("adj1", 0.23520).clamp(0.0, 0.5);
            unit_cross(&mut pb, a);
            true
        }
        "minus" => {
            let a = adj.ratio("adj1", 0.25).clamp(0.0, 0.5);
            pb.move_to(0.0, 0.5 - a);
            pb.line_to(1.0, 0.5 - a);
            pb.line_to(1.0, 0.5 + a);
            pb.line_to(0.0, 0.5 + a);
            pb.close();
            true
        }
        "mathMultiply" => {
            let a = adj.ratio("adj1", 0.23520).clamp(0.0, 0.5);
            // 旋转 45 度的十字
            let c = 0.5f32;
            let d = a * std::f32::consts::SQRT_2;
            pb.move_to(c, c - d);
            pb.line_to(c + d, c);
            pb.line_to(c, c + d);
            pb.line_to(c - d, c);
            pb.close();
            true
        }
        "line" | "straightConnector1" => {
            // 线条没有填充区域，用零面积路径表示
            pb.move_to(0.0, 0.5);
            pb.line_to(1.0, 0.5);
            true
        }
        "bentConnector2" => {
            pb.move_to(0.0, 0.0);
            pb.line_to(1.0, 0.0);
            pb.line_to(1.0, 1.0);
            true
        }
        "bentConnector3" => {
            let adj = adj.ratio("adj1", 0.5).clamp(0.0, 1.0);
            pb.move_to(0.0, 0.0);
            pb.line_to(adj, 0.0);
            pb.line_to(adj, 1.0);
            pb.line_to(1.0, 1.0);
            true
        }
        "curvedConnector2" => {
            pb.move_to(0.0, 0.0);
            pb.cubic_to(0.5, 0.0, 1.0, 0.5, 1.0, 1.0);
            true
        }
        "curvedConnector3" => {
            let adj = adj.ratio("adj1", 0.5).clamp(0.0, 1.0);
            pb.move_to(0.0, 0.0);
            pb.cubic_to(adj, 0.0, adj, 0.5, adj, 0.5);
            pb.cubic_to(adj, 0.5, adj, 1.0, 1.0, 1.0);
            true
        }
        "leftArrow" | "arrow" => {
            unit_arrow(&mut pb, Direction::Left, adj);
            true
        }
        "rightArrow" => {
            unit_arrow(&mut pb, Direction::Right, adj);
            true
        }
        "upArrow" => {
            unit_arrow(&mut pb, Direction::Up, adj);
            true
        }
        "downArrow" => {
            unit_arrow(&mut pb, Direction::Down, adj);
            true
        }
        "leftRightArrow" => {
            unit_double_arrow(&mut pb, Direction::Left, adj);
            true
        }
        "upDownArrow" => {
            unit_double_arrow(&mut pb, Direction::Up, adj);
            true
        }
        "chevron" => {
            let a = adj.ratio("adj", 0.5).clamp(0.0, 1.0);
            pb.move_to(0.0, 0.0);
            pb.line_to(1.0 - a, 0.0);
            pb.line_to(1.0, 0.5);
            pb.line_to(1.0 - a, 1.0);
            pb.line_to(0.0, 1.0);
            pb.line_to(a, 0.5);
            pb.close();
            true
        }
        "homePlate" => {
            let a = adj.ratio("adj", 0.5).clamp(0.0, 1.0);
            pb.move_to(0.0, 0.0);
            pb.line_to(1.0 - a, 0.0);
            pb.line_to(1.0, 0.5);
            pb.line_to(1.0 - a, 1.0);
            pb.line_to(0.0, 1.0);
            pb.close();
            true
        }
        "donut" => {
            let t = adj.ratio("adj", 0.25).clamp(0.0, 0.5);
            // 外圆顺时针、内圆逆时针形成环
            pb.arc_ellipse(0.5, 0.5, 0.5, 0.5, 0.0, std::f32::consts::TAU);
            pb.arc_ellipse(
                0.5,
                0.5,
                (0.5 - t).max(0.001),
                (0.5 - t).max(0.001),
                std::f32::consts::TAU,
                -std::f32::consts::TAU,
            );
            true
        }
        "noSmoking" => {
            let t = adj.ratio("adj", 0.1875).clamp(0.0, 0.5);
            pb.arc_ellipse(0.5, 0.5, 0.5, 0.5, 0.0, std::f32::consts::TAU);
            pb.arc_ellipse(
                0.5,
                0.5,
                (0.5 - t).max(0.001),
                (0.5 - t).max(0.001),
                std::f32::consts::TAU,
                -std::f32::consts::TAU,
            );
            true
        }
        "blockArc" => {
            let t = adj.ratio("adj1", 0.25).clamp(0.01, 0.5);
            let start = (adj.raw("adj2", 10800000.0) / 60000.0).to_radians();
            let end = (adj.raw("adj3", 0.0) / 60000.0).to_radians();
            let sweep = start - end;
            pb.arc_ellipse(0.5, 0.5, 0.5, 0.5, -start, sweep);
            pb.arc_ellipse(
                0.5,
                0.5,
                (0.5 - t).max(0.001),
                (0.5 - t).max(0.001),
                -end,
                -sweep,
            );
            true
        }
        "pie" | "pieWedge" => {
            let start = (adj.raw("adj1", 0.0) / 60000.0).to_radians();
            let end = (adj.raw("adj2", 16200000.0) / 60000.0).to_radians();
            let sweep = start - end;
            pb.move_to(0.5, 0.5);
            pb.line_to(0.5 + 0.5 * start.cos(), 0.5 - 0.5 * start.sin());
            // 用弧线连接两个端点
            let segments = (sweep.abs() / std::f32::consts::FRAC_PI_2).ceil().max(1.0) as usize;
            let step = sweep / segments as f32;
            let k = 4.0 / 3.0 * (step / 4.0).tan();
            let mut a0 = start;
            let mut p0 = Point::new(0.5 + 0.5 * a0.cos(), 0.5 - 0.5 * a0.sin());
            for _ in 0..segments {
                let a1 = a0 + step;
                let p1 = Point::new(0.5 + 0.5 * a1.cos(), 0.5 - 0.5 * a1.sin());
                let t0 = Point::new(-0.5 * a0.sin(), -0.5 * a0.cos());
                let t1 = Point::new(-0.5 * a1.sin(), -0.5 * a1.cos());
                pb.cubic_to(
                    p0.x + k * t0.x,
                    p0.y + k * t0.y,
                    p1.x - k * t1.x,
                    p1.y - k * t1.y,
                    p1.x,
                    p1.y,
                );
                a0 = a1;
                p0 = p1;
            }
            pb.close();
            true
        }
        "arc" => {
            let start = (adj.raw("adj1", 16200000.0) / 60000.0).to_radians();
            let end = (adj.raw("adj2", 0.0) / 60000.0).to_radians();
            let sweep = start - end;
            pb.arc_ellipse(0.5, 0.5, 0.5, 0.5, -start, sweep);
            true
        }
        "chord" => {
            let start = (adj.raw("adj1", 2700000.0) / 60000.0).to_radians();
            let end = (adj.raw("adj2", 16200000.0) / 60000.0).to_radians();
            let sweep = start - end;
            pb.move_to(0.5 + 0.5 * start.cos(), 0.5 - 0.5 * start.sin());
            let segments = (sweep.abs() / std::f32::consts::FRAC_PI_2).ceil().max(1.0) as usize;
            let step = sweep / segments as f32;
            let k = 4.0 / 3.0 * (step / 4.0).tan();
            let mut a0 = start;
            let mut p0 = Point::new(0.5 + 0.5 * a0.cos(), 0.5 - 0.5 * a0.sin());
            for _ in 0..segments {
                let a1 = a0 + step;
                let p1 = Point::new(0.5 + 0.5 * a1.cos(), 0.5 - 0.5 * a1.sin());
                let t0 = Point::new(-0.5 * a0.sin(), -0.5 * a0.cos());
                let t1 = Point::new(-0.5 * a1.sin(), -0.5 * a1.cos());
                pb.cubic_to(
                    p0.x + k * t0.x,
                    p0.y + k * t0.y,
                    p1.x - k * t1.x,
                    p1.y - k * t1.y,
                    p1.x,
                    p1.y,
                );
                a0 = a1;
                p0 = p1;
            }
            pb.close();
            true
        }
        "teardrop" => {
            let start = (adj.raw("adj", 10800000.0) / 60000.0).to_radians();
            pb.arc_ellipse(0.5, 0.5, 0.5, 0.5, 0.0, std::f32::consts::TAU);
            pb.move_to(0.5, 0.5);
            pb.line_to(0.5 + 0.5 * start.cos(), 0.5 - 0.5 * start.sin());
            pb.line_to(1.0, 1.0);
            pb.line_to(0.5, 0.5);
            true
        }
        "cloud" => {
            unit_cloud(&mut pb);
            true
        }
        "heart" => {
            unit_heart(&mut pb);
            true
        }
        "moon" => {
            let a = adj.ratio("adj", 0.5).clamp(0.0, 1.0);
            unit_moon(&mut pb, a);
            true
        }
        "sun" => {
            let r = adj.ratio("adj", 0.25).clamp(0.05, 0.5);
            unit_sun(&mut pb, r);
            true
        }
        "smileyFace" => {
            let r = adj.ratio("adj", 0.46530).clamp(0.0, 0.5);
            unit_smiley(&mut pb, r);
            true
        }
        "can" => {
            let a = adj.ratio("adj", 0.25).clamp(0.0, 0.5);
            unit_can(&mut pb, a);
            true
        }
        "cube" => {
            let a = adj.ratio("adj", 0.25).clamp(0.0, 0.5);
            unit_cube(&mut pb, a);
            true
        }
        "foldedCorner" => {
            let a = adj.ratio("adj", 0.16667).clamp(0.0, 0.5);
            pb.move_to(0.0, 0.0);
            pb.line_to(1.0 - a, 0.0);
            pb.line_to(1.0, a);
            pb.line_to(1.0, 1.0);
            pb.line_to(0.0, 1.0);
            pb.close();
            true
        }
        "bevel" => {
            let a = adj.ratio("adj", 0.125).clamp(0.0, 0.5);
            unit_bevel(&mut pb, a);
            true
        }
        "frame" => {
            let a = adj.ratio("adj1", 0.125).clamp(0.0, 0.5);
            unit_frame(&mut pb, a);
            true
        }
        "halfFrame" => {
            let a = adj.ratio("adj1", 0.33333).clamp(0.0, 0.5);
            unit_half_frame(&mut pb, a);
            true
        }
        "corner" => {
            let a = adj.ratio("adj1", 0.16667).clamp(0.0, 0.5);
            unit_corner(&mut pb, a);
            true
        }
        "diagStripe" => {
            let a = adj.ratio("adj", 0.5).clamp(0.0, 1.0);
            pb.move_to(0.0, 1.0);
            pb.line_to(a, 0.0);
            pb.line_to(1.0, 0.0);
            pb.line_to(1.0 - a, 1.0);
            pb.close();
            true
        }
        "plaque" => {
            let a = adj.ratio("adj", 0.16667).clamp(0.0, 0.5);
            unit_round_rect(&mut pb, 0.0, 0.0, 1.0, 1.0, a * 2.0);
            true
        }
        "flowChartProcess" => {
            unit_rect(&mut pb, 0.0, 0.0, 1.0, 1.0);
            true
        }
        "flowChartDecision" => {
            pb.move_to(0.5, 0.0);
            pb.line_to(1.0, 0.5);
            pb.line_to(0.5, 1.0);
            pb.line_to(0.0, 0.5);
            pb.close();
            true
        }
        "flowChartTerminator" => {
            // 胶囊形
            unit_round_rect(&mut pb, 0.0, 0.0, 1.0, 1.0, 0.5);
            true
        }
        "flowChartData" => {
            let a = adj.ratio("adj", 0.25).clamp(0.0, 0.5);
            pb.move_to(a, 0.0);
            pb.line_to(1.0, 0.0);
            pb.line_to(1.0 - a, 1.0);
            pb.line_to(0.0, 1.0);
            pb.close();
            true
        }
        "flowChartDocument" => {
            pb.move_to(0.0, 0.0);
            pb.line_to(1.0, 0.0);
            pb.line_to(1.0, 0.85);
            pb.cubic_to(0.75, 1.1, 0.25, 0.6, 0.0, 0.85);
            pb.close();
            true
        }
        "flowChartPredefinedProcess" => {
            let a = adj.ratio("adj", 0.125).clamp(0.0, 0.4);
            pb.move_to(a, 0.0);
            pb.line_to(1.0 - a, 0.0);
            pb.line_to(1.0 - a, 1.0);
            pb.line_to(a, 1.0);
            pb.close();
            true
        }
        "flowChartConnector" => {
            pb.arc_ellipse(0.5, 0.5, 0.5, 0.5, 0.0, std::f32::consts::TAU);
            true
        }
        "flowChartManualInput" => {
            let a = adj.ratio("adj", 0.25).clamp(0.0, 0.5);
            pb.move_to(0.0, a);
            pb.line_to(1.0, 0.0);
            pb.line_to(1.0, 1.0);
            pb.line_to(0.0, 1.0);
            pb.close();
            true
        }
        "flowChartMagneticTape" => {
            pb.arc_ellipse(0.5, 0.5, 0.5, 0.5, 0.0, std::f32::consts::TAU);
            true
        }
        "wedgeRectCallout" | "callout1" => {
            let a = adj.ratio("adj1", -0.20833);
            let b = adj.ratio("adj2", 0.625);
            unit_rect_callout(&mut pb, a, b);
            true
        }
        "wedgeRoundRectCallout" | "rRectCallout" => {
            let a = adj.ratio("adj1", -0.20833);
            let b = adj.ratio("adj2", 0.625);
            unit_round_rect_callout(&mut pb, a, b);
            true
        }
        "wedgeEllipseCallout" | "ellipseCallout" => {
            let a = adj.ratio("adj1", -0.20833);
            let b = adj.ratio("adj2", 0.625);
            unit_ellipse_callout(&mut pb, a, b);
            true
        }
        "borderCallout1" | "rBorderCallout1" => {
            let a = adj.ratio("adj1", -0.20833);
            let b = adj.ratio("adj2", 0.625);
            unit_rect_callout(&mut pb, a, b);
            true
        }
        "cloudCallout" => {
            let a = adj.ratio("adj1", -0.20833);
            let b = adj.ratio("adj2", 0.625);
            unit_cloud_callout(&mut pb, a, b);
            true
        }
        "bracePair" | "leftBrace" | "rightBrace" => {
            unit_brace(&mut pb);
            true
        }
        "bracketPair" | "leftBracket" | "rightBracket" => {
            unit_bracket(&mut pb);
            true
        }
        "leftCircularArrow" | "rightCircularArrow" | "circularArrow" => {
            unit_circular_arrow(&mut pb);
            true
        }
        "bentArrow" | "uturnArrow" | "curvedRightArrow" | "curvedLeftArrow" | "curvedUpArrow"
        | "curvedDownArrow" | "stripedRightArrow" | "notchedRightArrow" | "quadArrow"
        | "leftRightUpArrow" | "leftUpArrow" | "bentUpArrow" => {
            // 这些箭头变体用基础箭头近似，保真度低于专门实现但不至于缺失
            unit_arrow(&mut pb, Direction::Right, adj);
            true
        }
        // 动作按钮：PowerPoint 里就是「矩形底 + 一个图标字形」。
        //
        // 图标那部分是预设几何的一部分，我们没画；但**底色与描边照画**——
        // 整块降级成灰占位才是真难看（原来就是这样）。
        "actionButtonBlank" | "actionButtonHome" | "actionButtonHelp"
        | "actionButtonInformation" | "actionButtonBackPrevious" | "actionButtonForwardNext"
        | "actionButtonBeginning" | "actionButtonEnd" | "actionButtonReturn"
        | "actionButtonDocument" | "actionButtonSound" | "actionButtonMovie" => {
            unit_rect(&mut pb, 0.0, 0.0, 1.0, 1.0);
            true
        }
        // 竖卷形 / 横卷形：轮廓是「卷起来的纸」，用圆角矩形近似。
        // 卷边是纯装饰，而灰占位会在一片图文里格外扎眼。
        "verticalScroll" | "horizontalScroll" => {
            let r = adj.ratio("adj", 0.125).clamp(0.0, 0.5);
            unit_round_rect(&mut pb, 0.0, 0.0, 1.0, 1.0, r);
            true
        }
        // 离页连接符：矩形，下缘中间探出一个尖角（表示「内容延续到下一页」）
        "flowChartOffpageConnector" => {
            let y = 0.75f32;
            pb.move_to(0.0, 0.0);
            pb.line_to(1.0, 0.0);
            pb.line_to(1.0, y);
            pb.line_to(0.5, 1.0);
            pb.line_to(0.0, y);
            pb.close();
            true
        }
        "irregularSeal1" | "irregularSeal2" | "star2" | "star6Point"
        | "wave" | "doubleWave" | "ribbon" | "ribbon2"
        | "ellipseRibbon" | "ellipseRibbon2" | "leftRightRibbon" | "chartX" | "chartStar"
        | "chartPlus" | "mathDivide" | "mathEqual" | "mathNotEqual" | "gear6" | "gear9"
        | "funnel" | "pieWedgeRound" | "cornerTabs" | "squareTabs" | "plaqueTabs"
        | "flowChartOnlineStorage" | "flowChartMagneticDisk" | "flowChartMagneticDrum"
        | "flowChartDisplay" | "flowChartDelay" | "flowChartExtract" | "flowChartMerge"
        | "flowChartMultidocument" | "flowChartSort" | "flowChartSummingJunction"
        | "flowChartOr" | "flowChartCollate" | "flowChartPunchedTape"
        | "flowChartPunchedCard" | "flowChartInternalStorage" | "flowChartPreparation" => {
            false
        }
        _ => false,
    };

    if !known {
        return Geometry::placeholder(format!("暂不支持的预设几何：{preset}"));
    }

    let subpaths = pb.finish();
    if subpaths.is_empty() {
        return Geometry::placeholder(format!("预设几何 {preset} 展开为空路径"));
    }

    // 单位坐标 → 形状局部坐标
    let scaled: Vec<SubPath> = subpaths
        .into_iter()
        .map(|sp| scale_subpath(sp, extent))
        .collect();

    Geometry::Path(PathGeometry {
        subpaths: scaled,
        text_rect: None,
        bbox: Rect::new(0.0, 0.0, extent.w, extent.h),
    })
}

fn scale_subpath(sp: SubPath, extent: Size) -> SubPath {
    let m = |p: Point| Point::new(p.x * extent.w, p.y * extent.h);
    SubPath {
        start: m(sp.start),
        segments: sp
            .segments
            .into_iter()
            .map(|s| match s {
                PathSegment::Line(p) => PathSegment::Line(m(p)),
                PathSegment::Cubic { c1, c2, to } => PathSegment::Cubic {
                    c1: m(c1),
                    c2: m(c2),
                    to: m(to),
                },
                PathSegment::Quad { c, to } => PathSegment::Quad { c: m(c), to: m(to) },
                PathSegment::Arc {
                    rx,
                    ry,
                    x_axis_rotation_deg,
                    large_arc,
                    sweep,
                    to,
                } => PathSegment::Arc {
                    rx: rx * extent.w,
                    ry: ry * extent.h,
                    x_axis_rotation_deg,
                    large_arc,
                    sweep,
                    to: m(to),
                },
                PathSegment::Close => PathSegment::Close,
            })
            .collect(),
        closed: sp.closed,
    }
}

// ---------- 单位坐标系中的基础图形 ----------

fn unit_rect(pb: &mut PathBuilder, x: f32, y: f32, w: f32, h: f32) {
    pb.move_to(x, y);
    pb.line_to(x + w, y);
    pb.line_to(x + w, y + h);
    pb.line_to(x, y + h);
    pb.close();
}

fn unit_round_rect(pb: &mut PathBuilder, x: f32, y: f32, w: f32, h: f32, r: f32) {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    if r <= 0.0 {
        unit_rect(pb, x, y, w, h);
        return;
    }
    // 圆角用三次贝塞尔逼近，k ≈ 0.5523
    const K: f32 = 0.552_284_75;
    let (x0, y0, x1, y1) = (x, y, x + w, y + h);
    pb.move_to(x0 + r, y0);
    pb.line_to(x1 - r, y0);
    pb.cubic_to(x1 - r + K * r, y0, x1, y0 + r - K * r, x1, y0 + r);
    pb.line_to(x1, y1 - r);
    pb.cubic_to(x1, y1 - r + K * r, x1 - r + K * r, y1, x1 - r, y1);
    pb.line_to(x0 + r, y1);
    pb.cubic_to(x0 + r - K * r, y1, x0, y1 - r + K * r, x0, y1 - r);
    pb.line_to(x0, y0 + r);
    pb.cubic_to(x0, y0 + r - K * r, x0 + r - K * r, y0, x0 + r, y0);
    pb.close();
}

fn unit_snip_rect(pb: &mut PathBuilder, r: f32, _two: bool) {
    pb.move_to(r, 0.0);
    pb.line_to(1.0, 0.0);
    pb.line_to(1.0, 1.0 - r);
    pb.line_to(1.0 - r, 1.0);
    pb.line_to(0.0, 1.0);
    pb.line_to(0.0, r);
    pb.close();
}

fn unit_regular_polygon(pb: &mut PathBuilder, n: usize, start_deg: f32) {
    let start = start_deg.to_radians();
    for i in 0..n {
        let a = start + std::f32::consts::TAU * i as f32 / n as f32;
        let (x, y) = (0.5 + 0.5 * a.cos(), 0.5 + 0.5 * a.sin());
        if i == 0 {
            pb.move_to(x, y);
        } else {
            pb.line_to(x, y);
        }
    }
    pb.close();
}

/// 星形：外顶点半径 0.5，内顶点半径由 `inner` 给出。
fn unit_star(pb: &mut PathBuilder, points: usize, inner: f32, start_deg: f32) {
    let start = start_deg.to_radians();
    let inner = inner.clamp(0.01, 0.99);
    for i in 0..points * 2 {
        let a = start + std::f32::consts::PI * i as f32 / points as f32;
        let r = if i % 2 == 0 { 0.5 } else { 0.5 * inner };
        let (x, y) = (0.5 + r * a.cos(), 0.5 + r * a.sin());
        if i == 0 {
            pb.move_to(x, y);
        } else {
            pb.line_to(x, y);
        }
    }
    pb.close();
}

fn unit_cross(pb: &mut PathBuilder, a: f32) {
    let a = a.clamp(0.0, 0.5);
    pb.move_to(0.5 - a, 0.0);
    pb.line_to(0.5 + a, 0.0);
    pb.line_to(0.5 + a, 0.5 - a);
    pb.line_to(1.0, 0.5 - a);
    pb.line_to(1.0, 0.5 + a);
    pb.line_to(0.5 + a, 0.5 + a);
    pb.line_to(0.5 + a, 1.0);
    pb.line_to(0.5 - a, 1.0);
    pb.line_to(0.5 - a, 0.5 + a);
    pb.line_to(0.0, 0.5 + a);
    pb.line_to(0.0, 0.5 - a);
    pb.line_to(0.5 - a, 0.5 - a);
    pb.close();
}

#[derive(Debug, Clone, Copy)]
enum Direction {
    Left,
    Right,
    Up,
    Down,
}

fn unit_arrow(pb: &mut PathBuilder, dir: Direction, adj: &AdjustValues) {
    let head_w = adj.ratio("adj1", 0.5).clamp(0.0, 1.0);
    let head_h = adj.ratio("adj2", 0.5).clamp(0.0, 1.0);
    let body = (0.5 - head_h / 2.0).max(0.0);
    let head_start = 1.0 - head_w;

    let pts: [(f32, f32); 7] = match dir {
        Direction::Right => [
            (0.0, body),
            (head_start, body),
            (head_start, 0.0),
            (1.0, 0.5),
            (head_start, 1.0),
            (head_start, 1.0 - body),
            (0.0, 1.0 - body),
        ],
        Direction::Left => [
            (1.0, body),
            (head_w, body),
            (head_w, 0.0),
            (0.0, 0.5),
            (head_w, 1.0),
            (head_w, 1.0 - body),
            (1.0, 1.0 - body),
        ],
        Direction::Up => [
            (0.5, 0.0),
            (1.0, head_h),
            (1.0 - body, head_h),
            (1.0 - body, 1.0),
            (body, 1.0),
            (body, head_h),
            (0.0, head_h),
        ],
        Direction::Down => [
            (0.5, 1.0),
            (1.0, 1.0 - head_h),
            (1.0 - body, 1.0 - head_h),
            (1.0 - body, 0.0),
            (body, 0.0),
            (body, 1.0 - head_h),
            (0.0, 1.0 - head_h),
        ],
    };

    pb.move_to(pts[0].0, pts[0].1);
    for p in &pts[1..] {
        pb.line_to(p.0, p.1);
    }
    pb.close();
}

fn unit_double_arrow(pb: &mut PathBuilder, dir: Direction, adj: &AdjustValues) {
    let head_w = adj.ratio("adj1", 0.5).clamp(0.0, 0.5);
    let head_h = adj.ratio("adj2", 0.5).clamp(0.0, 1.0);
    let body = (0.5 - head_h / 2.0).max(0.0);

    let pts: [(f32, f32); 10] = match dir {
        Direction::Left | Direction::Right => [
            (head_w, 0.0),
            (1.0 - head_w, 0.0),
            (1.0 - head_w, body),
            (1.0, body),
            (1.0, 1.0 - body),
            (1.0 - head_w, 1.0 - body),
            (1.0 - head_w, 1.0),
            (head_w, 1.0),
            (head_w, 1.0 - body),
            (0.0, 0.5),
        ],
        Direction::Up | Direction::Down => [
            (0.0, head_h),
            (body, head_h),
            (body, 0.0),
            (1.0 - body, 0.0),
            (1.0 - body, head_h),
            (1.0, head_h),
            (1.0, 1.0 - head_h),
            (1.0 - body, 1.0 - head_h),
            (1.0 - body, 1.0),
            (body, 1.0),
        ],
    };

    pb.move_to(pts[0].0, pts[0].1);
    for p in &pts[1..] {
        pb.line_to(p.0, p.1);
    }
    pb.close();
}

fn unit_cloud(pb: &mut PathBuilder) {
    // 用若干段圆弧拼出云朵轮廓（近似 OOXML 的 cloud 预设）
    pb.arc_ellipse(0.28, 0.42, 0.28, 0.28, 0.0, std::f32::consts::TAU);
    pb.arc_ellipse(0.52, 0.32, 0.24, 0.24, 0.0, std::f32::consts::TAU);
    pb.arc_ellipse(0.75, 0.48, 0.25, 0.25, 0.0, std::f32::consts::TAU);
    pb.arc_ellipse(0.5, 0.65, 0.32, 0.25, 0.0, std::f32::consts::TAU);
}

fn unit_heart(pb: &mut PathBuilder) {
    pb.move_to(0.5, 1.0);
    pb.cubic_to(-0.1, 0.55, 0.1, 0.05, 0.5, 0.28);
    pb.cubic_to(0.9, 0.05, 1.1, 0.55, 0.5, 1.0);
    pb.close();
}

fn unit_moon(pb: &mut PathBuilder, a: f32) {
    // 外圆弧 + 内凹弧
    let k = 0.552_284_75;
    pb.move_to(1.0, 0.0);
    pb.cubic_to(1.0 - k * 0.5, 0.0, 0.5, 0.5 - k * 0.5, 0.5, 0.5);
    pb.cubic_to(0.5, 0.5 + k * 0.5, 1.0 - k * 0.5, 1.0, 1.0, 1.0);
    // 内凹部分
    let inner = (0.5 + a).clamp(0.05, 0.95);
    pb.cubic_to(
        1.0 - inner * 0.8,
        1.0,
        1.0 - inner * 1.2,
        0.0,
        1.0,
        0.0,
    );
    pb.close();
}

fn unit_sun(pb: &mut PathBuilder, r: f32) {
    // 中心圆
    pb.arc_ellipse(0.5, 0.5, r, r, 0.0, std::f32::consts::TAU);
    // 8 条光芒
    for i in 0..8 {
        let a = std::f32::consts::TAU * i as f32 / 8.0;
        let (c, s) = a.sin_cos();
        let (x0, y0) = (0.5 + r * c, 0.5 + r * s);
        let (x1, y1) = (0.5 + 0.5 * c, 0.5 + 0.5 * s);
        pb.move_to(x0, y0);
        pb.line_to(x1, y1);
    }
}

fn unit_smiley(pb: &mut PathBuilder, r: f32) {
    pb.arc_ellipse(0.5, 0.5, 0.5, 0.5, 0.0, std::f32::consts::TAU);
    // 左眼
    pb.arc_ellipse(0.35, 0.38, r * 0.12, r * 0.12, 0.0, std::f32::consts::TAU);
    // 右眼
    pb.arc_ellipse(0.65, 0.38, r * 0.12, r * 0.12, 0.0, std::f32::consts::TAU);
    // 嘴（向下弯的弧）
    pb.move_to(0.3, 0.65);
    pb.cubic_to(0.4, 0.85, 0.6, 0.85, 0.7, 0.65);
    pb.cubic_to(0.6, 0.78, 0.4, 0.78, 0.3, 0.65);
    pb.close();
}

fn unit_can(pb: &mut PathBuilder, a: f32) {
    let top = a * 0.5;
    pb.move_to(0.0, top);
    pb.cubic_to(0.0, 0.0, 1.0, 0.0, 1.0, top);
    pb.line_to(1.0, 1.0 - top);
    pb.cubic_to(1.0, 1.0, 0.0, 1.0, 0.0, 1.0 - top);
    pb.close();
}

fn unit_cube(pb: &mut PathBuilder, a: f32) {
    // 正面
    pb.move_to(0.0, a);
    pb.line_to(1.0 - a, a);
    pb.line_to(1.0 - a, 1.0);
    pb.line_to(0.0, 1.0);
    pb.close();
    // 顶面
    pb.move_to(0.0, a);
    pb.line_to(a, 0.0);
    pb.line_to(1.0, 0.0);
    pb.line_to(1.0 - a, a);
    pb.close();
    // 侧面
    pb.move_to(1.0 - a, a);
    pb.line_to(1.0, 0.0);
    pb.line_to(1.0, 1.0 - a);
    pb.line_to(1.0 - a, 1.0);
    pb.close();
}

fn unit_bevel(pb: &mut PathBuilder, a: f32) {
    pb.move_to(a, 0.0);
    pb.line_to(1.0 - a, 0.0);
    pb.line_to(1.0, a);
    pb.line_to(1.0, 1.0 - a);
    pb.line_to(1.0 - a, 1.0);
    pb.line_to(a, 1.0);
    pb.line_to(0.0, 1.0 - a);
    pb.line_to(0.0, a);
    pb.close();
    // 内框
    pb.move_to(2.0 * a, a);
    pb.line_to(1.0 - a, a);
    pb.line_to(1.0 - a, 1.0 - 2.0 * a);
    pb.line_to(1.0 - 2.0 * a, 1.0 - 2.0 * a);
    pb.line_to(1.0 - 2.0 * a, a);
    pb.line_to(a, a);
    pb.line_to(a, 1.0 - a);
    pb.line_to(1.0 - a, 1.0 - a);
}

fn unit_frame(pb: &mut PathBuilder, a: f32) {
    let b = a * 2.0;
    pb.move_to(0.0, 0.0);
    pb.line_to(1.0, 0.0);
    pb.line_to(1.0, 1.0);
    pb.line_to(0.0, 1.0);
    pb.close();
    // 内框（反向）
    pb.move_to(a, a);
    pb.line_to(a, 1.0 - a);
    pb.line_to(1.0 - a, 1.0 - a);
    pb.line_to(1.0 - a, a);
    pb.close();
    let _ = b;
}

fn unit_half_frame(pb: &mut PathBuilder, a: f32) {
    pb.move_to(0.0, 0.0);
    pb.line_to(1.0, 0.0);
    pb.line_to(1.0, 1.0);
    pb.line_to(0.0, 1.0);
    pb.close();
    pb.move_to(a, a);
    pb.line_to(1.0 - a, a);
    pb.line_to(1.0 - a, 1.0 - a);
    pb.line_to(a, 1.0 - a);
    pb.close();
}

fn unit_corner(pb: &mut PathBuilder, a: f32) {
    let b = a * 2.0;
    pb.move_to(0.0, 0.0);
    pb.line_to(1.0, 0.0);
    pb.line_to(1.0, a);
    pb.line_to(b, a);
    pb.line_to(b, 1.0 - b);
    pb.line_to(a, 1.0 - b);
    pb.line_to(a, 1.0);
    pb.line_to(0.0, 1.0);
    pb.close();
}

fn unit_rect_callout(pb: &mut PathBuilder, a: f32, b: f32) {
    unit_rect(pb, 0.0, 0.0, 1.0, 0.75);
    // 指示尾巴
    pb.move_to(0.4, 0.75);
    pb.line_to(a.max(0.0).min(1.0), b.max(0.0).min(1.0));
    pb.line_to(0.6, 0.75);
}

fn unit_round_rect_callout(pb: &mut PathBuilder, a: f32, b: f32) {
    unit_round_rect(pb, 0.0, 0.0, 1.0, 0.75, 0.15);
    pb.move_to(0.4, 0.75);
    pb.line_to(a.max(0.0).min(1.0), b.max(0.0).min(1.0));
    pb.line_to(0.6, 0.75);
}

fn unit_ellipse_callout(pb: &mut PathBuilder, a: f32, b: f32) {
    pb.arc_ellipse(0.5, 0.375, 0.5, 0.375, 0.0, std::f32::consts::TAU);
    pb.move_to(0.4, 0.72);
    pb.line_to(a.max(0.0).min(1.0), b.max(0.0).min(1.0));
    pb.line_to(0.6, 0.72);
}

/// 云朵标注（`cloudCallout`）：云朵 + 指示尾巴。
///
/// 云本身是「几个圆叠出来」的近似（同 [`unit_cloud`]），只是压到上方 3/4，
/// 给尾巴留出位置；尾巴按 `adj1`/`adj2` 指到的点画一个三角。
fn unit_cloud_callout(pb: &mut PathBuilder, a: f32, b: f32) {
    use std::f32::consts::TAU;
    pb.arc_ellipse(0.28, 0.30, 0.28, 0.21, 0.0, TAU);
    pb.arc_ellipse(0.52, 0.22, 0.24, 0.18, 0.0, TAU);
    pb.arc_ellipse(0.75, 0.34, 0.25, 0.19, 0.0, TAU);
    pb.arc_ellipse(0.50, 0.48, 0.32, 0.19, 0.0, TAU);
    pb.move_to(0.42, 0.60);
    pb.line_to(a.clamp(0.0, 1.0), b.clamp(0.0, 1.0));
    pb.line_to(0.60, 0.58);
}

fn unit_brace(pb: &mut PathBuilder) {
    // 左花括号轮廓（用贝塞尔近似）
    let k = 0.552_284_75;
    pb.move_to(1.0, 0.0);
    pb.cubic_to(1.0 - k * 0.5, 0.0, 0.5, 0.05, 0.5, 0.25);
    pb.line_to(0.5, 0.42);
    pb.cubic_to(0.5, 0.5, 0.4, 0.5, 0.25, 0.5);
    pb.cubic_to(0.4, 0.5, 0.5, 0.5, 0.5, 0.58);
    pb.line_to(0.5, 0.75);
    pb.cubic_to(0.5, 0.95, 1.0 - k * 0.5, 1.0, 1.0, 1.0);
    // 返回
    pb.line_to(1.0, 0.9);
    pb.cubic_to(0.7, 0.9, 0.6, 0.85, 0.6, 0.7);
    pb.line_to(0.6, 0.56);
    pb.cubic_to(0.6, 0.5, 0.55, 0.48, 0.45, 0.48);
    pb.cubic_to(0.55, 0.48, 0.6, 0.46, 0.6, 0.4);
    pb.line_to(0.6, 0.26);
    pb.cubic_to(0.6, 0.11, 0.7, 0.1, 1.0, 0.1);
    pb.close();
}

fn unit_bracket(pb: &mut PathBuilder) {
    pb.move_to(1.0, 0.0);
    pb.line_to(0.5, 0.0);
    pb.line_to(0.5, 0.2);
    pb.line_to(0.8, 0.2);
    pb.line_to(0.8, 0.8);
    pb.line_to(0.5, 0.8);
    pb.line_to(0.5, 1.0);
    pb.line_to(1.0, 1.0);
    pb.close();
}

fn unit_circular_arrow(pb: &mut PathBuilder) {
    // 环形 + 缺口 + 箭头，用近似画法
    pb.arc_ellipse(0.5, 0.5, 0.45, 0.45, -0.5, std::f32::consts::TAU - 1.0);
    pb.move_to(0.5, 0.05);
    pb.line_to(0.95, 0.5);
    pb.line_to(0.5, 0.5);
    pb.close();
}

/// 展开 `a:custGeom`。
///
/// 自定义几何的路径坐标直接位于形状局部空间（由 `a:path/@w`、`@h` 指定），
/// 因此只需按 `path_w/path_h → extent` 线性缩放。
pub fn expand_custom(
    cust_geom: &ppt_core::XmlNode,
    extent: Size,
) -> Geometry {
    let Some(path_lst) = cust_geom.child("pathLst") else {
        return Geometry::placeholder("custGeom 缺少 pathLst");
    };

    let mut subpaths: Vec<SubPath> = Vec::new();

    for path in path_lst.children_named("path") {
        let pw = path.attr_f64("w").unwrap_or(GUIDE_SPACE as f64) as f32;
        let ph = path.attr_f64("h").unwrap_or(GUIDE_SPACE as f64) as f32;
        let sx = if pw.abs() < f32::EPSILON { 1.0 } else { extent.w / pw };
        let sy = if ph.abs() < f32::EPSILON { 1.0 } else { extent.h / ph };

        let mut current: Option<SubPath> = None;
        let m = |x: f32, y: f32| Point::new(x * sx, y * sy);

        for cmd in &path.children {
            match cmd.name.as_str() {
                "moveTo" => {
                    if let Some(sp) = current.take() {
                        subpaths.push(sp);
                    }
                    if let Some(pt) = cmd.child("pt") {
                        current = Some(SubPath {
                            start: m(
                                pt.attr_f64("x").unwrap_or(0.0) as f32,
                                pt.attr_f64("y").unwrap_or(0.0) as f32,
                            ),
                            segments: Vec::new(),
                            closed: false,
                        });
                    }
                }
                "lnTo" => {
                    if let (Some(sp), Some(pt)) = (current.as_mut(), cmd.child("pt")) {
                        sp.segments.push(PathSegment::Line(m(
                            pt.attr_f64("x").unwrap_or(0.0) as f32,
                            pt.attr_f64("y").unwrap_or(0.0) as f32,
                        )));
                    }
                }
                "cubicBezTo" => {
                    let pts: Vec<_> = cmd.children_named("pt").collect();
                    if pts.len() == 3 {
                        if let Some(sp) = current.as_mut() {
                            let g = |i: usize, k: &str| {
                                pts[i].attr_f64(k).unwrap_or(0.0) as f32
                            };
                            sp.segments.push(PathSegment::Cubic {
                                c1: m(g(0, "x"), g(0, "y")),
                                c2: m(g(1, "x"), g(1, "y")),
                                to: m(g(2, "x"), g(2, "y")),
                            });
                        }
                    }
                }
                "quadBezTo" => {
                    let pts: Vec<_> = cmd.children_named("pt").collect();
                    if pts.len() == 2 {
                        if let Some(sp) = current.as_mut() {
                            let g = |i: usize, k: &str| {
                                pts[i].attr_f64(k).unwrap_or(0.0) as f32
                            };
                            sp.segments.push(PathSegment::Quad {
                                c: m(g(0, "x"), g(0, "y")),
                                to: m(g(1, "x"), g(1, "y")),
                            });
                        }
                    }
                }
                "arcTo" => {
                    if let Some(sp) = current.as_mut() {
                        let f = |k: &str, d: f64| cmd.attr_f64(k).unwrap_or(d) as f32;
                        // arcTo 的 wR/hR 是相对路径坐标系的角度半径
                        let st = f("stAng", 0.0).to_radians();
                        let sw = f("swAng", 0.0).to_radians();
                        // 由起点与半径推算圆心
                        let last = sp
                            .segments
                            .last()
                            .map(|s| match s {
                                PathSegment::Line(p) => *p,
                                PathSegment::Cubic { to, .. } => *to,
                                PathSegment::Quad { to, .. } => *to,
                                PathSegment::Arc { to, .. } => *to,
                                PathSegment::Close => sp.start,
                            })
                            .unwrap_or(sp.start);
                        let rx = f("wR", 0.0) * sx;
                        let ry = f("hR", 0.0) * sy;
                        let cx = last.x - rx * st.cos();
                        let cy = last.y - ry * st.sin();
                        let to = Point::new(cx + rx * (st + sw).cos(), cy + ry * (st + sw).sin());
                        sp.segments.push(PathSegment::Arc {
                            rx,
                            ry,
                            x_axis_rotation_deg: 0.0,
                            large_arc: sw.abs() > std::f32::consts::PI,
                            sweep: sw > 0.0,
                            to,
                        });
                    }
                }
                "close" => {
                    if let Some(sp) = current.as_mut() {
                        sp.closed = true;
                    }
                }
                _ => {}
            }
        }

        if let Some(sp) = current.take() {
            subpaths.push(sp);
        }
    }

    if subpaths.is_empty() {
        return Geometry::placeholder("custGeom 未解析出任何路径");
    }

    Geometry::Path(PathGeometry {
        subpaths,
        text_rect: cust_geom
            .child("rect")
            .map(|r| {
                let g = |k: &str, d: f64| r.attr_f64(k).unwrap_or(d) as f32;
                Rect::new(
                    g("l", 0.0) * extent.w / GUIDE_SPACE,
                    g("t", 0.0) * extent.h / GUIDE_SPACE,
                    (g("r", GUIDE_SPACE as f64) - g("l", 0.0)) * extent.w / GUIDE_SPACE,
                    (g("b", GUIDE_SPACE as f64) - g("t", 0.0)) * extent.h / GUIDE_SPACE,
                )
            }),
        bbox: Rect::new(0.0, 0.0, extent.w, extent.h),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ppt_core::xml;

    fn adj(xml_text: &str) -> AdjustValues {
        AdjustValues::parse(Some(&xml::parse_root("t", xml_text).unwrap()))
    }

    fn extent() -> Size {
        Size::new(200.0, 100.0)
    }

    /// 采样路径上的点，用于断言真实几何范围。
    ///
    /// 注意：**不能直接用贝塞尔控制点**来估算包围盒 ——
    /// 控制点通常落在曲线外侧（`chord`、`arc` 这类大跨度弧尤为明显），
    /// 会导致误判为「超出形状范围」。这里按固定步数把曲线离散成折线。
    fn all_points(g: &Geometry) -> Vec<Point> {
        let Geometry::Path(p) = g else {
            return Vec::new();
        };
        const CURVE_STEPS: usize = 32;
        let mut pts = Vec::new();

        for sp in &p.subpaths {
            pts.push(sp.start);
            let mut cursor = sp.start;
            for s in &sp.segments {
                match s {
                    PathSegment::Line(pt) => {
                        pts.push(*pt);
                        cursor = *pt;
                    }
                    PathSegment::Cubic { c1, c2, to } => {
                        for i in 1..=CURVE_STEPS {
                            let t = i as f32 / CURVE_STEPS as f32;
                            pts.push(cubic_at(cursor, *c1, *c2, *to, t));
                        }
                        cursor = *to;
                    }
                    PathSegment::Quad { c, to } => {
                        for i in 1..=CURVE_STEPS {
                            let t = i as f32 / CURVE_STEPS as f32;
                            pts.push(quad_at(cursor, *c, *to, t));
                        }
                        cursor = *to;
                    }
                    PathSegment::Arc { to, .. } => {
                        pts.push(*to);
                        cursor = *to;
                    }
                    PathSegment::Close => {}
                }
            }
        }
        pts
    }

    fn cubic_at(p0: Point, c1: Point, c2: Point, p1: Point, t: f32) -> Point {
        let u = 1.0 - t;
        let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
        Point::new(
            a * p0.x + b * c1.x + c * c2.x + d * p1.x,
            a * p0.y + b * c1.y + c * c2.y + d * p1.y,
        )
    }

    fn quad_at(p0: Point, c: Point, p1: Point, t: f32) -> Point {
        let u = 1.0 - t;
        Point::new(
            u * u * p0.x + 2.0 * u * t * c.x + t * t * p1.x,
            u * u * p0.y + 2.0 * u * t * c.y + t * t * p1.y,
        )
    }

    fn bbox(g: &Geometry) -> Rect {
        let pts = all_points(g);
        assert!(!pts.is_empty(), "路径不应为空");
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for p in pts {
            x0 = x0.min(p.x);
            y0 = y0.min(p.y);
            x1 = x1.max(p.x);
            y1 = y1.max(p.y);
        }
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }

    #[test]
    fn rect_fills_extent() {
        let g = expand("rect", &AdjustValues::default(), extent());
        let b = bbox(&g);
        assert!((b.x).abs() < 0.01);
        assert!((b.y).abs() < 0.01);
        assert!((b.w - 200.0).abs() < 0.01);
        assert!((b.h - 100.0).abs() < 0.01);
    }

    #[test]
    fn round_rect_stays_within_extent() {
        let g = expand("roundRect", &AdjustValues::default(), extent());
        let b = bbox(&g);
        assert!(b.x >= -0.01 && b.y >= -0.01);
        assert!(b.w <= 200.01 && b.h <= 100.01);
        assert!(b.w > 190.0 && b.h > 90.0, "圆角矩形应基本填满");
    }

    #[test]
    fn round_rect_with_zero_radius_is_rect() {
        let g = expand("roundRect", &adj(r#"<a:avLst><a:gd name="adj" fmla="val 0"/></a:avLst>"#), extent());
        let b = bbox(&g);
        assert!((b.w - 200.0).abs() < 0.01);
    }

    #[test]
    fn ellipse_stays_within_extent() {
        let g = expand("ellipse", &AdjustValues::default(), extent());
        let b = bbox(&g);
        // 三次贝塞尔逼近的椭圆会略微内缩，但不应超出
        assert!(b.x >= -0.5 && b.y >= -0.5);
        assert!(b.w <= 200.5 && b.h <= 100.5);
        assert!(b.w > 195.0 && b.h > 95.0, "椭圆应基本填满，实际 {b:?}");
    }

    #[test]
    fn triangle_has_three_corners() {
        let g = expand("triangle", &AdjustValues::default(), extent());
        let Geometry::Path(p) = &g else {
            panic!("应为路径几何");
        };
        assert_eq!(p.subpaths.len(), 1);
        // 起点 + 2 条线 = 3 个顶点
        assert_eq!(p.subpaths[0].segments.len(), 2);
        assert!(p.subpaths[0].closed);
    }

    #[test]
    fn diamond_is_centered() {
        let g = expand("diamond", &AdjustValues::default(), extent());
        let b = bbox(&g);
        assert!((b.w - 200.0).abs() < 0.01);
        assert!((b.h - 100.0).abs() < 0.01);
    }

    #[test]
    fn star5_has_ten_vertices() {
        let g = expand("star5", &AdjustValues::default(), extent());
        let Geometry::Path(p) = &g else {
            panic!("应为路径几何");
        };
        // 5 个外顶点 + 5 个内顶点 = 10 个顶点 → 9 条线段 + 闭合
        assert_eq!(p.subpaths[0].segments.len(), 9);
    }

    #[test]
    fn star_points_stay_within_extent() {
        for name in ["star4", "star5", "star6", "star8", "star12"] {
            let g = expand(name, &AdjustValues::default(), extent());
            let b = bbox(&g);
            assert!(
                b.x >= -0.5 && b.y >= -0.5 && b.w <= 200.5 && b.h <= 100.5,
                "{name} 超出包围盒：{b:?}"
            );
        }
    }

    #[test]
    fn arrows_stay_within_extent() {
        for name in [
            "leftArrow",
            "rightArrow",
            "upArrow",
            "downArrow",
            "leftRightArrow",
            "upDownArrow",
        ] {
            let g = expand(name, &AdjustValues::default(), extent());
            let b = bbox(&g);
            assert!(
                b.x >= -0.5 && b.y >= -0.5 && b.w <= 200.5 && b.h <= 100.5,
                "{name} 超出包围盒：{b:?}"
            );
            assert!(b.w > 100.0 && b.h > 50.0, "{name} 过小：{b:?}");
        }
    }

    #[test]
    fn plus_cross_is_symmetric() {
        let g = expand("plus", &AdjustValues::default(), extent());
        let b = bbox(&g);
        assert!((b.w - 200.0).abs() < 0.01);
        assert!((b.h - 100.0).abs() < 0.01);
    }

    #[test]
    fn chevron_stays_within_extent() {
        let g = expand("chevron", &AdjustValues::default(), extent());
        let b = bbox(&g);
        assert!(b.x >= -0.01 && b.w <= 200.01);
        assert!(b.h > 90.0);
    }

    #[test]
    fn line_preset_is_degenerate() {
        let g = expand("line", &AdjustValues::default(), extent());
        let b = bbox(&g);
        // 线条没有面积：高度为 0
        assert!(b.h.abs() < 0.01, "线条不应有高度，实际 {b:?}");
        assert!((b.w - 200.0).abs() < 0.01);
    }

    #[test]
    fn connectors_are_open_paths() {
        for name in ["bentConnector2", "bentConnector3", "curvedConnector2", "straightConnector1"] {
            let g = expand(name, &AdjustValues::default(), extent());
            let Geometry::Path(p) = &g else {
                panic!("{name} 应为路径");
            };
            assert!(!p.subpaths[0].closed, "{name} 连接线不应闭合");
        }
    }

    #[test]
    fn donut_has_two_subpaths() {
        let g = expand("donut", &AdjustValues::default(), extent());
        let Geometry::Path(p) = &g else {
            panic!("应为路径几何");
        };
        assert_eq!(p.subpaths.len(), 2, "圆环应由内外两条子路径构成");
    }

    #[test]
    fn preset_list_covers_common_teaching_shapes() {
        // 教学课件里最常见的形状必须都能展开，不能落到降级分支
        for name in [
            "rect",
            "roundRect",
            "ellipse",
            "triangle",
            "rtTriangle",
            "diamond",
            "parallelogram",
            "trapezoid",
            "pentagon",
            "hexagon",
            "octagon",
            "star5",
            "rightArrow",
            "leftArrow",
            "upArrow",
            "downArrow",
            "chevron",
            "plus",
            "minus",
            "line",
            "straightConnector1",
            "bentConnector3",
            "cloud",
            "heart",
            "sun",
            "moon",
            "smileyFace",
            "can",
            "cube",
            "bevel",
            "frame",
            // 扫过多份真实课件之后补上的三类：
            // 竖卷形（Unit 2 第 16 页）、离页连接符（流程图里成片出现）、
            // 动作按钮（PowerPoint 里就是矩形底，原来整块变灰占位）
            "verticalScroll",
            "horizontalScroll",
            "flowChartOffpageConnector",
            "cloudCallout",
            "actionButtonBlank",
            "actionButtonHome",
            "actionButtonForwardNext",
            "diagStripe",
            "flowChartProcess",
            "flowChartDecision",
            "flowChartTerminator",
            "wedgeRectCallout",
            "wedgeRoundRectCallout",
            "wedgeEllipseCallout",
            "bracePair",
            "bracketPair",
            "blockArc",
            "pie",
            "chord",
            "arc",
            "teardrop",
            "donut",
            "noSmoking",
            "homePlate",
            "foldedCorner",
        ] {
            let g = expand(name, &AdjustValues::default(), extent());
            assert!(
                !g.is_degraded(),
                "{name} 应被支持，实际走了降级：{g:?}"
            );
        }
    }

    #[test]
    fn unknown_preset_degrades_without_panic() {
        let g = expand("totallyUnknownShape", &AdjustValues::default(), extent());
        assert!(g.is_degraded(), "未知预设应降级为占位");
        match g {
            Geometry::Placeholder { reason } => {
                assert!(reason.contains("totallyUnknownShape"), "原因应包含预设名");
            }
            _ => panic!("应为占位几何"),
        }
    }

    #[test]
    fn all_presets_never_panic_and_never_escape_extent() {
        // 用一份尽可能全的预设清单做「模糊测试」：
        // 无论是否支持，都不能 panic，且支持的形状不应大幅越界
        let presets = [
            "rect", "roundRect", "snip1Rect", "snip2SameRect", "ellipse", "triangle",
            "rtTriangle", "diamond", "parallelogram", "trapezoid", "pentagon", "hexagon",
            "heptagon", "octagon", "star4", "star5", "star6", "star7", "star8", "star10",
            "star12", "star16", "star24", "star32", "plus", "mathPlus", "minus", "mathMultiply",
            "line", "straightConnector1", "bentConnector2", "bentConnector3", "curvedConnector2",
            "curvedConnector3", "leftArrow", "rightArrow", "upArrow", "downArrow",
            "leftRightArrow", "upDownArrow", "chevron", "homePlate", "donut", "noSmoking",
            "blockArc", "pie", "arc", "chord", "teardrop", "cloud", "heart", "moon", "sun",
            "smileyFace", "can", "cube", "foldedCorner", "bevel", "frame", "halfFrame",
            "corner", "diagStripe", "plaque", "flowChartProcess", "flowChartDecision",
            "flowChartTerminator", "flowChartData", "flowChartDocument",
            "flowChartPredefinedProcess", "flowChartConnector", "flowChartManualInput",
            "flowChartMagneticTape", "wedgeRectCallout", "wedgeRoundRectCallout",
            "wedgeEllipseCallout", "callout1", "rRectCallout", "borderCallout1",
            "rBorderCallout1", "bracePair", "bracketPair", "leftBrace", "rightBrace",
            "leftBracket", "rightBracket", "circularArrow", "bentArrow", "uturnArrow",
            "curvedRightArrow", "stripedRightArrow", "notchedRightArrow", "quadArrow",
            "leftRightUpArrow", "leftUpArrow", "bentUpArrow", "irregularSeal1", "irregularSeal2",
            "wave", "doubleWave", "ribbon", "ribbon2", "ellipseRibbon", "ellipseRibbon2",
            "leftRightRibbon", "verticalScroll", "horizontalScroll", "chartX", "chartStar",
            "chartPlus", "mathDivide", "mathEqual", "mathNotEqual", "gear6", "gear9",
            "funnel", "cornerTabs", "squareTabs", "plaqueTabs", "actionButtonBlank",
            "actionButtonHome", "actionButtonHelp", "actionButtonInformation",
            "actionButtonBackPrevious", "actionButtonForwardNext", "actionButtonBeginning",
            "actionButtonEnd", "actionButtonReturn", "actionButtonDocument",
            "actionButtonSound", "actionButtonMovie", "flowChartOffpageConnector",
            "flowChartOnlineStorage", "flowChartMagneticDisk", "flowChartMagneticDrum",
            "flowChartDisplay", "flowChartDelay", "flowChartExtract", "flowChartMerge",
            "flowChartMultidocument", "flowChartSort", "flowChartSummingJunction",
            "flowChartOr", "flowChartCollate", "flowChartPunchedTape", "flowChartPunchedCard",
            "flowChartInternalStorage", "flowChartPreparation", "leftCircularArrow",
            "rightCircularArrow", "curvedLeftArrow", "curvedUpArrow", "curvedDownArrow",
        ];

        for p in presets {
            let g = expand(p, &AdjustValues::default(), extent());
            if g.is_degraded() {
                continue;
            }
            let b = bbox(&g);
            assert!(
                b.x >= -1.0 && b.y >= -1.0 && b.w <= 201.0 && b.h <= 101.0,
                "{p} 展开后超出包围盒：{b:?}"
            );
        }
    }

    #[test]
    fn adjust_values_parse_and_normalize() {
        let a = adj(r#"<a:avLst><a:gd name="adj" fmla="val 25000"/></a:avLst>"#);
        // `val` 是百分数的十万分之一：25000 → 0.25
        assert!((a.ratio("adj", 0.0) - 0.25).abs() < 1e-5);
        assert_eq!(a.raw("adj", 0.0), 25000.0);
        // 缺失的调整值用默认
        assert!((a.ratio("missing", 0.5) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn adjust_values_empty_when_absent() {
        let a = AdjustValues::parse(None);
        assert!(a.is_empty());
        assert_eq!(a.raw("adj", 123.0), 123.0);
    }

    #[test]
    fn adjust_value_changes_round_rect_radius() {
        let small = expand(
            "roundRect",
            &adj(r#"<a:avLst><a:gd name="adj" fmla="val 1000"/></a:avLst>"#),
            extent(),
        );
        let large = expand(
            "roundRect",
            &adj(r#"<a:avLst><a:gd name="adj" fmla="val 20000"/></a:avLst>"#),
            extent(),
        );
        // 圆角越大，路径点越少贴近直角；用点数无法区分，改为比较控制点位置
        let small_pts = all_points(&small);
        let large_pts = all_points(&large);
        assert_eq!(small_pts.len(), large_pts.len());
        // 大圆角的第一个曲线控制点应更远离左上角
        assert!(
            large_pts[1].x < small_pts[1].x,
            "大圆角的控制点应更靠左：{:?} vs {:?}",
            large_pts[1],
            small_pts[1]
        );
    }

    #[test]
    fn custom_geometry_expands_lines() {
        let xml_text = r#"<a:custGeom>
            <a:avLst/><a:gdLst/><a:ahLst/><a:cxnLst/>
            <a:rect l="0" t="0" r="21600" b="21600"/>
            <a:pathLst>
              <a:path w="21600" h="21600">
                <a:moveTo><a:pt x="0" y="0"/></a:moveTo>
                <a:lnTo><a:pt x="21600" y="0"/></a:lnTo>
                <a:lnTo><a:pt x="21600" y="21600"/></a:lnTo>
                <a:lnTo><a:pt x="0" y="21600"/></a:lnTo>
                <a:close/>
              </a:path>
            </a:pathLst>
          </a:custGeom>"#;
        let n = xml::parse_root("t", xml_text).unwrap();
        let g = expand_custom(&n, extent());
        let b = bbox(&g);
        assert!((b.w - 200.0).abs() < 0.01);
        assert!((b.h - 100.0).abs() < 0.01);
    }

    #[test]
    fn custom_geometry_expands_cubic_and_quad() {
        let xml_text = r#"<a:custGeom>
            <a:pathLst>
              <a:path w="100" h="100">
                <a:moveTo><a:pt x="0" y="0"/></a:moveTo>
                <a:cubicBezTo>
                  <a:pt x="25" y="0"/><a:pt x="75" y="100"/><a:pt x="100" y="100"/>
                </a:cubicBezTo>
                <a:quadBezTo>
                  <a:pt x="50" y="50"/><a:pt x="0" y="0"/>
                </a:quadBezTo>
                <a:close/>
              </a:path>
            </a:pathLst>
          </a:custGeom>"#;
        let n = xml::parse_root("t", xml_text).unwrap();
        let g = expand_custom(&n, extent());
        let Geometry::Path(p) = &g else {
            panic!("应为路径");
        };
        assert_eq!(p.subpaths.len(), 1);
        assert!(matches!(p.subpaths[0].segments[0], PathSegment::Cubic { .. }));
        assert!(matches!(p.subpaths[0].segments[1], PathSegment::Quad { .. }));
    }

    #[test]
    fn custom_geometry_without_path_list_degrades() {
        let n = xml::parse_root("t", r#"<a:custGeom><a:avLst/></a:custGeom>"#).unwrap();
        let g = expand_custom(&n, extent());
        assert!(g.is_degraded());
    }

    #[test]
    fn custom_geometry_scales_non_square_path_space() {
        // 路径坐标系是 100x100，形状是 200x100 → x 方向拉伸 2 倍
        let xml_text = r#"<a:custGeom>
            <a:pathLst>
              <a:path w="100" h="100">
                <a:moveTo><a:pt x="0" y="0"/></a:moveTo>
                <a:lnTo><a:pt x="100" y="100"/></a:lnTo>
              </a:path>
            </a:pathLst>
          </a:custGeom>"#;
        let n = xml::parse_root("t", xml_text).unwrap();
        let g = expand_custom(&n, extent());
        let pts = all_points(&g);
        assert!((pts[1].x - 200.0).abs() < 0.01, "x 应被拉伸到 200");
        assert!((pts[1].y - 100.0).abs() < 0.01, "y 应保持 100");
    }

    #[test]
    fn zero_extent_does_not_panic() {
        for name in ["rect", "ellipse", "star5", "rightArrow", "donut"] {
            let g = expand(name, &AdjustValues::default(), Size::ZERO);
            assert!(!all_points(&g).is_empty(), "{name} 在零尺寸下仍应产出路径");
        }
    }

    #[test]
    fn negative_extent_does_not_panic() {
        // 课件里出现过负数尺寸的异常形状
        for name in ["rect", "ellipse", "roundRect"] {
            let _ = expand(name, &AdjustValues::default(), Size::new(-10.0, -20.0));
        }
    }
}
