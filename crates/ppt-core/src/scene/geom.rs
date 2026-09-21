//! 几何基元与仿射变换。

use serde::{Deserialize, Serialize};

/// 点（单位：pt）。
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const ZERO: Point = Point { x: 0.0, y: 0.0 };

    #[inline]
    pub const fn new(x: f32, y: f32) -> Self {
        Point { x, y }
    }
}

/// 尺寸（单位：pt）。
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Size {
    pub w: f32,
    pub h: f32,
}

impl Size {
    pub const ZERO: Size = Size { w: 0.0, h: 0.0 };

    #[inline]
    pub const fn new(w: f32, h: f32) -> Self {
        Size { w, h }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }

    /// 宽高比；宽或高为 0 时返回 1.0，避免调用方除零。
    #[inline]
    pub fn aspect_ratio(&self) -> f32 {
        if self.h.abs() < f32::EPSILON {
            1.0
        } else {
            self.w / self.h
        }
    }
}

/// 轴对齐矩形（单位：pt）。
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const ZERO: Rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 0.0,
        h: 0.0,
    };

    #[inline]
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Rect { x, y, w, h }
    }

    #[inline]
    pub fn from_size(size: Size) -> Self {
        Rect::new(0.0, 0.0, size.w, size.h)
    }

    #[inline]
    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    #[inline]
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    #[inline]
    pub fn center(&self) -> Point {
        Point::new(self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    #[inline]
    pub fn size(&self) -> Size {
        Size::new(self.w, self.h)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }

    /// 向外扩张（负值为收缩）。
    #[inline]
    pub fn expand(&self, d: f32) -> Rect {
        Rect::new(self.x - d, self.y - d, self.w + 2.0 * d, self.h + 2.0 * d)
    }

    /// 按四边内缩。
    #[inline]
    pub fn inset(&self, insets: Insets) -> Rect {
        Rect::new(
            self.x + insets.left,
            self.y + insets.top,
            self.w - insets.left - insets.right,
            self.h - insets.top - insets.bottom,
        )
    }

    #[inline]
    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.x && p.x <= self.right() && p.y >= self.y && p.y <= self.bottom()
    }

    #[inline]
    pub fn intersect(&self, other: &Rect) -> Rect {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let r = self.right().min(other.right());
        let b = self.bottom().min(other.bottom());
        Rect::new(x, y, (r - x).max(0.0), (b - y).max(0.0))
    }

    #[inline]
    pub fn union(&self, other: &Rect) -> Rect {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let r = self.right().max(other.right());
        let b = self.bottom().max(other.bottom());
        Rect::new(x, y, r - x, b - y)
    }
}

/// 四边内边距（单位：pt）。
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Insets {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl Insets {
    #[inline]
    pub const fn uniform(v: f32) -> Self {
        Insets {
            left: v,
            top: v,
            right: v,
            bottom: v,
        }
    }

    #[inline]
    pub fn horizontal(&self) -> f32 {
        self.left + self.right
    }

    #[inline]
    pub fn vertical(&self) -> f32 {
        self.top + self.bottom
    }
}

/// 2D 仿射变换，列主序 6 元组 `[a, b, c, d, e, f]`：
///
/// ```text
/// x' = a*x + c*y + e
/// y' = b*x + d*y + f
/// ```
///
/// SceneGraph 中每个节点的变换都是**相对画布（幻灯片）空间的绝对变换**。
/// 组合形状的父子变换在解析阶段就被烘焙进子节点，
/// 这样渲染器只需 `set_transform(node.transform)`，无需在渲染时递归求积。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Transform {
    pub m: [f32; 6],
}

impl Default for Transform {
    fn default() -> Self {
        Transform::IDENTITY
    }
}

impl Transform {
    pub const IDENTITY: Transform = Transform {
        m: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
    };

    #[inline]
    pub const fn new(a: f32, b: f32, c: f32, d: f32, e: f32, f: f32) -> Self {
        Transform {
            m: [a, b, c, d, e, f],
        }
    }

    #[inline]
    pub fn translate(tx: f32, ty: f32) -> Self {
        Transform::new(1.0, 0.0, 0.0, 1.0, tx, ty)
    }

    #[inline]
    pub fn scale(sx: f32, sy: f32) -> Self {
        Transform::new(sx, 0.0, 0.0, sy, 0.0, 0.0)
    }

    /// 顺时针旋转（角度制）。与 OOXML `rot` 属性方向一致。
    #[inline]
    pub fn rotate_deg(deg: f32) -> Self {
        let r = deg.to_radians();
        let (s, c) = r.sin_cos();
        Transform::new(c, s, -s, c, 0.0, 0.0)
    }

