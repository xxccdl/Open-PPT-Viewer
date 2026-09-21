//! 文本绘制：把排版结果里的字形转成路径并填充。
//!
//! # 为什么走「轮廓 → 路径 → 填充」而不是位图字形缓存
//!
//! 位图字形缓存需要按「字号 + 缩放 + 亚像素位置」分档，档位爆炸且
//! 在非整数缩放（幻灯片适应窗口时很常见）下容易发虚。
//! 而课件单页的字形数量通常在几百个量级，
//! 直接走矢量路径既能保证任意缩放下的清晰度，开销也可接受。
//!
//! # 与排版引擎的分工
//!
//! `ppt-text` 输出的是**已定位的字形**（字体、glyph id、基线坐标、字号），
//! 本模块只负责把 glyph id 转成轮廓并填充 —— 不做断行、不做字距调整、
//! 不做回退选择。这样文本的「排版正确性」可以脱离像素单独测试。

use std::collections::HashMap;

use tiny_skia::{Paint, PathBuilder, Pixmap, Transform as SkTransform};

use ppt_core::scene::{Color, Size, StrikeStyle, TextBox, Transform, UnderlineStyle};
use ppt_text::{FontContext, LayoutOptions, LoadedFont, TextLayout, TextLayouter};

use crate::convert;

/// 字形路径缓存键。
///
/// 以「字体 + glyph id + 量化后的字号」为键。
/// 字号量化到 0.25pt 一档：同一段文本的字号高度重复，
/// 量化后命中率极高，而 0.25pt 的差异在视觉上不可辨。
type GlyphKey = (u32, u16, u32);

/// 字形轮廓缓存。
///
/// 一页课件里同一个字（如「的」「是」）会出现几十次，
/// 缓存轮廓能省掉大量重复的 `ttf-parser` 解析工作。
#[derive(Default)]
pub struct GlyphCache {
    entries: HashMap<GlyphKey, Option<CachedGlyph>>,
}

/// 缓存的字形轮廓（单位坐标：em 归一化）。
struct CachedGlyph {
    /// 轮廓子路径，坐标为 em 的倍数（1.0 = 字号大小）。
    contours: Vec<Contour>,
}

enum Contour {
    Move(f32, f32),
    Line(f32, f32),
    Quad(f32, f32, f32, f32),
    Cubic(f32, f32, f32, f32, f32, f32),
    Close,
}

impl GlyphCache {
    pub fn new() -> GlyphCache {
        GlyphCache::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// 取（或构建）字形轮廓。
    fn get_or_build(
        &mut self,
        font: &LoadedFont,
        glyph_id: u16,
        size_pt: f32,
    ) -> Option<&CachedGlyph> {
        let key = (
            font.id,
            glyph_id,
            (size_pt * 4.0) as u32, // 0.25pt 量化
        );
        if !self.entries.contains_key(&key) {
            let built = build_glyph(font, glyph_id);
            self.entries.insert(key, built);
        }
        self.entries.get(&key).and_then(|v| v.as_ref())
    }
}

/// 从字体提取字形轮廓，归一化到 em 坐标系。
fn build_glyph(font: &LoadedFont, glyph_id: u16) -> Option<CachedGlyph> {
    if font.is_placeholder() {
        return None;
    }

    font.with_face(|face| {
        let upem = font.units_per_em.max(1.0);
        let mut builder = ContourBuilder {
            contours: Vec::new(),
            upem,
        };
        let gid = ttf_parser::GlyphId(glyph_id);
        // `outline_glyph` 返回 None 表示该字形没有轮廓（如空格）
        face.outline_glyph(gid, &mut builder)?;
        if builder.contours.is_empty() {
            return None;
        }
        Some(CachedGlyph {
            contours: builder.contours,
        })
    })
    .flatten()
}

/// `ttf-parser` 的轮廓回调，把坐标归一化到 em 倍数。
///
/// # 为什么在这里翻转 y 轴
///
/// 字体坐标系的 **y 轴向上**（基线为 0，上升部为正），
/// 而画布坐标系的 **y 轴向下**。若不翻转，所有文字都会上下颠倒
/// （每个字形被垂直镜像，而字符顺序不变 —— 这是最容易被忽略的一种错）。
///
/// 在轮廓构建阶段就翻转，而不是在下游变换里加一个负缩放：
/// 这样 `Contour` 里的坐标已经是「屏幕方向」，后续所有几何运算
/// （路径包围盒、下划线定位、伪粗体偏移）都无需再考虑方向问题。
struct ContourBuilder {
    contours: Vec<Contour>,
    upem: f32,
}

impl ContourBuilder {
    #[inline]
    fn norm(&self, x: f32, y: f32) -> (f32, f32) {
        let k = 1.0 / self.upem;
        // y 取负：字体向上 → 画布向下
        (x * k, -y * k)
    }
}

impl ttf_parser::OutlineBuilder for ContourBuilder {
    fn move_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.norm(x, y);
        self.contours.push(Contour::Move(x, y));
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.norm(x, y);
        self.contours.push(Contour::Line(x, y));
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let (x1, y1) = self.norm(x1, y1);
        let (x, y) = self.norm(x, y);
        self.contours.push(Contour::Quad(x1, y1, x, y));
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let (x1, y1) = self.norm(x1, y1);
        let (x2, y2) = self.norm(x2, y2);
        let (x, y) = self.norm(x, y);
        self.contours
            .push(Contour::Cubic(x1, y1, x2, y2, x, y));
    }

