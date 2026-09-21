//! 把课件的指定页渲染成 PNG，供人工核对保真度。
//!
//! # 为什么做成 example 而不是测试
//!
//! 渲染器的问题大多要靠**看**才发现（错位、遮挡、配色、行距），
//! 而看之前先得把像素落到磁盘上。测试擅长钉住「已知的正确」，
//! 但发现「还不知道错在哪」只能靠把图打出来。
//!
//! # 用法
//!
//! ```text
//! cargo run -p ppt-render --example render_page -- <课件路径> [页码...] [-s 缩放] [-r 动画步] [-L 步 -P 进度] [-t 次数]
//! ```
//!
//! 页码从 1 开始，不给就渲染全部；缩放默认 1.0（放大便于看清单个字形）；
//! `-r 0` 表示「一步动画都还没推进」，用来核对动画前的初始画面。
//! `-L 2 -P 0.5` 则演示「第 2 步动画播到一半」的样子：底帧 + 各目标图层
//! 按起止状态插值合成 —— 前端放映时干的就是这件事，这里用同一套几何把它
//! 落到 PNG 上，好核对图层位置对不对。
//! `-t 20` 把首页渲染 20 次并报出平均耗时。
//! 输出目录：`%TEMP%\openpptview-render\`。

use std::path::Path;
use std::sync::Arc;

use ppt_core::scene::{linear_state, EffectState};
use ppt_core::{DocumentSource, PageContent};
use ppt_format_pptx::PptxSource;
use ppt_render::{RenderOptions, Renderer};
use ppt_text::FontContext;
use tiny_skia::{IntSize, Pixmap, PixmapPaint, Transform as SkTransform};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(deck) = args.next() else {
        eprintln!("用法：render_page <课件路径> [页码...] [-s 缩放] [-r 动画步] [-L 步 -P 进度] [-t 次数]");
        std::process::exit(2);
    };

    // 页码从 1 开始更符合直觉，内部一律 0 起
    let mut pages: Vec<usize> = Vec::new();
    let mut scale = 1.0f32;
    // `-r N` 表示「前 N 步依次播完」，与渲染器的线性状态一致
    let mut steps: Option<u32> = None;
    let mut repeat = 0u32;
    let mut layer_step: Option<u32> = None;
    let mut layer_progress = 0.5f32;
    let mut audit = false;
    let mut rest = args.peekable();
    while let Some(a) = rest.next() {
        match a.as_str() {
            "-s" => {
                scale = rest.next().and_then(|v| v.parse().ok()).unwrap_or(1.0);
            }
            "-r" => {
                steps = rest.next().and_then(|v| v.parse().ok());
            }
            "-t" => {
                repeat = rest.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            }
            "-L" => {
                layer_step = rest.next().and_then(|v| v.parse().ok());
            }
            "-P" => {
                layer_progress = rest.next().and_then(|v| v.parse().ok()).unwrap_or(0.5);
            }
            "-A" => audit = true,
            _ => {
                if let Ok(p) = a.parse::<usize>() {
                    pages.push(p.saturating_sub(1));
                }
            }
        }
    }
    let state = match steps {
        Some(n) => ppt_core::scene::linear_state(n as usize),
        None => ppt_core::scene::PLAY_ALL,
    };

    let src = PptxSource::open(Path::new(&deck)).expect("打开课件失败");
    let out_dir = std::env::temp_dir().join("openpptview-render");
    std::fs::create_dir_all(&out_dir).expect("创建输出目录失败");

    let mut renderer = Renderer::new(Arc::new(FontContext::new()));
    let wanted = if pages.is_empty() {
        (0..src.page_count()).collect()
    } else {
        pages
    };

    let opts = RenderOptions {
        scale,
        // 渲染成 PNG 供人看，不需要为省内存而限制像素数
        max_pixels: u64::MAX,
        state,
        ..RenderOptions::default()
    };

    for page in wanted {
        if page >= src.page_count() {
            eprintln!("第 {} 页超出范围，已跳过", page + 1);
            continue;
        }
        let content = src.page_content(page).expect("解析失败");
        let bmp = match &content {
            PageContent::Scene(scene) => {
                println!(
                    "第 {} 页动画：{} 步 {:?}  转场 {:?}",
                    page + 1,
                    scene.anim.len(),
                    scene
                        .anim
                        .steps
                        .iter()
                        .map(|st| (
                            st.trigger,
                            st.kind,
                            st.dur_ms,
                            st.targets
                                .iter()
                                .map(|t| (t.shape_id, t.from, t.to, t.mask.is_some()))
                                .collect::<Vec<_>>()
                        ))
                        .collect::<Vec<_>>(),
                    scene.transition
                );
                // 先热身一次（字形缓存、图片解码），再计时 ——
                // 否则第一帧的耗时全是缓存填充，量出来的不是稳态
                let first = renderer.render(scene, &opts, &src).expect("渲染失败");
                if repeat > 0 {
                    let png_bytes = ppt_render::encode_png(&first).expect("编码 PNG 失败").len();
                    let t0 = std::time::Instant::now();
                    for _ in 1..repeat {
                        let b = renderer.render(scene, &opts, &src).expect("渲染失败");
                        let _ = ppt_render::encode_png(&b).expect("编码 PNG 失败");
                    }
                    let per = t0.elapsed().as_secs_f64() * 1000.0 / f64::from(repeat - 1).max(1.0);
                    println!(
                        "  一帧 {:.1}ms（{}×{}，PNG {:.0}KB）→ 上限约 {:.0} fps",
                        per,
                        first.width,
                        first.height,
                        png_bytes as f64 / 1024.0,
                        1000.0 / per.max(0.01)
                    );
                }
                first
            }
            PageContent::Bitmap(b) => (**b).clone(),
        };

        // `-L <步>`：把这一步动画合成到一半的样子打出来，核对图层位置
        if let (Some(step_no), PageContent::Scene(scene)) = (layer_step, &content) {
            if let Some(composed) =
                compose_layer_frame(&mut renderer, scene, step_no, layer_progress, scale, &src)
            {
                let out = out_dir.join(format!(
                    "page-{:03}-layer{}-p{:.1}.png",
                    page + 1,
                    step_no,
                    layer_progress
                ));
                std::fs::write(&out, ppt_render::encode_png(&composed).expect("编码 PNG 失败"))
                    .expect("写文件失败");
                println!("第 {} 页图层合成 → {}", page + 1, out.display());
            }
        }

        // `-A`：审计文本框「声明高度 vs 排版高度」。
        //
        // PowerPoint 给 `spAutoFit` 的框写的高度就是它渲染时的实际高度，
        // 所以两者的比值能直接看出我们的行高模型准不准。
        if audit {
            if let PageContent::Scene(scene) = &content {
                audit_text_boxes(&mut renderer, scene, scale, &src);
            }
        }

        let suffix = if steps.is_none() {
            String::new()
        } else {
            format!("-step{}", steps.unwrap_or(0))
        };
        let out = out_dir.join(format!("page-{:03}{}.png", page + 1, suffix));
        std::fs::write(&out, ppt_render::encode_png(&bmp).expect("编码 PNG 失败"))
            .expect("写文件失败");
        println!(
            "第 {} 页 → {}（{}×{}）",
            page + 1,
            out.display(),
            bmp.width,
            bmp.height
        );
    }
}

