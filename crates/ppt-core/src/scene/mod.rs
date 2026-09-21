//! SceneGraph —— 幻灯片与格式无关的中间表示。
//!
//! # 坐标约定
//!
//! - 所有长度单位为**点（pt）**，见 [`crate::units`]。
//! - 每个 [`Node`] 的 `transform` 是**相对幻灯片画布空间的绝对仿射变换**；
//!   节点的 `geometry` 描述的是**局部坐标**（通常以 `(0,0)` 为左上角）。
//!   组合形状的父子变换在解析阶段就被烘焙进子节点，渲染时无需递归求积。
//! - 自定义几何（`custGeom`）的路径坐标已在解析阶段归一化到形状局部空间。
//!
//! # 为什么要有这一层
//!
//! 格式解析（OOXML / PDF / 未来的二进制 PPT）与光栅化分离，
//! 好处有三：渲染器只面对一种数据、可以独立做视觉回归测试、
//! 后续新增格式不必改动渲染管线。

pub mod geom;
pub mod paint;
pub mod table;
pub mod text;

pub use geom::{Insets, PathGeometry, PathSegment, Point, Rect, Size, SubPath, Transform};
pub use paint::{
    Color, DashStyle, Effects, Fill, Glow, GradientFill, GradientFlip, GradientKind, GradientStop,
    ImageCompression, ImageFill, ImageRef, ImageTile, InnerShadow, LineCap, LineEnd, LineEndKind,
    LineJoin,
    OuterShadow, PatternFill, PenAlign, RectAlign, Reflection, RelativeRect, Stroke, StrokeFill,
    TileAlign,
};
pub use table::{BorderLine, Table, TableBorders, TableCell};
pub use text::{
    AutoFit, BodyProps, Bullet, BulletKind, FieldKind, FontAlign, FontSet, Hyperlink,
    HyperlinkTarget, LineSpacing, Paragraph, RunProps, StrikeStyle, TabAlign, TabStop, TextAlign,
    TextBox, TextCaps, TextColumn, TextDirection, TextRun, TextWrap, UnderlineStyle,
    VerticalAnchor,
};

use serde::{Deserialize, Serialize};

/// 一张幻灯片。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scene {
    /// 画布尺寸（pt）。
    pub size_pt: Size,
    pub background: SceneBackground,
    /// 顶层节点，按 z 序（先绘制的在前）。
    pub nodes: Vec<Node>,
    /// 演讲者备注。
    pub notes: Option<String>,
    /// 幻灯片标题（用于缩略图与导航）。
    pub title: Option<String>,
    /// 解析期间的降级与告警信息。
    ///
    /// 例如「SmartArt 已降级为占位」「图表暂不支持，已提取文本」。
    /// 这些信息会在 UI 上以非打扰的方式提示，并写入诊断日志。
    pub warnings: Vec<String>,
    /// 放映时的动画播放序列（点击推进）。
    ///
    /// 空表示这一页没有动画：点一下直接翻页。
    pub anim: AnimSequence,
    /// 从**上一页**切到本页时的转场效果（`p:transition`）。
    ///
    /// 按 OOXML 的规定，转场写在「进入的那一页」上。
    pub transition: Transition,
}

impl Scene {
    pub fn new(size_pt: Size) -> Scene {
        Scene {
            size_pt,
            background: SceneBackground::None,
            nodes: Vec::new(),
            notes: None,
            title: None,
            warnings: Vec::new(),
            anim: AnimSequence::default(),
            transition: Transition::default(),
        }
    }

    /// 空的错误占位场景，用于单页解析失败时的降级呈现。
    pub fn error_placeholder(size_pt: Size, message: impl Into<String>) -> Scene {
        Scene {
            size_pt,
            background: SceneBackground::Solid(Color::WHITE),
            nodes: Vec::new(),
            notes: None,
            title: None,
            warnings: vec![message.into()],
            anim: AnimSequence::default(),
            transition: Transition::default(),
        }
    }

    /// 本页动画总步数。
    #[inline]
    pub fn anim_step_count(&self) -> usize {
        self.anim.steps.len()
    }

    /// 把动画序列里的步号写回节点与段落。
    ///
    /// 时序解析（`ppt-format-pptx::timing`）只关心「第 N 步动了哪些 spid」，
    /// 落到 SceneGraph 上才是在这里做的 —— 这样渲染器只需要读 `build`，
    /// 不必认识 `p:timing` 那一套 XML。
    pub fn apply_anim_sequence(&mut self) {
        for (index, step) in self.anim.steps.iter().enumerate() {
            // 步号从 1 起：0 是「一步都没推进」的初始状态
            let step_no = (index + 1) as u32;
            for target in &step.targets {
                if step.changes_visibility() {
                    // 出现 / 消失：写进构建步
                    let build = if step.is_exit() {
                        Build {
                            appear: None,
                            disappear: Some(step_no),
                        }
                    } else {
                        Build {
                            appear: Some(step_no),
                            disappear: None,
                        }
                    };
                    apply_build(&mut self.nodes, target, build);
                } else {
                    // 强调：不改可见性，但留下一个**持续**的形变
                    apply_emphasis(
                        &mut self.nodes,
                        target,
                        EmphasisStep {
                            step: step_no,
                            to: target.to,
                        },
                    );
                }
            }
        }
    }
}

/// 把构建信息写到命中的节点上。
///
/// 组合形状里的目标要递归找：`spid` 是全局唯一的，但形状可能嵌在
/// 任意深度的组合里。
fn apply_build(nodes: &mut [Node], target: &AnimTarget, build: Build) {
    for node in nodes.iter_mut() {
        if node.shape_id == Some(target.shape_id) {
            match target.para_range {
                None => node.build = build,
                Some((st, end)) => {
                    // 段落级：只动指定段落，整个形状仍然常驻
                    let tb = node.text.as_mut();
                    if let Some(tb) = tb {
                        for (i, p) in tb.paragraphs.iter_mut().enumerate() {
                            let i = i as u32;
                            if i >= st && i <= end {
                                p.build = build;
                            }
                        }
                    }
                }
            }
        }
        apply_build(&mut node.children, target, build);
    }
}

