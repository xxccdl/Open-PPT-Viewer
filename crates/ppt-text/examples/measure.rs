//! 临时诊断工具：给一段文字与可用宽度，打印它排成几行、每行多宽。
//!
//! 用来核对「我们换行比 PowerPoint 早」这类问题 —— 行数差一行，
//! 表格就整体高出几十点、直接顶出页面底部。
//!
//! 用法：
//!   cargo run -p ppt-text --example measure -- "<文本>" <可用宽度pt> <字号pt> [i=斜体] [拉丁字体名]

use ppt_core::scene::{FontSet, Paragraph, RunProps, Size, TextBox, TextRun};
use ppt_text::{FontContext, LayoutOptions, TextLayouter};

fn main() {
    let mut args = std::env::args().skip(1);
    // 文本可以用 `@文件` 给：中文直接写在命令行上时，
    // Windows 的 AMSI 扫描偶尔会把整个 PowerShell 会话搞崩
    let text = match args.next() {
        Some(a) if a.starts_with('@') => {
            std::fs::read_to_string(&a[1..]).expect("读文本文件失败")
        }
        other => other.unwrap_or_default(),
    };
    let text = text.trim_end_matches(['\r', '\n']).to_string();
    let width: f32 = args.next().and_then(|v| v.parse().ok()).unwrap_or(229.5);
    let size: f32 = args.next().and_then(|v| v.parse().ok()).unwrap_or(22.0);
    let italic = args.next().map(|v| v == "i").unwrap_or(false);
    let latin = args
        .next()
        .unwrap_or_else(|| "Times New Roman".to_string());

    let fonts = FontContext::new();
    println!("字体索引：{} 个字体面", fonts.font_count());

    // 实际选中的是哪一张字面：找不到时会回退，宽度差异往往就出在这里
    for (label, family, bold, it) in [
        ("拉丁", latin.as_str(), false, italic),
        ("东亚", "黑体", false, false),
        ("东亚", "宋体", false, false),
        ("东亚", "SimHei", false, false),
        ("东亚", "微软雅黑", false, false),
    ] {
        let picked = fonts
            .resolve(family, bold, it)
            .and_then(|id| fonts.font(id))
            .map(|f| format!("{}（行高 {:.4}em）", f.family, f.line_height_em()))
            .unwrap_or_else(|| "未命中".to_string());
        println!("{label} {family:<12} → {picked}");
    }

    let props = RunProps {
        size_pt: size,
        italic,
        font: FontSet {
            latin: Some(latin.clone()),
            ea: Some("黑体".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };

    let tb = TextBox {
        body: Default::default(),
        paragraphs: vec![Paragraph {
            runs: vec![TextRun {
                text: text.clone(),
                props,
                hyperlink: None,
                field: None,
            }],
            ..Default::default()
        }],
    };

    let layout = TextLayouter::new(&fonts).layout(
        &tb,
        Size::new(width, 100_000.0),
        LayoutOptions::default(),
    );

    println!(
        "「{}…」字号 {}pt{} 字体 {} 可用宽 {:.1}pt",
        text.chars().take(8).collect::<String>(),
        size,
        if italic { " 斜体" } else { "" },
        latin,
        width
    );
    println!(
        "→ {} 行，总高 {:.2}pt，行高 {:.2}pt",
        layout.line_count(),
        layout.height,
        layout.height / layout.line_count().max(1) as f32
    );
    for (i, l) in layout.lines.iter().enumerate() {
        println!("  {:>2}. 宽 {:>7.2}  「{}」", i + 1, l.width, l.text);
    }
    for w in &layout.warnings {
        println!("  警告：{w}");
    }

    // 逐字形的前进宽度：与字体文件里的 hmtx 值对照，
    // 能立刻分辨「字宽算错」还是「字体选错」
    println!("逐字形：字符 / 命中字体 / 字号 / 前进宽度（em）");
    for g in layout.glyphs().take(60) {
        let idx = g.cluster as usize;
        let ch = text.get(idx..).and_then(|s| s.chars().next()).unwrap_or('?');
        println!(
            "   '{}'  {:<24} 字号 {:<5.1} gid {:<6} 前进 {:>7.4}em",
            ch,
            g.font.family,
            g.size_pt,
            g.glyph_id,
            g.advance / g.size_pt.max(0.01)
        );
    }
}