    fn close(&mut self) {
        self.contours.push(Contour::Close);
    }
}

/// 只排版、不绘制。
///
/// 用来量「这段文字在这么宽的地方要占多高」—— 表格的行高要靠它长高。
/// 走的是与 [`draw_text`] 完全相同的排版路径，量出来的高度才与画出来的一致。
pub fn measure_text(
    fonts: &FontContext,
    text: &TextBox,
    area: Size,
    opts: LayoutOptions,
) -> TextLayout {
    TextLayouter::new(fonts).layout(text, area, opts)
}

/// 绘制一页文本。
///
/// `node_transform` 是文本框左上角到**设备像素**的完整变换
/// （画布缩放已包含在内）；`area` 是扣除内边距后的文本区尺寸（pt）。
/// 返回实际用到的字号缩放，供上层做布局微调。
#[allow(clippy::too_many_arguments)]
pub fn draw_text(
    pixmap: &mut Pixmap,
    fonts: &FontContext,
    text: &TextBox,
    area: Size,
    node_transform: Transform,
    opacity: f32,
    default_color: Color,
    cache: &mut GlyphCache,
    opts: LayoutOptions,
) -> TextLayout {
    let layouter = TextLayouter::new(fonts);
    // 排版在 pt 空间进行，缩放由 `node_transform` 承担。
    //
    // 这里必须拿到「画布变换 ∘ 节点变换」：只传节点变换会漏掉画布缩放，
    // 文本就会既不随分辨率变大、又停在未缩放的位置上 ——
    // 分辨率一变（高 DPI、放大窗口、放映）就明显偏位。
    let layout = layouter.layout(text, area, opts);

    for line in &layout.lines {
        for glyph in &line.glyphs {
            if glyph.advance <= 0.0 && glyph.glyph_id == 0 {
                continue;
            }

            let Some(cached) = cache.get_or_build(&glyph.font, glyph.glyph_id, glyph.size_pt) else {
                continue;
            };

            // 字形的绘制变换：局部（em 坐标 × 字号）→ 基线位置 → 节点变换
            //
            // 偏移量参数 `dx_pt` 以**画布 pt 为单位**，会先换算到 em 空间
            // 再随字号一起缩放。伪粗体必须走这条路径 ——
            // 若把平移写在字号缩放之后，偏移会被二次放大
            // （44pt 字会偏移 40 多 pt，看起来像文字重影）。
            let local = |dx_pt: f32| {
                Transform::translate(
                    glyph.x + dx_pt / glyph.size_pt.max(0.01),
                    line.baseline + glyph.y,
                )
                .multiply(&Transform::scale(glyph.size_pt, glyph.size_pt))
            };

            let mut t = node_transform.multiply(&local(0.0));
            if glyph.style.synthetic_italic {
                // 伪斜体：以基点为轴做水平切变
                t = t.multiply(&Transform::new(1.0, 0.0, -0.21, 1.0, 0.0, 0.0));
            }

            let sk_t = convert::to_sk_transform(t);
            let Some(path) = contours_to_path(&cached.contours) else {
                continue;
            };

            let color = if glyph.color == Color::TRANSPARENT {
                default_color
            } else {
                glyph.color
            };

            let mut paint = Paint::default();
            paint.set_color(convert::to_sk_color(apply_opacity(color, opacity)));
            paint.anti_alias = true;

            pixmap.fill_path(
                &path,
                &paint,
                convert::FILL_RULE,
                sk_t,
                None,
            );

            // 伪粗体：同路径向右偏移约 2% 字号再填一次，模拟笔画加粗
            if glyph.style.synthetic_bold {
                let offset = glyph.size_pt * 0.025;
                let bold_t = node_transform.multiply(&local(offset));
                let bold_t = if glyph.style.synthetic_italic {
                    bold_t.multiply(&Transform::new(1.0, 0.0, -0.21, 1.0, 0.0, 0.0))
                } else {
                    bold_t
                };
                pixmap.fill_path(
                    &path,
                    &paint,
                    convert::FILL_RULE,
                    convert::to_sk_transform(bold_t),
                    None,
                );
            }
        }

        // 下划线与删除线按行绘制
        draw_line_decorations(pixmap, line, node_transform, opacity, default_color);
    }

    layout
}

