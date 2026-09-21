//! SceneGraph → tiny-skia 的几何与画笔转换。
//!
//! # 变换的处理方式
//!
//! SceneGraph 的每个节点 `transform` 已经是**绝对变换**，
//! 但形状几何是**局部坐标**。渲染时把两者相乘交给 tiny-skia，
//! 让它一次性完成「局部 → 设备像素」的映射。
//!
//! 代价是描边宽度会被变换一起缩放（这是想要的：缩放幻灯片时
//! 线条应随之变粗），但也会被旋转继承 —— 对非均匀缩放的形状，
//! 描边宽度取平均缩放系数做补偿，避免线条粗细突变。

use tiny_skia::{
    BlendMode, FillRule, GradientStop, LineCap, LineJoin, LinearGradient, Paint, PathBuilder,
    PixmapPaint, RadialGradient, SpreadMode, Stroke as SkStroke, Transform as SkTransform,
};

use ppt_core::scene::{
    Color, Fill, GradientFill, GradientKind, LineCap as CoreLineCap, LineJoin as CoreLineJoin,
    PathGeometry, PathSegment, Point, RelativeRect, Stroke, StrokeFill, Transform,
};

/// 把 SceneGraph 的仿射变换转成 tiny-skia 变换。
pub fn to_sk_transform(t: Transform) -> SkTransform {
    let [a, b, c, d, e, f] = t.m;
    SkTransform::from_row(a, b, c, d, e, f)
}

/// 把 tiny-skia 变换转回 SceneGraph 的仿射变换。
///
/// 文本绘制需要「画布变换 ∘ 节点变换」的**设备空间**矩阵，
/// 而排版器只认 [`Transform`]，因此要能转回来。
pub fn from_sk_transform(t: SkTransform) -> Transform {
    Transform::new(t.sx, t.ky, t.kx, t.sy, t.tx, t.ty)
}

/// 把 SceneGraph 的颜色转成 tiny-skia 颜色。
///
/// SceneGraph 用**直通（非预乘）** RGBA，tiny-skia 同样用直通，
/// 因此直接搬运即可。这条约定省掉了每次绘制前的预乘/反预乘开销。
pub fn to_sk_color(c: Color) -> tiny_skia::Color {
    tiny_skia::Color::from_rgba8(c.r, c.g, c.b, c.a)
}

/// 把路径几何转为 tiny-skia 路径。
///
/// `PathGeometry` 允许 `Arc` 段（来自 OOXML `arcTo`），
/// 这里降级为二次贝塞尔近似 —— 弧段在课件里极少见，
/// 且视觉差异只在大半径时才可辨。
pub fn to_sk_path(geom: &PathGeometry) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();

    for sub in &geom.subpaths {
        pb.move_to(sub.start.x, sub.start.y);
        let mut cursor = sub.start;

        for seg in &sub.segments {
            match seg {
                PathSegment::Line(to) => {
                    pb.line_to(to.x, to.y);
                    cursor = *to;
                }
                PathSegment::Cubic { c1, c2, to } => {
                    pb.cubic_to(c1.x, c1.y, c2.x, c2.y, to.x, to.y);
                    cursor = *to;
                }
                PathSegment::Quad { c, to } => {
                    pb.quad_to(c.x, c.y, to.x, to.y);
                    cursor = *to;
                }
                PathSegment::Arc {
                    rx,
                    ry,
                    large_arc,
                    sweep,
                    to,
                    ..
                } => {
                    append_arc(&mut pb, cursor, *to, *rx, *ry, *large_arc, *sweep);
                    cursor = *to;
                }
                PathSegment::Close => {}
            }
        }

        if sub.closed {
            pb.close();
        }
    }

    pb.finish()
}