/// 把强调动画的永久形变写到命中的节点上。
///
/// 段落级的强调（只让某一段变色/放大）在这里按整框处理 ——
/// 逐段的持久形变要连排版一起改，收益远小于代价。
fn apply_emphasis(nodes: &mut [Node], target: &AnimTarget, step: EmphasisStep) {
    for node in nodes.iter_mut() {
        if node.shape_id == Some(target.shape_id) {
            node.emphasis.push(step);
        }
        apply_emphasis(&mut node.children, target, step);
    }
}

impl Scene {
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// 按课件里的形状 id 找它在画布空间的包围盒。
    ///
    /// 触发器动画要用它：「点这个按钮才播第 3 步」——前端得知道按钮在哪儿，
    /// 才判断得出这一下点击该不该触发那一步。
    pub fn node_bounds(&self, shape_id: u32) -> Option<Rect> {
        self.walk()
            .find(|n| n.shape_id == Some(shape_id))
            .map(Node::canvas_bounds)
            .filter(|r| !r.is_empty())
    }

    /// 深度优先遍历所有节点（含子节点）。
    pub fn walk(&self) -> SceneWalker<'_> {
        SceneWalker {
            stack: self.nodes.iter().rev().collect(),
        }
    }

    /// 收集所有可点击的超链接区域（用于放映时的点击跳转）。
    ///
    /// 两类来源：
    ///
    /// 1. **形状级链接**（含 PPT 的「动作按钮」）：热区是整个形状的包围盒。
    ///    这与 PowerPoint 一致 —— 按钮整个可点，而不是只有文字可点。
    /// 2. **文字链接**（`a:rPr/a:hlinkClick`）：热区退化为**整个文本框**。
    ///    逐 run 的矩形属于排版层（`ppt-text`）的知识，SceneGraph 拿不到；
    ///    为避免把排版反向引进来，这里只在「该文本框内所有链接指向同一目标」
    ///    时才给热区 —— 一个框里挂了多个不同链接时宁可不给，
    ///    否则老师点到哪儿都跳同一个地方，比没有更糟。
    pub fn link_hotspots(&self) -> Vec<LinkHotspot> {
        let mut out = Vec::new();
        for node in self.walk() {
            // 热区取形状在画布空间下的轴对齐包围盒
            let bbox = node.canvas_bounds();
            if bbox.is_empty() {
                continue;
            }

            if let Some(link) = &node.hyperlink {
                out.push(LinkHotspot {
                    rect: bbox,
                    link: link.clone(),
                });
                continue;
            }

            if let Some(link) = node.text.as_ref().and_then(sole_text_link) {
                out.push(LinkHotspot { rect: bbox, link });
            }
        }
        out
    }

    /// 收集页内所有可播放的媒体热区。
    ///
    /// 与 [`Scene::link_hotspots`] 同构：热区取形状在画布空间的包围盒，
    /// 前端据此把播放器叠在正确的位置上。
    pub fn media_hotspots(&self) -> Vec<MediaHotspot> {
        let mut out = Vec::new();
        for node in self.walk() {
            let Some(media) = &node.media else { continue };
            let rect = node.canvas_bounds();
            if rect.is_empty() {
                continue;
            }
            out.push(MediaHotspot {
                rect,
                media: media.clone(),
            });
        }
        out
    }

    #[inline]
    pub fn push_warning(&mut self, msg: impl Into<String>) {
        // 同一类降级信息可能重复出现上百次，去重以免日志淹没
        let msg = msg.into();
        if !self.warnings.contains(&msg) {
            self.warnings.push(msg);
        }
    }
}

/// 幻灯片背景。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub enum SceneBackground {
    #[default]
    None,
    Solid(Color),
    Fill(Fill),
}

impl SceneBackground {
    /// 主色，供需要单色的降级路径使用（如先铺一层底色）。
    pub fn primary_color(&self) -> Option<Color> {
        match self {
            SceneBackground::None => None,
            SceneBackground::Solid(c) => Some(*c),
            SceneBackground::Fill(f) => match f {
                Fill::Solid(c) => Some(*c),
                Fill::Gradient(g) => g.stops.first().map(|s| s.color),
                _ => None,
            },
        }
    }
}

/// 可直接点击的链接热区。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkHotspot {
    pub rect: Rect,
    pub link: Hyperlink,
}

/// 可直接播放的媒体热区（视频/音频所在形状的画布包围盒）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaHotspot {
    pub rect: Rect,
    pub media: MediaRef,
}

/// 若文本框内所有链接都指向同一目标，返回该链接；否则返回 `None`。
///
/// 用来给「文字链接」造一个整框热区，见 [`Scene::link_hotspots`]。
fn sole_text_link(tb: &TextBox) -> Option<Hyperlink> {
    let mut found: Option<&Hyperlink> = None;
    for para in &tb.paragraphs {
        for run in &para.runs {
            let Some(link) = &run.hyperlink else { continue };
            match found {
                None => found = Some(link),
                // 同框内出现不同目标：不给整框热区，避免点哪儿都跳同一处
                Some(prev) if prev.target != link.target => return None,
                _ => {}
            }
        }
    }
    found.cloned()
}

