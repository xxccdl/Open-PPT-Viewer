//! 端到端测试：真实 `.pptx` → 解析 → 渲染 → 像素断言。
//!
//! 前面的单元测试分别验证了「解析正确」与「渲染正确」，
//! 但它们都在手工构造的数据上进行。这里把两者串起来：
//!
//! ```text
//! .pptx 字节 → PptxSource（解析） → Scene（中间表示） → Renderer（光栅化） → Bitmap
//! ```
//!
//! 这是唯一能发现「解析产出的坐标系与渲染期望的不一致」这类跨层缺陷的测试 ——
//! 例如子节点变换是否已烘焙、主题色是否真的用了、字体名是否被解析成了具体字体。

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use ppt_core::scene::{Color, Scene};
use ppt_core::{DocumentSource, PageContent};
use ppt_format_pptx::PptxSource;
use ppt_render::{RenderOptions, Renderer};
use ppt_text::FontContext;

// ---------- 最小但完整的 PPTX 语料 ----------

const CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Default Extension="png" ContentType="image/png"/>
  <Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
  <Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
  <Override PartName="/ppt/charts/chart1.xml" ContentType="application/vnd.openxmlformats-officedocument.drawingml.chart+xml"/>
  <Override PartName="/ppt/charts/chart2.xml" ContentType="application/vnd.openxmlformats-officedocument.drawingml.chart+xml"/>
  <Override PartName="/ppt/charts/chart3.xml" ContentType="application/vnd.openxmlformats-officedocument.drawingml.chart+xml"/>
  <Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>
  <Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/>
  <Override PartName="/ppt/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/>
</Types>"#;

const ROOT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/>
</Relationships>"#;

const PRESENTATION: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:p="urn:p" xmlns:r="urn:r">
  <p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst>
  <p:sldSz cx="9144000" cy="6858000"/>
</p:presentation>"#;

const PRESENTATION_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/>
</Relationships>"#;

const THEME: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<a:theme xmlns:a="urn:a" name="测试主题">
  <a:themeElements>
    <a:clrScheme name="测试">
      <a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1>
      <a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1>
      <a:dk2><a:srgbClr val="44546A"/></a:dk2>
      <a:lt2><a:srgbClr val="E7E6E6"/></a:lt2>
      <a:accent1><a:srgbClr val="C00000"/></a:accent1>
      <a:accent2><a:srgbClr val="00B050"/></a:accent2>
      <a:accent3><a:srgbClr val="0070C0"/></a:accent3>
      <a:accent4><a:srgbClr val="FFC000"/></a:accent4>
      <a:accent5><a:srgbClr val="7030A0"/></a:accent5>
      <a:accent6><a:srgbClr val="00B0F0"/></a:accent6>
      <a:hlink><a:srgbClr val="0563C1"/></a:hlink>
      <a:folHlink><a:srgbClr val="954F72"/></a:folHlink>
    </a:clrScheme>
    <a:fontScheme name="测试">
      <a:majorFont><a:latin typeface="Arial"/><a:ea typeface="宋体"/><a:cs typeface=""/></a:majorFont>
      <a:minorFont><a:latin typeface="Arial"/><a:ea typeface="宋体"/><a:cs typeface=""/></a:minorFont>
    </a:fontScheme>
    <a:fmtScheme name="测试">
      <a:fillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:fillStyleLst>
      <a:lnStyleLst><a:ln w="12700"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln></a:lnStyleLst>
      <a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst>
      <a:bgFillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:bgFillStyleLst>
    </a:fmtScheme>
  </a:themeElements>
</a:theme>"#;

const SLIDE_MASTER: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldMaster xmlns:p="urn:p" xmlns:a="urn:a">
  <p:cSld>
    <p:bg><p:bgPr><a:solidFill><a:schemeClr val="bg1"/></a:solidFill></p:bgPr></p:bg>
    <p:spTree>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="1" name="标题占位符"/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="9144000" cy="1200000"/></a:xfrm></p:spPr>
        <p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>标题</a:t></a:r></a:p></p:txBody>
      </p:sp>
    </p:spTree>
  </p:cSld>
  <p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2"
            accent1="accent1" accent2="accent2" accent3="accent3"
            accent4="accent4" accent5="accent5" accent6="accent6"
            hlink="hlink" folHlink="folHlink"/>
  <p:txStyles>
    <p:titleStyle>
      <a:lvl1pPr algn="ctr"><a:defRPr sz="4000" b="1">
        <a:solidFill><a:schemeClr val="accent1"/></a:solidFill>
      </a:defRPr></a:lvl1pPr>
    </p:titleStyle>
    <p:bodyStyle><a:lvl1pPr><a:defRPr sz="2400"/></a:lvl1pPr></p:bodyStyle>
    <p:otherStyle><a:defPPr><a:defRPr sz="1800"/></a:defPPr></p:otherStyle>
  </p:txStyles>
</p:sldMaster>"#;

const SLIDE_MASTER_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="../theme/theme1.xml"/>
</Relationships>"#;

const SLIDE_LAYOUT: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldLayout xmlns:p="urn:p" xmlns:a="urn:a" type="titleOnly">
  <p:cSld><p:spTree>
    <p:sp>
      <p:nvSpPr><p:cNvPr id="1" name="标题占位符"/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>
      <p:spPr><a:xfrm><a:off x="457200" y="457200"/><a:ext cx="8229600" cy="1200000"/></a:xfrm></p:spPr>
      <p:txBody><a:bodyPr/><a:lstStyle/></p:txBody>
    </p:sp>
  </p:spTree></p:cSld>
