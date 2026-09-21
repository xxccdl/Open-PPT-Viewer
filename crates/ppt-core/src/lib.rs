//! # ppt-core
//!
//! 极速课件讲演器的格式无关内核。提供三块能力：
//!
//! 1. **OPC 按需读取**（[`opc`]）—— 只解析 ZIP 中央目录，按条目惰性解压；
//! 2. **OOXML 轻量 XML 树**（[`xml`]）—— 剥离命名空间前缀，容错读取；
//! 3. **SceneGraph 中间表示**（[`scene`]）—— 渲染器唯一需要理解的数据结构；
//! 4. **文档抽象**（[`document`]）—— PPTX / PDF / 未来格式的统一入口。
//!
//! ## 坐标与单位
//!
//! 全链路统一使用**点（pt）**，详见 [`units`]。
//! 每个节点的 `transform` 是相对幻灯片画布的绝对仿射变换。
//!
//! ## 性能约定
//!
//! - 打开文档时**不得**解析任何页面；页面解析只发生在
//!   [`document::DocumentSource::page_content`] 被调用时。
//! - 媒体字节**不得**在解析阶段读入；只记录部件名，
//!   由渲染器通过 [`document::MediaProvider`] 按需拉取。

pub mod document;
pub mod error;
pub mod opc;
pub mod scene;
pub mod units;
pub mod xml;

pub use document::{
    detect_format, detect_format_by_extension, Bitmap, DocFormat, DocumentSource, MediaProvider,
    PageContent, PixelFormat, SharedSource,
};
pub use error::{Error, Result};
pub use opc::{
    load_relationships, rels_part_for, ContentTypes, Package, Relationship, Relationships,
};
pub use scene::{
    AutoFit, BodyProps, Color, Fill, Geometry, ImageRef, Insets, LinkHotspot, Node, Paragraph,
    PathGeometry, PathSegment, Point, Rect, RelativeRect, RunProps, Scene, SceneBackground,
    SceneWalker, Size, Stroke, StrokeFill, SubPath, Table, TableCell, TextAlign, TextBox, TextRun,
    Transform, VerticalAnchor,
};
pub use units::{emu_to_pt, pt_to_emu, scaled_px};
pub use xml::XmlNode;

/// 应用标识，用于缓存目录与日志前缀。
pub const APP_ID: &str = "openpptview";

/// 版本号（取自 Cargo 包版本）。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 初始化日志。
///
/// 采用极简的 `env_logger` 风格输出到 stderr；
/// 生产环境由 `ppt-app` 接管输出目标。
/// 这里只做一次性的默认级别设置，避免在库中硬编码全局 logger。
pub fn default_log_filter() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "warn"
    }
}

#[cfg(test)]
mod integration_smoke {
    //! 跨模块的冒烟测试：确认核心类型能组合使用。

    use crate::document::{Bitmap, PixelFormat};
    use crate::scene::*;
    use crate::xml;

    #[test]
    fn build_a_minimal_scene_end_to_end() {
        let mut scene = Scene::new(Size::new(960.0, 540.0));
        scene.background = SceneBackground::Solid(Color::WHITE);

        // 一个位于 (100, 100)、尺寸 300x120 的圆角矩形，内含居中文案
        let transform = Transform::from_ooxml(
            Point::new(100.0, 100.0),
            Size::new(300.0, 120.0),
            0.0,
            false,
            false,
        );
        let node = Node {
            id: "sp1".into(),
            transform,
            local_bbox: Some(Rect::new(0.0, 0.0, 300.0, 120.0)),
            geometry: Geometry::RoundRect {
                rx_pt: 8.0,
                ry_pt: 8.0,
            },
            fill: Fill::Solid(Color::rgb(0x1F, 0x6F, 0xEB)),
            text: Some(TextBox {
                body: BodyProps {
                    anchor: VerticalAnchor::Middle,
                    ..Default::default()
                },
                paragraphs: vec![Paragraph {
                    align: TextAlign::Center,
                    runs: vec![TextRun {
                        text: "光合作用".into(),
                        props: RunProps {
                            size_pt: 28.0,
                            color: Some(Color::WHITE),
                            ..Default::default()
                        },
                        hyperlink: None,
                        field: None,
                    }],
                    ..Default::default()
                }],
            }),
            ..Default::default()
        };
        scene.nodes.push(node);

        assert_eq!(scene.size_pt, Size::new(960.0, 540.0));
        assert_eq!(scene.walk().count(), 1);
        assert_eq!(
            scene.walk().next().unwrap().canvas_bounds(),
            Rect::new(100.0, 100.0, 300.0, 120.0)
        );
        assert_eq!(
            scene.walk().next().unwrap().text.as_ref().unwrap().plain_text(),
            "光合作用"
        );

        // 场景图应当可被序列化，以支撑快照测试
        let json = serde_json::to_string(&scene).expect("SceneGraph 应可序列化");
        let back: Scene = serde_json::from_str(&json).expect("SceneGraph 应可反序列化");
        assert_eq!(back, scene);
    }

    #[test]
    fn scene_is_send_and_sync() {
        // 调度管线会在多个渲染线程间共享 Scene，编译期即锁定该约束
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Scene>();
        assert_send_sync::<Node>();
        assert_send_sync::<Bitmap>();
    }

    #[test]
    fn bitmap_default_format_is_premultiplied() {
        assert_eq!(Bitmap::new_transparent(1, 1).format, PixelFormat::Rgba8Premultiplied);
    }

    #[test]
    fn xml_tree_feeds_scene_construction() {
        let slide_xml = r#"<?xml version="1.0"?>
<p:sld xmlns:p="urn:p" xmlns:a="urn:a">
  <p:cSld>
    <p:spTree>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="2" name="标题 1"/></p:nvSpPr>
        <p:spPr>
          <a:xfrm><a:off x="457200" y="274638"/><a:ext cx="8229600" cy="1143000"/></a:xfrm>
          <a:prstGeom prst="rect"><a:avLst/></a:prstGeom>
        </p:spPr>
        <p:txBody>
          <a:bodyPr/>
          <a:p><a:r><a:rPr lang="zh-CN" sz="2400"/><a:t>第一课</a:t></a:r></a:p>
        </p:txBody>
      </p:sp>
    </p:spTree>
  </p:cSld>
</p:sld>"#;

        let root = xml::parse_root("ppt/slides/slide1.xml", slide_xml).unwrap();
        let sp = root.path(&["cSld", "spTree", "sp"]).expect("应能找到形状");
        let xfrm = sp.path(&["spPr", "xfrm"]).unwrap();
        let off = xfrm.child("off").unwrap();
        assert_eq!(off.attr_f64("x"), Some(457200.0));
        // 457200 EMU = 36pt
        assert!((crate::units::emu_to_pt(457200.0) - 36.0).abs() < 0.001);

        let rpr = sp.path(&["txBody", "p", "r", "rPr"]).unwrap();
        assert_eq!(rpr.attr_f64("sz"), Some(2400.0));
        assert!((crate::units::centipoint_to_pt(2400.0) - 24.0).abs() < 0.001);

        let t = sp.path(&["txBody", "p", "r", "t"]).unwrap();
        assert_eq!(t.text(), "第一课");

        let cnv = sp.path(&["nvSpPr", "cNvPr"]).unwrap();
        assert_eq!(cnv.attr("name"), Some("标题 1"));
    }
}