/// 场景图节点。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    /// 解析期生成的稳定标识（用于调试与快照测试）。
    pub id: String,
    /// 形状名称（OOXML `p:cNvPr/@name`），便于定位问题。
    pub name: Option<String>,
    /// 局部坐标 → 画布坐标的绝对变换。
    pub transform: Transform,
    /// 整体不透明度，`0.0..=1.0`。
    pub opacity: f32,
    pub geometry: Geometry,
    pub fill: Fill,
    pub stroke: Option<Stroke>,
    pub effects: Effects,
    /// 形状内文本（可能为空）。
    pub text: Option<TextBox>,
    /// 子节点（组合形状）。
    pub children: Vec<Node>,
    pub hyperlink: Option<Hyperlink>,
    /// 页内嵌媒体（视频/音频）。
    ///
    /// 只记录部件名与播放参数：媒体字节按需解压、由前端播放器消费，
    /// 和图片一样**不在解析阶段读入**。
    pub media: Option<MediaRef>,
    /// `hidden="1"` 或不可见占位符。
    pub hidden: bool,
    /// 课件里的形状 id（`p:cNvPr/@id`）。
    ///
    /// 动画时序（`p:timing`）用 `p:spTgt/@spid` 指向目标，指的就是它。
    /// 解析器自己生成的 `id` 是给人看的，不稳定，所以另存一份。
    pub shape_id: Option<u32>,
    /// 动画构建步：这一块在第几步出现、第几步消失。
    pub build: Build,
    /// 强调动画留下的永久形变，按步号。
    pub emphasis: Vec<EmphasisStep>,
    /// 用于渐变、阴影按形状尺寸计算的包围盒；`None` 时由几何推导。
    pub local_bbox: Option<Rect>,
    /// 文字专用的变换；`None` 表示和 [`Node::transform`] 一样。
    ///
    /// # 为什么文字要单独一份
    ///
    /// PowerPoint 翻转形状（`flipH` / `flipV`）时，**形状镜像、文字仍然正着**
    /// —— 微软文档原话是「文本在翻转的对象里不会被自动翻转」。
    /// 我们的 `transform` 把翻转一并带在了文字上，文字就变成镜像的了
    /// （「REPORT」会渲染成反写的）。所以最终变换带镜像时，解析阶段
    /// 另存一份「抵消掉这次镜像」的变换给文字用，
    /// 见 [`Transform::text_unmirrored`]。
    ///
    /// 判据是**最终矩阵**，所以形状自身的翻转和外层组合
    /// （`p:grpSp` 的 `a:xfrm/@flipH`）的翻转都会处理到；
    /// 两层各翻一次是负负得正，那时两份变换相同。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_transform: Option<Transform>,
}

/// 动画「构建步」：在第几步出现、第几步消失。
///
/// 步号从 **1** 起 —— 0 表示「一步都还没推进」的初始状态。
/// 同一个步号的对象一起出现，对应 PowerPoint 的「与上一动画同时」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Build {
    /// 出现步；`None` 表示首屏就在。
    pub appear: Option<u32>,
    /// 消失步；`None` 表示一直保留。
    pub disappear: Option<u32>,
}

/// 动画播放状态：**位掩码**，第 i 位为 1 表示「第 i+1 步已经播过」。
///
/// # 为什么不是一个「推进到第几步」的数
///
/// 因为存在**触发器动画**（`p:cond evt="onClick"` + 指定形状）：
/// 老师可能先点了按钮播掉第 3 步，再点空白播第 1 步。
/// 单个数表达不了这种「跳着播」的状态，掩码可以。
/// 掩码还顺便成了渲染缓存的键 —— 两个不同的播放状态不会互相串图。
///
/// 超过 64 步的课件在现实中不存在；真出现时高位步号会挤在第 64 位上
/// （一起播），属于「不完美但不会卡死」的降级。
pub type PlayState = u64;

/// 全部播完（静态导出、缩略图、以及「这一页讲完了」的样子）。
pub const PLAY_ALL: PlayState = u64::MAX;

/// 步号（从 1 起）→ 掩码里的那一位。
#[inline]
pub fn step_bit(step: u32) -> PlayState {
    1u64 << step.saturating_sub(1).min(63)
}

/// 「前 `count` 步依次播完」的线性状态。
#[inline]
pub fn linear_state(count: usize) -> PlayState {
    if count >= 64 {
        PLAY_ALL
    } else {
        (1u64 << count) - 1
    }
}

impl Build {
    /// 首屏就在、且永不消失。
    pub const ALWAYS: Build = Build {
        appear: None,
        disappear: None,
    };

    /// 在播放状态 `state` 下是否可见。
    #[inline]
    pub fn visible_in(&self, state: PlayState) -> bool {
        if let Some(a) = self.appear {
            if state & step_bit(a) == 0 {
                return false;
            }
        }
        if let Some(d) = self.disappear {
            if state & step_bit(d) != 0 {
                return false;
            }
        }
        true
    }

    /// 是否参与了动画。
    #[inline]
    pub fn is_animated(&self) -> bool {
        self.appear.is_some() || self.disappear.is_some()
    }
}

/// 一步动画的触发方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum StepTrigger {
    /// 单击时（`p:cond delay="indefinite"` 的那一组）：点页面上任意位置都算。
    #[default]
    OnClick,
    /// 必须点到指定形状才触发（`p:cond evt="onClick"` + `p:tgtEl/p:spTgt`）。
    ///
    /// 这是课件里「点按钮才翻出答案」那种交互；点别处**不消耗**这一步。
    OnShape(u32),
    /// 上一动画之后自动接续播放，不需要点击。
    AfterPrevious,
}

/// 一步动画的目标。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnimTarget {
    /// `p:spTgt/@spid`。
    pub shape_id: u32,
    /// 只对形状里第 `st..=end` 段生效（`p:txEl/p:pRg`）。
    ///
    /// `None` 表示整个形状一起动。
    pub para_range: Option<(u32, u32)>,
    /// 这一步的**起点**形变状态（相对静止状态）。
    pub from: EffectState,
    /// 这一步的**终点**形变状态。
    pub to: EffectState,
    /// 遮罩式揭示（擦除一类）；`None` 表示不用遮罩。
    pub mask: Option<MaskKind>,
}

