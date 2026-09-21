//! 填充、描边与效果的解析。
//!
//! 这三个属性在 OOXML 里结构相似（都是「容器元素 + 具体类型子元素」），
//! 因此统一放在本模块，供形状、表格单元格、背景等复用。

use ppt_core::scene::{
    Color, DashStyle, Effects, Fill, Glow, GradientFill, GradientKind, ImageFill,
    ImageRef, ImageTile, InnerShadow, LineCap, LineEnd, LineEndKind, LineJoin, OuterShadow,
    PatternFill, PenAlign, RectAlign, Reflection, RelativeRect, Stroke, StrokeFill, TileAlign,
};
use ppt_core::units;
use ppt_core::XmlNode;

use crate::color::{self, ColorScheme};

/// 关系解析回调：把 `r:embed="rIdN"` 解析为媒体部件名。
///
/// 用闭包而非直接传 `Relationships`，是为了让本模块不依赖 OPC 层，
/// 便于单元测试（测试里传一个查表闭包即可）。
pub type RelResolver<'a> = &'a dyn Fn(&str) -> Option<String>;

/// 解析填充（`a:noFill` / `a:solidFill` / `a:gradFill` / `a:pattFill` / `a:blipFill` / `a:grpFill`）。
///
/// `sp_pr` 是形状属性元素（`p:spPr`），填充元素是它的直接子元素。
pub fn parse_fill(
    sp_pr: &XmlNode,
    scheme: Option<&ColorScheme>,
    resolve: RelResolver<'_>,
) -> Fill {
    if sp_pr.has_child("noFill") {
        return Fill::None;
    }
    if let Some(n) = sp_pr.child("solidFill") {
        return match color::parse_solid_fill(n, scheme) {
            Some(c) => Fill::Solid(c),
            None => Fill::None,
        };
    }
    if let Some(n) = sp_pr.child("gradFill") {
        return parse_gradient(n, scheme);
    }
    if let Some(n) = sp_pr.child("pattFill") {
        return parse_pattern(n, scheme);
    }
    if let Some(n) = sp_pr.child("blipFill") {
        return match parse_blip_fill(n, resolve) {
            Some(f) => Fill::Image(f),
            None => Fill::None,
        };
    }
    if sp_pr.has_child("grpFill") {
        return Fill::Inherit;
    }
    Fill::None
}

/// 解析 `a:gradFill`。
fn parse_gradient(node: &XmlNode, scheme: Option<&ColorScheme>) -> Fill {
    let stops = color::parse_gradient_stops(node, scheme);
    if stops.is_empty() {
        // 没有色标就无法绘制，按无填充处理避免整块变黑
        return Fill::None;
    }

    // `a:gSld` 决定色标插值空间；OOXML 里 sRGB 是默认，此处不再区分
    let kind = color::parse_gradient_kind(node).unwrap_or(GradientKind::Linear {
        angle_deg: 0.0,
        scaled: true,
    });

    let tile_rect = node
        .child("tileRect")
        .map(|_| compute_gradient_tile_rect(node));
    let rotate_with_shape = node.attr_bool_or("rotWithShape", true);
    let flip = node
        .child("tileRect")
        .map(color::parse_gradient_flip)
        .unwrap_or_default();

    Fill::Gradient(GradientFill {
        kind,
        stops,
        tile_rect,
        rotate_with_shape,
        flip,
    })
}

/// 由 `tileRect` 与 `a:lin` 的角度算出渐变的实际覆盖矩形。
fn compute_gradient_tile_rect(node: &XmlNode) -> RelativeRect {
    if let Some(tr) = node.child("tileRect") {
        return color::parse_relative_rect(tr);
    }
    RelativeRect::FULL
}

/// 解析 `a:pattFill`。
fn parse_pattern(node: &XmlNode, scheme: Option<&ColorScheme>) -> Fill {
    let preset = node.attr("prst").unwrap_or("pct50").to_string();
    let fg = node
        .child("fgClr")
        .and_then(|c| color::parse_solid_fill(c, scheme))
        .unwrap_or(Color::BLACK);
    let bg = node
        .child("bgClr")
        .and_then(|c| color::parse_solid_fill(c, scheme))
        .unwrap_or(Color::WHITE);
    Fill::Pattern(PatternFill {
        preset,
        foreground: fg,
        background: bg,
    })
}

/// 解析 `a:blipFill`。
///
/// 注意：`a:srcRect`、`a:stretch`、`a:tile` 都是 **`a:blipFill` 的子元素**，
/// 与 `a:blip` 平级，而不是 `a:blip` 的子元素。
fn parse_blip_fill(node: &XmlNode, resolve: RelResolver<'_>) -> Option<ImageFill> {
    let image = parse_blip_fill_image(node, resolve)?;
    // 图片部件名解析不出来（关系缺失/外部链接失效）时视为无填充，
    // 否则会得到一个「有填充但画不出任何东西」的退化状态
    if image.part.is_empty() {
        return None;
    }

    // `a:stretch` 与 `a:tile` 互斥
    let stretch = node
        .child("stretch")
        .and_then(|s| s.child("fillRect"))
        .map(color::parse_relative_rect)
        .filter(|r| !r.is_full());

    let tile = node.child("tile").map(|t| ImageTile {
        tx: t.attr_f64("tx").unwrap_or(0.0) as f32,
        ty: t.attr_f64("ty").unwrap_or(0.0) as f32,
        sx: t
            .attr_f64("sx")
            .map(|v| (v / 100_000.0) as f32)
            .unwrap_or(1.0),
        sy: t
            .attr_f64("sy")
            .map(|v| (v / 100_000.0) as f32)
            .unwrap_or(1.0),
        flip: color::parse_gradient_flip(t),
        align: parse_tile_align(t.attr("algn")),
    });

    Some(ImageFill {
        image,
        stretch,
        tile,
    })
}