/// 审计文本框：把「框声明的高度」与「我们排版算出的高度」并排打出来。
fn audit_text_boxes(
    renderer: &mut Renderer,
    scene: &ppt_core::scene::Scene,
    scale: f32,
    src: &PptxSource,
) {
    let fonts = renderer.fonts().clone();
    for node in scene.walk() {
        let Some(tb) = &node.text else { continue };
        if tb.is_empty() {
            continue;
        }
        let Some(bbox) = node.local_bbox else { continue };
        // 局部单位 → 物理 pt。组合里的子形状局部坐标是被缩放过的，
        // 而内边距是绝对磅值，不参与缩放（与渲染器的 `text_area` 同一套算法）
        let unit = node.effective_scale();
        let area = ppt_core::scene::Size::new(
            (bbox.w * unit - tb.body.insets.horizontal()).max(0.0),
            (bbox.h * unit - tb.body.insets.vertical()).max(0.0),
        );
        if area.w <= 0.0 {
            continue;
        }
        let opts = ppt_text::LayoutOptions {
            reveal: ppt_core::scene::PLAY_ALL,
            ..ppt_text::LayoutOptions::default()
        };
        let laid = ppt_render::text::measure_text(&fonts, tb, area, opts);
        let declared = bbox.h * unit;
        // 声明的高度是**含内边距的框高**，排版高度只是文字本身 ——
        // 不加内边距就跟「框高」比，单行框会稳定偏低 15%（7.2pt 占 40pt 的六分之一），
        // 看上去像处处都有问题，其实只是口径不一致
        let ours = laid.height + tb.body.insets.vertical();
        let ratio = if declared > 0.0 { ours / declared } else { 0.0 };
        if !(0.93..=1.07).contains(&ratio) {
            let max_size = laid
                .glyphs()
                .map(|g| g.size_pt)
                .fold(0.0f32, f32::max);
            println!(
                "    文本框 id={:?} 声明高 {:.1} / 我们 {:.1} = {:.2}（{} 行，字号 {:.1}，行高 {:.1}）「{}」",
                node.shape_id,
                declared,
                ours,
                ratio,
                laid.line_count(),
                max_size,
                laid.height / laid.line_count().max(1) as f32,
                laid.plain_text().replace('\n', "⏎").chars().take(28).collect::<String>()
            );
        }
    }

    // 表格：`p:graphicFrame` 的 `a:ext/@cy` 是 PowerPoint 排完之后写下的高度。
    // 比文本框更值得比 —— 表格的每一行都会随内容长高，行高模型差一点，
    // 整张表就长出一截，直接顶出画面底部（课件里最常见的一类「表格超出页面」）。
    for node in scene.walk() {
        let ppt_core::scene::Geometry::Table(table) = &node.geometry else {
            continue;
        };
        let Some(bbox) = node.local_bbox else { continue };
        if table.is_empty() {
            continue;
        }
        let opts = ppt_render::RenderOptions {
            scale,
            ..Default::default()
        };
        let ours = renderer.table_height(table, &opts);
        let declared = bbox.h;
        let ratio = if declared > 0.0 { ours / declared } else { 0.0 };
        // 表格底边离画面底部还有多少：负数就是溢出
        let bounds = node.canvas_bounds();
        let bottom = bounds.y + ours;
        let slack = scene.size_pt.h - bottom;
        if !(0.9..=1.1).contains(&ratio) || slack < 0.0 {
            println!(
                "    表格  id={:?}  声明高 {:.1} / 排版高 {:.1} = {:.2}（{} 行）  底边 {:.1}，距画面底 {:.1}pt{}",
                node.shape_id,
                declared,
                ours,
                ratio,
                table.rows.len(),
                bottom,
                slack,
                if slack < 0.0 { "  ← 溢出" } else { "" }
            );
        }
    }
    let _ = (scale, src);
}