impl AnimTarget {
    /// 只有出现/消失、没有过程（例如「出现」动画）。
    #[inline]
    pub fn is_static(&self) -> bool {
        self.from == self.to && self.mask.is_none()
    }
}

/// 动画在某一时刻的「形变与透明度」，全部是**相对静止状态**的量。
///
/// # 为什么用「相对量」而不是绝对量
///
/// 静态那一帧（`p:timing` 之外的样子）是渲染器已经画好的基准；
/// 动画只是在这之上做偏移、缩放、旋转、调透明度。
/// 用相对量表示，动画播完之后回到「全是恒等值」，
/// 与静态帧严丝合缝地对上，不会出现收尾时跳一下。
///
/// 位移用**点（pt）**而不是画布比例：解析时幻灯片尺寸已知，
/// 早点换算好，渲染端与前端都不必再关心画布多大。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectState {
    /// 不透明度倍率（0 = 透明，1 = 原样）。
    pub opacity: f32,
    /// 绕图层中心缩放（1 = 原尺寸）。
    pub scale: f32,
    /// 旋转角度（度，顺时针）。
    pub rotate: f32,
    /// 水平位移（pt）。
    pub dx: f32,
    /// 垂直位移（pt）。
    pub dy: f32,
}

impl EffectState {
    /// 静止状态：不平移、不缩放、不旋转、不透明。
    pub const IDENTITY: EffectState = EffectState {
        opacity: 1.0,
        scale: 1.0,
        rotate: 0.0,
        dx: 0.0,
        dy: 0.0,
    };

    #[inline]
    pub fn is_identity(&self) -> bool {
        *self == EffectState::IDENTITY
    }

    /// 线性插值，供渲染端按播放进度取中间帧。
    #[inline]
    pub fn lerp(a: &EffectState, b: &EffectState, t: f32) -> EffectState {
        let t = t.clamp(0.0, 1.0);
        let mix = |x: f32, y: f32| x + (y - x) * t;
        EffectState {
            opacity: mix(a.opacity, b.opacity),
            scale: mix(a.scale, b.scale),
            rotate: mix(a.rotate, b.rotate),
            dx: mix(a.dx, b.dx),
            dy: mix(a.dy, b.dy),
        }
    }
}

impl Default for EffectState {
    fn default() -> Self {
        EffectState::IDENTITY
    }
}

/// 一条强调动画给节点**永久留下**的形变。
///
/// 「放大到 150%」这类强调动画在 PowerPoint 里是**持续**的：播完之后
/// 对象就停在放大后的样子，后面几步看到的也是它。所以不能只在前端播一下了事 ——
/// 那会在动画结束、前端换成静态帧的瞬间弹回原样。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EmphasisStep {
    /// 第几步（从 1 起）。
    pub step: u32,
    /// 播完之后留下的形变状态。
    pub to: EffectState,
}

impl EmphasisStep {
    /// 把两条形变叠加起来。
    ///
    /// 逐项按各自的性质合并：透明度相乘、缩放相乘、旋转相加、位移相加。
    /// 顺序无关（都满足交换律），所以不必关心强调动画的先后。
    pub fn compose(a: EffectState, b: EffectState) -> EffectState {
        EffectState {
            opacity: a.opacity * b.opacity,
            scale: a.scale * b.scale,
            rotate: a.rotate + b.rotate,
            dx: a.dx + b.dx,
            dy: a.dy + b.dy,
        }
    }
}

/// 页内（或形状内）某个矩形在形变后的外接盒。
///
/// 形变是**绕矩形中心**的缩放加旋转，再叠加平移 —— 与 PowerPoint 一致。
pub fn transformed_bounds(rect: Rect, s: &EffectState) -> Rect {
    if s.is_identity() {
        return rect;
    }
    let c = rect.center();
    let (sin, cos) = s.rotate.to_radians().sin_cos();
    let k = s.scale;
    let mut min_x = f32::MAX;
    let mut min_y = f32::MAX;
    let mut max_x = f32::MIN;
    let mut max_y = f32::MIN;
    for (px, py) in [
        (rect.x, rect.y),
        (rect.right(), rect.y),
        (rect.right(), rect.bottom()),
        (rect.x, rect.bottom()),
    ] {
        let vx = (px - c.x) * k;
        let vy = (py - c.y) * k;
        let x = c.x + vx * cos - vy * sin + s.dx;
        let y = c.y + vx * sin + vy * cos + s.dy;
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    Rect::new(min_x, min_y, max_x - min_x, max_y - min_y)
}

/// 遮罩式揭示：新内容从一个方向「擦」出来。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MaskKind {
    /// 揭示推进的方向。
    pub dir: MaskDir,
}

/// 遮罩推进方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MaskDir {
    /// 从左往右推开。
    LeftToRight,
    RightToLeft,
    TopToBottom,
    BottomToTop,
}

/// 一步动画的性质。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StepKind {
    /// 出现：这一步之前不显示。
    #[default]
    Entrance,
    /// 消失：这一步之后不再显示。
    Exit,
    /// 强调：本来就在，只是动一下（放大、旋转、变色）。
    ///
    /// 关键区别是它**不改变可见性** —— 若照「出现」处理，
    /// 会把已经在屏幕上的对象先藏起来再放出来。
    Emphasis,
}

/// 一步动画：同一时刻开始的一组效果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnimStep {
    pub trigger: StepTrigger,
    pub targets: Vec<AnimTarget>,
    pub kind: StepKind,
    /// 本步动画的播放时长（毫秒），取组内各效果 `p:cTn/@dur` 的最大值。
    ///
    /// 0 表示时序里没写时长 —— 这种「瞬变」按 1ms 处理，前端不会为它插动画。
    pub dur_ms: u32,
}