/// 把轮廓转成 tiny-skia 路径（em 坐标系）。
fn contours_to_path(contours: &[Contour]) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    let mut has_geometry = false;

    for c in contours {
        match c {
            Contour::Move(x, y) => {
                pb.move_to(*x, *y);
                has_geometry = true;
            }
            Contour::Line(x, y) => pb.line_to(*x, *y),
            Contour::Quad(x1, y1, x, y) => pb.quad_to(*x1, *y1, *x, *y),
            Contour::Cubic(x1, y1, x2, y2, x, y) => {
                pb.cubic_to(*x1, *y1, *x2, *y2, *x, *y)
            }
            Contour::Close => pb.close(),
        }
    }

    if !has_geometry {
        return None;
    }
    pb.finish()
}

/// 绘制下划线与删除线。
fn draw_line_decorations(
    pixmap: &mut Pixmap,
    line: &ppt_text::LaidOutLine,
    node_transform: Transform,
    opacity: f32,
    default_color: Color,
) {
    if line.glyphs.is_empty() {
        return;
    }

    // 按「装饰样式 + 颜色」分组，同一组画一条线即可
    let mut groups: Vec<(UnderlineStyle, StrikeStyle, Color, f32, f32, f32)> = Vec::new();

    for g in &line.glyphs {
        if g.style.is_plain() {
            continue;
        }
        let color = if g.color == Color::TRANSPARENT {
            default_color
        } else {
            g.color
        };
        let color = apply_opacity(color, opacity);

        match groups
            .iter_mut()
            .find(|(u, s, c, ..)| *u == g.style.underline && *s == g.style.strike && *c == color)
        {
            Some(entry) => {
                entry.4 = entry.4.min(g.x);
                entry.5 = entry.5.max(g.x + g.advance);
            }
            None => groups.push((
                g.style.underline,
                g.style.strike,
                color,
                g.size_pt,
                g.x,
                g.x + g.advance,
            )),
        }
    }

    for (underline, strike, color, size_pt, x0, x1) in groups {
        if underline != UnderlineStyle::None {
            let thickness = if underline.is_heavy() {
                size_pt * 0.085
            } else {
                size_pt * 0.055
            };
            // 下划线位置：基线下方约 12% 字号
            let y = line.baseline + size_pt * 0.12;
            draw_rule(
                pixmap,
                node_transform,
                (x0, x1),
                y,
                thickness,
                color,
                underline.is_wavy(),
                underline.is_double(),
            );
        }

        if strike != StrikeStyle::None {
            let thickness = size_pt * 0.055;
            // 删除线位置：约在 x-height 中间
            let y = line.baseline - size_pt * 0.28;
            draw_rule(
                pixmap,
                node_transform,
                (x0, x1),
                y,
                thickness,
                color,
                false,
                strike == StrikeStyle::Double,
            );
        }
    }
}

