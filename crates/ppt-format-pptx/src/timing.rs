//! 动画时序：`p:timing` → [`AnimSequence`]。
//!
//! # 为什么必须读懂这棵树
//!
//! 课件作者用动画表达「讲到哪儿、露到哪儿」。最典型的是**答案先藏起来**：
//! 作者把答案文本框的动画设成「单击时出现」，PowerPoint 便在放映前把它藏了。
//! 我们若不读时序，就会把答案直接画在题干上 —— 在老师眼里这就是「错位」。
//!
//! # 这棵树的形状
//!
//! ```text
//! p:timing/p:tnLst/p:par/p:cTn[@nodeType=tmRoot]/p:childTnLst
//!   p:seq/p:cTn[@nodeType=mainSeq]/p:childTnLst
//!     p:par/p:cTn/p:stCondLst/p:cond[@delay=indefinite]   ← 一个「单击组」
//!       p:childTnLst/p:par/p:cTn/p:childTnLst
//!         p:par/p:cTn[@presetClass  @nodeType=clickEffect]  ← 一个效果
//!           p:childTnLst
//!             p:set   … 设 style.visibility
//!             p:anim  … 位移/缩放/透明度
//! ```
//!
//! 实际文件里的嵌套层数**不固定**（PowerPoint、WPS、第三方工具各写各的），
//! 所以这里不按固定路径取值，而是遍历整棵树找「带 `presetClass` 的 `cTn`」，
//! 那才是真正的一个效果。

use ppt_core::scene::{
    AnimSequence, AnimStep, AnimTarget, EffectState, MaskDir, MaskKind, StepKind, StepTrigger,
};
use ppt_core::XmlNode;

/// 形状在画布上的归一化几何。
///
/// 都相对幻灯片宽/高：`w`/`h` 是尺寸占比，`cx`/`cy` 是中心占比。
/// 时序里的位置表达式（`#ppt_x-0.1`、`1+#ppt_h/2`）就靠它求值。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ShapeBox {
    pub w: f32,
    pub h: f32,
    pub cx: f32,
    pub cy: f32,
}

/// 按形状 id 查它的归一化几何。
pub type ShapeLookup<'a> = &'a dyn Fn(u32) -> Option<ShapeBox>;

/// 解析一页的动画时序。
///
/// `slide_size` 是画布尺寸（pt）：时序里的位移是画布比例，这里就换算成点，
/// 免得渲染端与前端各自再乘一遍画布大小。
///
/// 返回的 `Vec<String>` 是要提示给用户的降级说明。
pub fn parse_timing(
    slide: &XmlNode,
    slide_size: (f32, f32),
    shapes: ShapeLookup<'_>,
) -> (AnimSequence, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let Some(timing) = slide.child("timing") else {
        return (AnimSequence::default(), warnings);
    };

    // 一棵时序树里可能有好几条序列：主序列（等点击）与若干**交互序列**
    // （点指定形状才播）。必须按出现顺序全部收进来，丢一条就等于丢一段动画。
    let mut seqs: Vec<&XmlNode> = Vec::new();
    collect_seqs(timing, &mut seqs);

    let mut steps: Vec<AnimStep> = Vec::new();
    if seqs.is_empty() {
        // 老工具写出来的时序可能没有 `p:seq`，整棵树当一条序列处理
        let trigger = group_trigger(timing);
        let mut effects = Vec::new();
        collect_effects(timing, &mut effects, slide_size, shapes);
        merge_effects(&mut steps, trigger, &effects, &mut warnings);
    } else {
        for seq in seqs {
            let node_type = seq
                .path(&["cTn"])
                .and_then(|c| c.attr("nodeType"))
                .unwrap_or("mainSeq");
            // 交互序列的触发形状写在序列自己的起始条件里
            let shape_trigger = interactive_trigger(seq);
            match shape_trigger {
                Some(shape_id) => {
                    let mut effects = Vec::new();
                    if let Some(body) = seq.path(&["cTn", "childTnLst"]) {
                        collect_effects(body, &mut effects, slide_size, shapes);
                    }
                    merge_effects(
                        &mut steps,
                        StepTrigger::OnShape(shape_id),
                        &effects,
                        &mut warnings,
                    );
                }
                None if node_type == "interactiveSeq" => {
                    // 认得出是交互序列却找不到触发对象：宁可丢掉也不要
                    // 变成「点空白就能播」—— 那会让老师多点一下才翻页
                    warnings.push("有一处触发器动画找不到触发对象，已忽略".to_string());
                }
                None => {
                    let groups = seq_groups(seq).unwrap_or_else(|| vec![seq]);
                    for group in groups {
                        let trigger = group_trigger(group);
                        let mut effects = Vec::new();
                        collect_effects(group, &mut effects, slide_size, shapes);
                        merge_effects(&mut steps, trigger, &effects, &mut warnings);
                    }
                }
            }
        }
    }

    (AnimSequence { steps }, warnings)
}

/// 收集整棵时序树里的所有 `p:seq`（按文档顺序）。
fn collect_seqs<'a>(node: &'a XmlNode, out: &mut Vec<&'a XmlNode>) {
    for child in &node.children {
        if child.name == "seq" {
            out.push(child);
        } else {
            collect_seqs(child, out);
        }
    }
}

/// 主序列下的「单击组」：`p:seq/p:cTn/p:childTnLst` 的 `p:par` 子节点。
fn seq_groups(seq: &XmlNode) -> Option<Vec<&XmlNode>> {
    let container = seq.path(&["cTn", "childTnLst"])?;
    let groups: Vec<&XmlNode> = container
        .children
        .iter()
        .filter(|c| c.name == "par")
        .collect();
    (!groups.is_empty()).then_some(groups)
}