impl AnimStep {
    #[inline]
    pub fn is_exit(&self) -> bool {
        matches!(self.kind, StepKind::Exit)
    }

    /// 这一步之后目标还能不能看见。
    #[inline]
    pub fn changes_visibility(&self) -> bool {
        matches!(self.kind, StepKind::Entrance | StepKind::Exit)
    }

    /// 这一步要不要触发才有意义。
    ///
    /// 「等点击」与「点指定形状」都算；只有 `AfterPrevious` 由程序自己接着播。
    #[inline]
    pub fn needs_input(&self) -> bool {
        matches!(
            self.trigger,
            StepTrigger::OnClick | StepTrigger::OnShape(_)
        )
    }
}

/// 一页的动画播放序列。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AnimSequence {
    pub steps: Vec<AnimStep>,
}

impl AnimSequence {
    #[inline]
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

/// 页间转场效果（`p:transition`）。
///
/// # 为什么把「效果」和「方向」拆成两个字段
///
/// OOXML 里 `<p:wipe dir="d"/>`、`<p:push dir="l"/>`、`<p:cover dir="u"/>`
/// 是同一套方向词表，而 `<p:split orient="horz" dir="out"/>` 用的是另一套。
/// 若为每种效果各定义一个带方向的枚举，前端就得写几十个分支；
/// 拆开之后前端只面对「效果 × 方向」两个正交维度。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Transition {
    pub kind: TransitionKind,
    pub dir: TransitionDir,
    /// 播放时长（毫秒）。`p14:dur` 优先，其次按 `spd` 档位折算。
    pub dur_ms: u32,
    /// 百叶窗叶片数 / 轮辐条数（`spokes`）。
    pub spokes: u8,
    /// 是否经黑场过渡（`thruBlk`）。
    pub through_black: bool,
    /// 本页自动前进的延时（`p:transition/@advTm`，毫秒）。
    ///
    /// `None` 表示不自动前进。这是作者写在课件里的放映节奏，要照做。
    pub advance_after_ms: Option<u32>,
    /// 是否响应点击前进（`advClick`，默认 true）。
    pub advance_on_click: bool,
}

impl Transition {
    /// 是否等于「直接切」——没有转场效果时不必做任何动画。
    #[inline]
    pub fn is_none(&self) -> bool {
        matches!(self.kind, TransitionKind::Cut)
    }
}

/// 转场效果种类。
///
/// 覆盖 PowerPoint 2010 以来 `p:transition` 里的全部子元素；
/// 少数几何极其特殊（`cube`/`vortex`/`morph` 等）在渲染端退化为淡入，
/// 但**解析阶段保留原名**，便于诊断与后续补齐。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum TransitionKind {
    /// 无效果（直接切）。
    #[default]
    Cut,
    Fade,
    Dissolve,
    Push,
    Cover,
    Uncover,
    Wipe,
    Split,
    Zoom,
    Blinds,
    Checker,
    Comb,
    Strips,
    Wheel,
    Circle,
    Diamond,
    Plus,
    Wedge,
    Doors,
    Window,
    Newsflash,
    Glitter,
    Honeycomb,
    Shred,
    Flash,
    Ripple,
    Pan,
    Reveal,
    Switch,
    Ferris,
    Flythrough,
    Prestige,
    Fallover,
    Drape,
    Curtains,
    Wind,
    Orbit,
    /// 随机：每次放映时从全部效果里挑一个。
    Random,
    /// 认得出来但还没实现的效果，保留 OOXML 里的名字。
    Other(String),
}

/// 转场方向词表（`dir`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum TransitionDir {
    #[default]
    None,
    Left,
    Right,
    Up,
    Down,
    /// 水平方向「从中间向两边」（`split` 的 `orient="horz" dir="out"` 一类）。
    HorzOut,
    HorzIn,
    VertOut,
    VertIn,
    /// 从左上/右上/左下/右下（`strips` 的 `lu`/`ru`/`ld`/`rd`）。
    LeftUp,
    RightUp,
    LeftDown,
    RightDown,
    /// 向里缩 / 向外扩（`zoom` 的 `in`/`out`）。
    In,
    Out,
}

/// 页内嵌媒体。
///
/// PPT 里的视频/音频都挂在一个「图片形状」上：`p:blipFill` 是**封页图**
/// （静止时显示的那一帧，已由图片链路正常绘制），
/// `p:nvPr` 里才是真正的媒体部件。
/// 因此静态渲染不需要本结构，它只服务于「点一下原位播放」。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaRef {
    /// 包内部件名，如 `ppt/media/media1.mp4`。
    pub part: String,
    pub kind: MediaKind,
    /// 循环播放（`p14:media@loop`）。
    pub loop_play: bool,
    /// 「裁剪视频」留下的播放区间（`p14:trim`）；没裁剪过为 `None`。
    pub trim: Option<MediaTrim>,
}

/// 媒体被裁剪后的播放区间。
///
/// PowerPoint 的「裁剪视频」不改动原文件，只记一段区间。
/// 课件里真有把 121 MB 的视频裁成 5 秒的用法 —— 照着裁剪放，
/// 才是老师在 PowerPoint 里预览到的内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaTrim {
    /// 起点（毫秒）。
    pub start_ms: u32,
    /// 终点（毫秒）；`None` 表示一直到结尾。
    pub end_ms: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaKind {
    Video,
    Audio,
}

impl Default for Node {
    fn default() -> Self {
        Node {
            id: String::new(),
            name: None,
            transform: Transform::IDENTITY,
            opacity: 1.0,
            geometry: Geometry::None,
            fill: Fill::None,
            stroke: None,
            effects: Effects::default(),
            text: None,
            children: Vec::new(),
            hyperlink: None,
            media: None,
            hidden: false,
            shape_id: None,
            build: Build::ALWAYS,
            emphasis: Vec::new(),
            local_bbox: None,
            text_transform: None,
        }
    }
}