/// 用三次贝塞尔逼近一段椭圆弧。
///
/// 起点/终点由调用方给出，半径来自 OOXML `arcTo`。
/// 半径退化或无法求解圆心时退化为直线，保证不丢段。
fn append_arc(
    pb: &mut PathBuilder,
    from: Point,
    to: Point,
    rx: f32,
    ry: f32,
    large_arc: bool,
    sweep: bool,
) {
    if rx.abs() < 1e-4 || ry.abs() < 1e-4 {
        pb.line_to(to.x, to.y);
        return;
    }

    // 按 SVG 规范的端点参数化 → 圆心参数化
    let (rx, ry) = (rx.abs(), ry.abs());
    let dx = (from.x - to.x) / 2.0;
    let dy = (from.y - to.y) / 2.0;
    let lambda = (dx * dx) / (rx * rx) + (dy * dy) / (ry * ry);

    // 半径不足以连接两点时按规范等比放大
    let scale = if lambda > 1.0 { lambda.sqrt() } else { 1.0 };
    let (rx, ry) = (rx * scale, ry * scale);

    let sign = if large_arc != sweep { 1.0 } else { -1.0 };
    let num = (rx * rx * ry * ry - rx * rx * dy * dy - ry * ry * dx * dx).max(0.0);
    let den = rx * rx * dy * dy + ry * ry * dx * dx;
    let coef = if den <= 0.0 {
        0.0
    } else {
        sign * (num / den).sqrt()
    };

    let cx = coef * rx * dy / ry + (from.x + to.x) / 2.0;
    let cy = -coef * ry * dx / rx + (from.y + to.y) / 2.0;

    let angle = |x: f32, y: f32| -> f32 { y.atan2(x) };
    let theta1 = angle((from.x - cx) / rx, (from.y - cy) / ry);
    let theta2 = angle((to.x - cx) / rx, (to.y - cy) / ry);

    let mut delta = theta2 - theta1;
    if !sweep && delta > 0.0 {
        delta -= std::f32::consts::TAU;
    } else if sweep && delta < 0.0 {
        delta += std::f32::consts::TAU;
    }

    // 按最多 90 度一段拆分，保证逼近精度
    let segments = (delta.abs() / std::f32::consts::FRAC_PI_2).ceil().max(1.0) as usize;
    let step = delta / segments as f32;
    let k = 4.0 / 3.0 * (step / 4.0).tan();

    let mut a0 = theta1;
    let mut p0 = from;
    for _ in 0..segments {
        let a1 = a0 + step;
        let p1 = Point::new(cx + rx * a1.cos(), cy + ry * a1.sin());
        let t0 = Point::new(-rx * a0.sin(), ry * a0.cos());
        let t1 = Point::new(-rx * a1.sin(), ry * a1.cos());
        pb.cubic_to(
            p0.x + k * t0.x,
            p0.y + k * t0.y,
            p1.x - k * t1.x,
            p1.y - k * t1.y,
            p1.x,
            p1.y,
        );
        p0 = p1;
        a0 = a1;
    }
}

/// 填充规则：OOXML 用 even-odd（环、镂空都依赖它）。
pub const FILL_RULE: FillRule = FillRule::EvenOdd;

/// 把 [`Fill`] 转为 tiny-skia 画笔。
///
/// `bbox` 是形状的局部包围盒，用于计算渐变填充的贴图区域。
/// 返回 `None` 表示该填充不产生可见像素。
///
/// **注意**：`Fill::Image` 一律返回 `None`。
/// 原因是 tiny-skia 的 `Pattern` 着色器**借用了位图**，
/// 无法放进返回给调用方的 `Paint<'static>` 里。
/// 图片填充改由渲染器用「形状路径建遮罩 + 直接绘制位图」实现，
/// 见 `Renderer::draw_image_fill`。这样也顺带避免了平铺位图的缓存开销。
pub fn paint_for_fill(fill: &Fill, bbox: (f32, f32, f32, f32)) -> Option<Paint<'static>> {
    match fill {
        Fill::None | Fill::Inherit | Fill::Image(_) => None,
        Fill::Solid(c) => {
            if c.is_transparent() {
                return None;
            }
            let mut p = Paint::default();
            p.set_color(to_sk_color(*c));
            p.anti_alias = true;
            Some(p)
        }
        Fill::Gradient(g) => paint_for_gradient(g, bbox),
        Fill::Pattern(p) => {
            // 图案填充降级为前景色实心填充：图案纹理在课件里极少见，
            // 实心填充能保住「有底色」这一视觉信息，比画不出好
            let mut paint = Paint::default();
            paint.set_color(to_sk_color(p.foreground));
            paint.anti_alias = true;
            Some(paint)
        }
    }
}

