//! 把一页课件渲染成 PNG，用于人工核对方向与排版。
//!
//! 运行：`cargo run -p ppt-render --example dump_page -- <课件路径> <输出图片> [页号] [缩放]`

use std::sync::Arc;

use ppt_core::scene::Geometry;
use ppt_core::{DocumentSource, PageContent};
use ppt_format_pptx::PptxSource;
use ppt_render::{encode_png, RenderOptions, Renderer};
use ppt_text::FontContext;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args
        .get(1)
        .expect("用法：dump_page <课件路径> <输出图片> [页号] [缩放]");
    let out = args.get(2).expect("缺少输出图片路径");
    let page: usize = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(0);
    let scale: f32 = args.get(4).map(|s| s.parse().unwrap()).unwrap_or(2.0);

    let src = PptxSource::open(path).expect("打开课件失败");
    println!("页数：{}", src.page_count());
    println!("页面尺寸：{:?}", src.default_page_size_pt());

    let scene = match src.page_content(page).expect("解析页面失败") {
        PageContent::Scene(s) => *s,
        PageContent::Bitmap(_) => panic!("PPTX 应产出场景图"),
    };
    println!(
        "节点数：{}，告警：{:?}",
        scene.nodes.len(),
        scene.warnings
    );

    // 逐节点的几何清单。
    //
    // 「位置对但内容不对」「某个东西整块不见了」这类问题，肉眼在应用里看不出来，
    // 但把每个节点画布坐标下的矩形列出来，一眼就知道是漏解析了、
    // 还是解析出来摆错地方了。
    println!("节点清单：");
    for node in scene.walk() {
        let b = node.canvas_bounds();
        let kind = match &node.geometry {
            Geometry::None => "无".to_string(),
            Geometry::Rect => "矩形".to_string(),
            Geometry::RoundRect { .. } => "圆角矩形".to_string(),
            Geometry::Ellipse => "椭圆".to_string(),
            Geometry::Path(_) => "路径".to_string(),
            Geometry::Image(img) => format!("图片({}) src={:?}", img.part, img.src_rect),
            Geometry::Table(t) => format!("表格({}×{})", t.row_count(), t.col_count()),
            Geometry::Placeholder { reason } => format!("占位[{reason}]"),
        };
        let preview: String = node
            .text
            .as_ref()
            .map(|t| t.plain_text().chars().take(28).collect())
            .unwrap_or_default();
        println!(
            "  id={:?} 「{}」 [{:.1},{:.1} {:.1}×{:.1}] {} 文本={}「{}」 媒体={} 构建={:?}",
            node.shape_id,
            node.name.as_deref().unwrap_or(""),
            b.x,
            b.y,
            b.w,
            b.h,
            kind,
            node.text.as_ref().map(|t| t.paragraphs.len()).unwrap_or(0),
            preview,
            node.media.is_some(),
            node.build,
        );
    }

    // 动画序列：排查「点了没反应」「一下子就全出来了」时必须看的一张表。
    //
    // 「按步出图」靠的就是 `targets[].shape_id` 与节点 `build` 两端对齐，
    // 这两张表任何一边空了，出来的每一帧都会是同一张整页图。
    println!("动画步数：{}", scene.anim.steps.len());
    for (i, step) in scene.anim.steps.iter().enumerate() {
        println!(
            "  第 {} 步 trigger={:?} kind={:?} dur={}ms targets={:?}",
            i + 1,
            step.trigger,
            step.kind,
            step.dur_ms,
            step.targets
                .iter()
                .map(|t| (t.shape_id, t.para_range))
                .collect::<Vec<_>>(),
        );
    }

    // 链接与媒体热区：排查「点了没反应」时最需要看的两张表
    let links = scene.link_hotspots();
    println!("链接热区：{}", links.len());
    for l in &links {
        println!(
            "  [{:.1},{:.1} {:.1}×{:.1}] {:?}",
            l.rect.x, l.rect.y, l.rect.w, l.rect.h, l.link.target
        );
    }
    let media = scene.media_hotspots();
    println!("媒体热区：{}", media.len());
    for m in &media {
        println!(
            "  [{:.1},{:.1} {:.1}×{:.1}] {:?} part={} loop={}",
            m.rect.x, m.rect.y, m.rect.w, m.rect.h, m.media.kind, m.media.part, m.media.loop_play
        );
        println!("    部件在包内：{}", src.package().contains(&m.media.part));
        println!("    裁剪区间：{:?}", m.media.trim);
    }

    let fonts = Arc::new(FontContext::new());
    println!("已索引字体面：{}", fonts.font_count());

    let mut renderer = Renderer::new(fonts);
    let opts = RenderOptions {
        scale,
        ..RenderOptions::default()
    };
    let bmp = renderer.render(&scene, &opts, &src).expect("渲染失败");
    println!("输出：{}×{}", bmp.width, bmp.height);

    let png = encode_png(&bmp).expect("PNG 编码失败");
    std::fs::write(out, png).expect("写文件失败");
    println!("已写出：{out}");
}
