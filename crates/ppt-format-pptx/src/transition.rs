//! 页间转场：`p:transition` → [`Transition`]。
//!
//! # 位置约定
//!
//! 转场写在**进入的那一页**上：第 3 页的 `p:transition` 描述的是
//! 「从第 2 页切到第 3 页」怎么演。所以渲染端是在「翻到本页时」取本页的转场，
//! 而不是取上一页的。
//!
//! # 效果与方向是两个维度
//!
//! `<p:wipe dir="d"/>`、`<p:push dir="l"/>`、`<p:cover dir="u"/>` 共用一套方向词表
//! （`l`/`r`/`u`/`d`/`lu`/`rd`…），而 `<p:split>` 用的是 `orient` + `dir="in|out"`,
//! `<p:zoom>` 用的是 `dir="in|out"`。这里统一折算成 [`TransitionDir`]，
//! 前端就只需要处理「效果 × 方向」两个正交维度。
//!
//! # 认得出来但没实现的
//!
//! `cube`/`vortex`/`morph` 这些三维转场需要真正的几何变换，
//! 解析阶段保留原名（[`TransitionKind::Other`]），渲染端退化为淡入 ——
//! 但**不静默**：名字留着，诊断时一眼能看出「是它没实现」，而不是「解析漏了」。

use ppt_core::scene::{Transition, TransitionDir, TransitionKind};
use ppt_core::XmlNode;

/// 解析一页的转场。
pub fn parse_transition(slide: &XmlNode) -> Transition {
    let mut t = Transition {
        advance_on_click: true,
        ..Transition::default()
    };
    let Some(node) = transition_element(slide) else {
        return t;
    };

    t.dur_ms = duration_ms(node);
    t.through_black = node.attr_bool_or("thruBlk", false);
    t.advance_on_click = node.attr_bool_or("advClick", true);
    t.advance_after_ms = node
        .attr_u32("advTm")
        .filter(|v| *v > 0)
        .map(|v| v.max(100));

    // 真正的效果是 `p:transition` 的第一个子元素；`p14:morph` 一类会被包在
    // `mc:AlternateContent` 里，所以找不到时再往里钻一层。
    let Some(effect) = effect_element(node) else {
        return t;
    };

    let name = effect.name.to_ascii_lowercase();
    t.kind = kind_of(&name);
    t.spokes = effect.attr_u32("spokes").unwrap_or(0).min(32) as u8;
    t.dir = direction_of(&name, effect);

    // 解析出来的时长若是 0（没写），补一个像样的默认值，
    // 否则前端会把它当「瞬切」，效果等于没设
    if t.dur_ms == 0 {
        t.dur_ms = 500;
    }
    t
}

/// 找到这一页的 `p:transition`。
///
/// # 为什么要钻一层 `mc:AlternateContent`
///
/// 现代 PowerPoint 会把**整个** `p:transition` 包进 `mc:AlternateContent`：
/// `mc:Choice Requires="p14"` 里是带 `p14:dur` 的版本，`mc:Fallback` 里是空壳。
/// 严格按 `mc` 的规矩（不认得 p14 就走 Fallback）只能拿到空壳，
/// 效果与时长全丢了 —— 而 `p:transition` 本身是基础规范的元素，
/// 我们认识的，只是多一个不认识的属性。所以这里优先读 `mc:Choice`。
fn transition_element(slide: &XmlNode) -> Option<&XmlNode> {
    if let Some(t) = slide.child("transition") {
        return Some(t);
    }
    let alt = slide.child("AlternateContent")?;
    for branch in ["Choice", "Fallback"] {
        if let Some(t) = alt.child(branch).and_then(|b| b.child("transition")) {
            return Some(t);
        }
    }
    None
}