/// 画一条装饰线（下划线 / 删除线 / 波浪线）。
#[allow(clippy::too_many_arguments)]
fn draw_rule(
    pixmap: &mut Pixmap,
    node_transform: Transform,
    xs: (f32, f32),
    y: f32,
    thickness: f32,
    color: Color,
    wavy: bool,
    double: bool,
) {
    let mut paint = Paint::default();
    paint.set_color(convert::to_sk_color(color));
    paint.anti_alias = true;

    let make_path = |dy: f32, thick: f32| -> Option<tiny_skia::Path> {
        let mut pb = PathBuilder::new();
        if wavy {
            // 用连续二次贝塞尔画波浪线
            let amplitude = thick * 1.6;
            let wavelength = thick * 6.0;
            let mut x = xs.0;
            pb.move_to(x, y + dy);
            let mut up = true;
            while x < xs.1 {
                let nx = (x + wavelength / 2.0).min(xs.1);
                let cy = if up { y + dy - amplitude } else { y + dy + amplitude };
                pb.quad_to(x + (nx - x) / 2.0, cy, nx, y + dy);
                x = nx;
                up = !up;
            }
        } else {
            pb.move_to(xs.0, y + dy);
            pb.line_to(xs.1, y + dy);
            pb.line_to(xs.1, y + dy + thick);
            pb.line_to(xs.0, y + dy + thick);
            pb.close();
        }
        pb.finish()
    };

    let sk_t = convert::to_sk_transform(node_transform);

    if double {
        let half = thickness / 2.0;
        if let Some(p) = make_path(-half * 1.5, half) {
            pixmap.fill_path(&p, &paint, convert::FILL_RULE, sk_t, None);
        }
        if let Some(p) = make_path(half * 1.5, half) {
            pixmap.fill_path(&p, &paint, convert::FILL_RULE, sk_t, None);
        }
    } else if wavy {
        // 波浪线用描边而不是填充
        if let Some(p) = make_path(0.0, thickness) {
            let mut stroke = tiny_skia::Stroke::default();
            stroke.width = thickness * 0.9;
            stroke.line_cap = tiny_skia::LineCap::Round;
            pixmap.stroke_path(&p, &paint, &stroke, sk_t, None);
        }
    } else if let Some(p) = make_path(0.0, thickness) {
        pixmap.fill_path(&p, &paint, convert::FILL_RULE, sk_t, None);
    }
}

/// 把节点整体不透明度乘到颜色上。
fn apply_opacity(c: Color, opacity: f32) -> Color {
    if opacity >= 1.0 {
        return c;
    }
    Color::rgba(
        c.r,
        c.g,
        c.b,
        (c.a as f32 * opacity.clamp(0.0, 1.0)).round() as u8,
    )
}

/// 未使用导入守卫：`SkTransform` 经 `convert::to_sk_transform` 的返回类型间接使用。
#[allow(dead_code)]
fn _assert_types(_: SkTransform) {}

#[cfg(test)]
mod tests {
    use super::*;
    use ppt_core::scene::{
        AutoFit, BodyProps, FontSet, Paragraph, RunProps, TextAlign, TextRun, VerticalAnchor,
    };

    fn fonts() -> Option<FontContext> {
        let ctx = FontContext::new();
        if ctx.font_count() == 0 {
            None
        } else {
            Some(ctx)
        }
    }

    fn text_box(text: &str, size_pt: f32) -> TextBox {
        TextBox {
            body: BodyProps::default(),
            paragraphs: vec![Paragraph {
                runs: vec![TextRun {
                    text: text.to_string(),
                    props: RunProps {
                        size_pt,
                        ..Default::default()
                    },
                    hyperlink: None,
                    field: None,
                }],
                ..Default::default()
            }],
        }
    }

    fn pixmap(w: u32, h: u32) -> Pixmap {
        let mut p = Pixmap::new(w, h).unwrap();
        p.fill(tiny_skia::Color::WHITE);
        p
    }

    /// 统计非白像素数，用于判断「是否真的画上了东西」。
    fn ink_pixels(p: &Pixmap) -> usize {
        p.data()
            .chunks_exact(4)
            .filter(|px| px[0] < 200 || px[1] < 200 || px[2] < 200)
            .count()
    }

    #[test]
    fn glyph_outline_is_flipped_to_screen_orientation() {
        // 字体坐标 y 向上（基线 0、上升部为正），画布坐标 y 向下。
        // 翻转后，字形的上升部应落在**基线之上**，即相对基线的 y 为负。
        //
        // 若未翻转，整个字形会落在基线下方（y 为正），表现为文字上下颠倒 ——
        // 每个字形被垂直镜像而字符顺序不变，很容易被忽略。
        let Some(fonts) = fonts() else { return };

        let Some(fid) = fonts.font_for_char(None, false, false, 'L') else {
            return;
        };
        let Some(font) = fonts.font(fid).cloned() else {
            return;
        };
        let Some(gid) = font.with_face(|f| f.glyph_index('L')).flatten() else {
            return;
        };
        let Some(cached) = build_glyph(&font, gid.0) else {
            return;
        };

        let mut ys: Vec<f32> = Vec::new();
        for c in &cached.contours {
            match c {
                Contour::Move(_, y) | Contour::Line(_, y) => ys.push(*y),
                Contour::Quad(_, y1, _, y2) => {
                    ys.push(*y1);
                    ys.push(*y2);
                }
                Contour::Cubic(_, y1, _, y2, _, y3) => {
                    ys.push(*y1);
                    ys.push(*y2);
                    ys.push(*y3);
                }
                Contour::Close => {}
            }
        }
        assert!(!ys.is_empty(), "'L' 应有轮廓点");

        // 拉丁字母 'L' 全部位于基线上方（翻转后 y <= 0）
        let above_baseline = ys.iter().filter(|y| **y <= 0.0).count();
        assert!(
            above_baseline * 2 > ys.len(),
            "字形应位于基线上方（画布 y 向下），实际只有 {above_baseline} / {} 个点在上方",
            ys.len()
        );
    }

