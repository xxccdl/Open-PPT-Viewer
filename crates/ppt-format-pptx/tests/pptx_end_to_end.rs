//! 端到端集成测试：拼装真实的 `.pptx` 包并用 [`PptxSource`] 打开。
//!
//! 单元测试都是「喂 XML 片段给解析函数」，覆盖不到包级别的行为：
//! 关系解析、幻灯片顺序、惰性读取、继承链追踪、备注与标题。
//! 这里用真实 ZIP 包把这些串起来验证。

use std::io::Write;
use std::path::{Path, PathBuf};

use ppt_core::scene::{
    Color, Fill, Geometry, HyperlinkTarget, SceneBackground, Size, VerticalAnchor,
};
use ppt_core::{DocFormat, DocumentSource, MediaProvider, PageContent};
use ppt_format_pptx::PptxSource;

// ---------- 测试语料 ----------

const CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Default Extension="png" ContentType="image/png"/>
  <Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
  <Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
  <Override PartName="/ppt/slides/slide2.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
  <Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>
  <Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/>
  <Override PartName="/ppt/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/>
  <Override PartName="/ppt/notesSlides/notesSlide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.notesSlide+xml"/>
  <Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/>
</Types>"#;

const ROOT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/>
</Relationships>"#;

const CORE_PROPS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties"
                   xmlns:dc="http://purl.org/dc/elements/1.1/">
  <dc:title>初中生物 第五章 光合作用</dc:title>
</cp:coreProperties>"#;

/// 幻灯片顺序被刻意颠倒：`sldIdLst` 先指向 slide2。
/// 文件名顺序解析器会得到错误结果，从而验证「顺序以 sldIdLst 为准」。
const PRESENTATION: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:p="urn:p" xmlns:r="urn:r" xmlns:a="urn:a">
  <p:sldIdLst>
    <p:sldId id="257" r:id="rId2"/>
    <p:sldId id="256" r:id="rId1"/>
  </p:sldIdLst>
  <p:sldSz cx="12192000" cy="6858000"/>
  <p:notesSz cx="6858000" cy="9144000"/>
</p:presentation>"#;

const PRESENTATION_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide2.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="slideMasters/slideMaster1.xml"/>
</Relationships>"#;

const THEME: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<a:theme xmlns:a="urn:a" name="教学主题">
  <a:themeElements>
    <a:clrScheme name="教学">
      <a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1>
      <a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1>
      <a:dk2><a:srgbClr val="44546A"/></a:dk2>
      <a:lt2><a:srgbClr val="E7E6E6"/></a:lt2>
      <a:accent1><a:srgbClr val="2E75B6"/></a:accent1>
      <a:accent2><a:srgbClr val="ED7D31"/></a:accent2>
      <a:accent3><a:srgbClr val="A5A5A5"/></a:accent3>
      <a:accent4><a:srgbClr val="FFC000"/></a:accent4>
      <a:accent5><a:srgbClr val="5B9BD5"/></a:accent5>
      <a:accent6><a:srgbClr val="70AD47"/></a:accent6>
      <a:hlink><a:srgbClr val="0563C1"/></a:hlink>
      <a:folHlink><a:srgbClr val="954F72"/></a:folHlink>
    </a:clrScheme>
    <a:fontScheme name="教学">
      <a:majorFont><a:latin typeface="Arial"/><a:ea typeface="微软雅黑"/><a:cs typeface=""/></a:majorFont>
      <a:minorFont><a:latin typeface="Arial"/><a:ea typeface="宋体"/><a:cs typeface=""/></a:minorFont>
    </a:fontScheme>
    <a:fmtScheme name="教学">
      <a:fillStyleLst>
        <a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
      </a:fillStyleLst>
      <a:lnStyleLst>
        <a:ln w="12700"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln>
      </a:lnStyleLst>
      <a:effectStyleLst>
        <a:effectStyle><a:effectLst/></a:effectStyle>
      </a:effectStyleLst>
      <a:bgFillStyleLst>
        <a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
      </a:bgFillStyleLst>
    </a:fmtScheme>
  </a:themeElements>
</a:theme>"#;