/// 交互序列的触发形状：`p:cond[@evt="onClick"] / p:tgtEl / p:spTgt/@spid`。
///
/// 与「点空白处前进」的区别就在这里：后者只有一个 `@delay="indefinite"`，
/// 没有 `p:tgtEl`。
fn interactive_trigger(seq: &XmlNode) -> Option<u32> {
    let conds = seq.path(&["cTn", "stCondLst"])?;
    for cond in conds.children_named("cond") {
        if cond.attr("evt").is_none_or(|e| e != "onClick") {
            continue;
        }
        // 只认「点形状」的；`p:tn` 表示这是可交互的那一类
        if let Some(spid) = cond
            .path(&["tgtEl", "spTgt"])
            .and_then(|t| t.attr_u32("spid"))
        {
            return Some(spid);
        }
    }
    None
}

/// 单击组的触发方式：`p:cond/@delay="indefinite"` 表示「等下一次点击」。
///
/// 数字延时的组会自己接在上一组之后播，不需要点击。
fn group_trigger(group: &XmlNode) -> StepTrigger {
    let indefinite = group
        .path(&["cTn", "stCondLst"])
        .is_some_and(|lst| {
            lst.children_named("cond")
                .any(|c| c.attr("delay").is_some_and(|d| d.trim() == "indefinite"))
        });
    if indefinite {
        StepTrigger::OnClick
    } else {
        StepTrigger::AfterPrevious
    }
}

/// 一个待归组的效果。
struct Effect {
    /// `clickEffect` / `withEffect` / `afterEffect`，决定它与上一步的关系。
    node_type: String,
    target: Option<AnimTarget>,
    kind: StepKind,
    /// 效果时长（毫秒）。
    dur_ms: u32,
}

fn collect_effects(
    node: &XmlNode,
    out: &mut Vec<Effect>,
    slide_size: (f32, f32),
    shapes: ShapeLookup<'_>,
) {
    for child in &node.children {
        if child.name == "cTn" {
            if let Some(class) = child.attr("presetClass") {
                let kind = if class.eq_ignore_ascii_case("exit") || sets_visibility_hidden(child) {
                    StepKind::Exit
                } else if class.eq_ignore_ascii_case("entr") {
                    StepKind::Entrance
                } else {
                    // 强调（变色/放大/旋转）：本来就在，只是动一下。
                    // 它同样有起止状态，和进出动画共用一套插值。
                    StepKind::Emphasis
                };
                out.push(Effect {
                    node_type: child.attr("nodeType").unwrap_or("").to_string(),
                    target: first_target(child, kind, slide_size, shapes),
                    kind,
                    dur_ms: effect_duration_ms(child),
                });
            }
        }
        collect_effects(child, out, slide_size, shapes);
    }
}

/// 效果时长：效果节点自身及其后代 `p:cTn/@dur` 里的最大值。
///
/// 为什么取最大而不是取第一个：一个「淡入」效果通常由两条并行子动画组成
/// （`p:set` 置可见性，`p:animEffect` 做透明渐变），前者 `dur="1"`、
/// 后者才是真正的时长。取第一个会把 500ms 的淡入当成 1ms 的瞬变。
///
/// `"indefinite"` 表示无限（停在原地等下一次触发），不能参与取最大值。
fn effect_duration_ms(effect: &XmlNode) -> u32 {
    let mut max = 0u32;
    visit(effect, &mut |n| {
        if n.name == "cTn" {
            if let Some(d) = n.attr("dur").and_then(|d| d.trim().parse::<u32>().ok()) {
                max = max.max(d);
            }
        }
    });
    max
}

/// 效果的目标：`p:spTgt` 指的 `spid`，外加这一步的起止形变状态。
fn first_target(
    effect: &XmlNode,
    kind: StepKind,
    slide_size: (f32, f32),
    shapes: ShapeLookup<'_>,
) -> Option<AnimTarget> {
    let sp_tgt = find_first(effect, "spTgt")?;
    let shape_id = sp_tgt.attr_u32("spid")?;
    let para_range = sp_tgt.path(&["txEl", "pRg"]).map(|r| {
        (
            r.attr_u32("st").unwrap_or(0),
            r.attr_u32("end").unwrap_or(u32::MAX),
        )
    });

    // 段落级的过程动画要把「只有这一段」的图层单独渲染出来
    // （见 `ppt_render::Renderer::render_layer`），否则整框一起飞
    let (from, to, mask) = parse_motion(
        effect,
        kind,
        shapes(shape_id).unwrap_or_default(),
        slide_size,
    );

    Some(AnimTarget {
        shape_id,
        para_range,
        from,
        to,
        mask,
    })
}