fn parse_tile_align(v: Option<&str>) -> TileAlign {
    match v {
        Some("tl") => TileAlign::TopLeft,
        Some("t") => TileAlign::Top,
        Some("tr") => TileAlign::TopRight,
        Some("l") => TileAlign::Left,
        Some("ctr") => TileAlign::Center,
        Some("r") => TileAlign::Right,
        Some("bl") => TileAlign::BottomLeft,
        Some("b") => TileAlign::Bottom,
        Some("br") => TileAlign::BottomRight,
        _ => TileAlign::TopLeft,
    }
}

/// 从 `a:blipFill` 解析图片引用（含平级 `a:srcRect` 裁剪信息）。
pub fn parse_blip_fill_image(container: &XmlNode, resolve: RelResolver<'_>) -> Option<ImageRef> {
    let blip = container.child("blip")?;
    let src_rect = container
        .child("srcRect")
        .map(color::parse_src_rect)
        .unwrap_or(RelativeRect::FULL);
    Some(build_image_ref(blip, src_rect, resolve))
}

/// 从 `a:blip` 直接解析图片引用（无 `a:srcRect` 上下文时使用）。
pub fn parse_blip(blip: &XmlNode, resolve: RelResolver<'_>) -> Option<ImageRef> {
    // 少数工具把 srcRect 挂在 blip 下，这里一并兼容
    let src_rect = blip
        .child("srcRect")
        .map(color::parse_src_rect)
        .unwrap_or(RelativeRect::FULL);
    Some(build_image_ref(blip, src_rect, resolve))
}

fn build_image_ref(blip: &XmlNode, src_rect: RelativeRect, resolve: RelResolver<'_>) -> ImageRef {
    // r:embed 指向包内图片，r:link 指向外部文件
    let part = blip
        .attr("embed")
        .or_else(|| blip.attr("link"))
        .and_then(|rel_id| resolve(rel_id))
        // 关系解析失败时留空部件名，由渲染阶段跳过并记录告警，
        // 比在这里返回 None 丢失整块填充更可诊断
        .unwrap_or_default();

    let dpi = blip
        .attr_f64("dpi")
        .map(|v| v as f32)
        .filter(|v| *v > 0.0);

    let compression = blip.attr("cstate").and_then(|v| match v {
        "none" => Some(ppt_core::scene::ImageCompression::None),
        "print" | "screen" | "email" => Some(ppt_core::scene::ImageCompression::Jpeg),
        _ => None,
    });

    let alpha = blip
        .child("alphaModFix")
        .and_then(|a| a.attr_f64("amt"))
        .map(|v| (v / 100_000.0) as f32);

    ImageRef {
        part,
        src_rect,
        dpi,
        rotate_with_shape: blip.attr_bool_or("rotWithShape", true),
        compression,
        alpha,
        native_size: None,
    }
}

/// 解析描边（`a:ln`）。
///
/// 返回 `None` 表示「未显式设置描边」，由调用方决定是否继承主题线样式。
pub fn parse_stroke(
    ln: Option<&XmlNode>,
    scheme: Option<&ColorScheme>,
    resolve: RelResolver<'_>,
) -> Option<Stroke> {
    let ln = ln?;

    let width_pt = ln
        .attr_f64("w")
        .map(|emu| units::emu_to_pt(emu))
        .unwrap_or(1.0);

    let fill = if ln.has_child("noFill") {
        StrokeFill::None
    } else if let Some(n) = ln.child("solidFill") {
        match color::parse_solid_fill(n, scheme) {
            Some(c) => StrokeFill::Solid(c),
            None => StrokeFill::None,
        }
    } else if let Some(n) = ln.child("gradFill") {
        match parse_gradient(n, scheme) {
            Fill::Gradient(g) => StrokeFill::Gradient(g),
            _ => StrokeFill::None,
        }
    } else if let Some(n) = ln.child("pattFill") {
        // 图案描边非常罕见，退化为前景色实线
        match parse_pattern(n, scheme) {
            Fill::Pattern(p) => StrokeFill::Solid(p.foreground),
            _ => StrokeFill::None,
        }
    } else if let Some(blip_fill) = ln.child("blipFill") {
        blip_fill
            .child("blip")
            .and_then(|b| parse_blip(b, resolve))
            .map(StrokeFill::Image)
            .unwrap_or(StrokeFill::None)
    } else {
        // `a:ln` 存在但没写填充：按规范视为无填充
        StrokeFill::None
    };

    let (dash, custom_dash) = parse_dash(ln);

    let cap = match ln.attr("cap") {
        Some("rnd") => LineCap::Round,
        Some("sq") => LineCap::Square,
        _ => LineCap::Flat,
    };

    let (join, miter_limit) = if let Some(m) = ln.child("miter") {
        (LineJoin::Miter, m.attr_f64("lim").unwrap_or(800_000.0) as f32 / 100_000.0)
    } else if ln.has_child("bevel") {
        (LineJoin::Bevel, 8.0)
    } else if ln.has_child("round") {
        (LineJoin::Round, 8.0)
    } else {
        (LineJoin::Round, 8.0)
    };

    let align = match ln.attr("algn") {
        Some("in") => PenAlign::Inset,
        Some("out") => PenAlign::Outset,
        _ => PenAlign::Center,
    };

    Some(Stroke {
        fill,
        width_pt,
        dash,
        custom_dash,
        cap,
        join,
        miter_limit,
        align,
        head_end: ln.child("headEnd").and_then(parse_line_end),
        tail_end: ln.child("tailEnd").and_then(parse_line_end),
    })
}

fn parse_dash(ln: &XmlNode) -> (DashStyle, Vec<f32>) {
    if let Some(d) = ln.child("prstDash") {
        let style = match d.attr("val") {
            Some("dot") | Some("sysDot") => DashStyle::Dot,
            Some("dash") => DashStyle::Dash,
            Some("lgDash") => DashStyle::LgDash,
            Some("dashDot") => DashStyle::DashDot,
            Some("lgDashDot") => DashStyle::LgDashDot,
            Some("lgDashDotDot") => DashStyle::LgDashDotDot,
            Some("sysDash") => DashStyle::SysDash,
            Some("sysDashDot") => DashStyle::SysDashDot,
            Some("sysDashDotDot") => DashStyle::SysDashDotDot,
            Some("solid") | None => DashStyle::Solid,
            _ => DashStyle::Solid,
        };
        return (style, Vec::new());
    }

    if let Some(cd) = ln.child("custDash") {
        // `a:ds` 的 d/sp 是相对线宽的百分比，换算为「线宽倍数」
        let mut pattern = Vec::new();
        for ds in cd.children_named("ds") {
            let d = ds.attr_f64("d").unwrap_or(0.0) / 100_000.0;
            let sp = ds.attr_f64("sp").unwrap_or(0.0) / 100_000.0;
            if d > 0.0 {
                pattern.push(d as f32);
                pattern.push(sp as f32);
            }
        }
        if !pattern.is_empty() {
            return (DashStyle::Custom, pattern);
        }
    }

    (DashStyle::Solid, Vec::new())
}