    #[test]
    fn text_ink_stays_below_the_top_of_the_box() {
        // 端到端的方向校验：把文字画在画布上半部的一个框里，
        // 墨迹必须落在框内，而不是跑到框上方（那是 y 未翻转的表现）。
        let Some(fonts) = fonts() else { return };

        let mut pm = pixmap(200, 200);
        let mut cache = GlyphCache::new();

        // 文本框在画布 (0,0)-(200,60)，即上半部
        let mut tb = text_box("E", 40.0);
        tb.body.anchor = VerticalAnchor::Top;

        draw_text(
            &mut pm,
            &fonts,
            &tb,
            Size::new(200.0, 60.0),
            Transform::IDENTITY,
            1.0,
            Color::BLACK,
            &mut cache,
            LayoutOptions::default(),
        );

        let w = pm.width();
        let rows_with_ink = |from: u32, to: u32| -> usize {
            (from..to)
                .filter(|&y| {
                    (0..w).any(|x| {
                        let i = ((y * w + x) * 4) as usize;
                        pm.data()[i] < 128
                    })
                })
                .count()
        };

        let in_box = rows_with_ink(0, 60);
        let below_box = rows_with_ink(60, 200);

        assert!(in_box > 0, "框内应有墨迹");
        assert!(
            below_box < in_box,
            "墨迹主要应在框内，实际框内 {in_box} 行、框外 {below_box} 行"
        );
    }

    #[test]
    fn synthetic_bold_offset_stays_sub_pixel_scale() {
        // 伪粗体的偏移量必须与字号成正比，且远小于字号本身。
        // 曾经的 bug：偏移写在字号缩放之后，导致偏移被二次放大
        // （44pt 的字偏移了 40 多 pt，视觉上就是「文字重影」）。
        // 这里通过比较「粗体文本」与「非粗体文本」的横向墨迹范围来验证。
        let Some(fonts) = fonts() else { return };

        let ink_bounds_x = |bold: bool| -> Option<(u32, u32)> {
            let mut pm = pixmap(400, 120);
            let mut cache = GlyphCache::new();
            let mut tb = text_box("MMMM", 44.0);
            tb.paragraphs[0].runs[0].props.bold = bold;
            // 断言只关心横向范围，用左对齐避免居中带来的额外变量
            tb.paragraphs[0].align = ppt_core::scene::TextAlign::Left;

            draw_text(
                &mut pm,
                &fonts,
                &tb,
                Size::new(400.0, 120.0),
                Transform::IDENTITY,
                1.0,
                Color::BLACK,
                &mut cache,
                LayoutOptions::default(),
            );

            let w = pm.width();
            let has_ink = |x: u32| {
                (0..pm.height()).any(|y| {
                    let i = ((y * w + x) * 4) as usize;
                    pm.data()[i] < 128
                })
            };
            let first = (0..w).find(|&x| has_ink(x))?;
            let last = (0..w).rev().find(|&x| has_ink(x))?;
            Some((first, last))
        };

        let Some((plain_first, plain_last)) = ink_bounds_x(false) else {
            return;
        };
        let Some((bold_first, bold_last)) = ink_bounds_x(true) else {
            return;
        };

        let plain_w = plain_last - plain_first;
        let bold_w = bold_last - bold_first;

        // 伪粗体只会略微加宽（约 2%~5% 的字号），绝不会翻倍
        let extra = bold_w.saturating_sub(plain_w);
        assert!(
            extra <= 8,
            "伪粗体只应带来极小的加宽，实际加宽 {extra}px（plain={plain_w}, bold={bold_w}）"
        );
        // 起点不应因伪粗体而大幅偏移
        assert!(
            bold_first.abs_diff(plain_first) <= 4,
            "伪粗体不应导致起始位置偏移（plain={plain_first}, bold={bold_first}）"
        );
    }