/// 把第 `step_no` 步动画合成到 `progress` 处的样子。
///
/// 干的就是前端放映时那一套：底帧（这一步之前的样子）+ 每个目标单独的图层
/// 按 `from → to` 插值叠加。用同一套几何在离线工具里复现一遍，
/// 图层位置对不对就不必靠肉眼在应用里猜了。
fn compose_layer_frame(
    renderer: &mut Renderer,
    scene: &ppt_core::scene::Scene,
    step_no: u32,
    progress: f32,
    scale: f32,
    src: &PptxSource,
) -> Option<ppt_core::Bitmap> {
    let step = scene.anim.steps.get(step_no.checked_sub(1)? as usize)?;
    // 这一步**之前**的样子：前 step_no-1 步已播
    let before = linear_state(step_no as usize - 1);
    let base_opts = RenderOptions {
        scale,
        max_pixels: u64::MAX,
        state: before,
        ..RenderOptions::default()
    };

    let base = renderer.render(scene, &base_opts, src).ok()?;
    let size = IntSize::from_wh(base.width, base.height)?;
    let mut pixmap = Pixmap::from_vec(base.data.clone(), size)?;
    let k = scale;

    for t in &step.targets {
        if t.is_static() {
            continue;
        }
        let Some((layer, rect)) = renderer
            .render_layer(scene, t.shape_id, t.para_range, &[t.from, t.to], &base_opts, src)
            .ok()
            .flatten()
        else {
            continue;
        };
        let Some(lsize) = IntSize::from_wh(layer.width, layer.height) else {
            continue;
        };
        println!(
            "  图层 spid={} {}×{} @ ({:.1},{:.1},{:.1},{:.1})",
            t.shape_id, layer.width, layer.height, rect.x, rect.y, rect.w, rect.h
        );
        let Some(lp) = Pixmap::from_vec(layer.data.clone(), lsize) else {
            continue;
        };

        let s = EffectState::lerp(&t.from, &t.to, progress);
        let (cx, cy) = ((rect.x + rect.w / 2.0) * k, (rect.y + rect.h / 2.0) * k);
        let tr = SkTransform::from_translate(s.dx * k, s.dy * k)
            .pre_concat(SkTransform::from_translate(cx, cy))
            .pre_concat(SkTransform::from_rotate(s.rotate))
            .pre_concat(SkTransform::from_scale(s.scale, s.scale))
            .pre_concat(SkTransform::from_translate(-cx, -cy))
            .pre_concat(SkTransform::from_translate(rect.x * k, rect.y * k));
        pixmap.draw_pixmap(
            0,
            0,
            lp.as_ref(),
            &PixmapPaint {
                opacity: s.opacity.clamp(0.0, 1.0),
                ..Default::default()
            },
            tr,
            None,
        );
    }

    Some(ppt_core::Bitmap {
        width: pixmap.width(),
        height: pixmap.height(),
        format: ppt_core::PixelFormat::Rgba8Premultiplied,
        data: pixmap.take(),
    })
}