/// 把一个效果里的 `p:animEffect` / `p:anim` / `p:animScale` / `p:animRot` /
/// `p:animMotion` 折算成「从哪儿动到哪儿」。
///
/// # 为什么取「起点」和「终点」而不是逐帧关键帧
///
/// PowerPoint 的动画在文件里就是一组并行的属性轨道，每条轨道给首末两个值
/// （`p:tavLst` 的 `tm="0"` 与 `tm="100000"`），中间由动画引擎按 `calcmode`
/// 插值。前端只要拿到首末值 + 时长，就能自己按 60fps 插出来，
/// 不必让后端逐帧光栅化。
fn parse_motion(
    effect: &XmlNode,
    kind: StepKind,
    shape: ShapeBox,
    slide_size: (f32, f32),
) -> (EffectState, EffectState, Option<MaskKind>) {
    let mut from = EffectState::IDENTITY;
    let mut to = EffectState::IDENTITY;
    let mut mask: Option<MaskKind> = None;

    visit(effect, &mut |n| match n.name.as_str() {
        "animEffect" => {
            let filter = n.attr("filter").unwrap_or("");
            if filter.starts_with("fade") || filter.starts_with("dissolve") {
                // `transition="in"` 才是「淡入」；缺省时按效果的性质判断
                let entering = n
                    .attr("transition")
                    .map(|t| t.eq_ignore_ascii_case("in"))
                    .unwrap_or(!matches!(kind, StepKind::Exit));
                if entering {
                    from.opacity = 0.0;
                    to.opacity = 1.0;
                } else {
                    from.opacity = 1.0;
                    to.opacity = 0.0;
                }
            } else if let Some(inner) = filter
                .strip_prefix("wipe(")
                .and_then(|s| s.strip_suffix(')'))
            {
                // 方向词描述的是**擦除边缘往哪边走**：
                // `wipe(up)` 是「自底部」（边从下往上推），`wipe(left)` 是「自右侧」
                mask = Some(MaskKind {
                    dir: match inner {
                        "left" => MaskDir::RightToLeft,
                        "up" => MaskDir::BottomToTop,
                        "down" => MaskDir::TopToBottom,
                        _ => MaskDir::LeftToRight,
                    },
                });
            } else if let Some(inner) = filter
                .strip_prefix("slide(")
                .and_then(|s| s.strip_suffix(')'))
            {
                // 「从画面外滑进来」：起点挪到画布之外一屏
                match inner {
                    "fromLeft" => from.dx = -slide_size.0,
                    "fromRight" => from.dx = slide_size.0,
                    "fromTop" => from.dy = -slide_size.1,
                    "fromBottom" => from.dy = slide_size.1,
                    _ => {}
                }
            }
        }
        "anim" => {
            let attr = find_first(n, "attrName").map(|a| a.text.trim().to_string());
            let along_x = match attr.as_deref() {
                Some("ppt_x") => true,
                Some("ppt_y") => false,
                _ => return,
            };
            let Some((a, b)) = tav_range(n, along_x, shape, slide_size) else {
                return;
            };
            if along_x {
                from.dx = a;
                to.dx = b;
            } else {
                from.dy = a;
                to.dy = b;
            }
        }
        "animScale" => {
            let pct = |tag: &str| {
                n.child(tag)
                    .and_then(|v| v.attr_f32("x"))
                    .map(|v| v / 100_000.0)
            };
            match (pct("from"), pct("to"), pct("by")) {
                (Some(f), Some(t), _) => {
                    from.scale = f;
                    to.scale = t;
                }
                (None, None, Some(b)) => {
                    from.scale = 1.0;
                    to.scale = b;
                }
                (Some(f), None, Some(b)) => {
                    from.scale = f;
                    to.scale = f * b;
                }
                _ => {}
            }
        }
        "animRot" => {
            // `p:animRot` 的单位是 1/60000 度（与 `a:rot` 一致）
            let deg = |tag: &str| {
                n.child(tag)
                    .and_then(|v| v.attr_f32("val"))
                    .map(|v| v / 60_000.0)
            };
            match (deg("from"), deg("to"), deg("by")) {
                // `p:by` 是**增量**：起始值 + by 才是终点
                (Some(f), Some(t), _) => {
                    from.rotate = f;
                    to.rotate = t;
                }
                (None, None, Some(b)) => {
                    from.rotate = 0.0;
                    to.rotate = b;
                }
                (Some(f), None, Some(b)) => {
                    from.rotate = f;
                    to.rotate = f + b;
                }
                _ => {}
            }
        }
        "animMotion" => {
            if let Some((p0, p1)) = motion_path_range(n.attr("path").unwrap_or("")) {
                // 路径坐标是画布比例，换算成点
                from.dx = p0.0 * slide_size.0;
                from.dy = p0.1 * slide_size.1;
                to.dx = p1.0 * slide_size.0;
                to.dy = p1.1 * slide_size.1;
            }
        }
        "iterate" => {
            // 逐字/逐词（`type="lt"` / `"wd"`）：PowerPoint 把这一段文字拆成
            // 一个个单元依次播同一个效果。这里近似成「左→右的擦除」——
            // 视觉效果与打字机一致，代价是每个单元共用同一个时长。
            if n.attr("type").is_some_and(|t| t == "lt" || t == "wd") && mask.is_none() {
                mask = Some(MaskKind {
                    dir: MaskDir::LeftToRight,
                });
            }
        }
        _ => {}
    });

    (from, to, mask)
}

/// `p:anim` 的 `p:tavLst` 首末值 → 相对静止位置的位移（pt）。
fn tav_range(
    anim: &XmlNode,
    along_x: bool,
    shape: ShapeBox,
    slide_size: (f32, f32),
) -> Option<(f32, f32)> {
    let lst = anim.child("tavLst")?;
    let extent = if along_x { slide_size.0 } else { slide_size.1 };
    let at = |tm: &str| -> Option<f32> {
        let tav = lst
            .children_named("tav")
            .find(|t| t.attr("tm").is_some_and(|v| v.trim() == tm))?;
        let expr = tav.path(&["val", "strVal"])?.attr("val")?;
        eval_position(expr, along_x, shape).map(|v| v * extent)
    };
    Some((at("0")?, at("100000")?))
}

/// 求值 `p:strVal` 里的位置表达式。
///
/// 语法上是「常数项 + 若干 `#ppt_*` 项」的加减组合，例如：
///
/// ```text
/// #ppt_x              形状原位（偏移 0）
/// #ppt_x-0.1          原位往左 0.1 个画布宽
/// 1+#ppt_h/2          画布下方一屏再加自身半个高度
/// #ppt_y-1-#ppt_h/2   画布上方一屏再减自身半个高度
/// ```
///
/// **不含** `#ppt_*` 的表达式是绝对位置（相对画布左上角），
/// 转成偏移时要减掉形状中心 —— 这就是 `ShapeBox.cx/cy` 的用途。
fn eval_position(expr: &str, along_x: bool, shape: ShapeBox) -> Option<f32> {
    let w = shape.w;
    let h = shape.h;
    let mut total = 0.0f32;
    let mut relative = false;
    let mut sign = 1.0f32;
    let bytes = expr.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            b' ' => {
                i += 1;
                continue;
            }
            b'+' => {
                sign = 1.0;
                i += 1;
            }
            b'-' => {
                sign = -1.0;
                i += 1;
            }
            _ => {}
        }
        let start = i;
        while i < bytes.len() && bytes[i] != b'+' && bytes[i] != b'-' {
            i += 1;
        }
        let term = expr[start..i].trim();
        if term.is_empty() {
            return None;
        }
        let value = if let Some(rest) = term.strip_prefix('#') {
            relative = true;
            let (name, div) = match rest.split_once('/') {
                Some((n, d)) => (n.trim(), d.trim().parse::<f32>().ok()?),
                None => (rest.trim(), 1.0),
            };
            match name {
                // 形状原位 = 偏移 0
                "ppt_x" | "ppt_y" | "ppt_cx" | "ppt_cy" => 0.0,
                "ppt_w" => w / div,
                "ppt_h" => h / div,
                _ => return None,
            }
        } else {
            term.parse::<f32>().ok()?
        };
        total += sign * value;
        sign = 1.0;
    }

    // 绝对位置 → 偏移
    if !relative {
        total -= if along_x { shape.cx } else { shape.cy };
    }
    Some(total)
}