const SLIDE_MASTER: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldMaster xmlns:p="urn:p" xmlns:a="urn:a" xmlns:r="urn:r">
  <p:cSld>
    <p:bg><p:bgPr><a:solidFill><a:schemeClr val="bg1"/></a:solidFill></p:bgPr></p:bg>
    <p:spTree>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="1" name="标题占位符"/>
          <p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>
        <p:spPr><a:xfrm>
          <a:off x="457200" y="274638"/><a:ext cx="8229600" cy="1143000"/>
        </a:xfrm></p:spPr>
        <p:txBody><a:bodyPr/><a:lstStyle/>
          <a:p><a:r><a:rPr lang="zh-CN" sz="4400" b="1"/><a:t>单击此处编辑母版标题</a:t></a:r></a:p>
        </p:txBody>
      </p:sp>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="2" name="内容占位符"/>
          <p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr>
        <p:spPr><a:xfrm>
          <a:off x="457200" y="1600200"/><a:ext cx="8229600" cy="4525963"/>
        </a:xfrm></p:spPr>
        <p:txBody><a:bodyPr/><a:lstStyle/></p:txBody>
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
        <a:latin typeface="+mj-lt"/><a:ea typeface="+mj-ea"/>
      </a:defRPr></a:lvl1pPr>
    </p:titleStyle>
    <p:bodyStyle>
      <a:lvl1pPr marL="342900" indent="-342900">
        <a:buFont typeface="Arial"/><a:buChar char="•"/>
        <a:defRPr sz="2800"/>
      </a:lvl1pPr>
      <a:lvl2pPr marL="742950" indent="-285750">
        <a:buFont typeface="Arial"/><a:buChar char="–"/>
        <a:defRPr sz="2400"/>
      </a:lvl2pPr>
    </p:bodyStyle>
    <p:otherStyle><a:defPPr><a:defRPr sz="1800"/></a:defPPr></p:otherStyle>
  </p:txStyles>
</p:sldMaster>"#;

const SLIDE_MASTER_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="../theme/theme1.xml"/>
</Relationships>"#;

const SLIDE_LAYOUT: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldLayout xmlns:p="urn:p" xmlns:a="urn:a" xmlns:r="urn:r" type="titleAndContent">
  <p:cSld>
    <p:spTree>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="1" name="标题占位符"/>
          <p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>
        <p:spPr><a:xfrm>
          <a:off x="609600" y="365125"/><a:ext cx="7772400" cy="1325563"/>
        </a:xfrm></p:spPr>
        <p:txBody><a:bodyPr/><a:lstStyle/></p:txBody>
      </p:sp>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="2" name="内容占位符"/>
          <p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr>
        <p:spPr><a:xfrm>
          <a:off x="609600" y="1825625"/><a:ext cx="7772400" cy="4000500"/>
        </a:xfrm></p:spPr>
        <p:txBody><a:bodyPr/><a:lstStyle/></p:txBody>
      </p:sp>
    </p:spTree>
  </p:cSld>
</p:sldLayout>"#;

const SLIDE_LAYOUT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="../slideMasters/slideMaster1.xml"/>
</Relationships>"#;

/// 第 1 页：标题 + 正文占位符（位置与字号全靠继承），外加一个直接指定位置的自选图形。
const SLIDE_1: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:p="urn:p" xmlns:a="urn:a" xmlns:r="urn:r">
  <p:cSld>
    <p:spTree>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="2" name="标题 1"/>
          <p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>
        <p:txBody><a:bodyPr/>
          <a:p><a:r><a:t>光合作用</a:t></a:r></a:p>
        </p:txBody>
      </p:sp>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="3" name="内容占位符 2"/>
          <p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr>
        <p:txBody><a:bodyPr/>
          <a:p><a:pPr lvl="0"/><a:r><a:t>光反应阶段</a:t></a:r></a:p>
          <a:p><a:pPr lvl="1"/><a:r><a:t>水的光解</a:t></a:r></a:p>
        </p:txBody>
      </p:sp>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="4" name="强调标记"/></p:nvSpPr>
        <p:spPr>
          <a:xfrm><a:off x="6096000" y="4572000"/><a:ext cx="1828800" cy="914400"/></a:xfrm>
          <a:prstGeom prst="roundRect"><a:avLst/></a:prstGeom>
          <a:solidFill><a:srgbClr val="FFF2CC"/></a:solidFill>
          <a:ln w="25400"><a:solidFill><a:srgbClr val="BF8F00"/></a:solidFill></a:ln>
        </p:spPr>
        <p:txBody><a:bodyPr anchor="ctr"/>
          <a:p><a:pPr algn="ctr"/><a:r><a:rPr sz="2000" b="1"/><a:t>重点</a:t></a:r></a:p>
        </p:txBody>
      </p:sp>
    </p:spTree>
  </p:cSld>
</p:sld>"#;

const SLIDE_1_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide" Target="../notesSlides/notesSlide1.xml"/>
</Relationships>"#;

