//! 反推 PowerPoint 的**文字宽度度量**，用来查「为什么同一句话它两行、我们三行」。
//!
//! # 原理
//!
//! `spAutoFit` 的文本框里，PowerPoint 写下的 `a:ext/@cy` 就是它排版的实际高度。
//! 这个高度反过来约束了**它能折成几行**：
//!
//! ```text
//! 可用高度 = 框高 - 上下内边距
//! 它能折的行数 = floor(可用高度 / 行高)
//! ```
//!
//! 于是在「它折的行数 < 我们折的行数」的框上，可以做一次二分搜索：
//! **把可用宽度放到多宽，我们的排版才也会折成同样多的行**。
//! 那个宽度记为 `需要的宽度`，必然满足 `需要的宽度 > 框实际可用宽度`；
//! 两者之比就是「它的度量比我们窄多少」。
//!
//! 单个框可能是作者改过文字却没同步高度（历史残留），所以要看**一批框的分布**，
//! 不看单个。若比例集中在某一个系数附近，那就是系统性的度量差异。
//!
//! 用法：
//! ```text
//! cargo run --release -p ppt-render --example width_probe -- <课件路径>
//! ```

use std::sync::Arc;

use ppt_core::{DocumentSource, PageContent};
use ppt_format_pptx::PptxSource;
use ppt_text::{FontContext, LayoutOptions, TextLayouter};

/// 缩放系数的分档（把「需要的宽度 / 可用宽度」落到区间里统计）。
const BUCKETS: [(f32, f32, &str); 7] = [
    (0.90, 0.95, "0.90~0.95"),
    (0.95, 0.98, "0.95~0.98"),
    (0.98, 0.995, "0.98~0.995"),
    (0.995, 1.02, "0.995~1.02"),
    (1.02, 1.05, "1.02~1.05"),
    (1.05, 1.10, "1.05~1.10"),
    (1.10, 9.0, ">1.10"),
];

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(deck) = args.next() else {
        eprintln!("用法：width_probe <课件路径>");
        std::process::exit(2);
    };

    let fonts = Arc::new(FontContext::new());
    let layouter = TextLayouter::new(&fonts);
    let src = PptxSource::open(&deck).expect("打开课件失败");
    let pages = src.page_count();

    println!("页\tid\t字号\t我们行数\t它行数\t可用宽\t需要宽\t系数\t文本");
    let mut hits = 0usize;
    let mut hist = [0usize; BUCKETS.len()];
    let mut ratios: Vec<f32> = Vec::new();

    for page in 0..pages {
        let Ok(PageContent::Scene(scene)) = src.page_content(page) else {
            continue;
        };
        for node in scene.walk() {
            let Some(tb) = &node.text else { continue };
            if tb.is_empty() {
                continue;
            }
            let Some(bbox) = node.local_bbox else { continue };

            let unit = node.effective_scale();
            let insets = tb.body.insets;
            let avail_w = bbox.w * unit - insets.horizontal();
            let avail_h = bbox.h * unit - insets.vertical();
            if avail_w <= 1.0 || avail_h <= 0.0 {
                continue;
            }

            let size = tb
                .paragraphs
                .iter()
                .flat_map(|p| p.runs.iter())
                .map(|r| r.props.size_pt)
                .fold(0.0f32, f32::max);
            if size <= 0.0 {
                continue;
            }

            let lines_at = |w: f32| {
                layouter
                    .layout(
                        tb,
                        ppt_core::scene::Size::new(w, avail_h),
                        LayoutOptions::default(),
                    )
                    .line_count()
            };

            let ours = lines_at(avail_w);
            if ours == 0 {
                continue;
            }
            // 行高：按「可用高度 ÷ 行数」估不准（可能刚好差一点），
            // 直接用一行的高度：把宽度放到极大，量第一行的高度
            let one_line = layouter.layout(
                tb,
                ppt_core::scene::Size::new(1_000_000.0, avail_h),
                LayoutOptions::default(),
            );
            let line_h = if one_line.line_count() > 0 {
                one_line.height / one_line.line_count() as f32
            } else {
                continue;
            };
            let allowed = (avail_h / line_h).floor().max(1.0) as usize;
            if allowed >= ours {
                continue;
            }

            // 二分：把可用宽度放到多宽，我们才也折成 allowed 行
            let (mut lo, mut hi) = (avail_w, avail_w * 4.0 + 100.0);
            if lines_at(hi) > allowed {
                continue; // 加宽也降不下来，说明差异不是宽度引起的，跳过
            }
            for _ in 0..24 {
                let mid = (lo + hi) * 0.5;
                if lines_at(mid) <= allowed {
                    hi = mid;
                } else {
                    lo = mid;
                }
            }

            let ratio = avail_w / hi; // 它的度量 ÷ 我们的度量
            let _ = ratio;
            let shrink = hi / avail_w; // 我们要把宽度放大多少倍才能少折一行
            for (i, (lo, hi_, _)) in BUCKETS.iter().enumerate() {
                if shrink >= *lo && shrink < *hi_ {
                    hist[i] += 1;
                }
            }
            ratios.push(shrink);
            hits += 1;

            let text: String = tb
                .plain_text()
                .replace('\n', " ")
                .chars()
                .take(22)
                .collect();
            println!(
                "{}\t{:?}\t{:.0}\t{}\t{}\t{:.1}\t{:.1}\t{:.3}\t{}",
                page + 1,
                node.shape_id,
                size,
                ours,
                allowed,
                avail_w,
                hi,
                shrink,
                text
            );
        }
    }

    println!("\n-- 共 {hits} 个「我们多折了行」的框；系数 = 需要宽度 / 可用宽度（1.00 表示不用放大）");
    for (i, (_, _, label)) in BUCKETS.iter().enumerate() {
        if hist[i] > 0 {
            println!("   {label}\t{}", hist[i]);
        }
    }
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if !ratios.is_empty() {
        let mid = ratios[ratios.len() / 2];
        let avg = ratios.iter().sum::<f32>() / ratios.len() as f32;
        println!("   中位数 {mid:.3}，平均 {avg:.3}，最小 {:.3}，最大 {:.3}", ratios[0], ratios[ratios.len() - 1]);
    }
}