impl Node {
    /// 新建一个带标识的节点。
    pub fn new(id: impl Into<String>) -> Node {
        Node {
            id: id.into(),
            ..Default::default()
        }
    }

    #[inline]
    pub fn with_transform(mut self, t: Transform) -> Self {
        self.transform = t;
        self
    }

    #[inline]
    pub fn with_geometry(mut self, g: Geometry) -> Self {
        self.geometry = g;
        self
    }

    #[inline]
    pub fn with_fill(mut self, f: Fill) -> Self {
        self.fill = f;
        self
    }

    /// 已播的强调动画叠加出的形变状态（未播的不算）。
    #[inline]
    pub fn emphasis_state(&self, state: PlayState) -> EffectState {
        let mut out = EffectState::IDENTITY;
        for e in &self.emphasis {
            if state & step_bit(e.step) == 0 {
                continue;
            }
            out = EmphasisStep::compose(out, e.to);
        }
        out
    }

    /// 叠加了已播强调动画之后的画布包围盒。
    ///
    /// 渲染「动画图层」时要按它决定位图尺寸 ——
    /// 被放大到 150% 的对象，用原始包围盒去裁会被切掉一圈。
    pub fn canvas_bounds_at(&self, state: PlayState) -> Rect {
        transformed_bounds(self.canvas_bounds(), &self.emphasis_state(state))
    }

    /// 是否应当被渲染（不可见节点直接跳过，避免无谓的光栅化开销）。
    #[inline]
    pub fn is_visible(&self) -> bool {
        if self.hidden || self.opacity <= 0.0 {
            return false;
        }
        // 有文本或有效果的空形状仍需绘制
        if self.text.as_ref().is_some_and(|t| !t.is_empty()) {
            return true;
        }
        if !self.children.is_empty() {
            return true;
        }
        if self.stroke.as_ref().is_some_and(|s| s.is_visible()) {
            return true;
        }
        match &self.geometry {
            Geometry::Image(_) | Geometry::Table(_) => true,
            Geometry::Placeholder { .. } => true,
            _ => self.fill.is_visible(),
        }
    }

    /// 形状的局部包围盒（局部坐标）。
    ///
    /// 对路径几何取路径点集的包围盒；其余按 `local_bbox` 或约定值推导。
    pub fn local_bounds(&self) -> Rect {
        if let Some(bbox) = self.local_bbox {
            return bbox;
        }
        match &self.geometry {
            Geometry::Rect | Geometry::Ellipse | Geometry::RoundRect { .. } => Rect::ZERO,
            Geometry::Path(p) => {
                if p.bbox.is_empty() {
                    path_bounds(p)
                } else {
                    p.bbox
                }
            }
            Geometry::Image(_) => Rect::ZERO,
            Geometry::Table(t) => Rect::new(0.0, 0.0, t.total_width_pt(), t.total_height_pt()),
            Geometry::None | Geometry::Placeholder { .. } => Rect::ZERO,
        }
    }

    /// 形状在画布空间下的轴对齐包围盒。
    ///
    /// 注意：带旋转的形状返回的是「旋转后」的轴对齐包围盒，
    /// 用于点击热区、脏矩形估算等场景；不参与绘制路径。
    pub fn canvas_bounds(&self) -> Rect {
        let lb = self.local_bounds();
        let corners = [
            self.transform.apply(Point::new(lb.x, lb.y)),
            self.transform.apply(Point::new(lb.right(), lb.y)),
            self.transform.apply(Point::new(lb.right(), lb.bottom())),
            self.transform.apply(Point::new(lb.x, lb.bottom())),
        ];
        let mut min_x = f32::MAX;
        let mut min_y = f32::MAX;
        let mut max_x = f32::MIN;
        let mut max_y = f32::MIN;
        for c in corners {
            min_x = min_x.min(c.x);
            min_y = min_y.min(c.y);
            max_x = max_x.max(c.x);
            max_y = max_y.max(c.y);
        }
        Rect::new(min_x, min_y, max_x - min_x, max_y - min_y)
    }

    /// 近似缩放系数，用于决定图片解码的降采样倍率。
    #[inline]
    pub fn effective_scale(&self) -> f32 {
        self.transform.mean_scale().abs().max(0.0001)
    }

    /// 递归统计节点总数（含自身与所有后代）。
    pub fn node_count(&self) -> usize {
        1 + self.children.iter().map(|c| c.node_count()).sum::<usize>()
    }
}

/// 形状几何。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub enum Geometry {
    /// 无独立几何（如仅作为组合容器）。
    #[default]
    None,
    Rect,
    RoundRect { rx_pt: f32, ry_pt: f32 },
    Ellipse,
    /// 通用路径：来自 `custGeom`，或由预设几何（`prstGeom`）展开而来。
    Path(PathGeometry),
    Image(ImageRef),
    Table(Table),
    /// 无法解析的形状，渲染为占位并记录原因。
    Placeholder { reason: String },
}

impl Geometry {
    /// 占位几何：解析失败时的降级表示。
    pub fn placeholder(reason: impl Into<String>) -> Geometry {
        Geometry::Placeholder {
            reason: reason.into(),
        }
    }

    /// 该几何是否包含图片资源（决定是否需要走媒体解压路径）。
    #[inline]
    pub fn image_part(&self) -> Option<&str> {
        match self {
            Geometry::Image(i) => Some(&i.part),
            _ => None,
        }
    }

    /// 是否属于「降级」几何（用于统计保真度）。
    #[inline]
    pub fn is_degraded(&self) -> bool {
        matches!(self, Geometry::Placeholder { .. })
    }
}