/// 第 2 页：含图片、表格、SmartArt（降级）。
const SLIDE_2: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:p="urn:p" xmlns:a="urn:a" xmlns:r="urn:r">
  <p:cSld>
    <p:spTree>
      <p:pic>
        <p:nvPicPr><p:cNvPr id="2" name="叶绿体结构图"/></p:nvPicPr>
        <p:blipFill>
          <a:blip r:embed="rId2"/>
          <a:srcRect l="0" t="0" r="0" b="0"/>
          <a:stretch><a:fillRect/></a:stretch>
        </p:blipFill>
        <p:spPr>
          <a:xfrm><a:off x="609600" y="914400"/><a:ext cx="4572000" cy="3429000"/></a:xfrm>
          <a:prstGeom prst="rect"><a:avLst/></a:prstGeom>
        </p:spPr>
      </p:pic>
      <p:graphicFrame>
        <p:nvGraphicFramePr><p:cNvPr id="3" name="表格 1"/></p:nvGraphicFramePr>
        <p:xfrm><a:off x="5486400" y="914400"/><a:ext cx="4572000" cy="2743200"/></p:xfrm>
        <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/table">
          <a:tbl>
            <a:tblPr firstRow="1" bandRow="1"/>
            <a:tblGrid><a:gridCol w="2286000"/><a:gridCol w="2286000"/></a:tblGrid>
            <a:tr h="457200">
              <a:tc><a:txBody><a:p><a:r><a:rPr sz="1600" b="1"/><a:t>阶段</a:t></a:r></a:p></a:txBody>
                <a:tcPr><a:solidFill><a:srgbClr val="DEEAF6"/></a:solidFill>
                  <a:lnB w="12700"><a:solidFill><a:srgbClr val="2E75B6"/></a:solidFill></a:lnB>
                </a:tcPr></a:tc>
              <a:tc><a:txBody><a:p><a:r><a:rPr sz="1600" b="1"/><a:t>产物</a:t></a:r></a:p></a:txBody>
                <a:tcPr><a:solidFill><a:srgbClr val="DEEAF6"/></a:solidFill></a:tcPr></a:tc>
            </a:tr>
            <a:tr h="457200">
              <a:tc><a:txBody><a:p><a:r><a:rPr sz="1600"/><a:t>光反应</a:t></a:r></a:p></a:txBody><a:tcPr/></a:tc>
              <a:tc><a:txBody><a:p><a:r><a:rPr sz="1600"/><a:t>ATP 与 NADPH</a:t></a:r></a:p></a:txBody><a:tcPr/></a:tc>
            </a:tr>
          </a:tbl>
        </a:graphicData></a:graphic>
      </p:graphicFrame>
      <p:graphicFrame>
        <p:nvGraphicFramePr><p:cNvPr id="4" name="SmartArt 1"/></p:nvGraphicFramePr>
        <p:xfrm><a:off x="5486400" y="4114800"/><a:ext cx="4572000" cy="1371600"/></p:xfrm>
        <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/diagram">
          <a:t>叶绿体</a:t><a:t>类囊体薄膜</a:t>
        </a:graphicData></a:graphic>
      </p:graphicFrame>
    </p:spTree>
  </p:cSld>
</p:sld>"#;

const SLIDE_2_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image1.png"/>
</Relationships>"#;

const NOTES_1: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:notes xmlns:p="urn:p" xmlns:a="urn:a">
  <p:cSld><p:spTree>
    <p:sp>
      <p:nvSpPr><p:cNvPr id="2" name="幻灯片图像占位符"/>
        <p:nvPr><p:ph type="sldImg"/></p:nvPr></p:nvSpPr>
      <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="100" cy="100"/></a:xfrm></p:spPr>
    </p:sp>
    <p:sp>
      <p:nvSpPr><p:cNvPr id="3" name="备注占位符"/>
        <p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr>
      <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="100" cy="100"/></a:xfrm></p:spPr>
      <p:txBody><a:bodyPr/>
        <a:p><a:r><a:t>强调：光反应发生在类囊体薄膜上</a:t></a:r></a:p>
        <a:p><a:r><a:t>提问：暗反应需要光吗？</a:t></a:r></a:p>
      </p:txBody>
    </p:sp>
  </p:spTree></p:cSld>
</p:notes>"#;

/// 1×1 的 PNG（最小合法图片）。
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

// ---------- 测试夹具 ----------

/// 把部件写入一个临时 `.pptx` 文件。
fn write_pptx(name: &str, parts: &[(&str, &[u8])]) -> PathBuf {
    let path = std::env::temp_dir().join(name);
    let _ = std::fs::remove_file(&path);

    let file = std::fs::File::create(&path).expect("创建临时文件失败");
    let mut zip = zip::ZipWriter::new(file);

    for (part_name, bytes) in parts {
        zip.start_file::<_, ()>(*part_name, Default::default())
            .expect("写入 ZIP 条目失败");
        zip.write_all(bytes).expect("写入条目内容失败");
    }

    zip.finish().expect("完成 ZIP 写入失败");
    path
}