/// `p:animMotion/@path` 的首末点。
///
/// 路径形如 `M 0 0 L 0.25 0 E`，坐标是相对形状位置的画布占比。
/// 中间点表示曲线，这里按直线近似（直线路径覆盖了绝大多数课件）。
fn motion_path_range(path: &str) -> Option<((f32, f32), (f32, f32))> {
    let nums: Vec<f32> = path
        .split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<f32>().ok())
        .collect();
    if nums.len() < 4 {
        return None;
    }
    let first = (nums[0], nums[1]);
    let last = (nums[nums.len() - 2], nums[nums.len() - 1]);
    Some((first, last))
}

/// `p:set` 把 `style.visibility` 置为 `hidden` → 这是「消失」而不是「出现」。
fn sets_visibility_hidden(effect: &XmlNode) -> bool {
    let mut found = false;
    visit(effect, &mut |n| {
        if n.name == "set" {
            let hidden = n.path(&["to", "strVal"]).is_some_and(|v| {
                v.attr("val").is_some_and(|x| x.eq_ignore_ascii_case("hidden"))
            });
            if hidden {
                found = true;
            }
        }
    });
    found
}

fn merge_effects(
    steps: &mut Vec<AnimStep>,
    group_trigger: StepTrigger,
    effects: &[Effect],
    warnings: &mut Vec<String>,
) {
    let mut ignored = 0usize;
    for effect in effects {
        let trigger = match effect.node_type.as_str() {
            "withEffect" => {
                // 与上一动画同时：并进当前步
                if steps.is_empty() {
                    group_trigger
                } else {
                    match steps.last_mut() {
                        Some(step) => {
                            if let Some(t) = &effect.target {
                                step.targets.push(t.clone());
                            }
                            step.dur_ms = step.dur_ms.max(effect.dur_ms);
                            continue;
                        }
                        None => group_trigger,
                    }
                }
            }
            "afterEffect" => StepTrigger::AfterPrevious,
            // clickEffect 与其它：等下一次点击
            _ => StepTrigger::OnClick,
        };

        // 交互序列里的效果自己带着触发形状，别被 clickEffect 覆盖成「点空白」
        let trigger = match (group_trigger, trigger) {
            (StepTrigger::OnShape(id), StepTrigger::OnClick) => StepTrigger::OnShape(id),
            (_, t) => t,
        };

        // 只有「没有任何可见变化」的效果才需要降级告警：
        // 强调动画现在也能逐帧插值了，能做的就别报「不支持」
        let blank = match &effect.target {
            None => true,
            Some(t) => effect.kind == StepKind::Emphasis && t.is_static(),
        };
        if blank {
            ignored += 1;
        }
        steps.push(AnimStep {
            trigger,
            targets: effect.target.clone().into_iter().collect(),
            kind: effect.kind,
            dur_ms: effect.dur_ms,
        });
    }

    if ignored > 0 {
        let msg = format!(
            "有 {ignored} 处强调动画（变色/字体效果）暂不支持，已按「空点击」保留 —— 点击次数与课件一致，但看不到动效"
        );
        if !warnings.contains(&msg) {
            warnings.push(msg);
        }
    }
}

fn find_first<'a>(node: &'a XmlNode, name: &str) -> Option<&'a XmlNode> {
    if node.name == name {
        return Some(node);
    }
    node.children.iter().find_map(|c| find_first(c, name))
}