/// 构造渐变画笔。
fn paint_for_gradient(g: &GradientFill, bbox: (f32, f32, f32, f32)) -> Option<Paint<'static>> {
    let stops = g.sorted_stops();
    if stops.is_empty() {
        return None;
    }

    let sk_stops: Vec<GradientStop> = stops
        .iter()
        .map(|s| GradientStop::new(s.pos, to_sk_color(s.color)))
        .collect();

    let (x, y, w, h) = bbox;
    let center = (x + w / 2.0, y + h / 2.0);

    // 渐变的贴图区域：OOXML 允许用 tileRect 把渐变限制在形状的一部分
    let (rx, ry, rw, rh) = match g.tile_rect {
        Some(r) if !r.is_full() => (x + r.l * w, y + r.t * h, (r.r - r.l) * w, (r.b - r.t) * h),
        _ => (x, y, w, h),
    };
    if rw.abs() < 1e-4 || rh.abs() < 1e-4 {
        return None;
    }

    let shader = match g.kind {
        GradientKind::Linear { angle_deg, scaled } => {
            // OOXML 角度：0 度指向正右方，顺时针为正。
            // 线性渐变应横跨形状，因此按角度在包围盒上取两端点。
            let rad = angle_deg.to_radians();
            let (sin, cos) = rad.sin_cos();
            // 半宽投影，保证渐变覆盖整个包围盒
            let half = if scaled {
                (rw * cos.abs() + rh * sin.abs()) / 2.0
            } else {
                rw.max(rh) / 2.0
            };
            let (cx, cy) = (rx + rw / 2.0, ry + rh / 2.0);
            let start = tiny_skia::Point::from_xy(cx - half * cos, cy - half * sin);
            let end = tiny_skia::Point::from_xy(cx + half * cos, cy + half * sin);

            LinearGradient::new(start, end, sk_stops, SpreadMode::Pad, SkTransform::identity())
        }
        GradientKind::Radial {
            ellipse,
            fill_to_rect,
            ..
        } => {
            let (fx, fy) = focus_of(fill_to_rect, rx, ry, rw, rh);
            let radius = if ellipse {
                // 椭圆渐变按包围盒对角线的一半近似，视觉上足够接近
                (rw * rw + rh * rh).sqrt() / 2.0
            } else {
                rw.min(rh) / 2.0
            };
            RadialGradient::new(
                tiny_skia::Point::from_xy(fx, fy),
                0.0,
                tiny_skia::Point::from_xy(center.0, center.1),
                radius,
                sk_stops,
                SpreadMode::Pad,
                SkTransform::identity(),
            )
        }
        GradientKind::Rect { fill_to_rect } | GradientKind::Shape { fill_to_rect } => {
            // 矩形/形状渐变在本渲染器里按径向近似 ——
            // 这两种类型在课件里极罕见，近似比不支持更有价值
            let (fx, fy) = focus_of(fill_to_rect, rx, ry, rw, rh);
            let radius = (rw * rw + rh * rh).sqrt() / 2.0;
            RadialGradient::new(
                tiny_skia::Point::from_xy(fx, fy),
                0.0,
                tiny_skia::Point::from_xy(center.0, center.1),
                radius,
                sk_stops,
                SpreadMode::Pad,
                SkTransform::identity(),
            )
        }
    };

    let mut paint = Paint::default();
    paint.shader = shader?;
    paint.anti_alias = true;
    Some(paint)
}

/// 由 `fillToRect` 计算径向渐变的焦点。
fn focus_of(
    rect: Option<RelativeRect>,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
) -> (f32, f32) {
    match rect {
        Some(r) => (x + (r.l + r.r) / 2.0 * w, y + (r.t + r.b) / 2.0 * h),
        None => (x + w / 2.0, y + h / 2.0),
    }
}

/// 图片绘制的混合参数。
pub fn image_paint(opacity: f32) -> PixmapPaint {
    PixmapPaint {
        opacity: opacity.clamp(0.0, 1.0),
        blend_mode: BlendMode::SourceOver,
        quality: tiny_skia::FilterQuality::Bilinear,
    }
}

/// 把 [`Stroke`] 转为 tiny-skia 描边。
pub fn stroke_for(s: &Stroke) -> Option<SkStroke> {
    stroke_for_scaled(s, 1.0)
}