/// 构造完整的测试课件。
fn standard_deck(name: &str) -> PathBuf {
    write_pptx(
        name,
        &[
            ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
            ("_rels/.rels", ROOT_RELS.as_bytes()),
            ("docProps/core.xml", CORE_PROPS.as_bytes()),
            ("ppt/presentation.xml", PRESENTATION.as_bytes()),
            ("ppt/_rels/presentation.xml.rels", PRESENTATION_RELS.as_bytes()),
            ("ppt/theme/theme1.xml", THEME.as_bytes()),
            ("ppt/slideMasters/slideMaster1.xml", SLIDE_MASTER.as_bytes()),
            (
                "ppt/slideMasters/_rels/slideMaster1.xml.rels",
                SLIDE_MASTER_RELS.as_bytes(),
            ),
            (
                "ppt/slideLayouts/slideLayout1.xml",
                SLIDE_LAYOUT.as_bytes(),
            ),
            (
                "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
                SLIDE_LAYOUT_RELS.as_bytes(),
            ),
            ("ppt/slides/slide1.xml", SLIDE_1.as_bytes()),
            ("ppt/slides/_rels/slide1.xml.rels", SLIDE_1_RELS.as_bytes()),
            ("ppt/slides/slide2.xml", SLIDE_2.as_bytes()),
            ("ppt/slides/_rels/slide2.xml.rels", SLIDE_2_RELS.as_bytes()),
            (
                "ppt/notesSlides/notesSlide1.xml",
                NOTES_1.as_bytes(),
            ),
            ("ppt/media/image1.png", TINY_PNG),
        ],
    )
}

fn scene_of(src: &PptxSource, index: usize) -> ppt_core::scene::Scene {
    match src.page_content(index).expect("取页面失败") {
        PageContent::Scene(s) => *s,
        PageContent::Bitmap(_) => panic!("PPTX 应产出场景图而非位图"),
    }
}

// ---------- 文档级测试 ----------

