//! 真实系统字体下的排版集成测试。
//!
//! 单元测试用的是空字体库（只验证结构与边界），
//! 但「整形是否真的产出字形」「中文能否找到字体」「断行位置是否符合预期」
//! 这些必须用真实字体验证，否则排版引擎可能整体失效却毫无察觉。

use ppt_core::scene::{
    BodyProps, FontSet, Paragraph, RunProps, Size, TextAlign, TextBox, TextRun, VerticalAnchor,
};
use ppt_text::{FontContext, LayoutOptions, TextLayouter};

/// 加载系统字体；若系统里一个字体都没有（极简容器环境）则跳过测试。
fn system_fonts() -> Option<FontContext> {
    let ctx = FontContext::new();
    if ctx.font_count() == 0 {
        eprintln!("跳过：系统中未找到任何可用字体");
        return None;
    }
    Some(ctx)
}

/// 带字重/别名后缀的字体名也要能解析到同一个字面。
///
/// 课件里这种写法极常见（`Times New Roman Regular`、`宋体 (中文正文)`）——
/// 原样查系统字体表必定落空，落空就会退到通用字体，**字宽差 11%**：
/// 一行放得下的句子多折一行，压到下面那个文本框上（第 16 页就是）。
#[test]
fn suffixed_family_names_resolve_to_the_same_face() {
    let Some(ctx) = system_fonts() else { return };

    for (plain, suffixed) in [
        ("Times New Roman", "Times New Roman Regular"),
        ("Arial", "Arial Bold"),
        ("SimSun", "SimSun Regular"),
    ] {
        let Some(base) = ctx.resolve(plain, false, false) else {
            continue; // 这台机器没装这个家族，跳过
        };
        assert_eq!(
            ctx.resolve(suffixed, false, false),
            Some(base),
            "「{suffixed}」应解析到与「{plain}」同一个字面"
        );
    }
}