/// 解析 `a:headEnd` / `a:tailEnd`。
///
/// `type="none"` 与不认识的写法都返回 `None` —— 「没有装饰」这件事
/// 用 `Option` 表达就够了，不必再留一个 `LineEndKind::None` 的空壳。
fn parse_line_end(node: &XmlNode) -> Option<LineEnd> {
    let kind = match node.attr("type")? {
        "triangle" => LineEndKind::Triangle,
        "stealth" => LineEndKind::Stealth,
        "diamond" => LineEndKind::Diamond,
        "oval" => LineEndKind::Oval,
        "arrow" => LineEndKind::Arrow,
        _ => return None,
    };
    Some(LineEnd {
        kind,
        width_scale: end_size(node.attr("w")),
        length_scale: end_size(node.attr("len")),
    })
}

/// `w` / `len` 的 `sm | med | lg` → 相对线宽的倍数。
///
/// 这三个档位在 OOXML 里就是 2 / 3 / 5 倍（缺省 `med`）。
fn end_size(v: Option<&str>) -> f32 {
    match v {
        Some("sm") => 2.0,
        Some("lg") => 5.0,
        _ => 3.0,
    }
}

/// 解析效果（`a:effectLst`）。
pub fn parse_effects(effect_lst: Option<&XmlNode>, scheme: Option<&ColorScheme>) -> Effects {
    let Some(lst) = effect_lst else {
        return Effects::default();
    };

    let mut fx = Effects::default();

    if let Some(s) = lst.child("outerShdw") {
        let color = s
            .children
            .iter()
            .find(|c| {
                matches!(
                    c.name.as_str(),
                    "srgbClr" | "schemeClr" | "prstClr" | "sysClr" | "scrgbClr" | "hslClr"
                )
            })
            .and_then(|c| color::resolve_color_node(c, scheme))
            // 默认半透明黑，与 PowerPoint 默认阴影一致
            .unwrap_or(Color::rgba(0, 0, 0, 102));

        fx.outer_shadow = Some(OuterShadow {
            color,
            blur_pt: units::emu_to_pt(s.attr_f64("blurRad").unwrap_or(0.0)),
            distance_pt: units::emu_to_pt(s.attr_f64("dist").unwrap_or(0.0)),
            direction_deg: units::ooxml_angle_to_deg(s.attr_f64("dir").unwrap_or(0.0)),
            scale_x: s
                .attr_f64("sx")
                .map(|v| (v / 100_000.0) as f32)
                .unwrap_or(1.0),
            scale_y: s
                .attr_f64("sy")
                .map(|v| (v / 100_000.0) as f32)
                .unwrap_or(1.0),
            align: parse_rect_align(s.attr("algn")),
            rotate_with_shape: s.attr_bool_or("rotWithShape", true),
            skew_x_deg: units::ooxml_angle_to_deg(s.attr_f64("kx").unwrap_or(0.0)),
            skew_y_deg: units::ooxml_angle_to_deg(s.attr_f64("ky").unwrap_or(0.0)),
        });
    }

    if let Some(s) = lst.child("innerShdw") {
        let color = s
            .children
            .iter()
            .find(|c| {
                matches!(
                    c.name.as_str(),
                    "srgbClr" | "schemeClr" | "prstClr" | "sysClr" | "scrgbClr" | "hslClr"
                )
            })
            .and_then(|c| color::resolve_color_node(c, scheme))
            .unwrap_or(Color::rgba(0, 0, 0, 128));

        fx.inner_shadow = Some(InnerShadow {
            color,
            blur_pt: units::emu_to_pt(s.attr_f64("blurRad").unwrap_or(0.0)),
            distance_pt: units::emu_to_pt(s.attr_f64("dist").unwrap_or(0.0)),
            direction_deg: units::ooxml_angle_to_deg(s.attr_f64("dir").unwrap_or(0.0)),
        });
    }

    if let Some(g) = lst.child("glow") {
        let color = g
            .children
            .iter()
            .find(|c| {
                matches!(
                    c.name.as_str(),
                    "srgbClr" | "schemeClr" | "prstClr" | "sysClr" | "scrgbClr" | "hslClr"
                )
            })
            .and_then(|c| color::resolve_color_node(c, scheme))
            .unwrap_or(Color::rgba(255, 255, 0, 200));

        fx.glow = Some(Glow {
            color,
            radius_pt: units::emu_to_pt(g.attr_f64("rad").unwrap_or(0.0)),
        });
    }

    if let Some(e) = lst.child("softEdge") {
        fx.soft_edge_pt = Some(units::emu_to_pt(e.attr_f64("rad").unwrap_or(0.0)));
    }

    if let Some(b) = lst.child("blur") {
        fx.blur_pt = Some(units::emu_to_pt(b.attr_f64("rad").unwrap_or(0.0)));
    }

    if let Some(r) = lst.child("reflection") {
        fx.reflection = Some(Reflection {
            blur_pt: units::emu_to_pt(r.attr_f64("blurRad").unwrap_or(0.0)),
            start_alpha: r
                .attr_f64("stA")
                .map(|v| (v / 100_000.0) as f32)
                .unwrap_or(0.5),
            end_alpha: r
                .attr_f64("endA")
                .map(|v| (v / 100_000.0) as f32)
                .unwrap_or(0.0),
            distance_pt: units::emu_to_pt(r.attr_f64("dist").unwrap_or(0.0)),
            direction_deg: units::ooxml_angle_to_deg(r.attr_f64("dir").unwrap_or(0.0)),
            fade_direction_deg: units::ooxml_angle_to_deg(r.attr_f64("fadeDir").unwrap_or(5400000.0)),
        });
    }

    fx
}