</p:sldLayout>"#;

const SLIDE_LAYOUT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="../slideMasters/slideMaster1.xml"/>
</Relationships>"#;

/// 幻灯片：标题占位符（继承位置与样式）+ 一个渐变填充的矩形
/// + 一个带描边的椭圆 + 一个组合形状内的子矩形。
const SLIDE_1: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:p="urn:p" xmlns:a="urn:a" xmlns:r="urn:r">
  <p:cSld><p:spTree>
    <p:sp>
      <p:nvSpPr><p:cNvPr id="2" name="标题 1"/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>
      <p:txBody><a:bodyPr/><a:p><a:r><a:t>光合作用</a:t></a:r></a:p></p:txBody>
    </p:sp>
    <p:sp>
      <p:nvSpPr><p:cNvPr id="3" name="渐变矩形"/></p:nvSpPr>
      <p:spPr>
        <a:xfrm><a:off x="457200" y="2286000"/><a:ext cx="3657600" cy="2286000"/></a:xfrm>
        <a:prstGeom prst="rect"/>
        <a:gradFill>
          <a:gsLst>
            <a:gs pos="0"><a:srgbClr val="FF0000"/></a:gs>
            <a:gs pos="100000"><a:srgbClr val="0000FF"/></a:gs>
          </a:gsLst>
          <a:lin ang="0" scaled="1"/>
        </a:gradFill>
      </p:spPr>
    </p:sp>
    <p:sp>
      <p:nvSpPr><p:cNvPr id="4" name="描边椭圆"/></p:nvSpPr>
      <p:spPr>
        <a:xfrm><a:off x="4572000" y="2286000"/><a:ext cx="3657600" cy="2286000"/></a:xfrm>
        <a:prstGeom prst="ellipse"/>
        <a:solidFill><a:srgbClr val="FFFF00"/></a:solidFill>
        <a:ln w="38100"><a:solidFill><a:srgbClr val="000000"/></a:solidFill></a:ln>
      </p:spPr>
    </p:sp>
    <p:grpSp>
      <p:nvGrpSpPr><p:cNvPr id="5" name="组合"/></p:nvGrpSpPr>
      <p:grpSpPr><a:xfrm>
        <a:off x="457200" y="5029200"/><a:ext cx="1828800" cy="914400"/>
        <a:chOff x="0" y="0"/><a:chExt cx="1828800" cy="914400"/>
      </a:xfrm></p:grpSpPr>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="6" name="组合内矩形"/></p:nvSpPr>
        <p:spPr>
          <a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm>
          <a:prstGeom prst="rect"/>
          <a:solidFill><a:srgbClr val="00FF00"/></a:solidFill>
        </p:spPr>
      </p:sp>
    </p:grpSp>
    <p:sp>
      <p:nvSpPr><p:cNvPr id="7" name="直接文本"/></p:nvSpPr>
      <p:spPr>
        <a:xfrm><a:off x="4572000" y="5029200"/><a:ext cx="3657600" cy="914400"/></a:xfrm>
        <a:prstGeom prst="rect"/>
        <a:solidFill><a:srgbClr val="FFFFFF"/></a:solidFill>
      </p:spPr>
      <p:txBody><a:bodyPr anchor="ctr"/>
        <a:p><a:pPr algn="ctr"/><a:r><a:rPr sz="2400" b="1">
          <a:solidFill><a:srgbClr val="000000"/></a:solidFill>
        </a:rPr><a:t>重点文字</a:t></a:r></a:p>
      </p:txBody>
    </p:sp>
  </p:spTree></p:cSld>
</p:sld>"#;

const SLIDE_1_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
</Relationships>"#;

// ---------- 图表语料 ----------

/// 一页里放三张图：柱状图（展开为图元）、饼图（展开为扇形）、
/// 雷达图（本版不支持，应降级为占位）。
const SLIDE_CHART: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:p="urn:p" xmlns:a="urn:a" xmlns:r="urn:r" xmlns:c="urn:c">
  <p:cSld><p:spTree>
    <p:graphicFrame>
      <p:nvGraphicFramePr><p:cNvPr id="2" name="柱状图"/></p:nvGraphicFramePr>
      <p:xfrm><a:off x="457200" y="2286000"/><a:ext cx="3657600" cy="2286000"/></p:xfrm>
      <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/chart">
        <c:chart r:id="rId2"/>
      </a:graphicData></a:graphic>
    </p:graphicFrame>
    <p:graphicFrame>
      <p:nvGraphicFramePr><p:cNvPr id="3" name="饼图"/></p:nvGraphicFramePr>
      <p:xfrm><a:off x="5029200" y="2286000"/><a:ext cx="3657600" cy="2286000"/></p:xfrm>
      <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/chart">
        <c:chart r:id="rId3"/>
      </a:graphicData></a:graphic>
    </p:graphicFrame>
    <p:graphicFrame>
      <p:nvGraphicFramePr><p:cNvPr id="4" name="雷达图"/></p:nvGraphicFramePr>
      <p:xfrm><a:off x="457200" y="5029200"/><a:ext cx="3657600" cy="1524000"/></p:xfrm>
      <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/chart">
        <c:chart r:id="rId4"/>
      </a:graphicData></a:graphic>
    </p:graphicFrame>
  </p:spTree></p:cSld>