/// 同上，但把线宽整体乘一个系数。
///
/// # 为什么要这个系数
///
/// `a:ln/@w` 与 `a:prstDash` 记的都是**绝对磅值**，不随所在坐标系缩放。
/// 而组合（`p:grpSp`）的子形状活在「被组合缩放过的局部坐标系」里，
/// 描边宽度却按局部单位交给变换 —— 于是组合里 4.5pt 的虚线边框
/// 会被缩成 0.2pt，屏幕上等于没有边框。传 `1 / 组合的单位缩放比`
/// 进去，经过节点变换后就正好回到它声明的磅值。
///
/// 虚线间隔由「线宽倍数 × 线宽」算出，所以必须跟着缩放后的宽度一起算，
/// 否则虚线的疏密会走样。
pub fn stroke_for_scaled(s: &Stroke, factor: f32) -> Option<SkStroke> {
    if !s.is_visible() {
        return None;
    }
    let width = s.width_pt * factor;
    let dash = match s.dash {
        ppt_core::scene::DashStyle::Solid => None,
        other => {
            let pattern = other.pattern(&s.custom_dash);
            if pattern.is_empty() {
                None
            } else {
                // OOXML 的虚线长度以「线宽倍数」表示，tiny-skia 要求绝对长度
                Some(tiny_skia::StrokeDash::new(
                    pattern.iter().map(|m| m * width).collect(),
                    0.0,
                )?)
            }
        }
    };

    Some(SkStroke {
        width,
        miter_limit: s.miter_limit,
        line_cap: match s.cap {
            CoreLineCap::Flat => LineCap::Butt,
            CoreLineCap::Round => LineCap::Round,
            CoreLineCap::Square => LineCap::Square,
        },
        line_join: match s.join {
            CoreLineJoin::Miter => LineJoin::Miter,
            CoreLineJoin::Round => LineJoin::Round,
            CoreLineJoin::Bevel => LineJoin::Bevel,
        },
        dash,
    })
}

/// 把 [`StrokeFill`] 转为描边画笔。
pub fn paint_for_stroke(s: &StrokeFill, bbox: (f32, f32, f32, f32)) -> Option<Paint<'static>> {
    match s {
        StrokeFill::None => None,
        StrokeFill::Solid(c) => {
            if c.is_transparent() {
                return None;
            }
            let mut p = Paint::default();
            p.set_color(to_sk_color(*c));
            p.anti_alias = true;
            Some(p)
        }
        StrokeFill::Gradient(g) => paint_for_gradient(g, bbox),
        StrokeFill::Image(_) => {
            // 图片描边极罕见，退化为黑色细线
            let mut p = Paint::default();
            p.set_color(tiny_skia::Color::BLACK);
            p.anti_alias = true;
            Some(p)
        }
    }
}