fn parse_rect_align(v: Option<&str>) -> RectAlign {
    match v {
        Some("t") => RectAlign::Top,
        Some("tr") => RectAlign::TopRight,
        Some("l") => RectAlign::Left,
        Some("ctr") => RectAlign::Center,
        Some("r") => RectAlign::Right,
        Some("bl") => RectAlign::BottomLeft,
        Some("b") => RectAlign::Bottom,
        Some("br") => RectAlign::BottomRight,
        _ => RectAlign::TopLeft,
    }
}

/// 描边的默认值（当形状未指定 `a:ln` 时使用）。
///
/// OOXML 中未指定描边意味着「不描边」，但主题的 `lnStyleLst`
/// 可能通过 `a:lnRef` 指定样式；这里给出一个可见的默认值，
/// 供调用方在需要「确保形状有轮廓」时使用。
pub fn default_stroke(scheme: Option<&ColorScheme>) -> Stroke {
    let color = scheme.map(|s| s.tx1).unwrap_or(Color::BLACK);
    Stroke {
        fill: StrokeFill::Solid(color),
        ..Stroke::default()
    }
}

/// `p:spPr` 里有没有**显式**写填充（`a:noFill` / `a:solidFill` / … 任意一种）。
///
/// # 为什么必须能区分
///
/// [`parse_fill`] 对「显式 `a:noFill`」和「压根没写填充」都返回 [`Fill::None`]，
/// 但这两者在 OOXML 里**语义完全不同**：
///
/// - 没写填充 → 去 `p:style/a:fillRef` 取主题填充样式；
/// - 写了 `a:noFill` → 就是**不要填充**，到此为止。
///
/// 分不清的后果是实打实的：课件里老师用「透明 + 红边」的圆角矩形挖空答案，
/// 形状写的是 `<a:noFill/>` 加 `p:style/a:fillRef idx="1"`，
/// 于是被回退成主题的 accent1 —— 透明的红框变成**蓝色实心块**，
/// 一页答案连同文字整片被盖住。
pub fn has_fill_spec(sp_pr: &XmlNode) -> bool {
    [
        "noFill", "solidFill", "gradFill", "pattFill", "blipFill", "grpFill",
    ]
    .iter()
    .any(|name| sp_pr.has_child(name))
}

/// 判断填充元素是否为「无效但仍需占位」的情形。
///
/// 用于诊断：课件里出现 `a:solidFill` 但颜色解析失败时，
/// 应当记录告警而不是静默变透明。
pub fn is_degenerate_fill(fill: &Fill) -> bool {
    matches!(fill, Fill::None)
}

/// 把 OOXML 的 `a:grpFill` 解析为父级填充。
///
/// 组合形状的 `a:grpFill` 语义是「沿用组合形状自身的填充」，
/// 需要在解析组合时把父级填充向下传递。
pub fn resolve_inherited_fill(child: &Fill, parent: &Fill) -> Fill {
    match child {
        Fill::Inherit => parent.clone(),
        other => other.clone(),
    }
}

/// 主题样式节点里的 `phClr` 占位色替换。
///
/// # 为什么需要这一步
///
/// 主题的 `a:fillStyleLst` / `a:lnStyleLst` 用 `phClr` 作为占位符，例如：
///
/// ```xml
/// <a:fillStyleLst>
///   <a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
/// </a:fillStyleLst>
/// ```
///
/// 形状通过 `a:fillRef` 引用它，并在**引用处**给出实际颜色：
///
/// ```xml
/// <a:fillRef idx="1"><a:schemeClr val="accent1"/></a:fillRef>
/// ```
///
/// 因此必须先把这个颜色「注入」到主题样式里，再走常规的颜色解析。
/// 采用「深拷贝 + 替换节点」而不是给颜色解析加参数，
/// 是为了让解析逻辑保持单一入口 —— 替换之后一切都走既有路径。
pub fn substitute_placeholder_color(style_node: &XmlNode, color: &XmlNode) -> XmlNode {
    let mut out = style_node.clone();
    replace_ph_clr(&mut out, color);
    out
}

fn replace_ph_clr(node: &mut XmlNode, color: &XmlNode) {
    for child in node.children.iter_mut() {
        if child.name == "schemeClr" && child.attr("val") == Some("phClr") {
            // 用引用处的颜色节点替换，但保留原节点上的变换子元素
            // （如 `a:shade`），因为变换是主题样式定义的。
            let transforms = std::mem::take(&mut child.children);
            let mut replacement = color.clone();
            replacement.children.extend(transforms);
            *child = replacement;
        } else {
            replace_ph_clr(child, color);
        }
    }
}