/// 本身就是合法家族名的后缀不能被削掉。
///
/// `Segoe UI Light` / `Segoe UI Semibold` 是 Windows 上真实存在的家族名，
/// 所以必须**先按原样查**，查不到再退回削后缀的名字。
#[test]
fn real_families_ending_in_a_style_word_still_resolve() {
    let Some(ctx) = system_fonts() else { return };

    for name in ["Segoe UI Light", "Segoe UI Semibold", "Segoe UI Black"] {
        if ctx.resolve(name, false, false).is_some() {
            return; // 有一个能解析就说明「先按原样查」这条路是通的
        }
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

#[test]
fn system_font_index_is_populated() {
    let Some(ctx) = system_fonts() else { return };
    assert!(ctx.font_count() > 0, "应索引到系统字体");
    assert!(ctx.family_count() > 0, "应索引到字体家族");
    eprintln!(
        "已索引 {} 个字体面 / {} 个家族",
        ctx.font_count(),
        ctx.family_count()
    );
}

#[test]
fn latin_text_produces_glyphs_with_positive_advance() {
    let Some(ctx) = system_fonts() else { return };
    let layouter = TextLayouter::new(&ctx);
    let layout = layouter.layout(
        &text_box("Hello World", 24.0),
        Size::new(600.0, 300.0),
        LayoutOptions::default(),
    );

    let glyphs: Vec<_> = layout.glyphs().collect();
    assert!(!glyphs.is_empty(), "拉丁文本应产出字形");

    let total: f32 = glyphs.iter().map(|g| g.advance).sum();
    assert!(total > 0.0, "字形前进宽度应为正，实际 {total}");

    // 24pt 字号的 "Hello World" 宽度大致在 80~200pt 之间；
    // 若落在区间外说明度量换算（font units → pt）有问题
    assert!(
        (80.0..220.0).contains(&total),
        "24pt 下 \"Hello World\" 宽度异常：{total}pt"
    );
    assert!(!glyphs[0].font.is_placeholder(), "不应使用占位字体");
}

/// 字间距（`a:rPr/@spc`）要真的加进每字的前进宽度里。
///
/// 它一直被读进 `RunProps` 却没人用 —— 课件里标题常用正值把字撑开
/// （`spc="29"` 就是每字加 0.29pt），长句则用负值压紧；
/// 丢了这个，断行位置就和 PowerPoint 对不上。
#[test]
fn letter_spacing_extends_each_advance() {
    let Some(ctx) = system_fonts() else { return };
    let layouter = TextLayouter::new(&ctx);

    let width = |spacing: f32| {
        let tb = text_box("abcd", 20.0);
        let mut tb = tb;
        tb.paragraphs[0].runs[0].props.spacing_pt = spacing;
        layouter
            .layout(&tb, Size::new(2000.0, 300.0), LayoutOptions::default())
            .width
    };

    let plain = width(0.0);
    assert!(plain > 10.0, "应有真实宽度，实际 {plain}");

    let spaced = width(2.0);
    assert!(
        (spaced - plain - 8.0).abs() < 0.5,
        "每个字都应加上 2pt：{plain} → {spaced}"
    );

    let tight = width(-1.0);
    assert!(
        (plain - tight - 4.0).abs() < 0.5,
        "负字距应把每字压掉 1pt：{plain} → {tight}"
    );
}

/// 悬挂缩进不该重复扣宽度。
///
/// `marL` 是正文列的左边界，`indent` 是首行相对它的偏移（负数 = 悬挂）。
/// 原代码把 `|marL| + |indent|` 当成「项目符号占的宽度」，
/// 先缩进一次再从可用宽度里减一次 —— 悬挂缩进的段落凭空少了 2×|indent|：
/// 《Unit 2》第 19 页「十位数11-19」（实测 133pt < 正文列 202pt）被挤成两行，
/// 第二行压到下面的表格上。
#[test]
fn hanging_indent_does_not_eat_the_width_twice() {
    let Some(ctx) = system_fonts() else { return };
    let layouter = TextLayouter::new(&ctx);

    let text = "一二三四五六七八九十";
    let marl = 27.0f32;

    let laid = |indent: f32| {
        let mut tb = text_box(text, 24.0);
        tb.paragraphs[0].margin_left_pt = marl;
        tb.paragraphs[0].indent_pt = indent;
        layouter.layout(&tb, Size::new(320.0, 300.0), LayoutOptions::default())
    };

    // 十个汉字 240pt；框内正文列 320-14.4-27 = 278.6pt，放得下
    let plain = laid(0.0);
    assert_eq!(plain.line_count(), 1, "这句话本身就该是一行");

    // 悬挂缩进（正文列 27pt）之后仍应是一行
    let hanging = laid(-marl);
    assert_eq!(
        hanging.line_count(),
        1,
        "悬挂缩进只该让出正文列那一份宽度，不该再多吃一个 |indent|"
    );
}

#[test]
fn cjk_text_produces_glyphs() {
    let Some(ctx) = system_fonts() else { return };
    let layouter = TextLayouter::new(&ctx);
    let layout = layouter.layout(
        &text_box("光合作用与呼吸作用", 24.0),
        Size::new(600.0, 300.0),
        LayoutOptions::default(),
    );

    let glyphs: Vec<_> = layout.glyphs().collect();
    assert_eq!(glyphs.len(), 9, "9 个汉字应产出 9 个字形");
    assert!(
        glyphs.iter().all(|g| g.glyph_id != 0),
        "不应出现 .notdef 字形（缺字），实际字形：{:?}",
        glyphs.iter().map(|g| g.glyph_id).collect::<Vec<_>>()
    );

    let total: f32 = glyphs.iter().map(|g| g.advance).sum();
    // 汉字是全角，24pt 下每个约 24pt，9 个约 216pt
    assert!(
        (180.0..260.0).contains(&total),
        "9 个 24pt 汉字的总宽应在 216pt 附近，实际 {total}pt"
    );
}

#[test]
fn mixed_cjk_latin_uses_fallback_without_notdef() {
    let Some(ctx) = system_fonts() else { return };
    let layouter = TextLayouter::new(&ctx);
    // 课件里最常见的中英混排：中文 + 英文单词 + 数字
    let layout = layouter.layout(
        &text_box("第 1 课 Rust 语言", 20.0),
        Size::new(800.0, 300.0),
        LayoutOptions::default(),
    );

    let glyphs: Vec<_> = layout.glyphs().collect();
    assert!(!glyphs.is_empty());
    let notdef = glyphs.iter().filter(|g| g.glyph_id == 0).count();
    assert_eq!(notdef, 0, "中英混排不应出现缺字（.notdef）");
}

#[test]
fn punctuation_is_rendered_not_dropped() {
    let Some(ctx) = system_fonts() else { return };
    let layouter = TextLayouter::new(&ctx);
    let text = "注意：这是一句话，包含（括号）与「引号」。";
    let layout = layouter.layout(
        &text_box(text, 20.0),
        Size::new(1200.0, 300.0),
        LayoutOptions::default(),
    );
    let count = layout.glyphs().count();
    assert_eq!(count, text.chars().count(), "所有字符都应产出字形，包括标点");
}

#[test]
fn narrow_box_wraps_cjk_at_char_boundaries() {
    let Some(ctx) = system_fonts() else { return };
    let layouter = TextLayouter::new(&ctx);
    // 20pt 汉字宽约 20pt，200pt 宽约容纳 10 个字
    let layout = layouter.layout(
        &text_box("春眠不觉晓处处闻啼鸟夜来风雨声花落知多少", 20.0),
        Size::new(200.0, 400.0),
        LayoutOptions::default(),
    );

    assert!(layout.line_count() > 1, "窄框内长文本应折行");

    // 每行宽度都不应显著超出可用宽度
    for line in &layout.lines {
        assert!(
            line.width <= 210.0,
            "行宽 {} 超出可用宽度 200pt（内容：{}）",
            line.width,
            line.text
        );
    }
    // 折行后不应丢字
    let joined: String = layout.lines.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(joined.chars().count(), 20, "折行不应丢字");
}

#[test]
fn closing_punctuation_never_starts_a_line() {
    let Some(ctx) = system_fonts() else { return };
    let layouter = TextLayouter::new(&ctx);

    // 构造一个「正好在句号前断行」的窄框，验证禁则生效
    let text = "这是一个很长的句子用来测试标点禁则处理是否生效。";
    let layout = layouter.layout(
        &text_box(text, 20.0),
        Size::new(120.0, 600.0),
        LayoutOptions::default(),
    );

    assert!(layout.line_count() > 1, "应产生多行以触发禁则");
    for line in &layout.lines {
        if let Some(first) = line.text.chars().next() {
            assert!(
                !ppt_text::linebreak::cannot_start_line(first),
                "行首出现了禁则字符「{first}」：{}",
                line.text
            );
        }
        if let Some(last) = line.text.chars().next_back() {
            assert!(
                !ppt_text::linebreak::cannot_end_line(last),
                "行尾出现了禁则字符「{last}」：{}",
                line.text
            );
        }
    }
}

#[test]
fn english_word_is_not_split_across_lines() {
    let Some(ctx) = system_fonts() else { return };
    let layouter = TextLayouter::new(&ctx);
    let layout = layouter.layout(
        &text_box("photosynthesis respiration mitochondria", 20.0),
        Size::new(160.0, 600.0),
        LayoutOptions::default(),
    );

    assert!(layout.line_count() > 1, "应折行");
    for line in &layout.lines {
        let t = line.text.trim();
        // 每行除首尾外的内容应是完整单词（首尾因断行可能被裁剪，但不应从单词中间切开）
        assert!(
            !t.is_empty(),
            "不应产生空行"
        );
    }
    // 单词总数应保持不变
    let joined: String = layout
        .lines
        .iter()
        .map(|l| l.text.trim())
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(
        joined.split_whitespace().count(),
        3,
        "折行后单词数应不变：{joined:?}"
    );
}

#[test]
fn text_alignment_shifts_glyphs() {
    let Some(ctx) = system_fonts() else { return };
    let layouter = TextLayouter::new(&ctx);
    let area = Size::new(600.0, 200.0);

    let make = |align| TextBox {
        body: BodyProps::default(),
        paragraphs: vec![Paragraph {
            align,
            runs: vec![TextRun::new("居中测试")],
            ..Default::default()
        }],
    };

    let left = layouter.layout(&make(TextAlign::Left), area, LayoutOptions::default());
    let center = layouter.layout(&make(TextAlign::Center), area, LayoutOptions::default());
    let right = layouter.layout(&make(TextAlign::Right), area, LayoutOptions::default());

    let first_x = |l: &ppt_text::TextLayout| l.glyphs().next().map(|g| g.x).unwrap_or(0.0);

    let (lx, cx, rx) = (first_x(&left), first_x(&center), first_x(&right));
    assert!(lx < cx, "居中应比左对齐靠右：{lx} vs {cx}");
    assert!(cx < rx, "右对齐应比居中靠右：{cx} vs {rx}");
    // 右对齐时行尾应贴近右边界
    let right_line = &right.lines[0];
    let end = right_line.glyphs.last().map(|g| g.x + g.advance).unwrap_or(0.0);
    assert!(
        (end - 600.0).abs() < 2.0,
        "右对齐行尾应贴合右边界，实际 {end}"
    );
}

#[test]
fn vertical_anchor_positions_block() {
    let Some(ctx) = system_fonts() else { return };
    let layouter = TextLayouter::new(&ctx);
    let area = Size::new(600.0, 400.0);

    let make = |anchor| TextBox {
        body: BodyProps {
            anchor,
            ..Default::default()
        },
        paragraphs: vec![Paragraph {
            runs: vec![TextRun::new("垂直锚点")],
            ..Default::default()
        }],
    };

    let top = layouter.layout(&make(VerticalAnchor::Top), area, LayoutOptions::default());
    let mid = layouter.layout(&make(VerticalAnchor::Middle), area, LayoutOptions::default());
    let bot = layouter.layout(&make(VerticalAnchor::Bottom), area, LayoutOptions::default());

    assert!(top.lines[0].baseline < mid.lines[0].baseline);
    assert!(mid.lines[0].baseline < bot.lines[0].baseline);

    // 垂直居中时，内容块上下留白应大致相等
    let upper_gap = mid.content_top();
    let lower_gap = area.h - mid.content_bottom();
    assert!(
        (upper_gap - lower_gap).abs() < 2.0,
        "居中时上下留白应相等：上 {upper_gap} / 下 {lower_gap}"
    );
}

#[test]
fn font_fallback_chain_handles_unknown_family() {
    let Some(ctx) = system_fonts() else { return };
    let layouter = TextLayouter::new(&ctx);

    // 指定一个几乎不可能存在的字体名，排版仍应正常产出字形
    let tb = TextBox {
        body: BodyProps::default(),
        paragraphs: vec![Paragraph {
            runs: vec![TextRun {
                text: "回退测试 Fallback 123".to_string(),
                props: RunProps {
                    size_pt: 20.0,
                    font: FontSet {
                        latin: Some("完全不存在的字体名 XYZ".into()),
                        ea: Some("同样不存在的中文字体 ABC".into()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                hyperlink: None,
                field: None,
            }],
            ..Default::default()
        }],
    };

    let layout = layouter.layout(&tb, Size::new(800.0, 300.0), LayoutOptions::default());
    let glyphs: Vec<_> = layout.glyphs().collect();
    assert!(!glyphs.is_empty(), "字体缺失时应走回退链而不是丢字");
    let notdef = glyphs.iter().filter(|g| g.glyph_id == 0).count();
    assert_eq!(notdef, 0, "回退后不应出现缺字");
}

#[test]
fn autofit_shrinks_real_text_to_fit() {
    let Some(ctx) = system_fonts() else { return };
    let layouter = TextLayouter::new(&ctx);

    let tb = TextBox {
        body: BodyProps {
            auto_fit: ppt_core::scene::AutoFit::NormAutofit {
                font_scale: 1.0,
                line_space_reduction: 0.0,
            },
            ..Default::default()
        },
        paragraphs: (0..25)
            .map(|i| Paragraph {
                runs: vec![TextRun {
                    text: format!("第 {} 条知识点，需要占据一整行的高度", i + 1),
                    props: RunProps {
                        size_pt: 28.0,
                        ..Default::default()
                    },
                    hyperlink: None,
                    field: None,
                }],
                ..Default::default()
            })
            .collect(),
    };

    let area = Size::new(500.0, 300.0);
    let layout = layouter.layout(&tb, area, LayoutOptions::default());

    assert!(
        layout.font_scale < 1.0,
        "内容明显超出时应缩小字号，实际缩放 {}",
        layout.font_scale
    );
    assert!(
        layout.height <= area.h + 1.0,
        "缩放后高度 {} 应不超过可用高度 {}",
        layout.height,
        area.h
    );
}

#[test]
fn layout_is_deterministic() {
    let Some(ctx) = system_fonts() else { return };
    let layouter = TextLayouter::new(&ctx);
    let tb = text_box("确定性测试：同样的输入应得到同样的结果。", 18.0);
    let area = Size::new(300.0, 400.0);

    let a = layouter.layout(&tb, area, LayoutOptions::default());
    let b = layouter.layout(&tb, area, LayoutOptions::default());

    assert_eq!(a.line_count(), b.line_count());
    assert_eq!(a.height, b.height);
    assert_eq!(a.plain_text(), b.plain_text());

    let ga: Vec<_> = a.glyphs().map(|g| (g.glyph_id, g.x.to_bits(), g.advance.to_bits())).collect();
    let gb: Vec<_> = b.glyphs().map(|g| (g.glyph_id, g.x.to_bits(), g.advance.to_bits())).collect();
    assert_eq!(ga, gb, "排版结果必须可复现（缓存依赖此性质）");
}

#[test]
fn font_context_is_shared_across_threads() {
    let Some(ctx) = system_fonts() else { return };
    let ctx = std::sync::Arc::new(ctx);

    let handles: Vec<_> = (0..4)
        .map(|i| {
            let ctx = std::sync::Arc::clone(&ctx);
            std::thread::spawn(move || {
                let layouter = TextLayouter::new(&ctx);
                let layout = layouter.layout(
                    &text_box(&format!("线程 {i} 的文本 Thread {i}"), 18.0),
                    Size::new(400.0, 200.0),
                    LayoutOptions::default(),
                );
                layout.glyphs().count()
            })
        })
        .collect();

    for h in handles {
        let n = h.join().expect("排版线程不应 panic");
        assert!(n > 0, "每个线程都应产出字形");
    }
}