/// 计算路径的精确包围盒（局部坐标）。
///
/// 与 `ppt_core::scene::path_bounds` 的区别：那个用于粗筛（把控制点也算进去），
/// 这里用于渐变填充的贴图区域，需要真实范围。
pub fn exact_bounds(path: &tiny_skia::Path) -> (f32, f32, f32, f32) {
    let b = path.bounds();
    (b.left(), b.top(), b.width(), b.height())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ppt_core::scene::{DashStyle, PathSegment, SubPath};

    fn rect_geometry(w: f32, h: f32) -> PathGeometry {
        PathGeometry {
            subpaths: vec![SubPath {
                start: Point::new(0.0, 0.0),
                segments: vec![
                    PathSegment::Line(Point::new(w, 0.0)),
                    PathSegment::Line(Point::new(w, h)),
                    PathSegment::Line(Point::new(0.0, h)),
                ],
                closed: true,
            }],
            text_rect: None,
            bbox: ppt_core::scene::Rect::new(0.0, 0.0, w, h),
        }
    }

    #[test]
    fn transform_conversion_preserves_matrix() {
        let t = Transform::new(2.0, 0.5, -0.5, 3.0, 10.0, 20.0);
        let sk = to_sk_transform(t);
        let mut p = tiny_skia::Point::from_xy(1.0, 1.0);
        sk.map_point(&mut p);
        // x' = 2*1 + (-0.5)*1 + 10 = 11.5
        assert!((p.x - 11.5).abs() < 1e-4, "实际 {}", p.x);
        // y' = 0.5*1 + 3*1 + 20 = 23.5
        assert!((p.y - 23.5).abs() < 1e-4, "实际 {}", p.y);
    }

    #[test]
    fn color_conversion_preserves_channels() {
        let sk = to_sk_color(Color::rgba(10, 20, 30, 40));
        let (r, g, b, a) = (
            (sk.red() * 255.0).round() as u8,
            (sk.green() * 255.0).round() as u8,
            (sk.blue() * 255.0).round() as u8,
            (sk.alpha() * 255.0).round() as u8,
        );
        assert_eq!((r, g, b, a), (10, 20, 30, 40));
    }

    #[test]
    fn path_conversion_builds_closed_rect() {
        let path = to_sk_path(&rect_geometry(100.0, 50.0)).unwrap();
        let (x, y, w, h) = exact_bounds(&path);
        assert!(x.abs() < 0.01 && y.abs() < 0.01);
        assert!((w - 100.0).abs() < 0.01, "实际 {w}");
        assert!((h - 50.0).abs() < 0.01, "实际 {h}");
    }

    #[test]
    fn path_conversion_handles_empty_geometry() {
        let empty = PathGeometry::default();
        // 空路径不应 panic（可能返回 None 或空路径）
        let _ = to_sk_path(&empty);
    }

    #[test]
    fn path_conversion_handles_cubic_segments() {
        let g = PathGeometry {
            subpaths: vec![SubPath {
                start: Point::new(0.0, 0.0),
                segments: vec![PathSegment::Cubic {
                    c1: Point::new(10.0, 0.0),
                    c2: Point::new(20.0, 10.0),
                    to: Point::new(30.0, 10.0),
                }],
                closed: false,
            }],
            text_rect: None,
            bbox: ppt_core::scene::Rect::ZERO,
        };
        let path = to_sk_path(&g).expect("应能构造路径");
        let (_, _, w, _) = exact_bounds(&path);
        assert!(w > 25.0, "曲线应横向展开，实际宽度 {w}");
    }

    #[test]
    fn arc_with_zero_radius_degrades_to_line() {
        let g = PathGeometry {
            subpaths: vec![SubPath {
                start: Point::new(0.0, 0.0),
                segments: vec![PathSegment::Arc {
                    rx: 0.0,
                    ry: 0.0,
                    x_axis_rotation_deg: 0.0,
                    large_arc: false,
                    sweep: true,
                    to: Point::new(50.0, 30.0),
                }],
                closed: false,
            }],
            text_rect: None,
            bbox: ppt_core::scene::Rect::ZERO,
        };
        let path = to_sk_path(&g).expect("退化弧应仍能生成路径");
        let (_, _, w, h) = exact_bounds(&path);
        assert!((w - 50.0).abs() < 0.1);
        assert!((h - 30.0).abs() < 0.1);
    }

    #[test]
    fn solid_fill_produces_paint() {
        let p = paint_for_fill(&Fill::Solid(Color::rgb(255, 0, 0)), (0.0, 0.0, 10.0, 10.0));
        assert!(p.is_some());
    }

    #[test]
    fn transparent_solid_fill_produces_no_paint() {
        let p = paint_for_fill(
            &Fill::Solid(Color::TRANSPARENT),
            (0.0, 0.0, 10.0, 10.0),
        );
        assert!(p.is_none());
    }

    #[test]
    fn none_and_inherit_fills_produce_no_paint() {
        assert!(paint_for_fill(&Fill::None, (0.0, 0.0, 10.0, 10.0)).is_none());
        assert!(paint_for_fill(&Fill::Inherit, (0.0, 0.0, 10.0, 10.0)).is_none());
    }

    #[test]
    fn image_fill_without_resource_produces_no_paint() {
        let fill = Fill::Image(ppt_core::scene::ImageFill {
            image: ppt_core::scene::ImageRef::new("ppt/media/missing.png"),
            stretch: None,
            tile: None,
        });
        assert!(paint_for_fill(&fill, (0.0, 0.0, 100.0, 100.0)).is_none());
    }

    #[test]
    fn gradient_fill_produces_shader() {
        let g = GradientFill {
            kind: GradientKind::Linear {
                angle_deg: 0.0,
                scaled: true,
            },
            stops: vec![
                ppt_core::scene::GradientStop {
                    pos: 0.0,
                    color: Color::BLACK,
                },
                ppt_core::scene::GradientStop {
                    pos: 1.0,
                    color: Color::WHITE,
                },
            ],
            tile_rect: None,
            rotate_with_shape: true,
            flip: Default::default(),
        };
        let p = paint_for_fill(&Fill::Gradient(g), (0.0, 0.0, 100.0, 50.0));
        assert!(p.is_some());
    }

    #[test]
    fn gradient_without_stops_produces_no_paint() {
        let g = GradientFill {
            kind: GradientKind::Linear {
                angle_deg: 0.0,
                scaled: true,
            },
            stops: Vec::new(),
            tile_rect: None,
            rotate_with_shape: true,
            flip: Default::default(),
        };
        assert!(paint_for_fill(&Fill::Gradient(g), (0.0, 0.0, 100.0, 50.0)).is_none());
    }

    #[test]
    fn stroke_conversion_maps_caps_and_joins() {
        let s = Stroke {
            width_pt: 3.0,
            cap: CoreLineCap::Square,
            join: CoreLineJoin::Bevel,
            ..Stroke::default()
        };
        let sk = stroke_for(&s).unwrap();
        assert_eq!(sk.width, 3.0);
        assert_eq!(sk.line_cap, LineCap::Square);
        assert_eq!(sk.line_join, LineJoin::Bevel);
    }

    #[test]
    fn invisible_stroke_produces_none() {
        let s = Stroke {
            width_pt: 0.0,
            ..Stroke::default()
        };
        assert!(stroke_for(&s).is_none());
        // 缩放的入口也得挡住不可见描边，不能因为多了个系数就漏判
        assert!(stroke_for_scaled(&s, 20.0).is_none());
    }

    #[test]
    fn scaled_stroke_multiplies_width() {
        // 组合里的子形状，局部单位被组合缩放得极小（这里取 1/23 量级）。
        // `a:ln/@w` 是绝对磅值，靠这个系数还原回去 ——
        // 不还原的话，组合里 4.5pt 的虚线边框会变成 0.2pt，等于没有边框。
        let s = Stroke {
            width_pt: 4.5,
            dash: DashStyle::Dash,
            ..Stroke::default()
        };
        let factor = 1.0 / 0.0439;
        let sk = stroke_for_scaled(&s, factor).unwrap();
        assert!(
            (sk.width - 4.5 * factor).abs() < 1e-3,
            "线宽应乘上系数，实际 {}",
            sk.width
        );
        assert!(sk.dash.is_some(), "缩放过之后仍应是虚线");
    }

    #[test]
    fn dashed_stroke_scales_pattern_by_width() {
        let s = Stroke {
            width_pt: 2.0,
            dash: DashStyle::Dash,
            ..Stroke::default()
        };
        let sk = stroke_for(&s).unwrap();
        // tiny-skia 的虚线数组是私有字段，只能确认「确实设了虚线」
        assert!(sk.dash.is_some(), "应为虚线描边");
        assert_eq!(sk.width, 2.0);

        // 实线则不应有虚线参数
        let solid = Stroke {
            width_pt: 2.0,
            dash: DashStyle::Solid,
            ..Stroke::default()
        };
        assert!(stroke_for(&solid).unwrap().dash.is_none());
    }

    #[test]
    fn image_paint_clamps_opacity() {
        assert_eq!(image_paint(1.5).opacity, 1.0);
        assert_eq!(image_paint(-0.5).opacity, 0.0);
        assert_eq!(image_paint(0.5).opacity, 0.5);
    }

    #[test]
    fn exact_bounds_matches_geometry() {
        let path = to_sk_path(&rect_geometry(80.0, 40.0)).unwrap();
        let (x, y, w, h) = exact_bounds(&path);
        assert!(x.abs() < 0.01 && y.abs() < 0.01);
        assert!((w - 80.0).abs() < 0.01);
        assert!((h - 40.0).abs() < 0.01);
    }
}