/// 构造一个实心填充节点（供需要合成填充的调用方使用）。
pub fn solid_fill_node(color: Color) -> XmlNode {
    XmlNode {
        name: "solidFill".to_string(),
        attrs: Vec::new(),
        children: vec![XmlNode {
            name: "srgbClr".to_string(),
            attrs: vec![(
                "val".to_string(),
                format!("{:02X}{:02X}{:02X}", color.r, color.g, color.b),
            )],
            children: Vec::new(),
            text: String::new(),
        }],
        text: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ppt_core::xml;

    fn n(xml_text: &str) -> XmlNode {
        xml::parse_root("t", xml_text).unwrap()
    }

    fn no_rel(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn parse_no_fill() {
        let sp = n(r#"<p:spPr><a:noFill/></p:spPr>"#);
        assert_eq!(parse_fill(&sp, None, &no_rel), Fill::None);
    }

    #[test]
    fn parse_solid_fill() {
        let sp = n(r#"<p:spPr><a:solidFill><a:srgbClr val="FF0000"/></a:solidFill></p:spPr>"#);
        assert_eq!(
            parse_fill(&sp, None, &no_rel),
            Fill::Solid(Color::rgb(255, 0, 0))
        );
    }

    #[test]
    fn parse_group_fill_as_inherit() {
        let sp = n(r#"<p:spPr><a:grpFill/></p:spPr>"#);
        assert_eq!(parse_fill(&sp, None, &no_rel), Fill::Inherit);
    }

    #[test]
    fn inherited_fill_resolves_to_parent() {
        let parent = Fill::Solid(Color::rgb(1, 2, 3));
        assert_eq!(
            resolve_inherited_fill(&Fill::Inherit, &parent),
            Fill::Solid(Color::rgb(1, 2, 3))
        );
        // 非 Inherit 的填充不被覆盖
        assert_eq!(
            resolve_inherited_fill(&Fill::Solid(Color::WHITE), &parent),
            Fill::Solid(Color::WHITE)
        );
    }

    #[test]
    fn parse_gradient_fill_with_stops() {
        let sp = n(
            r#"<p:spPr><a:gradFill rotWithShape="1">
                 <a:gsLst>
                   <a:gs pos="0"><a:srgbClr val="FF0000"/></a:gs>
                   <a:gs pos="100000"><a:srgbClr val="0000FF"/></a:gs>
                 </a:gsLst>
                 <a:lin ang="0" scaled="1"/>
               </a:gradFill></p:spPr>"#,
        );
        match parse_fill(&sp, None, &no_rel) {
            Fill::Gradient(g) => {
                assert_eq!(g.stops.len(), 2);
                assert_eq!(g.stops[0].color, Color::rgb(255, 0, 0));
                assert!(g.rotate_with_shape);
            }
            other => panic!("应为渐变填充，实际 {other:?}"),
        }
    }

    #[test]
    fn gradient_without_stops_is_treated_as_no_fill() {
        let sp = n(r#"<p:spPr><a:gradFill><a:lin ang="0"/></a:gradFill></p:spPr>"#);
        assert_eq!(parse_fill(&sp, None, &no_rel), Fill::None);
    }

    #[test]
    fn parse_pattern_fill() {
        let sp = n(
            r#"<p:spPr><a:pattFill prst="ltHorz">
                 <a:fgClr><a:srgbClr val="000000"/></a:fgClr>
                 <a:bgClr><a:srgbClr val="FFFFFF"/></a:bgClr>
               </a:pattFill></p:spPr>"#,
        );
        match parse_fill(&sp, None, &no_rel) {
            Fill::Pattern(p) => {
                assert_eq!(p.preset, "ltHorz");
                assert_eq!(p.foreground, Color::BLACK);
                assert_eq!(p.background, Color::WHITE);
            }
            other => panic!("应为图案填充，实际 {other:?}"),
        }
    }

    #[test]
    fn parse_blip_fill_resolves_relationship() {
        let sp = n(
            r#"<p:spPr><a:blipFill>
                 <a:blip r:embed="rId2"/>
                 <a:srcRect l="10000" t="20000" r="30000" b="40000"/>
                 <a:stretch><a:fillRect/></a:stretch>
               </a:blipFill></p:spPr>"#,
        );
        let resolve = |id: &str| {
            if id == "rId2" {
                Some("ppt/media/image1.png".to_string())
            } else {
                None
            }
        };
        match parse_fill(&sp, None, &resolve) {
            Fill::Image(img) => {
                assert_eq!(img.image.part, "ppt/media/image1.png");
                assert!((img.image.src_rect.l - 0.1).abs() < 1e-6);
                assert!((img.image.src_rect.t - 0.2).abs() < 1e-6);
            }
            other => panic!("应为图片填充，实际 {other:?}"),
        }
    }

    #[test]
    fn blip_fill_with_unresolvable_relationship_is_no_fill() {
        let sp = n(r#"<p:spPr><a:blipFill><a:blip r:embed="rId999"/></a:blipFill></p:spPr>"#);
        assert_eq!(parse_fill(&sp, None, &no_rel), Fill::None);
    }

    #[test]
    fn parse_tile_fill() {
        let sp = n(
            r#"<p:spPr><a:blipFill>
                 <a:blip r:embed="rId1"/>
                 <a:tile tx="10" ty="20" sx="50000" sy="25000" algn="ctr" flip="x"/>
               </a:blipFill></p:spPr>"#,
        );
        let resolve = |_: &str| Some("ppt/media/bg.png".to_string());
        match parse_fill(&sp, None, &resolve) {
            Fill::Image(img) => {
                let t = img.tile.expect("应解析出平铺参数");
                assert_eq!(t.tx, 10.0);
                assert!((t.sx - 0.5).abs() < 1e-6);
                assert!((t.sy - 0.25).abs() < 1e-6);
                assert_eq!(t.align, TileAlign::Center);
                assert!(t.flip.x);
            }
            other => panic!("应为图片填充，实际 {other:?}"),
        }
    }

    #[test]
    fn missing_fill_element_is_no_fill() {
        let sp = n(r#"<p:spPr><a:xfrm/></p:spPr>"#);
        assert_eq!(parse_fill(&sp, None, &no_rel), Fill::None);
        assert!(is_degenerate_fill(&Fill::None));
    }

    #[test]
    fn parse_stroke_width_converted_from_emu() {
        // 12700 EMU = 1pt
        let ln = n(r#"<a:ln w="25400"><a:solidFill><a:srgbClr val="000000"/></a:solidFill></a:ln>"#);
        let s = parse_stroke(Some(&ln), None, &no_rel).unwrap();
        assert!((s.width_pt - 2.0).abs() < 1e-4, "实际 {}", s.width_pt);
    }

    #[test]
    fn parse_stroke_without_width_defaults_to_one_point() {
        let ln = n(r#"<a:ln><a:solidFill><a:srgbClr val="000000"/></a:solidFill></a:ln>"#);
        let s = parse_stroke(Some(&ln), None, &no_rel).unwrap();
        assert!((s.width_pt - 1.0).abs() < 1e-4);
    }

    #[test]
    fn parse_stroke_no_fill() {
        let ln = n(r#"<a:ln w="12700"><a:noFill/></a:ln>"#);
        let s = parse_stroke(Some(&ln), None, &no_rel).unwrap();
        assert_eq!(s.fill, StrokeFill::None);
        assert!(!s.is_visible(), "无填充的描边不应可见");
    }

    #[test]
    fn missing_ln_returns_none() {
        assert!(parse_stroke(None, None, &no_rel).is_none());
    }

    #[test]
    fn parse_stroke_dash_styles() {
        for (val, expected) in [
            ("dash", DashStyle::Dash),
            ("dot", DashStyle::Dot),
            ("lgDash", DashStyle::LgDash),
            ("dashDot", DashStyle::DashDot),
            ("sysDash", DashStyle::SysDash),
            ("solid", DashStyle::Solid),
        ] {
            let ln = n(&format!(
                r#"<a:ln><a:solidFill><a:srgbClr val="000000"/></a:solidFill><a:prstDash val="{val}"/></a:ln>"#
            ));
            let s = parse_stroke(Some(&ln), None, &no_rel).unwrap();
            assert_eq!(s.dash, expected, "预设虚线 {val} 解析错误");
        }
    }

    #[test]
    fn parse_custom_dash() {
        let ln = n(
            r#"<a:ln><a:custDash>
                 <a:ds d="400000" sp="300000"/>
               </a:custDash></a:ln>"#,
        );
        let s = parse_stroke(Some(&ln), None, &no_rel).unwrap();
        assert_eq!(s.dash, DashStyle::Custom);
        assert_eq!(s.custom_dash, vec![4.0, 3.0]);
    }

    #[test]
    fn custom_dash_with_zero_length_falls_back_to_solid() {
        let ln = n(r#"<a:ln><a:custDash><a:ds d="0" sp="300000"/></a:custDash></a:ln>"#);
        let s = parse_stroke(Some(&ln), None, &no_rel).unwrap();
        assert_eq!(s.dash, DashStyle::Solid);
    }

    #[test]
    fn parse_stroke_cap_join_and_align() {
        let ln = n(
            r#"<a:ln cap="rnd" algn="in"><a:miter lim="1000000"/></a:ln>"#,
        );
        let s = parse_stroke(Some(&ln), None, &no_rel).unwrap();
        assert_eq!(s.cap, LineCap::Round);
        assert_eq!(s.join, LineJoin::Miter);
        assert!((s.miter_limit - 10.0).abs() < 1e-4);
        assert_eq!(s.align, PenAlign::Inset);
    }

    #[test]
    fn parse_stroke_join_variants() {
        let round = parse_stroke(Some(&n(r#"<a:ln><a:round/></a:ln>"#)), None, &no_rel).unwrap();
        assert_eq!(round.join, LineJoin::Round);
        let bevel = parse_stroke(Some(&n(r#"<a:ln><a:bevel/></a:ln>"#)), None, &no_rel).unwrap();
        assert_eq!(bevel.join, LineJoin::Bevel);
    }

    #[test]
    fn parse_line_ends() {
        let ln = n(
            r#"<a:ln>
                 <a:solidFill><a:srgbClr val="000000"/></a:solidFill>
                 <a:headEnd type="oval"/><a:tailEnd type="triangle" w="lg" len="sm"/>
               </a:ln>"#,
        );
        let s = parse_stroke(Some(&ln), None, &no_rel).unwrap();
        // 不写尺寸时按 OOXML 缺省的 med（3 倍线宽）
        assert_eq!(s.head_end, Some(LineEnd::new(LineEndKind::Oval)));
        // `w` / `len` 是相对线宽的倍数：lg = 5，sm = 2
        let tail = s.tail_end.expect("应解析出尾端箭头");
        assert_eq!(tail.kind, LineEndKind::Triangle);
        assert!((tail.width_scale - 5.0).abs() < 1e-6);
        assert!((tail.length_scale - 2.0).abs() < 1e-6);
    }

    #[test]
    fn line_end_none_type_maps_to_none() {
        let ln = n(r#"<a:ln><a:tailEnd type="none"/></a:ln>"#);
        let s = parse_stroke(Some(&ln), None, &no_rel).unwrap();
        assert_eq!(s.tail_end, None, "type=none 就是「不画」，用 None 表达");
    }

    #[test]
    fn parse_gradient_stroke() {
        let ln = n(
            r#"<a:ln>
                 <a:gradFill>
                   <a:gsLst>
                     <a:gs pos="0"><a:srgbClr val="FF0000"/></a:gs>
                     <a:gs pos="100000"><a:srgbClr val="00FF00"/></a:gs>
                   </a:gsLst>
                   <a:lin ang="0"/>
                 </a:gradFill>
               </a:ln>"#,
        );
        let s = parse_stroke(Some(&ln), None, &no_rel).unwrap();
        assert!(matches!(s.fill, StrokeFill::Gradient(_)));
        assert!(s.is_visible());
    }

    #[test]
    fn parse_outer_shadow() {
        let fx = parse_effects(
            Some(&n(
                r#"<a:effectLst>
                     <a:outerShdw blurRad="76200" dist="38100" dir="2700000" algn="tl" rotWithShape="0">
                       <a:srgbClr val="000000"><a:alpha val="40000"/></a:srgbClr>
                     </a:outerShdw>
                   </a:effectLst>"#,
            )),
            None,
        );
        let s = fx.outer_shadow.expect("应解析出外阴影");
        assert!((s.blur_pt - 6.0).abs() < 0.01, "实际模糊半径 {}", s.blur_pt);
        assert!((s.distance_pt - 3.0).abs() < 0.01);
        assert!((s.direction_deg - 45.0).abs() < 0.01);
        assert_eq!(s.color.a, 102);
        assert!(!s.rotate_with_shape);
    }

    #[test]
    fn parse_glow() {
        let fx = parse_effects(
            Some(&n(
                r#"<a:effectLst>
                     <a:glow rad="63500"><a:srgbClr val="FFC000"/></a:glow>
                   </a:effectLst>"#,
            )),
            None,
        );
        let g = fx.glow.expect("应解析出发光");
        assert!((g.radius_pt - 5.0).abs() < 0.01);
        assert_eq!(g.color, Color::rgb(255, 192, 0));
    }

    #[test]
    fn parse_soft_edge_and_blur_are_flagged_expensive() {
        let fx = parse_effects(
            Some(&n(
                r#"<a:effectLst>
                     <a:softEdge rad="127000"/>
                     <a:blur rad="63500"/>
                   </a:effectLst>"#,
            )),
            None,
        );
        assert!((fx.soft_edge_pt.unwrap() - 10.0).abs() < 0.01);
        assert!((fx.blur_pt.unwrap() - 5.0).abs() < 0.01);
        assert!(fx.has_expensive_effect(), "柔化边缘应标记为高开销");
    }

    #[test]
    fn parse_inner_shadow_and_reflection() {
        let fx = parse_effects(
            Some(&n(
                r#"<a:effectLst>
                     <a:innerShdw blurRad="50800" dist="25400" dir="5400000">
                       <a:srgbClr val="000000"/>
                     </a:innerShdw>
                     <a:reflection blurRad="6350" stA="60000" endA="10000" dist="25400" dir="5400000"/>
                   </a:effectLst>"#,
            )),
            None,
        );
        assert!(fx.inner_shadow.is_some());
        let r = fx.reflection.expect("应解析出倒影");
        assert!((r.start_alpha - 0.6).abs() < 1e-6);
        assert!((r.end_alpha - 0.1).abs() < 1e-6);
    }

    #[test]
    fn no_effect_list_yields_empty_effects() {
        let fx = parse_effects(None, None);
        assert!(fx.is_empty());
        assert!(!fx.has_expensive_effect());
    }

    #[test]
    fn empty_effect_list_yields_empty_effects() {
        let fx = parse_effects(Some(&n(r#"<a:effectLst/>"#)), None);
        assert!(fx.is_empty());
    }

    #[test]
    fn shadow_without_color_uses_visible_default() {
        let fx = parse_effects(
            Some(&n(r#"<a:effectLst><a:outerShdw blurRad="1000" dist="1000" dir="0"/></a:effectLst>"#)),
            None,
        );
        let s = fx.outer_shadow.unwrap();
        // 默认阴影必须可见，否则等于没画
        assert!(s.color.a > 0, "默认阴影不应完全透明");
    }

    #[test]
    fn scheme_color_in_effect_resolves() {
        let scheme = ColorScheme::default();
        let fx = parse_effects(
            Some(&n(
                r#"<a:effectLst>
                     <a:glow rad="10000"><a:schemeClr val="accent1"/></a:glow>
                   </a:effectLst>"#,
            )),
            Some(&scheme),
        );
        assert_eq!(fx.glow.unwrap().color, scheme.accent1);
    }

    #[test]
    fn default_stroke_is_visible() {
        let s = default_stroke(None);
        assert!(s.is_visible());
        assert_eq!(s.fill, StrokeFill::Solid(Color::BLACK));
    }

    #[test]
    fn default_stroke_uses_scheme_text_color() {
        let scheme = ColorScheme {
            tx1: Color::rgb(0x11, 0x22, 0x33),
            ..Default::default()
        };
        let s = default_stroke(Some(&scheme));
        assert_eq!(s.primary_color(), Some(Color::rgb(0x11, 0x22, 0x33)));
    }

    #[test]
    fn tile_align_values() {
        assert_eq!(parse_tile_align(Some("tl")), TileAlign::TopLeft);
        assert_eq!(parse_tile_align(Some("ctr")), TileAlign::Center);
        assert_eq!(parse_tile_align(Some("br")), TileAlign::BottomRight);
        assert_eq!(parse_tile_align(None), TileAlign::TopLeft);
        assert_eq!(parse_tile_align(Some("bogus")), TileAlign::TopLeft);
    }

    #[test]
    fn rect_align_values() {
        assert_eq!(parse_rect_align(Some("t")), RectAlign::Top);
        assert_eq!(parse_rect_align(Some("ctr")), RectAlign::Center);
        assert_eq!(parse_rect_align(Some("br")), RectAlign::BottomRight);
        assert_eq!(parse_rect_align(None), RectAlign::TopLeft);
    }

    #[test]
    fn image_compression_state() {
        let blip = n(r#"<a:blip r:embed="rId1" cstate="print"/>"#);
        let resolve = |_: &str| Some("ppt/media/i.jpg".to_string());
        let img = parse_blip(&blip, &resolve).unwrap();
        assert_eq!(
            img.compression,
            Some(ppt_core::scene::ImageCompression::Jpeg)
        );
    }

    #[test]
    fn image_alpha_mod_fix() {
        let blip = n(r#"<a:blip r:embed="rId1"><a:alphaModFix amt="70000"/></a:blip>"#);
        let resolve = |_: &str| Some("ppt/media/i.png".to_string());
        let img = parse_blip(&blip, &resolve).unwrap();
        assert!((img.alpha.unwrap() - 0.7).abs() < 1e-6);
    }

    #[test]
    fn image_link_attribute_is_accepted() {
        let blip = n(r#"<a:blip r:link="rId5"/>"#);
        let resolve = |id: &str| {
            if id == "rId5" {
                Some("ppt/media/linked.png".to_string())
            } else {
                None
            }
        };
        assert_eq!(
            parse_blip(&blip, &resolve).unwrap().part,
            "ppt/media/linked.png"
        );
    }

    #[test]
    fn blip_without_relationship_yields_empty_part() {
        // 直接调底层函数时保留空部件名，便于上层诊断
        let blip = n(r#"<a:blip/>"#);
        assert_eq!(parse_blip(&blip, &no_rel).unwrap().part, "");
    }

    #[test]
    fn src_rect_is_read_from_blip_fill_not_blip() {
        // 这是容易写错的一点：srcRect 与 blip 平级，不是 blip 的子元素
        let container = n(
            r#"<a:blipFill>
                 <a:blip r:embed="rId1"/>
                 <a:srcRect l="25000" t="0" r="0" b="0"/>
               </a:blipFill>"#,
        );
        let resolve = |_: &str| Some("ppt/media/i.png".to_string());
        let img = parse_blip_fill_image(&container, &resolve).unwrap();
        assert!((img.src_rect.l - 0.25).abs() < 1e-6, "实际 {}", img.src_rect.l);

        // 反例：srcRect 挂在 blip 下时，blipFill 层级读不到
        let wrong = n(
            r#"<a:blipFill>
                 <a:blip r:embed="rId1"><a:srcRect l="25000"/></a:blip>
               </a:blipFill>"#,
        );
        let img = parse_blip_fill_image(&wrong, &resolve).unwrap();
        assert!(img.src_rect.is_full(), "平级无 srcRect 时应视为整图");
    }

    #[test]
    fn ph_clr_substitution_replaces_placeholder_color() {
        // 主题样式用 phClr 占位
        let style = n(r#"<a:solidFill><a:schemeClr val="phClr"/></a:solidFill>"#);
        // 引用处指定 accent1
        let ref_color = n(r#"<a:schemeClr val="accent1"/>"#);

        let substituted = substitute_placeholder_color(&style, &ref_color);
        let scheme = ColorScheme::default();
        let fill = parse_fill(
            &n(&format!(
                "<p:spPr>{}</p:spPr>",
                xml_to_string(&substituted)
            )),
            Some(&scheme),
            &no_rel,
        );
        assert_eq!(fill, Fill::Solid(scheme.accent1));
    }

    #[test]
    fn ph_clr_substitution_preserves_style_transforms() {
        // 主题样式里 phClr 带 shade 变换，替换颜色后变换必须保留
        let style = n(
            r#"<a:solidFill><a:schemeClr val="phClr"><a:shade val="50000"/></a:schemeClr></a:solidFill>"#,
        );
        let ref_color = n(r#"<a:srgbClr val="FF0000"/>"#);
        let substituted = substitute_placeholder_color(&style, &ref_color);

        let fill = parse_fill(
            &n(&format!(
                "<p:spPr>{}</p:spPr>",
                xml_to_string(&substituted)
            )),
            None,
            &no_rel,
        );
        match fill {
            // shade 50% 应把红色压暗
            Fill::Solid(c) => {
                assert!(c.r > 100 && c.r < 160, "shade 后红色分量应约为 128，实际 {}", c.r);
                assert_eq!(c.g, 0);
                assert_eq!(c.b, 0);
            }
            other => panic!("应为实心填充，实际 {other:?}"),
        }
    }

    #[test]
    fn ph_clr_substitution_handles_nested_occurrences() {
        // 渐变里多处 phClr 都应被替换
        let style = n(
            r#"<a:gradFill><a:gsLst>
                 <a:gs pos="0"><a:schemeClr val="phClr"/></a:gs>
                 <a:gs pos="100000"><a:schemeClr val="phClr"><a:tint val="100000"/></a:schemeClr></a:gs>
               </a:gsLst><a:lin ang="0"/></a:gradFill>"#,
        );
        let ref_color = n(r#"<a:srgbClr val="0000FF"/>"#);
        let substituted = substitute_placeholder_color(&style, &ref_color);

        let fill = parse_fill(
            &n(&format!(
                "<p:spPr>{}</p:spPr>",
                xml_to_string(&substituted)
            )),
            None,
            &no_rel,
        );
        match fill {
            Fill::Gradient(g) => {
                assert_eq!(g.stops.len(), 2);
                assert_eq!(g.stops[0].color, Color::rgb(0, 0, 255));
                // 第二处带 tint 100% → 变白
                assert_eq!(g.stops[1].color, Color::WHITE);
            }
            other => panic!("应为渐变填充，实际 {other:?}"),
        }
    }

    #[test]
    fn ph_clr_substitution_leaves_other_colors_untouched() {
        let style = n(
            r#"<a:gradFill><a:gsLst>
                 <a:gs pos="0"><a:srgbClr val="00FF00"/></a:gs>
                 <a:gs pos="100000"><a:schemeClr val="accent1"/></a:gs>
               </a:gsLst></a:gradFill>"#,
        );
        let ref_color = n(r#"<a:srgbClr val="FF0000"/>"#);
        let substituted = substitute_placeholder_color(&style, &ref_color);
        let scheme = ColorScheme::default();

        match parse_fill(
            &n(&format!(
                "<p:spPr>{}</p:spPr>",
                xml_to_string(&substituted)
            )),
            Some(&scheme),
            &no_rel,
        ) {
            Fill::Gradient(g) => {
                assert_eq!(g.stops[0].color, Color::rgb(0, 255, 0), "非 phClr 应保持不变");
                assert_eq!(g.stops[1].color, scheme.accent1);
            }
            other => panic!("应为渐变填充，实际 {other:?}"),
        }
    }

    #[test]
    fn solid_fill_node_roundtrips() {
        let node = solid_fill_node(Color::rgb(0x12, 0x34, 0x56));
        assert_eq!(node.name, "solidFill");
        let fill = parse_fill(&n("<p:spPr/>"), None, &no_rel);
        assert_eq!(fill, Fill::None);
        // 直接验证构造出的节点能被解析回同一颜色
        match parse_fill(
            &XmlNode {
                name: "spPr".into(),
                children: vec![node],
                ..Default::default()
            },
            None,
            &no_rel,
        ) {
            Fill::Solid(c) => assert_eq!(c, Color::rgb(0x12, 0x34, 0x56)),
            other => panic!("应为实心填充，实际 {other:?}"),
        }
    }

    /// 把节点树序列化回 XML 字符串（仅测试用，用于复用基于字符串的构造方式）。
    fn xml_to_string(node: &XmlNode) -> String {
        let mut out = String::new();
        write_node(node, &mut out);
        out
    }

    fn write_node(node: &XmlNode, out: &mut String) {
        out.push('<');
        out.push_str(&node.name);
        for (k, v) in &node.attrs {
            // 测试里出现的属性值均为简单字面量，无需转义
            out.push_str(&format!(" {k}=\"{v}\""));
        }
        if node.children.is_empty() && node.text.is_empty() {
            out.push_str("/>");
            return;
        }
        out.push('>');
        out.push_str(&node.text);
        for c in &node.children {
            write_node(c, out);
        }
        out.push_str("</");
        out.push_str(&node.name);
        out.push('>');
    }
}