    /// `self ∘ other`：先应用 `other`，再应用 `self`。
    #[inline]
    pub fn multiply(&self, other: &Transform) -> Transform {
        let [a1, b1, c1, d1, e1, f1] = self.m;
        let [a2, b2, c2, d2, e2, f2] = other.m;
        Transform::new(
            a1 * a2 + c1 * b2,
            b1 * a2 + d1 * b2,
            a1 * c2 + c1 * d2,
            b1 * c2 + d1 * d2,
            a1 * e2 + c1 * f2 + e1,
            b1 * e2 + d1 * f2 + f1,
        )
    }

    #[inline]
    pub fn apply(&self, p: Point) -> Point {
        let [a, b, c, d, e, f] = self.m;
        Point::new(a * p.x + c * p.y + e, b * p.x + d * p.y + f)
    }

    /// 仅应用线性部分（不含平移），用于变换方向向量。
    #[inline]
    pub fn apply_vector(&self, v: Point) -> Point {
        let [a, b, c, d, _, _] = self.m;
        Point::new(a * v.x + c * v.y, b * v.x + d * v.y)
    }

    /// 近似缩放系数（取两条基向量长度的平均值）。
    ///
    /// 用于需要「大致缩放比」的场景，如按显示尺寸决定图片解码降采样倍率。
    #[inline]
    pub fn mean_scale(&self) -> f32 {
        let [a, b, c, d, _, _] = self.m;
        let sx = (a * a + b * b).sqrt();
        let sy = (c * c + d * d).sqrt();
        (sx + sy) / 2.0
    }

    #[inline]
    pub fn is_identity(&self) -> bool {
        self.m == Transform::IDENTITY.m
    }

    /// 由 OOXML 形状变换（`a:xfrm`）构造绝对变换。
    ///
    /// OOXML 语义：
    /// - `off` 是形状左上角在父坐标系中的位置；
    /// - `ext` 是形状尺寸，形状局部坐标以 `(0,0)` 为左上角；
    /// - `rot` 是**绕形状中心**顺时针旋转的角度（度）；
    /// - `flipH` / `flipV` 在旋转之前应用到局部坐标。
    pub fn from_ooxml(
        offset: Point,
        extent: Size,
        rot_deg: f32,
        flip_h: bool,
        flip_v: bool,
    ) -> Transform {
        let cx = extent.w / 2.0;
        let cy = extent.h / 2.0;
        // 把局部原点平移到中心 -> 翻转 -> 旋转 -> 移回并按 off 定位
        let mut m = Transform::translate(offset.x + cx, offset.y + cy);
        if rot_deg.abs() > f32::EPSILON {
            m = m.multiply(&Transform::rotate_deg(rot_deg));
        }
        if flip_h || flip_v {
            m = m.multiply(&Transform::scale(
                if flip_h { -1.0 } else { 1.0 },
                if flip_v { -1.0 } else { 1.0 },
            ));
        }
        m.multiply(&Transform::translate(-cx, -cy))
    }

    /// 关于矩形中心做水平/垂直镜像。
    ///
    /// 就是 [`Transform::from_ooxml`] 去掉平移和旋转之后剩下的那部分。
    ///
    /// **用途：把形状自身的翻转从文字上抵消掉。**
    /// PowerPoint 翻转形状（`flipH` / `flipV`）时，形状镜像、**文字仍然正着**
    /// （微软文档原话：「文本在翻转的对象里不会被自动翻转」）。
    /// 镜像轴是形状自己的矩形中心，而文字框就在这个矩形里，
    /// 所以 `形状变换 ∘ 这个镜像` 之后，文字的位置一点没动，
    /// 只有字形朝向回到可读状态。
    ///
    /// 没有翻转时返回单位阵。
    pub fn mirror_about_center(extent: Size, flip_h: bool, flip_v: bool) -> Transform {
        let cx = extent.w / 2.0;
        let cy = extent.h / 2.0;
        Transform::translate(cx, cy)
            .multiply(&Transform::scale(
                if flip_h { -1.0 } else { 1.0 },
                if flip_v { -1.0 } else { 1.0 },
            ))
            .multiply(&Transform::translate(-cx, -cy))
    }

