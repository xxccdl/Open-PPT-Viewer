//! 量「翻页延迟」：同一页在不同缩放档下各要多久，以及预热之后还剩多少。
//!
//! # 为什么需要它
//!
//! 「卡不卡」是**用户感受到的**指标，代码里看不出来。同一页在
//! 「首次解释」「内存命中」「换个缩放档重来」三种情况下差两个数量级，
//! 而肉眼翻两页是区分不出来的 —— 只有把每一档的毫秒数打出来，
//! 才知道优化该往哪儿使劲、改完到底快了多少。
//!
//! 判读方法：
//!
//! - **首次**那一列是 PDF 内容流的真实解释代价（自带光栅化的来源才有）
//! - **再次**那一列是缓存命中 + 缩放，理应在个位数毫秒
//! - 两列差距就是「原生档」方案省下来的东西
//!
//! ```text
//! cargo run --release -p ppt-pipeline --example page_latency -- <文档> [页码...]
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ppt_core::scene::PLAY_ALL;
use ppt_core::{detect_format, detect_format_by_extension, DocFormat, SharedSource};
use ppt_format_pdf::PdfSource;
use ppt_format_pptx::PptxSource;
use ppt_pipeline::{Pipeline, PipelineConfig};
use ppt_text::FontContext;

/// 要量的缩放档：缩略图 / 窗口适应 / 全屏。
const SCALES: &[(&str, f32)] = &[("缩略图 0.36×", 0.36), ("窗口 1.05×", 1.05), ("全屏 2.0×", 2.0)];

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(input) = args.next() else {
        eprintln!("用法：page_latency <文档> [页码...]");
        std::process::exit(2);
    };
    let path = PathBuf::from(input);
    let pages: Vec<usize> = args
        .filter_map(|a| a.parse::<usize>().ok())
        .filter(|n| *n >= 1)
        .map(|n| n - 1)
        .collect();

    let bytes = std::fs::read(&path).unwrap_or_default();
    let format = detect_format(bytes.get(..8).unwrap_or(&[]))
        .or_else(|| detect_format_by_extension(&path.to_string_lossy()))
        .unwrap_or(DocFormat::Pdf);

    let source: SharedSource = match format {
        DocFormat::Pdf => match PdfSource::open(&path) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                eprintln!("打开失败：{e}");
                std::process::exit(1);
            }
        },
        _ => match PptxSource::open(&path) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                eprintln!("打开失败：{e}");
                std::process::exit(1);
            }
        },
    };

    let pages = if pages.is_empty() { vec![0] } else { pages };
    let direct = source.rasterizes_directly();
    println!(
        "{}：{} 页，{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        source.page_count(),
        if direct {
            "自带光栅化（PDF）"
        } else {
            "场景图（OOXML）"
        }
    );

    // 磁盘缓存**必须开**：原生档位图 1920×1080 是 8.3MB/页，
    // 内存 LRU 装不下几页，全本的预热结果得靠磁盘留下来。
    // 关掉它测出来的数字完全不是用户会遇到的（会反复重新解释 PDF）。
    let disk = std::env::temp_dir().join("openpptview-latency-cache");
    let _ = std::fs::remove_dir_all(&disk);
    let config = PipelineConfig {
        disk_cache_dir: Some(disk),
        ..PipelineConfig::default()
    };
    let pipeline = match Pipeline::new(Arc::clone(&source), Arc::new(FontContext::new()), config) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("建管线失败：{e}");
            std::process::exit(1);
        }
    };

    // 逐档量「首次」与「再次」
    println!("\n{:<14}{:>12}{:>12}", "缩放档", "首次 ms", "再次 ms");
    for (label, scale) in SCALES {
        let mut first = 0.0f64;
        let mut again = 0.0f64;
        for &page in &pages {
            let t = Instant::now();
            let _ = pipeline.render_now_at(page, *scale, PLAY_ALL);
            first += t.elapsed().as_secs_f64() * 1000.0;

            let t = Instant::now();
            let _ = pipeline.render_now_at(page, *scale, PLAY_ALL);
            again += t.elapsed().as_secs_f64() * 1000.0;
        }
        let n = pages.len() as f64;
        println!("{label:<14}{:>12.1}{:>12.1}", first / n, again / n);
    }

    // 预热：把整本排进后台队列，看要多久能全部就绪
    if direct {
        let t = Instant::now();
        pipeline.prewarm_all();
        let done = pipeline.wait_idle(Duration::from_secs(120));
        println!(
            "\n整本预热：{:.2}s（{}）",
            t.elapsed().as_secs_f64(),
            if done { "队列已排空" } else { "超时未完成" }
        );

        // 预热之后再量一次：应该全是个位数毫秒
        let mut after = 0.0f64;
        for &page in &pages {
            for (_, scale) in SCALES {
                let t = Instant::now();
                let _ = pipeline.render_now_at(page, *scale, PLAY_ALL);
                after += t.elapsed().as_secs_f64() * 1000.0;
            }
        }
        println!(
            "预热后每页三档合计：{:.1} ms",
            after / pages.len() as f64
        );
    }

    pipeline.shutdown();
}