/// 深度优先遍历器。
pub struct SceneWalker<'a> {
    stack: Vec<&'a Node>,
}

impl<'a> Iterator for SceneWalker<'a> {
    type Item = &'a Node;

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.stack.pop()?;
        for child in node.children.iter().rev() {
            self.stack.push(child);
        }
        Some(node)
    }
}

/// 计算路径的点集包围盒。
pub fn path_bounds(path: &PathGeometry) -> Rect {
    let mut min_x = f32::MAX;
    let mut min_y = f32::MAX;
    let mut max_x = f32::MIN;
    let mut max_y = f32::MIN;

    let mut visit = |p: Point| {
        min_x = min_x.min(p.x);
        min_y = min_y.min(p.y);
        max_x = max_x.max(p.x);
        max_y = max_y.max(p.y);
    };

    for sp in &path.subpaths {
        visit(sp.start);
        for seg in &sp.segments {
            match seg {
                PathSegment::Line(p) | PathSegment::Quad { to: p, .. } | PathSegment::Arc { to: p, .. } => {
                    visit(*p)
                }
                PathSegment::Cubic { c1, c2, to } => {
                    // 用控制点做保守估计；精确包围盒由渲染器按需计算
                    visit(*c1);
                    visit(*c2);
                    visit(*to);
                }
                PathSegment::Close => {}
            }
        }
    }

    if min_x > max_x {
        return Rect::ZERO;
    }
    Rect::new(min_x, min_y, max_x - min_x, max_y - min_y)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect_node(id: &str, x: f32, y: f32, w: f32, h: f32) -> Node {
        Node {
            id: id.into(),
            transform: Transform::translate(x, y),
            local_bbox: Some(Rect::new(0.0, 0.0, w, h)),
            fill: Fill::Solid(Color::WHITE),
            geometry: Geometry::Rect,
            ..Default::default()
        }
    }

    #[test]
    fn walk_visits_all_nodes_depth_first() {
        let mut root = Node::new("root");
        root.children = vec![rect_node("a", 0.0, 0.0, 10.0, 10.0)];
        root.children[0].children = vec![rect_node("a1", 0.0, 0.0, 5.0, 5.0)];
        let scene = Scene {
            size_pt: Size::new(960.0, 540.0),
            background: SceneBackground::default(),
            nodes: vec![root, rect_node("b", 0.0, 0.0, 1.0, 1.0)],
            notes: None,
            title: None,
            warnings: Vec::new(),
            anim: AnimSequence::default(),
            transition: Transition::default(),
        };
        let ids: Vec<_> = scene.walk().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["root", "a", "a1", "b"]);
    }

    #[test]
    fn canvas_bounds_applies_translation() {
        let n = rect_node("a", 100.0, 50.0, 20.0, 30.0);
        assert_eq!(n.canvas_bounds(), Rect::new(100.0, 50.0, 20.0, 30.0));
    }

    #[test]
    fn canvas_bounds_of_rotated_shape_is_aabb() {
        // 100x100 方块绕中心旋转 45 度，AABB 边长应为 100*sqrt(2)
        let n = Node {
            id: "r".into(),
            transform: Transform::from_ooxml(
                Point::new(0.0, 0.0),
                Size::new(100.0, 100.0),
                45.0,
                false,
                false,
            ),
            local_bbox: Some(Rect::new(0.0, 0.0, 100.0, 100.0)),
            geometry: Geometry::Rect,
            fill: Fill::Solid(Color::BLACK),
            ..Default::default()
        };
        let b = n.canvas_bounds();
        let expected = 100.0 * 2f32.sqrt();
        assert!((b.w - expected).abs() < 0.5, "w = {}", b.w);
        assert!((b.h - expected).abs() < 0.5, "h = {}", b.h);
    }

    #[test]
    fn visibility_rules() {
        // 无填充无描边无文本 -> 不可见
        let invisible = Node {
            geometry: Geometry::Rect,
            ..Default::default()
        };
        assert!(!invisible.is_visible());

        // hidden 优先
        let mut hidden = rect_node("h", 0.0, 0.0, 1.0, 1.0);
        hidden.hidden = true;
        assert!(!hidden.is_visible());

        // 全透明也不可见
        let mut transparent = rect_node("t", 0.0, 0.0, 1.0, 1.0);
        transparent.opacity = 0.0;
        assert!(!transparent.is_visible());

        // 只有文本也要可见
        let text_only = Node {
            text: Some(TextBox {
                body: BodyProps::default(),
                paragraphs: vec![Paragraph {
                    runs: vec![TextRun::new("hi")],
                    ..Default::default()
                }],
            }),
            ..Default::default()
        };
        assert!(text_only.is_visible());

        // 只有描边也要可见
        let stroke_only = Node {
            geometry: Geometry::Rect,
            stroke: Some(Stroke::default()),
            ..Default::default()
        };
        assert!(stroke_only.is_visible());
    }

    #[test]
    fn node_count_includes_descendants() {
        let mut root = Node::new("root");
        root.children = vec![rect_node("a", 0.0, 0.0, 1.0, 1.0)];
        root.children[0].children = vec![rect_node("a1", 0.0, 0.0, 1.0, 1.0)];
        assert_eq!(root.node_count(), 3);
    }

    #[test]
    fn path_bounds_covers_all_points() {
        let p = PathGeometry {
            subpaths: vec![SubPath {
                start: Point::new(10.0, 10.0),
                segments: vec![
                    PathSegment::Line(Point::new(50.0, 10.0)),
                    PathSegment::Line(Point::new(50.0, 80.0)),
                ],
                closed: true,
            }],
            text_rect: None,
            bbox: Rect::ZERO,
        };
        let b = path_bounds(&p);
        assert_eq!(b, Rect::new(10.0, 10.0, 40.0, 70.0));
    }

    #[test]
    fn empty_path_has_zero_bounds() {
        let p = PathGeometry::default();
        assert_eq!(path_bounds(&p), Rect::ZERO);
    }

    #[test]
    fn link_hotspots_collected_from_children() {
        let mut child = rect_node("c", 10.0, 20.0, 30.0, 40.0);
        child.hyperlink = Some(Hyperlink {
            target: HyperlinkTarget::Slide(4),
            tooltip: None,
        });
        let scene = Scene {
            size_pt: Size::new(960.0, 540.0),
            background: SceneBackground::default(),
            nodes: vec![child],
            notes: None,
            title: None,
            warnings: Vec::new(),
            anim: AnimSequence::default(),
            transition: Transition::default(),
        };
        let spots = scene.link_hotspots();
        assert_eq!(spots.len(), 1);
        assert_eq!(spots[0].rect, Rect::new(10.0, 20.0, 30.0, 40.0));
        assert_eq!(spots[0].link.target, HyperlinkTarget::Slide(4));
    }

    /// 造一个只含单个链接 run 的文本框。
    fn text_box_with_links(targets: &[HyperlinkTarget]) -> TextBox {
        let runs = targets
            .iter()
            .map(|t| TextRun {
                text: "链接".to_string(),
                props: RunProps::default(),
                hyperlink: Some(Hyperlink {
                    target: t.clone(),
                    tooltip: None,
                }),
                field: None,
            })
            .collect();
        TextBox {
            body: BodyProps::default(),
            paragraphs: vec![Paragraph {
                runs,
                ..Default::default()
            }],
        }
    }

    #[test]
    fn text_link_becomes_a_box_hotspot() {
        // 文字链接没有形状级链接，热区退化为整个文本框 ——
        // 这条曾经被完全漏掉：解析出了链接却永远点不到
        let mut node = rect_node("t", 0.0, 0.0, 200.0, 50.0);
        node.text = Some(text_box_with_links(&[HyperlinkTarget::Url(
            "https://example.com".to_string(),
        )]));

        let scene = Scene {
            size_pt: Size::new(960.0, 540.0),
            background: SceneBackground::default(),
            nodes: vec![node],
            notes: None,
            title: None,
            warnings: Vec::new(),
            anim: AnimSequence::default(),
            transition: Transition::default(),
        };

        let spots = scene.link_hotspots();
        assert_eq!(spots.len(), 1);
        assert_eq!(spots[0].rect, Rect::new(0.0, 0.0, 200.0, 50.0));
    }

    #[test]
    fn text_box_with_conflicting_links_gets_no_hotspot() {
        // 一个框里挂了两个不同目标：整框热区会让点哪儿都跳同一处，
        // 因此宁可不给热区
        let mut node = rect_node("t", 0.0, 0.0, 200.0, 50.0);
        node.text = Some(text_box_with_links(&[
            HyperlinkTarget::Slide(1),
            HyperlinkTarget::Slide(7),
        ]));

        let scene = Scene {
            size_pt: Size::new(960.0, 540.0),
            background: SceneBackground::default(),
            nodes: vec![node],
            notes: None,
            title: None,
            warnings: Vec::new(),
            anim: AnimSequence::default(),
            transition: Transition::default(),
        };

        assert!(
            scene.link_hotspots().is_empty(),
            "目标不一致时不应生成整框热区"
        );
    }

    #[test]
    fn text_box_with_same_target_twice_still_yields_one_hotspot() {
        let mut node = rect_node("t", 0.0, 0.0, 200.0, 50.0);
        node.text = Some(text_box_with_links(&[
            HyperlinkTarget::Slide(3),
            HyperlinkTarget::Slide(3),
        ]));

        let scene = Scene {
            size_pt: Size::new(960.0, 540.0),
            background: SceneBackground::default(),
            nodes: vec![node],
            notes: None,
            title: None,
            warnings: Vec::new(),
            anim: AnimSequence::default(),
            transition: Transition::default(),
        };

        assert_eq!(scene.link_hotspots().len(), 1);
    }

    #[test]
    fn shape_link_wins_over_text_link() {
        // 形状级链接（动作按钮）优先：它的热区才是按钮本身
        let mut node = rect_node("s", 0.0, 0.0, 200.0, 50.0);
        node.hyperlink = Some(Hyperlink {
            target: HyperlinkTarget::NextSlide,
            tooltip: None,
        });
        node.text = Some(text_box_with_links(&[HyperlinkTarget::Slide(9)]));

        let scene = Scene {
            size_pt: Size::new(960.0, 540.0),
            background: SceneBackground::default(),
            nodes: vec![node],
            notes: None,
            title: None,
            warnings: Vec::new(),
            anim: AnimSequence::default(),
            transition: Transition::default(),
        };

        let spots = scene.link_hotspots();
        assert_eq!(spots.len(), 1, "同一个节点不应产出两个热区");
        assert_eq!(spots[0].link.target, HyperlinkTarget::NextSlide);
    }

    #[test]
    fn warnings_are_deduplicated() {
        let mut s = Scene::new(Size::new(960.0, 540.0));
        s.push_warning("SmartArt 已降级");
        s.push_warning("SmartArt 已降级");
        s.push_warning("图表已降级");
        assert_eq!(s.warnings.len(), 2);
    }

    #[test]
    fn effective_scale_of_translated_node_is_one() {
        let n = rect_node("a", 500.0, 300.0, 10.0, 10.0);
        assert!((n.effective_scale() - 1.0).abs() < 1e-4);
    }

    #[test]
    fn background_primary_color() {
        assert_eq!(
            SceneBackground::Solid(Color::rgb(1, 2, 3)).primary_color(),
            Some(Color::rgb(1, 2, 3))
        );
        assert_eq!(SceneBackground::None.primary_color(), None);
    }
}