</p:sld>"#;

const SLIDE_CHART_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart2.xml"/>
  <Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart3.xml"/>
</Relationships>"#;

/// 柱状图：一个系列，显式指定颜色 `2E86DE`，两个类别。
const CHART_BAR: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<c:chartSpace xmlns:c="urn:c" xmlns:a="urn:a">
  <c:chart><c:plotArea>
    <c:barChart>
      <c:barDir val="col"/>
      <c:ser>
        <c:idx val="0"/><c:order val="0"/>
        <c:tx><c:strRef><c:strCache><c:pt idx="0"><c:v>甲</c:v></c:pt></c:strCache></c:strRef></c:tx>
        <c:spPr><a:solidFill><a:srgbClr val="2E86DE"/></a:solidFill></c:spPr>
        <c:cat><c:strRef><c:strCache>
          <c:pt idx="0"><c:v>一</c:v></c:pt><c:pt idx="1"><c:v>二</c:v></c:pt>
        </c:strCache></c:strRef></c:cat>
        <c:val><c:numRef><c:numCache>
          <c:pt idx="0"><c:v>10</c:v></c:pt><c:pt idx="1"><c:v>20</c:v></c:pt>
        </c:numCache></c:numRef></c:val>
      </c:ser>
    </c:barChart>
    <c:catAx><c:delete val="0"/><c:axPos val="b"/></c:catAx>
    <c:valAx><c:delete val="0"/><c:axPos val="l"/><c:majorGridlines/></c:valAx>
  </c:plotArea></c:chart>
</c:chartSpace>"#;

/// 饼图：三个数据点 → 三块扇形，颜色取主题 accent1/accent2/accent3。
const CHART_PIE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<c:chartSpace xmlns:c="urn:c" xmlns:a="urn:a">
  <c:chart><c:plotArea>
    <c:pieChart>
      <c:ser>
        <c:idx val="0"/><c:order val="0"/>
        <c:cat><c:strRef><c:strCache>
          <c:pt idx="0"><c:v>甲</c:v></c:pt>
          <c:pt idx="1"><c:v>乙</c:v></c:pt>
          <c:pt idx="2"><c:v>丙</c:v></c:pt>
        </c:strCache></c:strRef></c:cat>
        <c:val><c:numRef><c:numCache>
          <c:pt idx="0"><c:v>3</c:v></c:pt>
          <c:pt idx="1"><c:v>1</c:v></c:pt>
          <c:pt idx="2"><c:v>4</c:v></c:pt>
        </c:numCache></c:numRef></c:val>
      </c:ser>
    </c:pieChart>
  </c:plotArea></c:chart>
</c:chartSpace>"#;

const CHART_RADAR: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<c:chartSpace xmlns:c="urn:c" xmlns:a="urn:a">
  <c:chart><c:plotArea>
    <c:radarChart>
      <c:ser>
        <c:idx val="0"/><c:order val="0"/>
        <c:val><c:numRef><c:numCache><c:pt idx="0"><c:v>5</c:v></c:pt></c:numCache></c:numRef></c:val>
      </c:ser>
    </c:radarChart>
  </c:plotArea></c:chart>
</c:chartSpace>"#;

// ---------- 测试夹具 ----------

/// 组装一个最小 PPTX：唯一的一页用给定的 `slide` / `slide_rels`，
/// `extra` 是额外部件（图表正文等）。
fn build_pptx(name: &str, slide: &str, slide_rels: &str, extra: &[(&str, &str)]) -> PathBuf {
    let path = std::env::temp_dir().join(name);
    let _ = std::fs::remove_file(&path);

    let mut parts: Vec<(&str, &[u8])> = vec![
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", ROOT_RELS.as_bytes()),
        ("ppt/presentation.xml", PRESENTATION.as_bytes()),
        (
            "ppt/_rels/presentation.xml.rels",
            PRESENTATION_RELS.as_bytes(),
        ),
        ("ppt/theme/theme1.xml", THEME.as_bytes()),
        ("ppt/slideMasters/slideMaster1.xml", SLIDE_MASTER.as_bytes()),
        (
            "ppt/slideMasters/_rels/slideMaster1.xml.rels",
            SLIDE_MASTER_RELS.as_bytes(),
        ),
        ("ppt/slideLayouts/slideLayout1.xml", SLIDE_LAYOUT.as_bytes()),
        (
            "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
            SLIDE_LAYOUT_RELS.as_bytes(),
        ),
        ("ppt/slides/slide1.xml", slide.as_bytes()),
        ("ppt/slides/_rels/slide1.xml.rels", slide_rels.as_bytes()),
    ];
    for (part_name, text) in extra {
        parts.push((part_name, text.as_bytes()));
    }

    let file = std::fs::File::create(&path).expect("创建临时文件失败");
    let mut zip = zip::ZipWriter::new(file);
    for (part_name, bytes) in parts {
        zip.start_file::<_, ()>(part_name, Default::default())
            .expect("写入 ZIP 条目失败");
        zip.write_all(bytes).expect("写入内容失败");
    }
    zip.finish().expect("完成 ZIP 失败");
    path
}