/// 效果元素：`p:transition` 的直接子元素。
///
/// 现代 PowerPoint 会把 `p14:morph` 这类扩展效果包在 `mc:AlternateContent` 里，
/// 用 `mc:Choice Requires="p14"` 标出「认得 p14 就用它」。
/// 我们实现的是基础效果集，所以按 OOXML 的规矩**走 `mc:Fallback`** ——
/// 那是老版本 PowerPoint 会看到的样子，而不是把不认识的效果硬认下来。
fn effect_element(node: &XmlNode) -> Option<&XmlNode> {
    let direct = node
        .children
        .iter()
        .find(|c| !matches!(c.name.as_str(), "extLst" | "AlternateContent"));
    if let Some(c) = direct {
        return Some(c);
    }
    let alt = node.child("AlternateContent")?;
    let branch = alt.child("Fallback").or_else(|| alt.child("Choice"))?;
    branch.children.iter().find(|c| c.name != "extLst")
}

fn kind_of(name: &str) -> TransitionKind {
    match name {
        "fade" => TransitionKind::Fade,
        "dissolve" => TransitionKind::Dissolve,
        "push" => TransitionKind::Push,
        "cover" => TransitionKind::Cover,
        "uncover" => TransitionKind::Uncover,
        "wipe" => TransitionKind::Wipe,
        "split" => TransitionKind::Split,
        "zoom" => TransitionKind::Zoom,
        "blinds" => TransitionKind::Blinds,
        "checker" => TransitionKind::Checker,
        "comb" => TransitionKind::Comb,
        "strips" => TransitionKind::Strips,
        "wheel" => TransitionKind::Wheel,
        "circle" => TransitionKind::Circle,
        "diamond" => TransitionKind::Diamond,
        "plus" => TransitionKind::Plus,
        "wedge" => TransitionKind::Wedge,
        "doors" => TransitionKind::Doors,
        "window" => TransitionKind::Window,
        "newsflash" => TransitionKind::Newsflash,
        "glitter" => TransitionKind::Glitter,
        "honeycomb" => TransitionKind::Honeycomb,
        "shred" => TransitionKind::Shred,
        "flash" => TransitionKind::Flash,
        "ripple" => TransitionKind::Ripple,
        "pan" => TransitionKind::Pan,
        "reveal" => TransitionKind::Reveal,
        "switch" => TransitionKind::Switch,
        "ferris" => TransitionKind::Ferris,
        "flythrough" => TransitionKind::Flythrough,
        "prestige" => TransitionKind::Prestige,
        "fallover" => TransitionKind::Fallover,
        "drape" => TransitionKind::Drape,
        "curtains" => TransitionKind::Curtains,
        "wind" => TransitionKind::Wind,
        "orbit" => TransitionKind::Orbit,
        "random" => TransitionKind::Random,
        other => TransitionKind::Other(other.to_string()),
    }
}

/// 方向：`split` 用 `orient` + `dir="in|out"`，其余用 `dir` 的四/八向词表。
fn direction_of(kind_lower: &str, effect: &XmlNode) -> TransitionDir {
    let dir = effect.attr("dir").unwrap_or("");
    if kind_lower == "split" {
        let vert = effect
            .attr("orient")
            .is_some_and(|o| o.eq_ignore_ascii_case("vert"));
        return match (vert, dir) {
            (true, "in") => TransitionDir::VertIn,
            (true, _) => TransitionDir::VertOut,
            (false, "in") => TransitionDir::HorzIn,
            (false, _) => TransitionDir::HorzOut,
        };
    }
    match dir {
        "l" => TransitionDir::Left,
        "r" => TransitionDir::Right,
        "u" => TransitionDir::Up,
        "d" => TransitionDir::Down,
        "lu" => TransitionDir::LeftUp,
        "ru" => TransitionDir::RightUp,
        "ld" => TransitionDir::LeftDown,
        "rd" => TransitionDir::RightDown,
        "in" => TransitionDir::In,
        "out" => TransitionDir::Out,
        _ => TransitionDir::None,
    }
}