fn visit(node: &XmlNode, f: &mut impl FnMut(&XmlNode)) {
    f(node);
    for c in &node.children {
        visit(c, f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ppt_core::xml;

    fn slide(body: &str) -> XmlNode {
        let xml_text = format!("<p:sld xmlns:p=\"urn:p\" xmlns:a=\"urn:a\">{body}</p:sld>");
        xml::parse_root("slide1.xml", &xml_text).expect("测试 XML 应能解析")
    }

    /// 不含形状几何的解析入口（这些用例不涉及位置表达式）。
    ///
    /// 画布取 100×100pt：比例与点 1:1，断言好读。
    fn parse(node: &XmlNode) -> (AnimSequence, Vec<String>) {
        parse_timing(node, (100.0, 100.0), &|_| None)
    }

    /// 固定尺寸的形状（宽 0.2 / 高 0.1，中心在画布正中）。
    fn boxed(_id: u32) -> Option<ShapeBox> {
        Some(ShapeBox {
            w: 0.2,
            h: 0.1,
            cx: 0.5,
            cy: 0.5,
        })
    }

    /// 带形状几何的解析入口，画布仍是 100×100pt。
    fn parse_boxed(node: &XmlNode) -> (AnimSequence, Vec<String>) {
        parse_timing(node, (100.0, 100.0), &boxed)
    }

    /// 一个「单击时淡入 + 从下方滑入」的效果，目标 spid=2 的第 0 段。
    fn entrance_effect() -> &'static str {
        r#"<p:timing><p:tnLst><p:par><p:cTn id="1" nodeType="tmRoot"><p:childTnLst>
             <p:seq concurrent="1" nextAc="seek"><p:cTn id="2" nodeType="mainSeq"><p:childTnLst>
               <p:par><p:cTn id="3" fill="hold"><p:stCondLst>
                   <p:cond delay="indefinite"/>
                 </p:stCondLst><p:childTnLst>
                 <p:par><p:cTn id="4" fill="hold"><p:stCondLst>
                     <p:cond delay="0"/>
                   </p:stCondLst><p:childTnLst>
                   <p:par><p:cTn id="5" presetID="2" presetClass="entr" presetSubtype="4"
                                 fill="hold" grpId="0" nodeType="clickEffect"><p:stCondLst>
                       <p:cond delay="0"/>
                     </p:stCondLst><p:childTnLst>
                     <p:set><p:cBhvr><p:cTn id="6" dur="1" fill="hold"/><p:tgtEl>
                           <p:spTgt spid="2"><p:txEl><p:pRg st="0" end="0"/></p:txEl></p:spTgt>
                         </p:tgtEl><p:attrNameLst>
                           <p:attrName>style.visibility</p:attrName>
                         </p:attrNameLst></p:cBhvr><p:to><p:strVal val="visible"/></p:to></p:set>
                   </p:childTnLst></p:cTn></p:par>
                 </p:childTnLst></p:cTn></p:par>
               </p:childTnLst></p:cTn></p:par>
             </p:childTnLst></p:cTn></p:seq>
           </p:childTnLst></p:cTn></p:par></p:tnLst></p:timing>"#
    }

    #[test]
    fn click_entrance_becomes_one_step() {
        let (seq, warnings) = parse(&slide(entrance_effect()));
        assert!(warnings.is_empty(), "不应有告警：{warnings:?}");
        assert_eq!(seq.len(), 1, "只有一个点击组");
        let step = &seq.steps[0];
        assert_eq!(step.trigger, StepTrigger::OnClick);
        assert!(!step.is_exit(), "set visible 是「出现」");
        assert_eq!(step.targets.len(), 1);
        assert_eq!(step.targets[0].shape_id, 2);
        assert_eq!(step.targets[0].para_range, Some((0, 0)));
    }

    #[test]
    fn two_click_groups_become_two_steps() {
        // 两个并列的点击组：各出一个 spid
        let xml_text = r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:timing><p:tnLst><p:par><p:cTn nodeType="tmRoot"><p:childTnLst>
             <p:seq><p:cTn nodeType="mainSeq"><p:childTnLst>
               <p:par><p:cTn fill="hold"><p:stCondLst><p:cond delay="indefinite"/></p:stCondLst><p:childTnLst>
                 <p:par><p:cTn presetClass="entr" nodeType="clickEffect"><p:childTnLst>
                   <p:set><p:cBhvr><p:tgtEl><p:spTgt spid="7"/></p:tgtEl></p:cBhvr>
                     <p:to><p:strVal val="visible"/></p:to></p:set>
                 </p:childTnLst></p:cTn></p:par>
               </p:childTnLst></p:cTn></p:par>
               <p:par><p:cTn fill="hold"><p:stCondLst><p:cond delay="indefinite"/></p:stCondLst><p:childTnLst>
                 <p:par><p:cTn presetClass="entr" nodeType="clickEffect"><p:childTnLst>
                   <p:set><p:cBhvr><p:tgtEl><p:spTgt spid="8"/></p:tgtEl></p:cBhvr>
                     <p:to><p:strVal val="visible"/></p:to></p:set>
                 </p:childTnLst></p:cTn></p:par>
               </p:childTnLst></p:cTn></p:par>
             </p:childTnLst></p:cTn></p:seq>
           </p:childTnLst></p:cTn></p:par></p:tnLst></p:timing></p:sld>"#;
        let root = xml::parse_root("slide1.xml", xml_text).unwrap();
        let (seq, _) = parse(&root);
        assert_eq!(seq.len(), 2, "两次点击 = 两步");
        assert_eq!(seq.steps[0].targets[0].shape_id, 7);
        assert_eq!(seq.steps[1].targets[0].shape_id, 8);
    }

    #[test]
    fn emphasis_without_visible_change_still_keeps_the_click() {
        // 只有 `p:anim` 没写属性名：这处强调动画我们复现不出来。
        // 但这一步**仍要占一次点击**，否则老师会以为程序「多点了一下」；
        // 同时要告警说明，不能静默吞掉。
        let xml_text = r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:timing><p:tnLst><p:par><p:cTn nodeType="tmRoot"><p:childTnLst>
             <p:seq><p:cTn nodeType="mainSeq"><p:childTnLst>
               <p:par><p:cTn fill="hold"><p:stCondLst><p:cond delay="indefinite"/></p:stCondLst><p:childTnLst>
                 <p:par><p:cTn presetClass="emph" nodeType="clickEffect"><p:childTnLst>
                   <p:anim><p:cBhvr><p:tgtEl><p:spTgt spid="9"/></p:tgtEl></p:cBhvr></p:anim>
                 </p:childTnLst></p:cTn></p:par>
               </p:childTnLst></p:cTn></p:par>
             </p:childTnLst></p:cTn></p:seq>
           </p:childTnLst></p:cTn></p:par></p:tnLst></p:timing></p:sld>"#;
        let root = xml::parse_root("slide1.xml", xml_text).unwrap();
        let (seq, warnings) = parse(&root);
        assert_eq!(seq.len(), 1, "强调动画仍要占一次点击");
        assert_eq!(seq.steps[0].kind, StepKind::Emphasis, "强调不改可见性");
        assert!(
            seq.steps[0].targets[0].is_static(),
            "这一处没有能复现的过程变化"
        );
        assert!(
            warnings.iter().any(|w| w.contains("强调")),
            "应说明降级原因：{warnings:?}"
        );
    }

    #[test]
    fn fly_in_from_bottom_reads_its_offset() {
        // `ppt_y` 的起点写成 `1+#ppt_h/2`：画布下方一屏再加自身半个高度。
        // 夹具里形状高占画布 0.1，所以起点位移 = 1 + 0.05。
        let xml_text = r##"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:timing><p:tnLst><p:par><p:cTn nodeType="tmRoot"><p:childTnLst>
             <p:seq><p:cTn nodeType="mainSeq"><p:childTnLst>
               <p:par><p:cTn fill="hold"><p:stCondLst><p:cond delay="indefinite"/></p:stCondLst><p:childTnLst>
                 <p:par><p:cTn presetClass="entr" nodeType="clickEffect"><p:childTnLst>
                   <p:set><p:cBhvr><p:cTn dur="1"/><p:tgtEl><p:spTgt spid="2"/></p:tgtEl></p:cBhvr>
                     <p:to><p:strVal val="visible"/></p:to></p:set>
                   <p:anim><p:cBhvr><p:cTn dur="500"/><p:tgtEl><p:spTgt spid="2"/></p:tgtEl>
                       <p:attrNameLst><p:attrName>ppt_y</p:attrName></p:attrNameLst></p:cBhvr>
                     <p:tavLst><p:tav tm="0"><p:val><p:strVal val="1+#ppt_h/2"/></p:val></p:tav>
                       <p:tav tm="100000"><p:val><p:strVal val="#ppt_y"/></p:val></p:tav></p:tavLst></p:anim>
                 </p:childTnLst></p:cTn></p:par>
               </p:childTnLst></p:cTn></p:par>
             </p:childTnLst></p:cTn></p:seq>
           </p:childTnLst></p:cTn></p:par></p:tnLst></p:timing></p:sld>"##;
        let root = xml::parse_root("slide1.xml", xml_text).unwrap();
        let (seq, _) = parse_boxed(&root);
        let t = &seq.steps[0].targets[0];
        assert!(
            (t.from.dy - 105.0).abs() < 1e-3,
            "起点在画布下方一屏再加自身半高：{:?}",
            t.from
        );
        assert_eq!(t.to.dy, 0.0, "终点回到原位");
        assert_eq!(t.from.dx, 0.0, "横向不动");
        assert!(!t.is_static());
    }

    #[test]
    fn grow_emphasis_interpolates_scale() {
        // 强调-放大：`p:animScale/p:by` 是目标倍率（100000 = 原尺寸）
        let xml_text = r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:timing><p:tnLst><p:par><p:cTn nodeType="tmRoot"><p:childTnLst>
             <p:seq><p:cTn nodeType="mainSeq"><p:childTnLst>
               <p:par><p:cTn fill="hold"><p:stCondLst><p:cond delay="indefinite"/></p:stCondLst><p:childTnLst>
                 <p:par><p:cTn presetClass="emph" nodeType="clickEffect"><p:childTnLst>
                   <p:animScale><p:cBhvr><p:cTn dur="500"/><p:tgtEl><p:spTgt spid="5"/></p:tgtEl></p:cBhvr>
                     <p:by x="150000" y="150000"/></p:animScale>
                 </p:childTnLst></p:cTn></p:par>
               </p:childTnLst></p:cTn></p:par>
             </p:childTnLst></p:cTn></p:seq>
           </p:childTnLst></p:cTn></p:par></p:tnLst></p:timing></p:sld>"#;
        let root = xml::parse_root("slide1.xml", xml_text).unwrap();
        let (seq, warnings) = parse(&root);
        let t = &seq.steps[0].targets[0];
        assert_eq!(t.from.scale, 1.0);
        assert_eq!(t.to.scale, 1.5);
        assert!(warnings.is_empty(), "能复现就不该再告警：{warnings:?}");
    }

    #[test]
    fn spin_emphasis_interpolates_rotation() {
        // 陀螺旋：`p:animRot/p:by` 是增量，单位 1/60000 度
        let xml_text = r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:timing><p:tnLst><p:par><p:cTn nodeType="tmRoot"><p:childTnLst>
             <p:seq><p:cTn nodeType="mainSeq"><p:childTnLst>
               <p:par><p:cTn fill="hold"><p:stCondLst><p:cond delay="indefinite"/></p:stCondLst><p:childTnLst>
                 <p:par><p:cTn presetClass="emph" nodeType="clickEffect"><p:childTnLst>
                   <p:animRot><p:cBhvr><p:cTn dur="700"/><p:tgtEl><p:spTgt spid="5"/></p:tgtEl></p:cBhvr>
                     <p:by val="21600000"/></p:animRot>
                 </p:childTnLst></p:cTn></p:par>
               </p:childTnLst></p:cTn></p:par>
             </p:childTnLst></p:cTn></p:seq>
           </p:childTnLst></p:cTn></p:par></p:tnLst></p:timing></p:sld>"#;
        let root = xml::parse_root("slide1.xml", xml_text).unwrap();
        let (seq, _) = parse(&root);
        let t = &seq.steps[0].targets[0];
        assert_eq!(t.from.rotate, 0.0);
        assert!((t.to.rotate - 360.0).abs() < 1e-3, "{:?}", t.to);
        assert_eq!(seq.steps[0].dur_ms, 700);
    }

    #[test]
    fn wipe_animation_becomes_a_mask() {
        let xml_text = r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:timing><p:tnLst><p:par><p:cTn nodeType="tmRoot"><p:childTnLst>
             <p:seq><p:cTn nodeType="mainSeq"><p:childTnLst>
               <p:par><p:cTn fill="hold"><p:stCondLst><p:cond delay="indefinite"/></p:stCondLst><p:childTnLst>
                 <p:par><p:cTn presetClass="entr" nodeType="clickEffect"><p:childTnLst>
                   <p:animEffect transition="in" filter="wipe(right)">
                     <p:cBhvr><p:cTn dur="400"/><p:tgtEl><p:spTgt spid="4"/></p:tgtEl></p:cBhvr>
                   </p:animEffect>
                 </p:childTnLst></p:cTn></p:par>
               </p:childTnLst></p:cTn></p:par>
             </p:childTnLst></p:cTn></p:seq>
           </p:childTnLst></p:cTn></p:par></p:tnLst></p:timing></p:sld>"#;
        let root = xml::parse_root("slide1.xml", xml_text).unwrap();
        let (seq, _) = parse(&root);
        assert_eq!(
            seq.steps[0].targets[0].mask,
            Some(MaskKind {
                dir: MaskDir::LeftToRight
            }),
            "wipe(right) = 擦除边缘向右推 = 自左侧露出"
        );
    }

    #[test]
    fn fade_entrance_starts_transparent() {
        let xml_text = r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:timing><p:tnLst><p:par><p:cTn nodeType="tmRoot"><p:childTnLst>
             <p:seq><p:cTn nodeType="mainSeq"><p:childTnLst>
               <p:par><p:cTn fill="hold"><p:stCondLst><p:cond delay="indefinite"/></p:stCondLst><p:childTnLst>
                 <p:par><p:cTn presetClass="entr" nodeType="clickEffect"><p:childTnLst>
                   <p:animEffect transition="in" filter="fade">
                     <p:cBhvr><p:cTn dur="300"/><p:tgtEl><p:spTgt spid="4"/></p:tgtEl></p:cBhvr>
                   </p:animEffect>
                 </p:childTnLst></p:cTn></p:par>
               </p:childTnLst></p:cTn></p:par>
             </p:childTnLst></p:cTn></p:seq>
           </p:childTnLst></p:cTn></p:par></p:tnLst></p:timing></p:sld>"#;
        let root = xml::parse_root("slide1.xml", xml_text).unwrap();
        let (seq, _) = parse(&root);
        let t = &seq.steps[0].targets[0];
        assert_eq!(t.from.opacity, 0.0);
        assert_eq!(t.to.opacity, 1.0);
    }

    #[test]
    fn letter_iterate_becomes_a_reveal() {
        // 逐字出现（打字机）：`p:iterate type="lt"`。近似成左→右的擦除 ——
        // 视觉效果一致，代价是每个字共用同一个时长
        let xml_text = r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:timing><p:tnLst><p:par><p:cTn nodeType="tmRoot"><p:childTnLst>
             <p:seq><p:cTn nodeType="mainSeq"><p:childTnLst>
               <p:par><p:cTn fill="hold"><p:stCondLst><p:cond delay="indefinite"/></p:stCondLst><p:childTnLst>
                 <p:par><p:cTn presetClass="entr" nodeType="clickEffect"><p:stCondLst><p:cond delay="0"/></p:stCondLst><p:iterate type="lt"><p:tmPct val="10000"/></p:iterate><p:childTnLst>
                   <p:set><p:cBhvr><p:cTn dur="1" fill="hold"/><p:tgtEl><p:spTgt spid="5"/></p:tgtEl><p:attrNameLst><p:attrName>style.visibility</p:attrName></p:attrNameLst></p:cBhvr><p:to><p:strVal val="visible"/></p:to></p:set>
                 </p:childTnLst></p:cTn></p:par>
               </p:childTnLst></p:cTn></p:par>
             </p:childTnLst></p:cTn></p:seq>
           </p:childTnLst></p:cTn></p:par></p:tnLst></p:timing></p:sld>"#;
        let root = xml::parse_root("slide1.xml", xml_text).unwrap();
        let (seq, _) = parse(&root);
        let t = &seq.steps[0].targets[0];
        assert_eq!(
            t.mask,
            Some(MaskKind {
                dir: MaskDir::LeftToRight
            }),
            "逐字出现要能看见字一个个冒出来"
        );
        assert!(!t.is_static(), "有遮罩就不是「没有变化」");
    }

    #[test]
    fn absolute_position_expression_is_converted_to_offset() {
        // 不含 `#ppt_*` 的表达式是画布绝对位置，转偏移要减掉形状中心
        let shape = ShapeBox {
            w: 0.2,
            h: 0.1,
            cx: 0.5,
            cy: 0.25,
        };
        assert_eq!(eval_position("#ppt_x", true, shape), Some(0.0));
        assert_eq!(eval_position("#ppt_x-0.1", true, shape), Some(-0.1));
        assert_eq!(eval_position("1+#ppt_h/2", false, shape), Some(1.05));
        assert_eq!(
            eval_position("#ppt_y-1-#ppt_h/2", false, shape),
            Some(-1.05)
        );
        assert_eq!(eval_position("0.5", true, shape), Some(0.0));
        assert_eq!(eval_position("0.8", true, shape), Some(0.3));
    }

    #[test]
    fn exit_effect_is_marked_as_disappear() {
        let xml_text = r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:timing><p:tnLst><p:par><p:cTn nodeType="tmRoot"><p:childTnLst>
             <p:seq><p:cTn nodeType="mainSeq"><p:childTnLst>
               <p:par><p:cTn fill="hold"><p:stCondLst><p:cond delay="indefinite"/></p:stCondLst><p:childTnLst>
                 <p:par><p:cTn presetClass="exit" nodeType="clickEffect"><p:childTnLst>
                   <p:set><p:cBhvr><p:tgtEl><p:spTgt spid="3"/></p:tgtEl></p:cBhvr>
                     <p:to><p:strVal val="hidden"/></p:to></p:set>
                 </p:childTnLst></p:cTn></p:par>
               </p:childTnLst></p:cTn></p:par>
             </p:childTnLst></p:cTn></p:seq>
           </p:childTnLst></p:cTn></p:par></p:tnLst></p:timing></p:sld>"#;
        let root = xml::parse_root("slide1.xml", xml_text).unwrap();
        let (seq, _) = parse(&root);
        assert!(seq.steps[0].is_exit());
    }

    #[test]
    fn no_timing_means_no_animation() {
        let (seq, warnings) = parse(&slide(""));
        assert!(seq.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn effect_duration_is_the_longest_child() {
        // 一个「飞入」由两条并行子动画组成：置可见性 `dur="1"`、
        // 位移 `dur="500"`。取第一个会把 500ms 的飞入当成 1ms 的瞬变。
        let xml_text = r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:timing><p:tnLst><p:par><p:cTn nodeType="tmRoot"><p:childTnLst>
             <p:seq><p:cTn nodeType="mainSeq"><p:childTnLst>
               <p:par><p:cTn fill="hold"><p:stCondLst><p:cond delay="indefinite"/></p:stCondLst><p:childTnLst>
                 <p:par><p:cTn presetClass="entr" nodeType="clickEffect"><p:childTnLst>
                   <p:set><p:cBhvr><p:cTn dur="1"/><p:tgtEl><p:spTgt spid="2"/></p:tgtEl></p:cBhvr>
                     <p:to><p:strVal val="visible"/></p:to></p:set>
                   <p:anim><p:cBhvr><p:cTn dur="500"/><p:tgtEl><p:spTgt spid="2"/></p:tgtEl></p:cBhvr></p:anim>
                 </p:childTnLst></p:cTn></p:par>
               </p:childTnLst></p:cTn></p:par>
             </p:childTnLst></p:cTn></p:seq>
           </p:childTnLst></p:cTn></p:par></p:tnLst></p:timing></p:sld>"#;
        let root = xml::parse_root("slide1.xml", xml_text).unwrap();
        let (seq, _) = parse(&root);
        assert_eq!(seq.steps[0].dur_ms, 500);
    }

    #[test]
    fn pure_appear_has_almost_no_duration() {
        // 只有「出现」没有过程：`dur="1"`，前端会当成瞬间完成
        let (seq, _) = parse(&slide(entrance_effect()));
        assert_eq!(seq.steps[0].dur_ms, 1);
    }

    #[test]
    fn interactive_sequence_becomes_a_shape_trigger() {
        // 交互序列：点 spid=7 才播它下面的动画，点空白不消耗这一步
        let xml_text = r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:timing><p:tnLst><p:par><p:cTn nodeType="tmRoot"><p:childTnLst>
             <p:seq><p:cTn nodeType="mainSeq"><p:childTnLst>
               <p:par><p:cTn fill="hold"><p:stCondLst><p:cond delay="indefinite"/></p:stCondLst><p:childTnLst>
                 <p:par><p:cTn presetClass="entr" nodeType="clickEffect"><p:childTnLst>
                   <p:set><p:cBhvr><p:tgtEl><p:spTgt spid="3"/></p:tgtEl></p:cBhvr>
                     <p:to><p:strVal val="visible"/></p:to></p:set>
                 </p:childTnLst></p:cTn></p:par>
               </p:childTnLst></p:cTn></p:par>
             </p:childTnLst></p:cTn></p:seq>
             <p:seq><p:cTn nodeType="interactiveSeq"><p:stCondLst>
                 <p:cond evt="onClick" delay="0"><p:tgtEl><p:spTgt spid="7"/></p:tgtEl><p:tn val="2"/></p:cond>
               </p:stCondLst><p:childTnLst>
               <p:par><p:cTn fill="hold"><p:stCondLst><p:cond delay="0"/></p:stCondLst><p:childTnLst>
                 <p:par><p:cTn presetClass="entr" nodeType="clickEffect"><p:childTnLst>
                   <p:set><p:cBhvr><p:tgtEl><p:spTgt spid="9"/></p:tgtEl></p:cBhvr>
                     <p:to><p:strVal val="visible"/></p:to></p:set>
                 </p:childTnLst></p:cTn></p:par>
               </p:childTnLst></p:cTn></p:par>
             </p:childTnLst></p:cTn></p:seq>
           </p:childTnLst></p:cTn></p:par></p:tnLst></p:timing></p:sld>"#;
        let root = xml::parse_root("slide1.xml", xml_text).unwrap();
        let (seq, _) = parse(&root);
        assert_eq!(seq.len(), 2, "主序列一步 + 交互序列一步");
        assert_eq!(seq.steps[0].trigger, StepTrigger::OnClick);
        assert_eq!(
            seq.steps[1].trigger,
            StepTrigger::OnShape(7),
            "第二步要等点了 7 号形状才播"
        );
        assert_eq!(seq.steps[1].targets[0].shape_id, 9, "它动的是 9 号");
        assert!(seq.steps[1].needs_input(), "触发器也算「要输入」");
    }

    #[test]
    fn interactive_sequence_without_target_is_dropped() {
        // 认得出是交互序列却找不到触发对象：宁可丢掉，
        // 也不要退化成「点空白就能播」—— 那会让老师多点一下才翻页
        let xml_text = r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:timing><p:tnLst><p:par><p:cTn nodeType="tmRoot"><p:childTnLst>
             <p:seq><p:cTn nodeType="interactiveSeq"><p:childTnLst>
               <p:par><p:cTn presetClass="entr" nodeType="clickEffect"><p:childTnLst>
                 <p:set><p:cBhvr><p:tgtEl><p:spTgt spid="9"/></p:tgtEl></p:cBhvr>
                   <p:to><p:strVal val="visible"/></p:to></p:set>
               </p:childTnLst></p:cTn></p:par>
             </p:childTnLst></p:cTn></p:seq>
           </p:childTnLst></p:cTn></p:par></p:tnLst></p:timing></p:sld>"#;
        let root = xml::parse_root("slide1.xml", xml_text).unwrap();
        let (seq, warnings) = parse(&root);
        assert!(seq.is_empty(), "没有触发对象的交互序列不该变成普通点击步");
        assert!(warnings.iter().any(|w| w.contains("触发器")), "{warnings:?}");
    }
}