    /// 这个变换是不是**镜像**的（线性部分行列式为负）。
    ///
    /// 平移和旋转都会让它保持正：只有奇数次翻转才会翻符号。
    /// 所以「形状自己翻了一次、外层组合又翻了一次」得到的仍是正 —— 负负得正，
    /// 那种情况文字本来就不需要抵消。
    #[inline]
    pub fn is_mirrored(&self) -> bool {
        let [a, b, c, d, _, _] = self.m;
        a * d - b * c < 0.0
    }

    /// 给文字用的那份变换：把镜像抵消掉，**位置一动不动**。
    ///
    /// # 为什么不能只看形状自己的 `flipH`
    ///
    /// PowerPoint 的规矩是「文本在翻转的对象里不会被自动翻转」，
    /// 而翻转可能来自形状自身（`a:xfrm/@flipH`），也可能来自**外层的组合**
    /// （`p:grpSp` 的 `a:xfrm/@flipH`）—— 后者当初漏了：
    /// 组合里的卡片整组文字全都反写。所以判据用**最终矩阵**而不是局部标志：
    /// 镜像了才需要抵消，而镜像了这件事看行列式的符号就知道。
    ///
    /// # 为什么要在两个候选里挑
    ///
    /// 抵消镜像用的是「关于文字框中心的镜像」。水平轴和垂直轴都能让镜像消失，
    /// 两者恰好差一个 180° 旋转 —— 选错的后果是文字上下颠倒。
    /// 所以直接量一下**哪个候选让文字更接近正立**（基线方向偏离 0° 更小）：
    /// 形状转了 90° 时，文字本来就该跟着转 90°，不能把它掰回正立。
    ///
    /// 没有镜像时返回 `None`（绝大多数节点都是这种）。
    pub fn text_unmirrored(&self, extent: Size) -> Option<Transform> {
        if !self.is_mirrored() {
            return None;
        }
        let horizontal = self.multiply(&Transform::mirror_about_center(extent, true, false));
        let vertical = self.multiply(&Transform::mirror_about_center(extent, false, true));
        let lean = |t: &Transform| t.baseline_angle_deg().abs();
        Some(if lean(&horizontal) <= lean(&vertical) {
            horizontal
        } else {
            vertical
        })
    }

    /// 文字基线方向的偏角（度，顺时针为正，落在 -180~180）。
    ///
    /// 只对「已经消掉镜像」的变换有意义 —— 带镜像时基线方向本身反着，
    /// 这个角度没有意义。名字里带 baseline 是因为它量的不是矩阵的分解结果，
    /// 而是文字基线朝着哪儿。
    #[inline]
    fn baseline_angle_deg(&self) -> f32 {
        let [a, b, _, _, _, _] = self.m;
        // 基线方向就是局部 x 轴（1, 0）映射过去的方向
        b.atan2(a).to_degrees()
    }

    /// 组合形状的「子坐标系 → 组合局部坐标系」映射。
    ///
    /// 组合形状的 `chOff`/`chExt` 定义了子元素所在坐标系的原点与尺寸，
    /// 需要把它线性映射到组合自身的局部矩形 `(0,0,extent)` 上。
    pub fn child_space_to_local(child_off: Point, child_ext: Size, extent: Size) -> Transform {
        let sx = if child_ext.w.abs() < f32::EPSILON {
            1.0
        } else {
            extent.w / child_ext.w
        };
        let sy = if child_ext.h.abs() < f32::EPSILON {
            1.0
        } else {
            extent.h / child_ext.h
        };
        Transform::scale(sx, sy).multiply(&Transform::translate(-child_off.x, -child_off.y))
    }
}

/// 通用路径几何（局部坐标）。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PathGeometry {
    pub subpaths: Vec<SubPath>,
    /// 文本框在形状内的区域（来自 `custGeom` 的 `a:rect` / 预设几何的 text rect）。
    /// `None` 表示使用形状包围盒。
    pub text_rect: Option<Rect>,
    /// 形状包围盒（局部坐标，通常为 `(0,0,extent)`）。
    pub bbox: Rect,
}

impl PathGeometry {
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.subpaths.is_empty()
    }

    pub fn rect(bbox: Rect) -> PathGeometry {
        PathGeometry {
            subpaths: vec![SubPath {
                start: Point::new(bbox.x, bbox.y),
                segments: vec![
                    PathSegment::Line(Point::new(bbox.right(), bbox.y)),
                    PathSegment::Line(Point::new(bbox.right(), bbox.bottom())),
                    PathSegment::Line(Point::new(bbox.x, bbox.bottom())),
                ],
                closed: true,
            }],
            text_rect: None,
            bbox,
        }
    }
}