fn write_pptx(name: &str) -> PathBuf {
    build_pptx(name, SLIDE_1, SLIDE_1_RELS, &[])
}

fn fonts() -> Option<Arc<FontContext>> {
    let ctx = FontContext::new();
    if ctx.font_count() == 0 {
        eprintln!("跳过：系统中未找到可用字体");
        return None;
    }
    Some(Arc::new(ctx))
}

/// 解析出幻灯片场景。
fn scene(path: &std::path::Path) -> (PptxSource, Scene) {
    let src = PptxSource::open(path).expect("应能打开课件");
    let scene = match src.page_content(0).expect("应能解析第 1 页") {
        PageContent::Scene(s) => *s,
        PageContent::Bitmap(_) => panic!("PPTX 应产出场景图"),
    };
    (src, scene)
}

/// 统计与给定颜色「接近」的像素数（容差用于吸收抗锯齿与渐变过渡）。
fn count_near(bmp: &ppt_core::Bitmap, target: Color, tolerance: i32) -> usize {
    bmp.data
        .chunks_exact(4)
        .filter(|px| {
            let a = px[3] as i32;
            if a < 200 {
                return false; // 忽略半透明边缘
            }
            // 预乘格式下 alpha=255 时 RGB 即直通值
            (px[0] as i32 - target.r as i32).abs() <= tolerance
                && (px[1] as i32 - target.g as i32).abs() <= tolerance
                && (px[2] as i32 - target.b as i32).abs() <= tolerance
        })
        .count()
}

/// 取某点像素（转为直通 RGBA）。
fn pixel(bmp: &ppt_core::Bitmap, x: u32, y: u32) -> Color {
    bmp.pixel(x, y).unwrap_or(Color::TRANSPARENT)
}

/// 页面尺寸 720×540pt，渲染缩放 1.0 → 输出 720×540 像素。
fn render_default(
    renderer: &mut Renderer,
    scene: &Scene,
    media: &dyn ppt_core::MediaProvider,
) -> ppt_core::Bitmap {
    let opts = RenderOptions {
        scale: 1.0,
        ..RenderOptions::default()
    };
    renderer
        .render(scene, &opts, media)
        .expect("渲染应成功")
}

// ---------- 测试 ----------

