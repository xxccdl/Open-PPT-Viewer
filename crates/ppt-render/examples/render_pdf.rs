//! 把 PDF 的指定页光栅化成 PNG，并与 `render_page` 的输出并排对照。
//!
//! # 它在这个项目里的位置
//!
//! 接入「借用 WPS / Office 内核导出 PDF」这条路之后，同一页画面会有两个来源：
//!
//! - 自研渲染内核（快、能放视频、但没有文档作者工具那套排版长尾知识）
//! - WPS/Office 导出的 PDF（慢一次、但保真度 100%）
//!
//! 判断「自研还差在哪」不能再靠肉眼翻页，得把两边都落到 PNG 上按像素比。
//! 这个 example 就是其中一半：把 PDF 那一边打出来。
//!
//! # 用法
//!
//! ```text
//! cargo run -p ppt-render --example render_pdf -- <文档.pdf> [页码...] [-s 缩放] [-t 次数]
//! ```
//!
//! 页码从 1 开始，不给就全部；缩放是相对 PDF 自身尺寸（1.0 = 72dpi）。
//! 输出目录：`%TEMP%\openpptview-render\`，文件名前缀 `pdf-`。

use std::time::Instant;

use ppt_core::DocumentSource;
use ppt_format_pdf::PdfSource;
use ppt_render::encode_png;

/// 单页输出的像素上限，与渲染管线里的预算保持一致。
const MAX_PIXELS: u64 = 64_000_000;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(doc) = args.next() else {
        eprintln!("用法：render_pdf <文档.pdf> [页码...] [-s 缩放] [-t 次数]");
        std::process::exit(2);
    };

    let mut pages: Vec<usize> = Vec::new();
    let mut scale = 1.0f32;
    let mut repeat = 0u32;
    let mut rest = args.peekable();
    while let Some(a) = rest.next() {
        match a.as_str() {
            "-s" => scale = rest.next().and_then(|v| v.parse().ok()).unwrap_or(1.0),
            "-t" => repeat = rest.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            other => match other.parse::<usize>() {
                // 页码从 1 开始更符合直觉，内部一律 0 起
                Ok(n) if n >= 1 => pages.push(n - 1),
                _ => {
                    eprintln!("无法理解的参数：{other}");
                    std::process::exit(2);
                }
            },
        }
    }

    let t = Instant::now();
    let src = match PdfSource::open(&doc) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("打开失败：{e}");
            std::process::exit(1);
        }
    };
    let count = src.page_count();
    println!("打开 {doc}：{count} 页，耗时 {:.1?}", t.elapsed());

    if pages.is_empty() {
        pages = (0..count).collect();
    }

    let out_dir = std::env::temp_dir().join("openpptview-render");
    if std::fs::create_dir_all(&out_dir).is_err() {
        eprintln!("无法创建输出目录 {}", out_dir.display());
        std::process::exit(1);
    }

    // 首页多做几次，报出「稳态」单页耗时 —— 这才是翻页时用户感受到的延迟
    if repeat > 0 {
        if let Some(&first) = pages.first() {
            let t = Instant::now();
            for _ in 0..repeat {
                let _ = src.rasterize(first, scale, MAX_PIXELS);
            }
            println!(
                "第 {} 页光栅化 ×{repeat}：平均 {:.1}ms",
                first + 1,
                t.elapsed().as_secs_f64() * 1000.0 / repeat as f64
            );
        }
    }

    let mut failures = 0usize;
    let mut timings: Vec<(usize, f64)> = Vec::new();
    let mut encode_ms: f64 = 0.0;
    for &page in &pages {
        let started = Instant::now();
        match src.rasterize(page, scale, MAX_PIXELS) {
            Ok(bmp) => {
                timings.push((page, started.elapsed().as_secs_f64() * 1000.0));
                let out = out_dir.join(format!("pdf-{:03}.png", page + 1));
                let t = Instant::now();
                match encode_png(&bmp) {
                    Ok(bytes) => {
                        encode_ms += t.elapsed().as_secs_f64() * 1000.0;
                        if std::fs::write(&out, bytes).is_err() {
                            eprintln!("第 {} 页写入失败", page + 1);
                            failures += 1;
                        }
                    }
                    Err(e) => {
                        eprintln!("第 {} 页 PNG 编码失败：{e}", page + 1);
                        failures += 1;
                    }
                }
            }
            Err(e) => {
                eprintln!("第 {} 页光栅化失败：{e}", page + 1);
                failures += 1;
            }
        }
    }
    if !timings.is_empty() {
        // PNG 编码是「渲染 → 前端」之间的固定开销，与页面内容无关，
        // 单独量出来才知道它值不值得优化掉
        println!(
            "PNG 编码：平均 {:.1}ms/页（共 {:.0}ms）",
            encode_ms / timings.len() as f64,
            encode_ms
        );
    }

    // 逐页耗时：翻页卡不卡取决于**最慢的那几页**，平均值会把它盖住
    if timings.len() > 1 {
        let mut sorted: Vec<f64> = timings.iter().map(|(_, ms)| *ms).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let total: f64 = sorted.iter().sum();
        let median = sorted[sorted.len() / 2];
        let p95 = sorted[(sorted.len() * 95 / 100).min(sorted.len() - 1)];
        println!(
            "光栅化：平均 {:.1}ms / 中位 {:.1}ms / P95 {:.1}ms / 最慢 {:.1}ms（共 {} 页）",
            total / sorted.len() as f64,
            median,
            p95,
            sorted[sorted.len() - 1],
            sorted.len()
        );

        let mut slow: Vec<&(usize, f64)> = timings.iter().collect();
        slow.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        let worst: Vec<String> = slow
            .iter()
            .take(8)
            .map(|(p, ms)| format!("第{}页 {ms:.0}ms", p + 1))
            .collect();
        println!("最慢的几页：{}", worst.join("，"));
    }

    println!(
        "完成：{} 页成功，{} 页失败，输出到 {}",
        pages.len() - failures,
        failures,
        out_dir.display()
    );
    if failures > 0 {
        std::process::exit(1);
    }
}