/// 一条子路径。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubPath {
    pub start: Point,
    pub segments: Vec<PathSegment>,
    pub closed: bool,
}

/// 路径段。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum PathSegment {
    Line(Point),
    Cubic {
        c1: Point,
        c2: Point,
        to: Point,
    },
    Quad {
        c: Point,
        to: Point,
    },
    /// 椭圆弧（OOXML `arcTo` 展开后使用）。
    Arc {
        rx: f32,
        ry: f32,
        x_axis_rotation_deg: f32,
        large_arc: bool,
        sweep: bool,
        to: Point,
    },
    Close,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: Point, b: Point) {
        assert!((a.x - b.x).abs() < 1e-3, "x: {} vs {}", a.x, b.x);
        assert!((a.y - b.y).abs() < 1e-3, "y: {} vs {}", a.y, b.y);
    }

    #[test]
    fn identity_is_noop() {
        let p = Point::new(3.0, 7.0);
        approx(Transform::IDENTITY.apply(p), p);
    }

    #[test]
    fn multiply_order_is_other_first() {
        // 先平移 (10,0) 再缩放 2 倍 -> (20, 0)
        let m = Transform::scale(2.0, 2.0).multiply(&Transform::translate(10.0, 0.0));
        approx(m.apply(Point::ZERO), Point::new(20.0, 0.0));
    }

    #[test]
    fn rotate_90_clockwise() {
        // 顺时针 90 度：(1,0) -> (0,1)
        let m = Transform::rotate_deg(90.0);
        approx(m.apply(Point::new(1.0, 0.0)), Point::new(0.0, 1.0));
    }

    #[test]
    fn ooxml_transform_no_rotation_places_shape() {
        let m = Transform::from_ooxml(
            Point::new(100.0, 50.0),
            Size::new(200.0, 100.0),
            0.0,
            false,
            false,
        );
        // 局部 (0,0) 应映射到 off
        approx(m.apply(Point::ZERO), Point::new(100.0, 50.0));
        // 局部右下角应映射到 off + extent
        approx(
            m.apply(Point::new(200.0, 100.0)),
            Point::new(300.0, 150.0),
        );
    }

    #[test]
    fn ooxml_transform_rotates_about_center() {
        let m = Transform::from_ooxml(
            Point::new(0.0, 0.0),
            Size::new(100.0, 100.0),
            90.0,
            false,
            false,
        );
        // 中心点不动
        approx(m.apply(Point::new(50.0, 50.0)), Point::new(50.0, 50.0));
    }

    #[test]
    fn flip_h_mirrors_about_center() {
        let m = Transform::from_ooxml(
            Point::new(0.0, 0.0),
            Size::new(100.0, 50.0),
            0.0,
            true,
            false,
        );
        // 左上角翻到右上角
        approx(m.apply(Point::ZERO), Point::new(100.0, 0.0));
    }

    #[test]
    fn mirror_about_center_cancels_the_flip() {
        let extent = Size::new(100.0, 50.0);
        let flip = Transform::from_ooxml(Point::new(30.0, 20.0), extent, 0.0, true, true);
        let unflipped =
            flip.multiply(&Transform::mirror_about_center(extent, true, true));
        let plain = Transform::from_ooxml(Point::new(30.0, 20.0), extent, 0.0, false, false);
        // 抵消之后和「压根没翻转」是同一个变换
        for p in [Point::ZERO, Point::new(100.0, 0.0), Point::new(37.0, 12.0)] {
            approx(unflipped.apply(p), plain.apply(p));
        }
    }

    #[test]
    fn mirror_about_center_is_identity_without_flip() {
        let m = Transform::mirror_about_center(Size::new(80.0, 40.0), false, false);
        assert!(m.is_identity());
    }

    #[test]
    fn is_mirrored_only_for_an_odd_number_of_flips() {
        let extent = Size::new(80.0, 40.0);
        let at = Point::new(10.0, 20.0);
        let of = |fh, fv| Transform::from_ooxml(at, extent, 0.0, fh, fv);
        assert!(!of(false, false).is_mirrored());
        assert!(of(true, false).is_mirrored());
        assert!(of(false, true).is_mirrored());
        // 两边都翻等于转 180°，文字本来就正着
        assert!(!of(true, true).is_mirrored());
        // 旋转不改变符号
        assert!(!Transform::from_ooxml(at, extent, 137.0, false, false).is_mirrored());
    }

    #[test]
    fn text_unmirrored_is_none_for_a_proper_transform() {
        let extent = Size::new(80.0, 40.0);
        let m = Transform::from_ooxml(Point::new(10.0, 20.0), extent, 60.0, false, false);
        assert!(m.text_unmirrored(extent).is_none());
    }

    /// 文字那份变换：镜像没了，**框的位置一动不动**。
    #[test]
    fn text_unmirrored_drops_the_mirror_and_keeps_the_box() {
        let extent = Size::new(80.0, 40.0);
        for (flip_h, flip_v) in [(true, false), (false, true)] {
            let m = Transform::from_ooxml(
                Point::new(10.0, 20.0),
                extent,
                0.0,
                flip_h,
                flip_v,
            );
            let t = m
                .text_unmirrored(extent)
                .expect("镜像了就该有一份文字变换");
            assert!(!t.is_mirrored(), "文字变换不该还是镜像的");

            // 框作为「一块矩形」一点没动。
            //
            // 注意只到矩形这一层：**框里**的点本来就该左右对调
            // （局部先镜像一次，再被形状的镜像映回去，读起来才是正的）。
            let corners = |tf: &Transform| {
                let mut xs = Vec::new();
                let mut ys = Vec::new();
                for p in [
                    Point::ZERO,
                    Point::new(extent.w, 0.0),
                    Point::new(0.0, extent.h),
                    Point::new(extent.w, extent.h),
                ] {
                    let q = tf.apply(p);
                    xs.push(q.x);
                    ys.push(q.y);
                }
                let lo = |v: &Vec<f32>| v.iter().cloned().fold(f32::INFINITY, f32::min);
                let hi = |v: &Vec<f32>| v.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
                (lo(&xs), hi(&xs), lo(&ys), hi(&ys))
            };
            let (a, b) = (corners(&m), corners(&t));
            assert!((a.0 - b.0).abs() < 0.01 && (a.1 - b.1).abs() < 0.01);
            assert!((a.2 - b.2).abs() < 0.01 && (a.3 - b.3).abs() < 0.01);

            // 而且文字是正立的：基线方向回到 +x
            let [ca, cb, _, _, _, _] = t.m;
            let deg = cb.atan2(ca).to_degrees();
            assert!(deg.abs() < 0.01, "文字应当正立，实际基线 {deg}°");
        }
    }

    /// 形状转了 90° 又翻转时，文字要跟着转，不能被掰回正立。
    #[test]
    fn text_unmirrored_keeps_the_shape_rotation() {
        let extent = Size::new(80.0, 40.0);
        let m = Transform::from_ooxml(Point::ZERO, extent, 90.0, true, false);
        let t = m.text_unmirrored(extent).expect("镜像了就该有一份文字变换");
        assert!(!t.is_mirrored());
        let [a, b, _, _, _, _] = t.m;
        let deg = b.atan2(a).to_degrees().abs();
        assert!(
            (deg - 90.0).abs() < 1.0,
            "基线应当保持 90°（跟着形状转），实际 {deg}°"
        );
    }

    #[test]
    fn child_space_maps_onto_extent() {
        // 子坐标系 (100,100)-(300,200) 映射到局部 (0,0)-(200,100)
        let m = Transform::child_space_to_local(
            Point::new(100.0, 100.0),
            Size::new(200.0, 100.0),
            Size::new(200.0, 100.0),
        );
        approx(m.apply(Point::new(100.0, 100.0)), Point::ZERO);
        approx(
            m.apply(Point::new(300.0, 200.0)),
            Point::new(200.0, 100.0),
        );
    }

    #[test]
    fn child_space_scales_to_different_extent() {
        // 子坐标系 100x100 映射到局部 50x200
        let m = Transform::child_space_to_local(
            Point::ZERO,
            Size::new(100.0, 100.0),
            Size::new(50.0, 200.0),
        );
        approx(m.apply(Point::new(100.0, 100.0)), Point::new(50.0, 200.0));
    }

    #[test]
    fn rect_helpers() {
        let r = Rect::new(10.0, 10.0, 100.0, 50.0);
        assert_eq!(r.right(), 110.0);
        assert_eq!(r.bottom(), 60.0);
        assert!(r.contains(Point::new(50.0, 30.0)));
        assert!(!r.contains(Point::new(5.0, 30.0)));
        assert_eq!(r.inset(Insets::uniform(10.0)), Rect::new(20.0, 20.0, 80.0, 30.0));
    }

    #[test]
    fn mean_scale_of_pure_scale() {
        let m = Transform::scale(3.0, 3.0);
        assert!((m.mean_scale() - 3.0).abs() < 1e-4);
    }
}