#[test]
fn charts_render_to_real_pixels() {
    let Some(fonts) = fonts() else { return };
    let path = build_pptx(
        "openpptview-render-e2e-chart.pptx",
        SLIDE_CHART,
        SLIDE_CHART_RELS,
        &[
            ("ppt/charts/chart1.xml", CHART_BAR),
            ("ppt/charts/chart2.xml", CHART_PIE),
            ("ppt/charts/chart3.xml", CHART_RADAR),
        ],
    );
    let (src, scene) = scene(&path);

    // 场景层：只有雷达图应当降级为占位
    let placeholders = scene.walk().filter(|n| n.geometry.is_degraded()).count();
    assert_eq!(
        placeholders, 1,
        "只有不支持的雷达图该降级，实际 {placeholders} 个占位"
    );

    let mut renderer = Renderer::new(fonts);
    let bmp = render_default(&mut renderer, &scene, &src);

    // 柱状图：系列显式指定了 2E86DE；两根柱子（10 与 20，基线 0）
    // 在 288×180pt 的框架里应覆盖上万像素
    let bars = count_near(&bmp, Color::rgb(0x2E, 0x86, 0xDE), 20);
    assert!(bars > 6000, "柱状图应画出足够多的蓝色像素，实际 {bars}");

    // 光看像素总数抓不到「位置全错」——柱子堆在框架左上角的像素数和
    // 正确摆放几乎一样多。因此必须**按坐标**断言：
    // 第二根柱子（值 20，占满绘图区高度）的中段应当是蓝的
    let blue = Color::rgb(0x2E, 0x86, 0xDE);
    assert_eq!(
        pixel(&bmp, 257, 250),
        blue,
        "第二根柱子应落在自己的类别槽里"
    );
    // 而框架左上角（若变换只写 local_bbox 就会堆在这里）应当是白底
    assert_eq!(
        pixel(&bmp, 50, 250),
        Color::WHITE,
        "柱子不该堆在框架左上角"
    );

    // 饼图：三块扇形按主题 accent1/2/3 上色，比例 3:1:4。
    // 半径 84pt → 整圆约 22167 像素，三块理论值约 8312 / 2770 / 11084。
    let pie_area = 22_167.0;
    let a1 = count_near(&bmp, Color::rgb(0xC0, 0x00, 0x00), 40);
    let a2 = count_near(&bmp, Color::rgb(0x00, 0xB0, 0x50), 40);
    let a3 = count_near(&bmp, Color::rgb(0x00, 0x70, 0xC0), 40);
    for (name, got, want) in [("3/8 块", a1, 0.375), ("1/8 块", a2, 0.125), ("4/8 块", a3, 0.5)] {
        let expect = pie_area * want;
        assert!(
            (got as f32) > expect * 0.85,
            "饼图 {name} 面积应接近 {expect:.0} 像素，实际 {got}"
        );
    }

    // 面积对不代表位置对 —— 各块扇形若各自偏一个量，面积照样分毫不差。
    // 圆心在 (540,270)，从 12 点方向顺时针铺开，因此按角度取三点定位：
    let center = (540u32, 270u32);
    assert_eq!(
        pixel(&bmp, center.0 + 30, center.1 - 30), // -45°：第一块
        Color::rgb(0xC0, 0x00, 0x00),
        "第一块应落在右上方"
    );
    assert_eq!(
        pixel(&bmp, center.0 + 20, center.1 + 50), // 68°：第二块
        Color::rgb(0x00, 0xB0, 0x50),
        "第二块应落在右下方"
    );
    assert_eq!(
        pixel(&bmp, center.0 - 40, center.1), // 180°：第三块（占一半）
        Color::rgb(0x00, 0x70, 0xC0),
        "跨度 4/8 的块应占满左半边"
    );

    // 雷达图降级为浅灰占位框（288×120pt ≈ 34560 像素）
    let grey = count_near(&bmp, Color::rgb(0xE8, 0xE8, 0xE8), 6);
    assert!(grey > 20000, "不支持的图表应留下可见的占位框，实际 {grey}");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn renders_full_pipeline_from_pptx_to_bitmap() {
    let Some(fonts) = fonts() else { return };
    let path = write_pptx("openpptview-render-e2e-basic.pptx");
    let (src, scene) = scene(&path);

    let mut renderer = Renderer::new(fonts);
    let bmp = render_default(&mut renderer, &scene, &src);

    // 720×540pt @1.0 → 720×540 像素
    assert_eq!((bmp.width, bmp.height), (720, 540));
    assert!(bmp.is_consistent(), "位图缓冲长度应与尺寸自洽");
    assert_eq!(bmp.format, ppt_core::PixelFormat::Rgba8Premultiplied);

    // 背景应来自母版（bg1 → lt1 → 白）
    assert_eq!(pixel(&bmp, 10, 10), Color::WHITE, "背景应为白色");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn gradient_fill_is_actually_gradient() {
    let Some(fonts) = fonts() else { return };
    let path = write_pptx("openpptview-render-e2e-gradient.pptx");
    let (src, scene) = scene(&path);

    let mut renderer = Renderer::new(fonts);
    let bmp = render_default(&mut renderer, &scene, &src);

    // 渐变矩形：off(36,180) ext(288,180) pt
    // 左端应是红色调，右端应是蓝色调
    let left = pixel(&bmp, 42, 270);
    let right = pixel(&bmp, 318, 270);

    assert!(
        left.r > left.b,
        "渐变左端应偏红，实际 {left:?}"
    );
    assert!(
        right.b > right.r,
        "渐变右端应偏蓝，实际 {right:?}"
    );
    // 中间应是混合色（不是纯色）
    let mid = pixel(&bmp, 180, 270);
    assert!(
        mid.r > 30 && mid.b > 30,
        "渐变中段应同时含有红蓝分量，实际 {mid:?}"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn ellipse_fill_and_stroke_render() {
    let Some(fonts) = fonts() else { return };
    let path = write_pptx("openpptview-render-e2e-ellipse.pptx");
    let (src, scene) = scene(&path);

    let mut renderer = Renderer::new(fonts);
    let bmp = render_default(&mut renderer, &scene, &src);

    // 椭圆：off(360,180) ext(288,180) → 中心在 (504, 270)
    assert_eq!(
        pixel(&bmp, 504, 270),
        Color::rgb(255, 255, 0),
        "椭圆中心应为黄色填充"
    );

    // 椭圆外（右上角）应是背景白
    assert_eq!(pixel(&bmp, 700, 190), Color::WHITE, "椭圆外应为背景");

    // 描边宽度 38100 EMU = 3pt；在椭圆左端点附近应有深色像素
    // 左端点 x = 360，描边中心在 x=360，取 360 处应命中
    let on_stroke = pixel(&bmp, 360, 270);
    assert!(
        on_stroke.luminance() < 0.5,
        "椭圆左端应有黑色描边，实际 {on_stroke:?}"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn group_children_render_at_baked_positions() {
    let Some(fonts) = fonts() else { return };
    let path = write_pptx("openpptview-render-e2e-group.pptx");
    let (src, scene) = scene(&path);

    let mut renderer = Renderer::new(fonts);
    let bmp = render_default(&mut renderer, &scene, &src);

    // 组合 off(36,396) ext(144,72)，子矩形在子坐标系占满 → 画布 (36,396)-(180,468)
    // 取内部一点
    let inside = pixel(&bmp, 100, 430);
    assert_eq!(
        inside,
        Color::rgb(0, 255, 0),
        "组合内的绿色矩形应被绘制（子变换已烘焙为绝对坐标）"
    );

    // 再右侧一点（超出组合宽度）应是背景
    assert_eq!(pixel(&bmp, 300, 430), Color::WHITE);

    let _ = std::fs::remove_file(&path);
}

/// 组合的子坐标系被缩得极小 —— 真实课件里「图标 + 文字」的小组合就是这样：
/// `a:chExt` 只有几千、`a:ext` 却是几百万 EMU，比值小到 0.04。
///
/// 组合尺寸 240×120pt、子坐标系 6000×3000 单位，因此**一个子单位折合 508pt**；
/// 组内文本框的子坐标 (1000,1500)+4000×1000 折合画布 (76,276)+160×40pt。
const SLIDE_SCALED_GROUP: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:p="urn:p" xmlns:a="urn:a" xmlns:r="urn:r"><p:cSld><p:spTree>
  <p:grpSp>
    <p:nvGrpSpPr><p:cNvPr id="2" name="小组合"/></p:nvGrpSpPr>
    <p:grpSpPr><a:xfrm>
      <a:off x="457200" y="2743200"/><a:ext cx="3048000" cy="1524000"/>
      <a:chOff x="0" y="0"/><a:chExt cx="6000" cy="3000"/>
    </a:xfrm></p:grpSpPr>
    <p:sp>
      <p:nvSpPr><p:cNvPr id="3" name="组内文字"/></p:nvSpPr>
      <p:spPr>
        <a:xfrm><a:off x="1000" y="1500"/><a:ext cx="4000" cy="1000"/></a:xfrm>
        <a:prstGeom prst="rect"/><a:noFill/>
      </p:spPr>
      <p:txBody><a:bodyPr/><a:p><a:r><a:rPr sz="2400" b="1">
        <a:solidFill><a:srgbClr val="000000"/></a:solidFill>
      </a:rPr><a:t>组内文字</a:t></a:r></a:p></p:txBody>
    </p:sp>
  </p:grpSp>
</p:spTree></p:cSld></p:sld>"#;

/// 组内文字必须按声明的字号画出来。
///
/// 字号是**绝对的**：`a:rPr/@sz="2400"` 永远是 24pt，不随组合把坐标缩到多小。
/// 曾经排版用的是组合里那套被缩小的局部尺寸，24pt 一路被缩成 1pt ——
/// 课件里「Lead-in」这类标题、以及整块「Here it is! …」的说明文字，
/// 在页面上一片空白，正是这么来的。
#[test]
fn text_inside_a_scaled_group_keeps_its_declared_size() {
    let Some(fonts) = fonts() else { return };
    let path = build_pptx(
        "openpptview-render-e2e-scaled-group.pptx",
        SLIDE_SCALED_GROUP,
        SLIDE_1_RELS,
        &[],
    );
    let (src, scene) = scene(&path);

    // 组内文本框的四个角都落在 (76,276)-(236,316) 里
    let bbox = scene
        .walk()
        .find(|n| n.shape_id == Some(3))
        .map(|n| n.canvas_bounds())
        .expect("应找到组内文本框");
    assert!(
        (bbox.x - 76.0).abs() < 1.0 && (bbox.y - 276.0).abs() < 1.0,
        "组内文本框的位置不对：{bbox:?}"
    );
    assert!(
        (bbox.w - 160.0).abs() < 1.0 && (bbox.h - 40.0).abs() < 1.0,
        "组内文本框的尺寸应是 160×40pt：{bbox:?}"
    );

    let mut renderer = Renderer::new(fonts);
    let bmp = render_default(&mut renderer, &scene, &src);

    let dark_in = |x0: u32, y0: u32, x1: u32, y1: u32| {
        (x0..x1)
            .flat_map(|x| (y0..y1).map(move |y| (x, y)))
            .filter(|&(x, y)| pixel(&bmp, x, y).luminance() < 0.5)
            .count()
    };

    let inside = dark_in(78, 278, 234, 314);
    assert!(
        inside > 60,
        "组内的 24pt 文字应按 24pt 画出来，实际框内只有 {inside} 个深色像素"
    );

    // 框外必须干净：若字号被组合的局部单位放大/缩小，文字会溢出到框外
    let below = dark_in(78, 320, 234, 380);
    assert_eq!(below, 0, "框下面不该有文字（字号算错时会溢出到这里）");

    let _ = std::fs::remove_file(&path);
}

/// 一个「白底 + 外阴影」的矩形 —— 课件里大量圆角标注框都是这个配方。
const SLIDE_SHADOW: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:p="urn:p" xmlns:a="urn:a" xmlns:r="urn:r"><p:cSld><p:spTree>
  <p:sp>
    <p:nvSpPr><p:cNvPr id="2" name="白框"/></p:nvSpPr>
    <p:spPr>
      <a:xfrm><a:off x="457200" y="457200"/><a:ext cx="1828800" cy="914400"/></a:xfrm>
      <a:prstGeom prst="rect"/>
      <a:solidFill><a:srgbClr val="FFFFFF"/></a:solidFill>
      <a:effectLst>
        <a:outerShdw blurRad="63500" dist="50800" dir="2700000" rot="0">
          <a:srgbClr val="000000"><a:alpha val="60000"/></a:srgbClr>
        </a:outerShdw>
      </a:effectLst>
    </p:spPr>
  </p:sp>
</p:spTree></p:cSld></p:sld>"#;

/// 阴影必须画在形状**下面**。
///
/// 模糊是用三个偏移填充近似的，它们与形状本身大面积重叠；
/// 若画在填充之后，白框就被自己的阴影压成了灰框 ——
/// 课件里那些白色圆角标注框（「cardinal numbers 基数词」、
/// 「Where can we see Cardinal numbers…」）一个个都发灰，
/// 与 WPS / PowerPoint 对不上。
#[test]
fn shadow_does_not_darken_the_shape_itself() {
    let Some(fonts) = fonts() else { return };
    let path = build_pptx(
        "openpptview-render-e2e-shadow.pptx",
        SLIDE_SHADOW,
        SLIDE_1_RELS,
        &[],
    );
    let (src, scene) = scene(&path);

    let mut renderer = Renderer::new(fonts);
    let bmp = render_default(&mut renderer, &scene, &src);

    // 形状占 (36,36)-(180,108)pt
    assert_eq!(
        pixel(&bmp, 100, 70),
        Color::WHITE,
        "白框内部不该被自己的阴影压暗"
    );

    // 阴影偏移 4pt、方向 45°，形状右下方应能看到它
    let cast = pixel(&bmp, 182, 110);
    assert!(
        cast.luminance() < 0.9,
        "形状右下方应投出阴影，实际 {cast:?}"
    );

    // 阴影不该跑到形状左上方去（方向是 45°）
    assert_eq!(
        pixel(&bmp, 30, 30),
        Color::WHITE,
        "阴影方向应朝右下，左上角不该有阴影"
    );

    let _ = std::fs::remove_file(&path);
}

/// 一条 6pt 粗、带 `a:tailEnd type="triangle"` 的直线箭头连接符。
const SLIDE_ARROW: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:p="urn:p" xmlns:a="urn:a" xmlns:r="urn:r"><p:cSld><p:spTree>
  <p:cxnSp>
    <p:nvCxnSpPr><p:cNvPr id="2" name="箭头"/><p:cNvCxnSpPr/><p:nvPr/></p:nvCxnSpPr>
    <p:spPr>
      <a:xfrm><a:off x="1270000" y="2540000"/><a:ext cx="3810000" cy="0"/></a:xfrm>
      <a:prstGeom prst="straightConnector1"><a:avLst/></a:prstGeom>
      <a:ln w="76200"><a:solidFill><a:srgbClr val="000000"/></a:solidFill>
        <a:tailEnd type="triangle"/>
      </a:ln>
    </p:spPr>
  </p:cxnSp>
</p:spTree></p:cSld></p:sld>"#;

/// 箭头必须真的画出来。
///
/// 解析层一直把 `a:headEnd` / `a:tailEnd` 存进 `Stroke`，但渲染器从没用过 ——
/// 课件里那些箭头连接符就成了一根**实心横条**（第 11 页那三条 4.5pt 的橙色箭头，
/// 页面上是三道突兀的橙杠）。
#[test]
fn line_ends_are_drawn_at_the_path_ends() {
    let Some(fonts) = fonts() else { return };
    let path = build_pptx(
        "openpptview-render-e2e-arrow.pptx",
        SLIDE_ARROW,
        SLIDE_1_RELS,
        &[],
    );
    let (src, scene) = scene(&path);

    let mut renderer = Renderer::new(fonts);
    let bmp = render_default(&mut renderer, &scene, &src);

    // 线：从 (100,200) 到 (400,200)，6pt 粗 → 只覆盖 y 197..203
    assert!(
        pixel(&bmp, 250, 200).luminance() < 0.3,
        "线本身要画出来"
    );
    assert!(
        pixel(&bmp, 250, 194).luminance() > 0.9,
        "线的中段不该变粗（那里没有箭头）"
    );

    // 箭头：尺寸是线宽的 3 倍（缺省 med）→ 18pt 长、18pt 宽，尖端在 (400,200)
    // 于是 x=383 处（离尖端 17pt）上下都应铺到 ±8pt 左右
    let near_tip = pixel(&bmp, 383, 194);
    assert!(
        near_tip.luminance() < 0.3,
        "端点附近应被箭头铺开，实际 {near_tip:?}（箭头没画出来时这里是白的）"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn text_is_rendered_with_inherited_styles() {
    let Some(fonts) = fonts() else { return };
    let path = write_pptx("openpptview-render-e2e-text.pptx");
    let (src, scene) = scene(&path);

    let mut renderer = Renderer::new(fonts);
    let bmp = render_default(&mut renderer, &scene, &src);

    // 标题占位符继承母版 titleStyle：40pt 粗体、居中、accent1（C00000 深红）
    // 位置继承版式：off(36,36) ext(648,96)
    let dark_red = Color::rgb(0xC0, 0x00, 0x00);
    let red_pixels = count_near(&bmp, dark_red, 60);
    assert!(
        red_pixels > 80,
        "标题应以主题 accent1 的深红色绘制，实际仅 {red_pixels} 个接近像素"
    );

    // 直接文本「重点文字」为黑色 24pt 粗体
    let black_pixels = count_near(&bmp, Color::BLACK, 60);
    assert!(
        black_pixels > 150,
        "正文与「重点文字」应画出足够黑色像素，实际 {black_pixels}"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn rendering_is_deterministic() {
    let Some(fonts) = fonts() else { return };
    let path = write_pptx("openpptview-render-e2e-determinism.pptx");
    let (src, scene) = scene(&path);

    let mut r1 = Renderer::new(Arc::clone(&fonts));
    let a = render_default(&mut r1, &scene, &src);

    // 换一个渲染器实例（模拟重启），结果必须一致
    let mut r2 = Renderer::new(fonts);
    let b = render_default(&mut r2, &scene, &src);

    assert_eq!(a.width, b.width);
    assert_eq!(a.height, b.height);
    assert_eq!(a.data, b.data, "相同输入应得到逐字节一致的位图");
}

#[test]
fn scale_produces_matching_resolution() {
    let Some(fonts) = fonts() else { return };
    let path = write_pptx("openpptview-render-e2e-scale.pptx");
    let (src, scene) = scene(&path);

    let mut renderer = Renderer::new(fonts);
    for (scale, expect) in [(0.5f32, (360u32, 270u32)), (1.0, (720, 540)), (2.0, (1440, 1080))] {
        let opts = RenderOptions {
            scale,
            max_pixels: 16_000_000,
            ..RenderOptions::default()
        };
        let bmp = renderer.render(&scene, &opts, &src).expect("渲染应成功");
        assert_eq!(
            (bmp.width, bmp.height),
            expect,
            "缩放 {scale} 应得到 {expect:?}"
        );
    }

    let _ = std::fs::remove_file(&path);
}

#[test]
fn content_scales_with_resolution() {
    let Some(fonts) = fonts() else { return };
    let path = write_pptx("openpptview-render-e2e-scaling-content.pptx");
    let (src, scene) = scene(&path);

    let mut renderer = Renderer::new(fonts);

    // 在两种分辨率下，绿色矩形占总面积的比例应大致相同
    let ratio_at = |renderer: &mut Renderer, scale: f32| -> f64 {
        let opts = RenderOptions {
            scale,
            max_pixels: 16_000_000,
            ..RenderOptions::default()
        };
        let bmp = renderer.render(&scene, &opts, &src).unwrap();
        let green = count_near(&bmp, Color::rgb(0, 255, 0), 40);
        green as f64 / (bmp.width as f64 * bmp.height as f64)
    };

    let r1 = ratio_at(&mut renderer, 1.0);
    let r2 = ratio_at(&mut renderer, 2.0);

    assert!(r1 > 0.0 && r2 > 0.0, "两种分辨率下都应画出绿色矩形");
    assert!(
        (r1 - r2).abs() < 0.01,
        "内容占比应与分辨率无关：{r1:.4} vs {r2:.4}"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn thumbnail_options_still_produce_valid_bitmap() {
    let Some(fonts) = fonts() else { return };
    let path = write_pptx("openpptview-render-e2e-thumb.pptx");
    let (src, scene) = scene(&path);

    let mut renderer = Renderer::new(fonts);
    let opts = RenderOptions::thumbnail(0.15);
    let bmp = renderer.render(&scene, &opts, &src).expect("缩略图应能渲染");

    // 720×540 × 0.15 ≈ 108×81
    assert_eq!((bmp.width, bmp.height), (108, 81));
    assert!(bmp.is_consistent());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn renderer_reuses_caches_across_pages() {
    let Some(fonts) = fonts() else { return };
    let path = write_pptx("openpptview-render-e2e-cache.pptx");
    let (src, scene) = scene(&path);

    let mut renderer = Renderer::new(fonts);
    let _ = render_default(&mut renderer, &scene, &src);
    let glyphs = renderer.cached_glyph_count();
    assert!(glyphs > 0, "渲染文本后应有字形缓存");

    let _ = render_default(&mut renderer, &scene, &src);
    assert_eq!(
        renderer.cached_glyph_count(),
        glyphs,
        "第二次渲染应完全命中字形缓存"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn scene_and_bitmap_are_send_across_threads() {
    let Some(fonts) = fonts() else { return };
    let path = write_pptx("openpptview-render-e2e-threads.pptx");
    let src = Arc::new(PptxSource::open(&path).unwrap());

    // 模拟「多线程预渲染不同页」：每个线程独立取页并渲染
    let handles: Vec<_> = (0..3)
        .map(|i| {
            let src = Arc::clone(&src);
            let fonts = Arc::clone(&fonts);
            std::thread::spawn(move || {
                let scene = match src.page_content(0).unwrap() {
                    PageContent::Scene(s) => *s,
                    PageContent::Bitmap(_) => panic!("应为场景图"),
                };
                let mut renderer = Renderer::new(fonts);
                let bmp = renderer
                    .render(&scene, &RenderOptions::default(), src.as_ref())
                    .unwrap();
                // 返回一个校验值，确认结果一致
                (i, bmp.width, bmp.height, bmp.data.len())
            })
        })
        .collect();

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    for (_, w, h, len) in &results {
        assert_eq!(*w, 720);
        assert_eq!(*h, 540);
        assert_eq!(*len, 720 * 540 * 4);
    }

    let _ = std::fs::remove_file(&path);
}

#[test]
fn full_page_render_completes_in_reasonable_time() {
    let Some(fonts) = fonts() else { return };
    let path = write_pptx("openpptview-render-e2e-perf.pptx");
    let (src, scene) = scene(&path);

    let mut renderer = Renderer::new(fonts);
    // 首次渲染包含字体索引与字形缓存构建，因此先热身
    let _ = render_default(&mut renderer, &scene, &src);

    let start = std::time::Instant::now();
    for _ in 0..10 {
        let _ = render_default(&mut renderer, &scene, &src);
    }
    let elapsed = start.elapsed();
    let per_page = elapsed / 10;

    // spec 的预算是「单页 ≤ 80ms（1000×562 输出）」。
    // 这里是 720×540 且内容简单，留出充足余量即可；
    // 真正的性能验收在基准机上跑 bench 脚本（见 Task 6.7）。
    assert!(
        per_page.as_millis() < 80,
        "单页渲染应远快于预算，实际 {per_page:?}"
    );

    let _ = std::fs::remove_file(&path);
}