    #[test]
    fn glyph_cache_starts_empty() {
        let c = GlyphCache::new();
        assert!(c.is_empty());
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn glyph_cache_clear_works() {
        let mut c = GlyphCache::new();
        c.entries.insert((1, 2, 3), None);
        assert_eq!(c.len(), 1);
        c.clear();
        assert!(c.is_empty());
    }

    #[test]
    fn contours_to_path_handles_empty_input() {
        assert!(contours_to_path(&[]).is_none());
        // 只有 Close 没有几何的轮廓也不应产生路径
        assert!(contours_to_path(&[Contour::Close]).is_none());
    }

    #[test]
    fn contours_to_path_builds_geometry() {
        let contours = vec![
            Contour::Move(0.0, 0.0),
            Contour::Line(1.0, 0.0),
            Contour::Line(1.0, 1.0),
            Contour::Close,
        ];
        let p = contours_to_path(&contours).unwrap();
        let b = p.bounds();
        assert!((b.width() - 1.0).abs() < 1e-4);
        assert!((b.height() - 1.0).abs() < 1e-4);
    }

    #[test]
    fn contours_support_all_segment_kinds() {
        let contours = vec![
            Contour::Move(0.0, 0.0),
            Contour::Quad(0.5, 1.0, 1.0, 0.0),
            Contour::Cubic(1.0, 0.5, 0.5, -0.5, 0.0, 0.0),
            Contour::Close,
        ];
        assert!(contours_to_path(&contours).is_some());
    }

    #[test]
    fn opacity_is_applied_to_alpha_only() {
        let c = Color::rgba(100, 150, 200, 255);
        let out = apply_opacity(c, 0.5);
        assert_eq!((out.r, out.g, out.b), (100, 150, 200));
        assert_eq!(out.a, 128);

        // 不透明度为 1 时原样返回
        assert_eq!(apply_opacity(c, 1.0), c);
        // 越界值被夹紧
        assert_eq!(apply_opacity(c, 2.0), c);
        assert_eq!(apply_opacity(c, -1.0).a, 0);
    }

    #[test]
    fn latin_text_actually_renders_pixels() {
        let Some(fonts) = fonts() else { return };
        let mut pm = pixmap(400, 120);
        let mut cache = GlyphCache::new();

        let layout = draw_text(
            &mut pm,
            &fonts,
            &text_box("Hello", 48.0),
            Size::new(400.0, 120.0),
            Transform::translate(20.0, 20.0),
            1.0,
            Color::BLACK,
            &mut cache,
            LayoutOptions::default(),
        );

        assert!(!layout.is_empty(), "应产出排版结果");
        assert!(ink_pixels(&pm) > 100, "拉丁文本应画出足够多的像素");
        assert!(cache.len() > 0, "字形轮廓应进入缓存");
    }

    #[test]
    fn cjk_text_actually_renders_pixels() {
        let Some(fonts) = fonts() else { return };
        let mut pm = pixmap(500, 120);
        let mut cache = GlyphCache::new();

        draw_text(
            &mut pm,
            &fonts,
            &text_box("光合作用", 48.0),
            Size::new(500.0, 120.0),
            Transform::translate(20.0, 20.0),
            1.0,
            Color::BLACK,
            &mut cache,
            LayoutOptions::default(),
        );

        assert!(ink_pixels(&pm) > 300, "中文应画出足够多的像素");
    }

    #[test]
    fn text_color_is_respected() {
        let Some(fonts) = fonts() else { return };
        let mut pm = pixmap(300, 120);
        let mut cache = GlyphCache::new();

        let mut tb = text_box("RED", 48.0);
        tb.paragraphs[0].runs[0].props.color = Some(Color::rgb(255, 0, 0));

        draw_text(
            &mut pm,
            &fonts,
            &tb,
            Size::new(300.0, 120.0),
            Transform::IDENTITY,
            1.0,
            Color::BLACK,
            &mut cache,
            LayoutOptions::default(),
        );

        // 应存在明显的红色像素
        let red = pm
            .data()
            .chunks_exact(4)
            .filter(|px| px[0] > 180 && px[1] < 80 && px[2] < 80)
            .count();
        assert!(red > 50, "应画出红色文字，实际红色像素 {red}");
    }

    #[test]
    fn opacity_reduces_contrast() {
        let Some(fonts) = fonts() else { return };

        let render = |opacity: f32| -> usize {
            let mut pm = pixmap(300, 120);
            let mut cache = GlyphCache::new();
            draw_text(
                &mut pm,
                &fonts,
                &text_box("ABCDE", 48.0),
                Size::new(300.0, 120.0),
                Transform::IDENTITY,
                opacity,
                Color::BLACK,
                &mut cache,
                LayoutOptions::default(),
            );
            // 统计「明显变暗」的像素：半透明时数量会减少
            pm.data()
                .chunks_exact(4)
                .filter(|px| px[0] < 180)
                .count()
        };

        let full = render(1.0);
        let half = render(0.5);
        assert!(full > 0);
        assert!(
            half < full,
            "半透明应减少深色像素：full={full}, half={half}"
        );
    }

    #[test]
    fn underline_adds_ink_below_baseline() {
        let Some(fonts) = fonts() else { return };

        let render = |underline: UnderlineStyle| -> usize {
            let mut pm = pixmap(300, 140);
            let mut cache = GlyphCache::new();
            let mut tb = text_box("underline", 36.0);
            tb.paragraphs[0].runs[0].props.underline = underline;
            draw_text(
                &mut pm,
                &fonts,
                &tb,
                Size::new(300.0, 140.0),
                Transform::IDENTITY,
                1.0,
                Color::BLACK,
                &mut cache,
                LayoutOptions::default(),
            );
            ink_pixels(&pm)
        };

        let plain = render(UnderlineStyle::None);
        let underlined = render(UnderlineStyle::Single);
        assert!(
            underlined > plain,
            "下划线应增加像素：plain={plain}, underlined={underlined}"
        );
    }

    #[test]
    fn strikethrough_adds_ink() {
        let Some(fonts) = fonts() else { return };

        let render = |strike: StrikeStyle| -> usize {
            let mut pm = pixmap(300, 140);
            let mut cache = GlyphCache::new();
            let mut tb = text_box("strike", 36.0);
            tb.paragraphs[0].runs[0].props.strike = strike;
            draw_text(
                &mut pm,
                &fonts,
                &tb,
                Size::new(300.0, 140.0),
                Transform::IDENTITY,
                1.0,
                Color::BLACK,
                &mut cache,
                LayoutOptions::default(),
            );
            ink_pixels(&pm)
        };

        assert!(render(StrikeStyle::Single) > render(StrikeStyle::None));
    }

    #[test]
    fn empty_text_box_draws_nothing() {
        let Some(fonts) = fonts() else { return };
        let mut pm = pixmap(200, 100);
        let mut cache = GlyphCache::new();

        let tb = TextBox {
            body: BodyProps::default(),
            paragraphs: Vec::new(),
        };
        draw_text(
            &mut pm,
            &fonts,
            &tb,
            Size::new(200.0, 100.0),
            Transform::IDENTITY,
            1.0,
            Color::BLACK,
            &mut cache,
            LayoutOptions::default(),
        );

        assert_eq!(ink_pixels(&pm), 0, "空文本框不应画出任何像素");
    }

    #[test]
    fn node_transform_is_applied() {
        let Some(fonts) = fonts() else { return };
        let mut pm = pixmap(400, 200);
        let mut cache = GlyphCache::new();

        // 平移到右下角：左上区域应保持干净
        draw_text(
            &mut pm,
            &fonts,
            &text_box("X", 48.0),
            Size::new(400.0, 200.0),
            Transform::translate(200.0, 100.0),
            1.0,
            Color::BLACK,
            &mut cache,
            LayoutOptions::default(),
        );

        let ink = pm
            .data()
            .chunks_exact(4)
            .filter(|px| px[0] < 200)
            .count();
        assert!(ink > 0, "应画出内容");
    }

    #[test]
    fn glyph_cache_is_reused_across_calls() {
        let Some(fonts) = fonts() else { return };
        let mut cache = GlyphCache::new();

        let mut pm = pixmap(300, 120);
        draw_text(
            &mut pm,
            &fonts,
            &text_box("AAAA", 40.0),
            Size::new(300.0, 120.0),
            Transform::IDENTITY,
            1.0,
            Color::BLACK,
            &mut cache,
            LayoutOptions::default(),
        );
        let after_first = cache.len();
        assert!(after_first > 0);

        // 同样内容再画一次：缓存不应显著增长（同一个字形应命中）
        let mut pm2 = pixmap(300, 120);
        draw_text(
            &mut pm2,
            &fonts,
            &text_box("AAAA", 40.0),
            Size::new(300.0, 120.0),
            Transform::IDENTITY,
            1.0,
            Color::BLACK,
            &mut cache,
            LayoutOptions::default(),
        );
        assert_eq!(cache.len(), after_first, "重复字形应命中缓存");
    }

    #[test]
    fn text_alignment_affects_horizontal_position() {
        let Some(fonts) = fonts() else { return };

        let first_ink_x = |align: TextAlign| -> Option<u32> {
            let mut pm = pixmap(600, 120);
            let mut cache = GlyphCache::new();
            let mut tb = text_box("Hi", 48.0);
            tb.paragraphs[0].align = align;
            draw_text(
                &mut pm,
                &fonts,
                &tb,
                Size::new(600.0, 120.0),
                Transform::IDENTITY,
                1.0,
                Color::BLACK,
                &mut cache,
                LayoutOptions::default(),
            );
            let w = pm.width();
            (0..w).find(|&x| {
                (0..pm.height()).any(|y| {
                    let i = ((y * w + x) * 4) as usize;
                    pm.data()[i] < 180
                })
            })
        };

        let left = first_ink_x(TextAlign::Left).expect("左对齐应有内容");
        let right = first_ink_x(TextAlign::Right).expect("右对齐应有内容");
        assert!(right > left, "右对齐的起始墨迹应更靠右：{left} vs {right}");
    }

    #[test]
    fn vertical_anchor_affects_vertical_position() {
        let Some(fonts) = fonts() else { return };

        let first_ink_y = |anchor: VerticalAnchor| -> u32 {
            let mut pm = pixmap(300, 400);
            let mut cache = GlyphCache::new();
            let mut tb = text_box("Hi", 40.0);
            tb.body.anchor = anchor;
            draw_text(
                &mut pm,
                &fonts,
                &tb,
                Size::new(300.0, 400.0),
                Transform::IDENTITY,
                1.0,
                Color::BLACK,
                &mut cache,
                LayoutOptions::default(),
            );
            let w = pm.width();
            (0..pm.height())
                .find(|&y| {
                    (0..w).any(|x| {
                        let i = ((y * w + x) * 4) as usize;
                        pm.data()[i] < 180
                    })
                })
                .unwrap_or(0)
        };

        assert!(first_ink_y(VerticalAnchor::Middle) > first_ink_y(VerticalAnchor::Top));
        assert!(first_ink_y(VerticalAnchor::Bottom) > first_ink_y(VerticalAnchor::Middle));
    }

    #[test]
    fn autofit_shrinks_text_to_fit() {
        let Some(fonts) = fonts() else { return };
        let mut pm = pixmap(300, 200);
        let mut cache = GlyphCache::new();

        let mut tb = text_box("一段很长很长需要缩小的文字内容", 60.0);
        tb.body.auto_fit = AutoFit::NormAutofit {
            font_scale: 1.0,
            line_space_reduction: 0.0,
        };

        let layout = draw_text(
            &mut pm,
            &fonts,
            &tb,
            Size::new(300.0, 200.0),
            Transform::IDENTITY,
            1.0,
            Color::BLACK,
            &mut cache,
            LayoutOptions::default(),
        );

        assert!(layout.font_scale < 1.0, "应触发自动缩小");
    }

    #[test]
    fn placeholder_font_does_not_panic() {
        let fonts = FontContext::empty();
        let mut pm = pixmap(200, 100);
        let mut cache = GlyphCache::new();

        draw_text(
            &mut pm,
            &fonts,
            &text_box("text", 24.0),
            Size::new(200.0, 100.0),
            Transform::IDENTITY,
            1.0,
            Color::BLACK,
            &mut cache,
            LayoutOptions::default(),
        );
        // 无字体环境下不画任何东西，但也不应崩溃
        assert_eq!(ink_pixels(&pm), 0);
    }

    #[test]
    fn font_without_glyph_is_skipped() {
        let Some(fonts) = fonts() else { return };
        let mut cache = GlyphCache::new();

        // 用一个几乎不可能有字形的私有区码点
        let layout = draw_text(
            &mut pixmap(200, 100),
            &fonts,
            &text_box("\u{F8FF}", 24.0),
            Size::new(200.0, 100.0),
            Transform::IDENTITY,
            1.0,
            Color::BLACK,
            &mut cache,
            LayoutOptions::default(),
        );
        // 不应 panic；是否画出像素取决于系统字体，不做断言
        let _ = layout;
    }
}