/// 时长（毫秒）。
///
/// `p14:dur` 才是作者设的时长；但它会被 WPS 之类的工具在「无效果」的转场上
/// 写成 `10` 充数，这种明显不成时长的值直接忽略，退回 `spd` 档位。
fn duration_ms(node: &XmlNode) -> u32 {
    if let Some(d) = node.attr_u32("dur") {
        if d >= 50 {
            return d.min(20_000);
        }
    }
    match node.attr("spd") {
        Some("slow") => 1000,
        Some("med") => 750,
        _ => 500,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ppt_core::xml;

    fn slide(body: &str) -> XmlNode {
        let text = format!("<p:sld xmlns:p=\"urn:p\" xmlns:a=\"urn:a\" xmlns:p14=\"urn:p14\">{body}</p:sld>");
        xml::parse_root("slide1.xml", &text).expect("测试 XML 应能解析")
    }

    #[test]
    fn no_transition_is_a_cut() {
        let t = parse_transition(&slide(""));
        assert!(t.is_none());
        assert!(t.advance_on_click, "默认响应点击前进");
        assert!(t.advance_after_ms.is_none());
    }

    #[test]
    fn empty_transition_is_a_cut() {
        // WPS 会写 `<p:transition p14:dur="10"/>` 表示「没有转场」
        let t = parse_transition(&slide(r#"<p:transition p14:dur="10"/>"#));
        assert!(t.is_none(), "没有效果元素就是直接切");
    }

    #[test]
    fn wipe_reads_direction_and_duration() {
        let t = parse_transition(&slide(
            r#"<p:transition p14:dur="800" spd="slow"><p:wipe dir="d"/></p:transition>"#,
        ));
        assert_eq!(t.kind, TransitionKind::Wipe);
        assert_eq!(t.dir, TransitionDir::Down);
        assert_eq!(t.dur_ms, 800, "p14:dur 优先于 spd");
    }

    #[test]
    fn split_uses_orient() {
        let t = parse_transition(&slide(
            r#"<p:transition spd="fast"><p:split orient="vert" dir="in"/></p:transition>"#,
        ));
        assert_eq!(t.kind, TransitionKind::Split);
        assert_eq!(t.dir, TransitionDir::VertIn);
        assert_eq!(t.dur_ms, 500);
    }

    #[test]
    fn advance_timing_is_kept() {
        let t = parse_transition(&slide(
            r#"<p:transition advClick="0" advTm="3000"><p:push dir="l"/></p:transition>"#,
        ));
        assert_eq!(t.kind, TransitionKind::Push);
        assert_eq!(t.dir, TransitionDir::Left);
        assert!(!t.advance_on_click, "作者取消了点击前进");
        assert_eq!(t.advance_after_ms, Some(3000));
    }

    #[test]
    fn unknown_effect_keeps_its_name() {
        let t = parse_transition(&slide(r#"<p:transition><p:vortex/></p:transition>"#));
        assert_eq!(t.kind, TransitionKind::Other("vortex".into()));
    }

    #[test]
    fn transition_wrapped_in_alternate_content_is_found() {
        // 真实课件里见过这种写法：整个 `p:transition` 被包在 `mc:AlternateContent` 里，
        // Choice 是带 `p14:dur` 的版本、Fallback 是空壳。
        // 只找直接子元素的话，这一页的转场会被整个当成「没有」。
        let t = parse_transition(&slide(
            r#"<mc:AlternateContent xmlns:mc="urn:mc"><mc:Choice Requires="p14"><p:transition p14:dur="700"><p:push dir="l"/></p:transition></mc:Choice><mc:Fallback><p:transition/></mc:Fallback></mc:AlternateContent>"#,
        ));
        assert_eq!(t.kind, TransitionKind::Push);
        assert_eq!(t.dir, TransitionDir::Left);
        assert_eq!(t.dur_ms, 700);
    }

    #[test]
    fn morph_falls_back_to_the_declared_fallback() {
        // `mc:Choice Requires="p14"` 的意思是「认得 p14 才用它」。
        // 我们不认 p14，就该按规范走 Fallback，而不是硬认下 morph 再画不出来。
        let t = parse_transition(&slide(
            r#"<p:transition><mc:AlternateContent xmlns:mc="urn:mc"><mc:Choice Requires="p14"><p14:morph option="byObject"/></mc:Choice><mc:Fallback><p:fade/></mc:Fallback></mc:AlternateContent></p:transition>"#,
        ));
        assert_eq!(t.kind, TransitionKind::Fade);
    }
}