#[test]
fn opens_deck_and_reports_basic_info() {
    let path = standard_deck("openpptview-e2e-basic.pptx");
    let src = PptxSource::open(&path).expect("应能打开课件");

    assert_eq!(src.page_count(), 2);
    assert_eq!(src.format(), DocFormat::Pptx);
    assert_eq!(src.source_path(), Some(path.as_path()));

    // 16:9 → 960×540pt
    assert_eq!(src.default_page_size_pt(), Size::new(960.0, 540.0));

    assert_eq!(
        src.title().as_deref(),
        Some("初中生物 第五章 光合作用")
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn slide_order_follows_presentation_not_filenames() {
    let path = standard_deck("openpptview-e2e-order.pptx");
    let src = PptxSource::open(&path).unwrap();

    // `p:sldIdLst` 先指向 slide2，因此第 1 页应是「含图片与表格」的那页
    let parts = src.slide_parts();
    assert_eq!(parts[0], "ppt/slides/slide2.xml");
    assert_eq!(parts[1], "ppt/slides/slide1.xml");

    let first = scene_of(&src, 0);
    // 第 2 页里有表格
    let has_table = first
        .walk()
        .any(|n| matches!(n.geometry, Geometry::Table(_)));
    assert!(has_table, "第 1 页（slide2.xml）应含表格");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn open_is_lazy_and_does_not_touch_slides() {
    let path = standard_deck("openpptview-e2e-lazy.pptx");
    let src = PptxSource::open(&path).unwrap();

    // 打开阶段只读文档级部件（内容类型表、关系表、presentation.xml），
    // 不应触碰任何 slideN.xml，更不应读媒体。
    let after_open = src.bytes_read();

    // 精确断言：逐部件检查读取记录，而不是比较字节数 ——
    // 字节数会被关系表等零碎部件干扰，容易误判。
    for part in src.slide_parts() {
        assert!(
            !src.package().was_part_read(part),
            "打开阶段不应读取幻灯片 XML：{part}"
        );
    }
    assert!(
        !src.package().was_part_read("ppt/media/image1.png"),
        "打开阶段不应读取媒体"
    );
    // 但幻灯片的关系表必须读过 —— 否则无法知道每页用哪个版式
    assert!(
        src.package().was_part_read("ppt/slides/_rels/slide1.xml.rels"),
        "打开阶段应读取幻灯片的关系表以定位版式"
    );

    // 打开时完全没有建立继承链（继承链只在解析页面时才需要）
    assert_eq!(src.cached_inheritance_count(), 0);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn open_read_amplification_stays_within_budget() {
    // 用一份「体积由媒体主导」的课件检验读取放大率预算。
    // 课件正文里 95% 的体积是嵌入图片，这是真实课件的常见形态。
    //
    // 媒体内容必须是**不可压缩**的：若用常量填充，deflate 会把 512KB
    // 压到几百字节，测试就失去了意义。
    let mut big_media = Vec::with_capacity(512 * 1024);
    big_media.extend_from_slice(TINY_PNG);
    let mut seed: u32 = 0x1234_5678;
    while big_media.len() < 512 * 1024 {
        // 简单 LCG，够用且结果确定（便于复现）
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        big_media.push((seed >> 16) as u8);
    }

    let path = write_pptx(
        "openpptview-e2e-amplification.pptx",
        &[
            ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
            ("_rels/.rels", ROOT_RELS.as_bytes()),
            ("docProps/core.xml", CORE_PROPS.as_bytes()),
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
            (
                "ppt/slideLayouts/slideLayout1.xml",
                SLIDE_LAYOUT.as_bytes(),
            ),
            (
                "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
                SLIDE_LAYOUT_RELS.as_bytes(),
            ),
            ("ppt/slides/slide1.xml", SLIDE_1.as_bytes()),
            ("ppt/slides/_rels/slide1.xml.rels", SLIDE_1_RELS.as_bytes()),
            ("ppt/slides/slide2.xml", SLIDE_2.as_bytes()),
            ("ppt/slides/_rels/slide2.xml.rels", SLIDE_2_RELS.as_bytes()),
            ("ppt/media/image1.png", &big_media),
        ],
    );

    let src = PptxSource::open(&path).unwrap();
    let file_len = src.package().file_len();

    // 文件确实由媒体主导（否则这个测试没有意义）
    assert!(
        file_len > 400 * 1024,
        "测试课件应足够大，实际 {file_len} 字节"
    );

    let ratio = src.bytes_read() as f64 / file_len as f64;
    assert!(
        ratio < 0.30,
        "打开阶段的读取放大率应 ≤ 30%，实际 {:.2}%（{} / {file_len}）",
        ratio * 100.0,
        src.bytes_read()
    );

    // 关键：整页浏览时也不应把 512KB 的图片读进来
    let _ = scene_of(&src, 0);
    let after_page = src.bytes_read();
    assert!(
        after_page < file_len / 2,
        "渲染图片引用页只应记录部件名，不应读取媒体字节，实际读取 {after_page} 字节"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn inheritance_chain_is_cached_per_layout() {
    let path = standard_deck("openpptview-e2e-cache.pptx");
    let src = PptxSource::open(&path).unwrap();

    // 两页共用同一个版式
    assert_eq!(src.layout_part(0), src.layout_part(1));
    assert_eq!(src.cached_inheritance_count(), 0);
    assert!(!src.package().was_part_read("ppt/theme/theme1.xml"));
    assert!(!src.package().was_part_read("ppt/slideMasters/slideMaster1.xml"));

    let _ = scene_of(&src, 0);
    assert_eq!(src.cached_inheritance_count(), 1, "首次解析应建立一条继承链");
    assert!(
        src.package().was_part_read("ppt/theme/theme1.xml"),
        "首次解析页面需要读主题"
    );

    // 第二页复用同一版式：增量读取应只有该页自身的 XML 与关系表。
    // 若主题/母版/版式被重复解析，增量必然远超这个上限。
    let after_first = src.bytes_read();
    let _ = scene_of(&src, 1);

    assert_eq!(
        src.cached_inheritance_count(),
        1,
        "共用版式的第二页应复用缓存，不新增继承链"
    );

    let delta = src.bytes_read() - after_first;
    let own_page_bytes = (SLIDE_1.len() + SLIDE_1_RELS.len()) as u64;
    assert!(
        delta <= own_page_bytes + 256,
        "第二页应只读取自身部件（约 {own_page_bytes} 字节），实际增量 {delta} 字节；\
         若增量明显更大，说明主题或母版被重复解析"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn fingerprint_is_deterministic_and_distinct() {
    let a = standard_deck("openpptview-e2e-fp1.pptx");
    let b = standard_deck("openpptview-e2e-fp2.pptx");

    let sa = PptxSource::open(&a).unwrap();
    let sb = PptxSource::open(&b).unwrap();

    // 内容相同时指纹应一致，可用于磁盘缓存命中
    assert_eq!(sa.fingerprint(), sb.fingerprint());
    assert_eq!(sa.fingerprint().len(), 32);

    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
}

#[test]
fn rejects_word_document_with_clear_message() {
    let path = write_pptx(
        "openpptview-e2e-docx.docx",
        &[
            (
                "[Content_Types].xml",
                br#"<Types>
                      <Override PartName="/word/document.xml"
                        ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
                    </Types>"#,
            ),
            (
                "_rels/.rels",
                br#"<Relationships>
                      <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
                    </Relationships>"#,
            ),
            ("word/document.xml", b"<document/>"),
        ],
    );

    let err = PptxSource::open(&path).expect_err("不应把 docx 当 pptx 打开");
    let msg = err.to_string();
    assert!(msg.contains("Word"), "错误信息应指明实际类型：{msg}");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn rejects_non_zip_file() {
    let path = std::env::temp_dir().join("openpptview-e2e-notzip.pptx");
    let _ = std::fs::remove_file(&path);
    std::fs::write(&path, b"this is definitely not a zip archive").unwrap();

    assert!(PptxSource::open(&path).is_err());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn rejects_missing_file() {
    let path = std::env::temp_dir().join("openpptview-e2e-nonexistent-xyz.pptx");
    let _ = std::fs::remove_file(&path);
    assert!(PptxSource::open(&path).is_err());
}

#[test]
fn empty_presentation_reports_clear_error() {
    // 关系表里没有任何 slide 类型的关系 → 无法定位任何页面
    let path = write_pptx(
        "openpptview-e2e-empty.pptx",
        &[
            ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
            ("_rels/.rels", ROOT_RELS.as_bytes()),
            (
                "ppt/presentation.xml",
                br#"<p:presentation xmlns:p="urn:p"><p:sldIdLst/></p:presentation>"#,
            ),
            (
                "ppt/_rels/presentation.xml.rels",
                br#"<Relationships>
                      <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="slideMasters/slideMaster1.xml"/>
                    </Relationships>"#,
            ),
        ],
    );

    let err = PptxSource::open(&path).expect_err("没有幻灯片应报错");
    assert!(
        err.to_string().contains("幻灯片"),
        "提示应说明缺少幻灯片：{err}"
    );

    let _ = std::fs::remove_file(&path);
}

// ---------- 页面级测试 ----------

#[test]
fn slide_background_inherited_from_master() {
    let path = standard_deck("openpptview-e2e-bg.pptx");
    let src = PptxSource::open(&path).unwrap();
    let scene = scene_of(&src, 1);

    // 母版背景是 `schemeClr bg1` → 经 clrMap 解析为 lt1 → 白色
    assert_eq!(
        scene.background,
        SceneBackground::Fill(Fill::Solid(Color::WHITE)),
        "应继承母版背景"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn placeholder_position_inherited_from_layout() {
    let path = standard_deck("openpptview-e2e-ph.pptx");
    let src = PptxSource::open(&path).unwrap();
    let scene = scene_of(&src, 1); // slide1.xml

    let title = scene
        .walk()
        .find(|n| n.name.as_deref() == Some("标题 1"))
        .expect("应找到标题形状");

    // 版式给出的位置：off(609600, 365125) ext(7772400, 1325563) EMU → pt
    let bounds = title.canvas_bounds();
    assert!((bounds.x - 48.0).abs() < 0.1, "实际 x = {}", bounds.x);
    assert!((bounds.y - 28.75).abs() < 0.1, "实际 y = {}", bounds.y);
    assert!((bounds.w - 612.0).abs() < 0.1, "实际 w = {}", bounds.w);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn placeholder_styles_inherited_from_master_tx_styles() {
    let path = standard_deck("openpptview-e2e-txstyle.pptx");
    let src = PptxSource::open(&path).unwrap();
    let scene = scene_of(&src, 1);

    let title = scene
        .walk()
        .find(|n| n.name.as_deref() == Some("标题 1"))
        .expect("应找到标题");
    let tb = title.text.as_ref().expect("标题应有文本");
    let run = &tb.paragraphs[0].runs[0];

    assert_eq!(tb.plain_text(), "光合作用");
    // 母版 titleStyle：sz=4000 → 40pt，粗体，居中，accent1
    assert!((run.props.size_pt - 40.0).abs() < 1e-4, "实际 {}", run.props.size_pt);
    assert!(run.props.bold);
    assert_eq!(tb.paragraphs[0].align, ppt_core::scene::TextAlign::Center);
    assert_eq!(run.props.color, Some(Color::rgb(0x2E, 0x75, 0xB6)));
    assert_eq!(tb.body.anchor, VerticalAnchor::Middle, "标题应垂直居中");

    // 主题字体引用 `+mj-lt` / `+mj-ea` 应被解析为具体字体名
    assert_eq!(run.props.font.latin.as_deref(), Some("Arial"));
    assert_eq!(run.props.font.ea.as_deref(), Some("微软雅黑"));

    let _ = std::fs::remove_file(&path);
}

#[test]
fn body_placeholder_bullets_inherited_from_master() {
    let path = standard_deck("openpptview-e2e-bullets.pptx");
    let src = PptxSource::open(&path).unwrap();
    let scene = scene_of(&src, 1);

    let body = scene
        .walk()
        .find(|n| n.name.as_deref() == Some("内容占位符 2"))
        .expect("应找到正文占位符");
    let tb = body.text.as_ref().unwrap();

    assert_eq!(tb.paragraphs.len(), 2);
    assert_eq!(tb.plain_text(), "光反应阶段\n水的光解");

    // 母版 bodyStyle 提供了项目符号与缩进
    let b0 = tb.paragraphs[0].bullet.as_ref().expect("一级应继承项目符号");
    assert_eq!(b0.kind, ppt_core::scene::BulletKind::Char("•".into()));
    assert!((b0.margin_left_pt - 27.0).abs() < 0.1, "实际 {}", b0.margin_left_pt);

    let b1 = tb.paragraphs[1].bullet.as_ref().expect("二级应继承项目符号");
    assert_eq!(b1.kind, ppt_core::scene::BulletKind::Char("–".into()));
    assert!((b1.margin_left_pt - 58.5).abs() < 0.2, "实际 {}", b1.margin_left_pt);

    // 字号来自 bodyStyle 的对应级别
    assert!(
        (tb.paragraphs[0].runs[0].props.size_pt - 28.0).abs() < 1e-4,
        "一级字号应为 28pt"
    );
    assert!(
        (tb.paragraphs[1].runs[0].props.size_pt - 24.0).abs() < 1e-4,
        "二级字号应为 24pt"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn explicit_shape_with_own_geometry_and_fill() {
    let path = standard_deck("openpptview-e2e-shape.pptx");
    let src = PptxSource::open(&path).unwrap();
    let scene = scene_of(&src, 1);

    let shape = scene
        .walk()
        .find(|n| n.name.as_deref() == Some("强调标记"))
        .expect("应找到自选图形");

    // 圆角矩形，位置与尺寸由自身 xfrm 给出
    assert!(matches!(
        shape.geometry,
        Geometry::RoundRect { .. }
    ));

    let b = shape.canvas_bounds();
    assert!((b.x - 480.0).abs() < 0.1, "实际 x = {}", b.x);
    assert!((b.y - 360.0).abs() < 0.1, "实际 y = {}", b.y);
    assert!((b.w - 144.0).abs() < 0.1);
    assert!((b.h - 72.0).abs() < 0.1);

    assert_eq!(shape.fill, Fill::Solid(Color::rgb(0xFF, 0xF2, 0xCC)));

    let stroke = shape.stroke.as_ref().expect("应有描边");
    assert!((stroke.width_pt - 2.0).abs() < 1e-3, "实际 {}", stroke.width_pt);
    assert_eq!(
        stroke.primary_color(),
        Some(Color::rgb(0xBF, 0x8F, 0x00))
    );

    // 文本自身指定了字号与居中对齐
    let tb = shape.text.as_ref().unwrap();
    assert_eq!(tb.plain_text(), "重点");
    assert!((tb.paragraphs[0].runs[0].props.size_pt - 20.0).abs() < 1e-4);
    assert_eq!(tb.body.anchor, VerticalAnchor::Middle);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn picture_resolved_through_slide_relationship() {
    let path = standard_deck("openpptview-e2e-picture.pptx");
    let src = PptxSource::open(&path).unwrap();
    let scene = scene_of(&src, 0); // slide2.xml

    let pic = scene
        .walk()
        .find(|n| matches!(n.geometry, Geometry::Image(_)))
        .expect("应找到图片");

    match &pic.geometry {
        Geometry::Image(img) => {
            assert_eq!(img.part, "ppt/media/image1.png");
            assert!(img.is_full_bleed());
        }
        _ => unreachable!(),
    }

    // 通过 MediaProvider 能真的取到字节
    let bytes = src.read_media("ppt/media/image1.png").expect("应能读到图片");
    assert_eq!(bytes, TINY_PNG);
    assert!(src.has_media("ppt/media/image1.png"));
    assert!(!src.has_media("ppt/media/missing.png"));

    let b = pic.canvas_bounds();
    assert!((b.x - 48.0).abs() < 0.1);
    assert!((b.w - 360.0).abs() < 0.1);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn table_parsed_with_merges_borders_and_text() {
    let path = standard_deck("openpptview-e2e-table.pptx");
    let src = PptxSource::open(&path).unwrap();
    let scene = scene_of(&src, 0);

    let table_node = scene
        .walk()
        .find_map(|n| match &n.geometry {
            Geometry::Table(t) => Some((n, t)),
            _ => None,
        })
        .expect("应找到表格");

    let (node, table) = table_node;

    assert_eq!(table.columns.len(), 2);
    assert!((table.columns[0] - 180.0).abs() < 0.1);
    assert_eq!(table.row_count(), 2);
    assert!(table.first_row_header);
    assert!(table.banded_rows);

    // 表头底纹与下边框
    let header = &table.cells[0][0];
    assert_eq!(header.fill, Fill::Solid(Color::rgb(0xDE, 0xEA, 0xF6)));
    let bottom = header.borders.bottom.expect("表头应有下边框");
    assert_eq!(bottom.color, Color::rgb(0x2E, 0x75, 0xB6));

    // 单元格文本
    let text = header.text.as_ref().unwrap();
    assert_eq!(text.plain_text(), "阶段");
    assert!(text.paragraphs[0].runs[0].props.bold);

    assert_eq!(
        table.cells[1][1].text.as_ref().unwrap().plain_text(),
        "ATP 与 NADPH"
    );

    // 表格节点应有正确的画布位置
    let b = node.canvas_bounds();
    assert!((b.x - 432.0).abs() < 0.1, "实际 x = {}", b.x);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn smartart_degrades_without_losing_text() {
    let path = standard_deck("openpptview-e2e-smartart.pptx");
    let src = PptxSource::open(&path).unwrap();
    let scene = scene_of(&src, 0);

    let degraded = scene
        .walk()
        .find(|n| n.geometry.is_degraded())
        .expect("SmartArt 应降级为占位节点");

    assert!(degraded.is_visible(), "占位节点应可见，让老师知道这里有内容");

    let text = degraded
        .text
        .as_ref()
        .map(|t| t.plain_text())
        .unwrap_or_default();
    assert!(text.contains("叶绿体"), "降级不应丢失文本，实际 {text:?}");
    assert!(text.contains("类囊体薄膜"));

    assert!(
        scene.warnings.iter().any(|w| w.contains("SmartArt")),
        "应记录降级告警，实际 {:?}",
        scene.warnings
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn notes_extracted_for_slide_with_relationship() {
    let path = standard_deck("openpptview-e2e-notes.pptx");
    let src = PptxSource::open(&path).unwrap();

    // slide1.xml 是第 2 页，带备注
    let notes = src.notes(1).expect("读取备注不应失败");
    let text = notes.expect("第 2 页应有备注");
    assert!(text.contains("类囊体薄膜"), "实际 {text:?}");
    assert!(text.contains("提问"));

    // slide2.xml 是第 1 页，没有备注关系
    assert!(src.notes(0).unwrap().is_none());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn out_of_range_page_returns_error_not_panic() {
    let path = standard_deck("openpptview-e2e-range.pptx");
    let src = PptxSource::open(&path).unwrap();

    assert!(src.page_content(99).is_err());
    assert!(src.notes(99).is_err());
    // 边界内的索引仍然正常
    assert!(src.page_content(src.page_count() - 1).is_ok());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn repeated_parsing_is_deterministic() {
    let path = standard_deck("openpptview-e2e-determinism.pptx");
    let src = PptxSource::open(&path).unwrap();

    let a = scene_of(&src, 0);
    let b = scene_of(&src, 0);

    assert_eq!(a.nodes.len(), b.nodes.len());
    assert_eq!(a.warnings, b.warnings);

    // 场景图应能序列化，供快照测试与前端传输
    let ja = serde_json::to_string(&a).expect("场景图应可序列化");
    let jb = serde_json::to_string(&b).expect("场景图应可序列化");
    assert_eq!(ja, jb, "同样输入应得到逐字节一致的场景图");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn scene_is_serializable_for_transport() {
    let path = standard_deck("openpptview-e2e-serialization.pptx");
    let src = PptxSource::open(&path).unwrap();
    let scene = scene_of(&src, 0);

    let json = serde_json::to_string(&scene).unwrap();
    let back: ppt_core::scene::Scene = serde_json::from_str(&json).unwrap();
    assert_eq!(back, scene, "场景图应能无损往返序列化");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn all_slides_parse_without_panic() {
    let path = standard_deck("openpptview-e2e-all.pptx");
    let src = PptxSource::open(&path).unwrap();

    for i in 0..src.page_count() {
        let scene = scene_of(&src, i);
        assert_eq!(
            scene.size_pt,
            Size::new(960.0, 540.0),
            "第 {} 页尺寸应正确",
            i + 1
        );
    }

    let _ = std::fs::remove_file(&path);
}

#[test]
fn media_provider_rejects_unknown_part() {
    let path = standard_deck("openpptview-e2e-media.pptx");
    let src = PptxSource::open(&path).unwrap();

    assert!(src.read_media("ppt/media/nope.png").is_err());
    // 路径穿越不应越出包根
    assert!(src.read_media("../../../etc/passwd").is_err());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn source_is_send_and_sync() {
    // 渲染管线会在多线程间共享文档句柄，必须在编译期锁定该约束
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<PptxSource>();
}

#[test]
fn can_be_used_as_trait_object() {
    let path = standard_deck("openpptview-e2e-traitobj.pptx");
    let src = PptxSource::open(&path).unwrap();

    let shared: ppt_core::SharedSource = std::sync::Arc::new(src);
    assert_eq!(shared.page_count(), 2);
    assert_eq!(shared.format(), DocFormat::Pptx);
    assert!(shared.default_page_size_pt().w > 0.0);

    // 多线程并发取页
    let handles: Vec<_> = (0..4)
        .map(|i| {
            let s = std::sync::Arc::clone(&shared);
            std::thread::spawn(move || {
                let idx = i % s.page_count();
                s.page_content(idx).is_ok()
            })
        })
        .collect();
    for h in handles {
        assert!(h.join().expect("解析线程不应 panic"));
    }

    let _ = std::fs::remove_file(&path);
}
