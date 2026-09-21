//! 形状解析：`p:spTree` → [`Node`] 列表。
//!
//! # 变换烘焙
//!
//! SceneGraph 约定：每个节点的 `transform` 是**相对幻灯片画布的绝对变换**。
//! 而 OOXML 的组合形状（`p:grpSp`）里，子形状的坐标是相对组合局部空间的。
//! 因此解析组合时必须把 `父变换 ∘ 组合的坐标映射 ∘ 子变换` 乘成一个矩阵
//! 写进子节点 —— 这样渲染器无需在绘制时递归求积，也便于按节点独立做脏矩形。
//!
//! # 降级策略
//!
//! 任何单个形状解析失败都只跳过该形状并记录告警，
//! 不影响同页其它形状；未支持的图形（SmartArt、图表、OLE）渲染为占位矩形
//! 并尽力提取其中的文本。**整条路径不允许 panic**。

use ppt_core::scene::{
    BodyProps, Build, Color, Fill, Geometry, Hyperlink, ImageRef, Insets, MediaKind, MediaRef,
    MediaTrim, Node, Point, Rect, RunProps, Scene, Size, Stroke, Transform, VerticalAnchor,
};
use ppt_core::{units, XmlNode};

use crate::color::ColorScheme;
use crate::inherit::{
    parse_transform, PhKey, PlaceholderType, SlideInheritance, StyleRole,
};
use crate::paint::{self, RelResolver};
use crate::preset::{self, AdjustValues};
use crate::table;
use crate::text::{self, LinkContext, TextStyles};
use crate::theme::Theme;

/// 图形框架（`p:graphicFrame`）里的 URI 片段，用于识别内嵌对象类型。
const URI_TABLE: &str = "table";
const URI_CHART: &str = "chart";
const URI_DIAGRAM: &str = "diagram";
const URI_OLE: &str = "ole";
const URI_MEDIA: &str = "media";

/// 形状树解析器。
pub struct NodeParser<'a> {
    pub inherit: &'a SlideInheritance,
    pub resolve: RelResolver<'a>,
    /// 跨部件读取回调（图表正文不在幻灯片里）。测试里为 `None`。
    pub read_part: Option<PartReader<'a>>,
    pub warnings: Vec<String>,
    /// 已解析的 `a:grpFill` 上下文（组合形状向下传递填充）。
    group_fill_stack: Vec<Fill>,
    id_counter: usize,
}

impl<'a> NodeParser<'a> {
    pub fn new(inherit: &'a SlideInheritance, resolve: RelResolver<'a>) -> NodeParser<'a> {
        NodeParser {
            inherit,
            resolve,
            read_part: None,
            warnings: Vec::new(),
            group_fill_stack: Vec::new(),
            id_counter: 0,
        }
    }

    fn next_id(&mut self, hint: &str) -> String {
        self.id_counter += 1;
        if hint.is_empty() {
            format!("n{}", self.id_counter)
        } else {
            format!("{hint}#{}", self.id_counter)
        }
    }

    fn warn(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        if !self.warnings.contains(&msg) {
            self.warnings.push(msg);
        }
    }

    #[inline]
    fn scheme(&self) -> &ColorScheme {
        &self.inherit.color_scheme
    }

    /// 供表格模块使用的配色方案访问器。
    #[inline]
    pub fn scheme_for_table(&self) -> &ColorScheme {
        &self.inherit.color_scheme
    }

    /// 供表格模块使用的关系解析器访问器。
    #[inline]
    pub fn resolver(&self) -> RelResolver<'a> {
        self.resolve
    }

    /// 解析表格单元格里的文本。
    ///
    /// 单元格没有占位符概念，因此用 `otherStyle` 作为默认样式来源。
    pub fn parse_cell_text(&self, tx_body: &XmlNode) -> ppt_core::scene::TextBox {
        self.parse_text_body(tx_body, &None, None)
    }

    #[inline]
    fn theme(&self) -> &Theme {
        &self.inherit.theme
    }

    fn link_ctx(&self) -> LinkContext<'_> {
        LinkContext {
            resolve_rel: self.resolve,
        }
    }

    /// 解析 `p:spTree`（或 `p:grpSpPr` 所在的组合子树）。
    pub fn parse_tree(&mut self, tree: &XmlNode) -> Vec<Node> {
        self.parse_children(tree, Transform::IDENTITY)
    }

    /// 解析一组形状子元素，`parent` 是父级累积的绝对变换。
    fn parse_children(&mut self, container: &XmlNode, parent: Transform) -> Vec<Node> {
        let mut out = Vec::new();
        for child in &container.children {
            let node = match child.name.as_str() {
                "sp" => self.parse_shape(child, parent),
                "pic" => self.parse_picture(child, parent),
                "grpSp" => self.parse_group(child, parent),
                "cxnSp" => self.parse_connector(child, parent),
                "graphicFrame" => self.parse_graphic_frame(child, parent),
                "contentPart" => {
                    // `p:contentPart` 是外部内容的引用，本版本不解析其内容
                    self.warn("文档引用了外部内容部件（contentPart），已跳过");
                    None
                }
                _ => None,
            };
            if let Some(n) = node {
                out.push(n);
            }
        }
        out
    }

    /// 解析 `p:sp`。
    fn parse_shape(&mut self, sp: &XmlNode, parent: Transform) -> Option<Node> {
        let nv = sp.child("nvSpPr")?;
        let c_nv_pr = nv.child("cNvPr")?;
        let name = c_nv_pr.attr("name").unwrap_or("").to_string();
        let id = self.next_id(&name);

        let ph = nv.path(&["nvPr", "ph"]);
        let ph_key = ph.map(PhKey::parse);
        // `p:ph` 省略 type 时按规范等价于 `type="obj"`（正文类）
        let ph_type = ph.map(PlaceholderType::from_ph_element);
        let ph_info = ph_key.and_then(|k| self.inherit.placeholder(k)).cloned();

        // 位置：自身 a:xfrm 优先，缺失时从版式/母版占位符继承
        let sp_pr = sp.child("spPr");
        let (own_transform, own_extent) = parse_transform(sp_pr);
        let (transform, extent) = match own_transform {
            Some(t) => (t, own_extent.unwrap_or(Size::ZERO)),
            None => match &ph_info {
                Some(p) => (
                    p.transform.unwrap_or(parent),
                    p.extent.unwrap_or(Size::ZERO),
                ),
                None => {
                    self.warn(format!("形状「{name}」没有位置信息且无占位符可继承，已跳过"));
                    return None;
                }
            },
        };

        let abs_transform = parent.multiply(&transform);

        // 文字不跟着形状一起镜像（见 `Transform::text_unmirrored`）。
        //
        // 判据用**最终矩阵**而不是「形状自己有没有 flip」：翻转可能来自形状自身
        // （`a:xfrm/@flipH`），也可能来自外层的组合（`p:grpSp` 的 `a:xfrm/@flipH`）。
        // 只看局部标志时，第二种漏了 —— 组合里的整组卡片文字全反写。
        let text_transform = abs_transform.text_unmirrored(extent);

        // 几何
        let geometry = if let Some(sp_pr) = sp_pr {
            self.parse_geometry(sp_pr, extent)
        } else {
            Geometry::None
        };

        // 填充 / 描边 / 效果（含 p:style 主题引用）
        let style_node = sp.child("style");
        let fill = self.parse_fill_with_ref(sp_pr, style_node, &geometry);
        let stroke = self.parse_stroke_with_ref(sp_pr, style_node);
        let effects = self.parse_effects_with_ref(sp_pr, style_node);

        // 文本
        let text_box = sp.child("txBody").map(|tx| {
            self.parse_text_body(tx, &ph_info, ph_type)
        });

        let hidden = c_nv_pr.attr_bool_or("hidden", false)
            || ph_info.as_ref().map(|p| p.hidden).unwrap_or(false);

        let hyperlink = self.parse_shape_hyperlink(c_nv_pr);

        Some(Node {
            id,
            name: Some(name),
            transform: abs_transform,
            opacity: 1.0,
            geometry,
            fill,
            stroke,
            effects,
            text: text_box,
            children: Vec::new(),
            hyperlink,
            media: None,
            hidden,
            shape_id: c_nv_pr.attr_u32("id"),
            build: Build::ALWAYS,
            emphasis: Vec::new(),
            local_bbox: Some(Rect::new(0.0, 0.0, extent.w, extent.h)),
            text_transform,
        })
    }

    /// 解析 `p:pic` 图片。
    fn parse_picture(&mut self, pic: &XmlNode, parent: Transform) -> Option<Node> {
        let nv = pic.child("nvPicPr")?;
        let c_nv_pr = nv.child("cNvPr")?;
        let name = c_nv_pr.attr("name").unwrap_or("").to_string();
        let id = self.next_id(&name);

        let ph = nv.path(&["nvPr", "ph"]);
        let ph_key = ph.map(PhKey::parse);
        let ph_info = ph_key.and_then(|k| self.inherit.placeholder(k)).cloned();

        let sp_pr = pic.child("spPr");
        let (own_transform, own_extent) = parse_transform(sp_pr);
        let (transform, extent) = match own_transform {
            Some(t) => (t, own_extent.unwrap_or(Size::ZERO)),
            None => match &ph_info {
                Some(p) => (
                    p.transform.unwrap_or(parent),
                    p.extent.unwrap_or(Size::ZERO),
                ),
                None => {
                    self.warn(format!("图片「{name}」没有位置信息，已跳过"));
                    return None;
                }
            },
        };
        let abs_transform = parent.multiply(&transform);

        // 图片填充：`p:blipFill`
        let blip_fill = pic.child("blipFill")?;
        let image = paint::parse_blip_fill_image(blip_fill, self.resolve)?;
        if image.part.is_empty() {
            self.warn(format!("图片「{name}」的资源关系无法解析，已跳过"));
            return None;
        }

        let geometry = Geometry::Image(ImageRef {
            native_size: Some(extent),
            ..image
        });

        let stroke = self.parse_stroke_with_ref(sp_pr, pic.child("style"));
        let effects = self.parse_effects_with_ref(sp_pr, pic.child("style"));
        let hyperlink = self.parse_shape_hyperlink(c_nv_pr);
        // 视频/音频挂在这个「图片形状」上：`blipFill` 是**封页图**，
        // `nvPr` 里才是真正的媒体部件。静态画面照旧由图片链路绘制，
        // 这里只是额外记下「点一下能播」。
        let media = self.parse_media(nv.child("nvPr"));

        Some(Node {
            id,
            name: Some(name),
            transform: abs_transform,
            opacity: 1.0,
            geometry,
            fill: Fill::None,
            stroke,
            effects,
            text: None,
            children: Vec::new(),
            hyperlink,
            media,
            hidden: c_nv_pr.attr_bool_or("hidden", false),
            shape_id: c_nv_pr.attr_u32("id"),
            build: Build::ALWAYS,
            emphasis: Vec::new(),
            local_bbox: Some(Rect::new(0.0, 0.0, extent.w, extent.h)),
            text_transform: None,
        })
    }

    /// 解析 `p:pic` 上挂的视频/音频。
    ///
    /// # 为什么有多个出处
    ///
    /// PowerPoint 各版本的写法不一致，同一个媒体可能出现在三处：
    /// - `a:videoFile` / `a:audioFile`：老写法，`r:embed`（包内）或 `r:link`（外部文件）
    /// - `p14:media`：新写法，`r:embed` 指向包内部件，另带 `loop` 属性
    ///
    /// **只接受包内嵌（`r:embed`）**：`r:link` 指向课件旁边的外部文件，
    /// 课件被单独拷走后必然失效，做成「能点的播放按钮」反而误导老师。
    /// 这种情况记一条告警，便于排查时知道原因。
    fn parse_media(&mut self, nv_pr: Option<&XmlNode>) -> Option<MediaRef> {
        let nv_pr = nv_pr?;

        // 三处出处都按**后代**找。
        //
        // `p14:media` 藏在 `p:extLst/p:ext` 里两层，这一点是实测出来的：
        //
        // ```xml
        // <p:nvPr>
        //   <a:videoFile r:link="rId1"/>          ← 只有 link，Target 甚至是 "NULL"
        //   <p:extLst><p:ext uri="{DAA4B4D4-…}">
        //     <p14:media r:embed="rId2"/>          ← 真正的嵌入关系在这儿
        //       <p14:trim st="1633" end="6864"/>   ← 「裁剪视频」的区间
        //     </p14:media>
        //   </p:ext></p:extLst>
        // </p:nvPr>
        // ```
        //
        // 以前用 `nv_pr.child("media")` 只看直接子节点，永远找不到它，
        // 于是退到只有 `r:link` 的 `a:videoFile`，判定为「链接到文件」整段跳过 ——
        // 课件里**所有视频和音频都点不动**，正是「视频无法播放」。
        //
        // `videoFile`/`audioFile` 其实就在 `p:nvPr` 直接子节点上，但一起按后代找
        // 不花什么代价，也免得将来哪版 PowerPoint 多包一层就再次失联。
        let media_el = nv_pr.find_descendant("media");
        let video_el = nv_pr.find_descendant("videoFile");
        let audio_el = nv_pr.find_descendant("audioFile");

        let kind = if video_el.is_some() {
            MediaKind::Video
        } else if audio_el.is_some() {
            MediaKind::Audio
        } else if media_el.is_some() {
            // `p14:media` 单独出现时按视频处理：音频没有画面，
            // 配一个 `p:pic` 的场景极少见
            MediaKind::Video
        } else {
            return None;
        };

        // `p14:media` 优先（新写法最可靠，`r:embed` 才是真的包内部件），
        // 再退到 `videoFile`/`audioFile` 的 `r:embed`
        let part = media_el
            .and_then(|el| el.attr("embed").and_then(|id| (self.resolve)(id)))
            .or_else(|| {
                [video_el, audio_el]
                    .into_iter()
                    .flatten()
                    .find_map(|el| el.attr("embed").and_then(|id| (self.resolve)(id)))
            });

        let Some(part) = part else {
            let linked = media_el.is_some_and(|m| m.attr("link").is_some())
                || [video_el, audio_el]
                    .into_iter()
                    .flatten()
                    .any(|el| el.attr("link").is_some());
            if linked {
                self.warn("该视频/音频是「链接到文件」而非嵌入，课件拷走后会失效，已跳过");
            } else {
                self.warn("视频/音频的媒体关系无法解析，已跳过");
            }
            return None;
        };

        // PowerPoint「裁剪视频」留下的区间（毫秒）。
        //
        // 不上心会出大错：课件里有一段 121 MB 的视频被裁到 1.633s~6.864s，
        // 也就是老师只想放那 5 秒 —— 从头发到尾放完整段，讲的内容完全不是他要的。
        let trim = media_el.and_then(|m| m.child("trim")).and_then(|t| {
            let start_ms = t.attr_u32("st").unwrap_or(0);
            let end_ms = t.attr_u32("end").unwrap_or(0);
            // `end="0"` 与缺失同义：表示「一直到结尾」
            if start_ms == 0 && end_ms == 0 {
                None
            } else {
                Some(MediaTrim {
                    start_ms,
                    end_ms: (end_ms > 0).then_some(end_ms),
                })
            }
        });

        let loop_play = media_el
            .and_then(|m| m.attr("loop"))
            .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));

        Some(MediaRef {
            part,
            kind,
            loop_play,
            trim,
        })
    }

    /// 解析 `p:grpSp` 组合形状。
    ///
    /// 关键：把组合的坐标映射与父变换**烘焙**进子节点的绝对变换。
    fn parse_group(&mut self, grp: &XmlNode, parent: Transform) -> Option<Node> {
        let nv = grp.child("nvGrpSpPr")?;
        let c_nv_pr = nv.child("cNvPr")?;
        let name = c_nv_pr.attr("name").unwrap_or("").to_string();
        let id = self.next_id(&name);

        let grp_sp_pr = grp.child("grpSpPr");
        let xfrm = grp_sp_pr.and_then(|p| p.child("xfrm"));

        let (offset, extent, child_off, child_ext, rot, flip_h, flip_v) = match xfrm {
            Some(x) => {
                let off = x.child("off");
                let ext = x.child("ext");
                let ch_off = x.child("chOff");
                let ch_ext = x.child("chExt");
                let get = |n: Option<&XmlNode>, k: &str| -> f32 {
                    n.and_then(|v| v.attr_f64(k)).map(units::emu_to_pt).unwrap_or(0.0)
                };
                (
                    Point::new(get(off, "x"), get(off, "y")),
                    Size::new(get(ext, "cx"), get(ext, "cy")),
                    Point::new(get(ch_off, "x"), get(ch_off, "y")),
                    Size::new(get(ch_ext, "cx"), get(ch_ext, "cy")),
                    units::ooxml_angle_to_deg(x.attr_f64("rot").unwrap_or(0.0)),
                    x.attr_bool_or("flipH", false),
                    x.attr_bool_or("flipV", false),
                )
            }
            None => (
                Point::ZERO,
                Size::ZERO,
                Point::ZERO,
                Size::ZERO,
                0.0,
                false,
                false,
            ),
        };

        // 组合自身在父坐标系中的变换
        let group_transform = Transform::from_ooxml(offset, extent, rot, flip_h, flip_v);

        // 子坐标系 → 组合局部坐标系
        let effective_child_ext = if child_ext.is_empty() {
            extent
        } else {
            child_ext
        };
        let child_map = if extent.is_empty() {
            Transform::IDENTITY
        } else {
            Transform::child_space_to_local(child_off, effective_child_ext, extent)
        };

        // 子节点累积到的绝对变换
        let child_parent = parent
            .multiply(&group_transform)
            .multiply(&child_map);

        // 组合自身的填充会向下传递给 `a:grpFill` 的子形状
        let group_fill = self
            .parse_fill_with_ref(grp_sp_pr, None, &Geometry::Rect);
        self.group_fill_stack.push(group_fill.clone());

        let children = self.parse_children(grp, child_parent);

        self.group_fill_stack.pop();

        if children.is_empty() {
            // 空的组合形状没有渲染意义
            return None;
        }

        Some(Node {
            id,
            name: Some(name),
            transform: parent.multiply(&group_transform),
            opacity: 1.0,
            geometry: Geometry::None,
            fill: Fill::None,
            stroke: None,
            effects: Default::default(),
            text: None,
            children,
            hyperlink: self.parse_shape_hyperlink(c_nv_pr),
            media: None,
            hidden: c_nv_pr.attr_bool_or("hidden", false),
            shape_id: c_nv_pr.attr_u32("id"),
            build: Build::ALWAYS,
            emphasis: Vec::new(),
            local_bbox: Some(Rect::new(0.0, 0.0, extent.w, extent.h)),
            text_transform: None,
        })
    }

    /// 解析 `p:cxnSp` 连接线。
    fn parse_connector(&mut self, cxn: &XmlNode, parent: Transform) -> Option<Node> {
        let nv = cxn.child("nvCxnSpPr")?;
        let c_nv_pr = nv.child("cNvPr")?;
        let name = c_nv_pr.attr("name").unwrap_or("").to_string();
        let id = self.next_id(&name);

        let sp_pr = cxn.child("spPr");
        let (transform, extent) = match parse_transform(sp_pr) {
            (Some(t), Some(e)) => (t, e),
            _ => {
                self.warn(format!("连接线「{name}」缺少位置信息，已跳过"));
                return None;
            }
        };

        let geometry = self.parse_geometry(sp_pr.unwrap_or(&EMPTY), extent);

        // 连接线以描边为主要视觉元素，填充通常无意义
        let fill = Fill::None;
        let stroke = self
            .parse_stroke_with_ref(sp_pr, cxn.child("style"))
            .or_else(|| Some(paint::default_stroke(Some(self.scheme()))));
        let effects = self.parse_effects_with_ref(sp_pr, cxn.child("style"));

        Some(Node {
            id,
            name: Some(name),
            transform: parent.multiply(&transform),
            opacity: 1.0,
            geometry,
            fill,
            stroke,
            effects,
            text: None,
            children: Vec::new(),
            hyperlink: self.parse_shape_hyperlink(c_nv_pr),
            media: None,
            hidden: c_nv_pr.attr_bool_or("hidden", false),
            shape_id: c_nv_pr.attr_u32("id"),
            build: Build::ALWAYS,
            emphasis: Vec::new(),
            local_bbox: Some(Rect::new(0.0, 0.0, extent.w, extent.h)),
            text_transform: None,
        })
    }

    /// 解析 `p:graphicFrame`：表格、图表、SmartArt、OLE、媒体。
    fn parse_graphic_frame(&mut self, frame: &XmlNode, parent: Transform) -> Option<Node> {
        let nv = frame.child("nvGraphicFramePr")?;
        let c_nv_pr = nv.child("cNvPr")?;
        let name = c_nv_pr.attr("name").unwrap_or("").to_string();
        let id = self.next_id(&name);

        let (transform, extent) = match crate::inherit::parse_xfrm(frame.child("xfrm")) {
            (Some(t), Some(e)) => (t, e),
            _ => {
                self.warn(format!("图形框架「{name}」缺少位置信息，已跳过"));
                return None;
            }
        };
        let abs_transform = parent.multiply(&transform);

        let graphic_data = frame.path(&["graphic", "graphicData"]);
        let uri = graphic_data
            .and_then(|d| d.attr("uri"))
            .unwrap_or("")
            .to_ascii_lowercase();

        if uri.contains(URI_TABLE) {
            let tbl = graphic_data.and_then(|d| d.child("tbl"))?;
            let table = table::parse_table(tbl, self);
            if table.is_empty() {
                self.warn(format!("表格「{name}」为空，已跳过"));
                return None;
            }
            return Some(Node {
                id,
                name: Some(name),
                transform: abs_transform,
                opacity: 1.0,
                geometry: Geometry::Table(table),
                fill: Fill::None,
                stroke: None,
                effects: Default::default(),
                text: None,
                children: Vec::new(),
                hyperlink: self.parse_shape_hyperlink(c_nv_pr),
                media: None,
                hidden: c_nv_pr.attr_bool_or("hidden", false),
                shape_id: c_nv_pr.attr_u32("id"),
                build: Build::ALWAYS,
                emphasis: Vec::new(),
                local_bbox: Some(Rect::new(0.0, 0.0, extent.w, extent.h)),
                text_transform: None,
            });
        }

        // 图表：正文在 ppt/charts/chartN.xml 里，跨部件读进来展开成图元。
        // 展开失败（类型不支持/部件损坏）时**继续走下面的占位路径**，
        // 而不是让这一页少一块内容。
        if uri.contains(URI_CHART) {
            if let Some(chart_nodes) = self.parse_chart(graphic_data, abs_transform, &id, &name, extent)
            {
                let bbox = Rect::new(0.0, 0.0, extent.w, extent.h);
                return Some(Node {
                    id,
                    name: Some(name),
                    transform: abs_transform,
                    opacity: 1.0,
                    // 纯容器：几何为 None，子节点带着**绝对变换**各自绘制
                    geometry: Geometry::None,
                    fill: Fill::None,
                    stroke: None,
                    effects: Default::default(),
                    text: None,
                    children: chart_nodes,
                    hyperlink: self.parse_shape_hyperlink(c_nv_pr),
                    media: None,
                    hidden: c_nv_pr.attr_bool_or("hidden", false),
                    shape_id: c_nv_pr.attr_u32("id"),
                    build: Build::ALWAYS,
                    emphasis: Vec::new(),
                    local_bbox: Some(bbox),
                    text_transform: None,
                });
            }
        }

        // 以下类型首版降级为占位矩形 + 尽力提取文本
        let (kind, reason) = if uri.contains(URI_CHART) {
            ("图表", "图表暂以占位呈现")
        } else if uri.contains(URI_DIAGRAM) {
            ("SmartArt", "SmartArt 暂以占位呈现")
        } else if uri.contains(URI_OLE) {
            ("嵌入对象", "嵌入对象暂以占位呈现")
        } else if uri.contains(URI_MEDIA) {
            ("媒体", "音视频暂以占位呈现")
        } else {
            ("未知对象", "未识别的图形对象，已降级为占位")
        };

        self.warn(format!("{kind}「{name}」：{reason}"));

        // 尽力从 graphicData 里抽取可读文本，至少让内容不丢失
        let extracted = graphic_data
            .map(|d| extract_texts(d))
            .unwrap_or_default();

        let text = if extracted.is_empty() {
            None
        } else {
            Some(crate::text::make_plain_text_box(&extracted))
        };

        Some(Node {
            id,
            name: Some(name),
            transform: abs_transform,
            opacity: 1.0,
            geometry: Geometry::placeholder(reason),
            // 占位矩形用浅灰底，让老师一眼看出这里有内容但未渲染
            fill: Fill::Solid(Color::rgba(0xE8, 0xE8, 0xE8, 0xFF)),
            stroke: Some(Stroke {
                fill: ppt_core::scene::StrokeFill::Solid(Color::rgb(0xBB, 0xBB, 0xBB)),
                width_pt: 0.75,
                dash: ppt_core::scene::DashStyle::Dash,
                ..Stroke::default()
            }),
            effects: Default::default(),
            text,
            children: Vec::new(),
            hyperlink: self.parse_shape_hyperlink(c_nv_pr),
            media: None,
            hidden: c_nv_pr.attr_bool_or("hidden", false),
            shape_id: c_nv_pr.attr_u32("id"),
            build: Build::ALWAYS,
            emphasis: Vec::new(),
            local_bbox: Some(Rect::new(0.0, 0.0, extent.w, extent.h)),
            text_transform: None,
        })
    }

    /// 解析 `p:graphicFrame` 里的图表。
    ///
    /// 图表的正文不在幻灯片里，`c:chart` 只留一个 `r:id` 指向
    /// `ppt/charts/chartN.xml`，所以必须跨部件读取。
    /// 成功时返回的子节点**已经带上框架的绝对变换**。
    fn parse_chart(
        &mut self,
        graphic_data: Option<&XmlNode>,
        abs_transform: Transform,
        id: &str,
        name: &str,
        extent: Size,
    ) -> Option<Vec<Node>> {
        let chart_ref = graphic_data?.child("chart")?;
        let Some(read_part) = self.read_part else {
            self.warn(format!(
                "图表「{name}」：当前解析路径无法读取图表部件，已降级为占位"
            ));
            return None;
        };

        let part = chart_ref.attr("id").and_then(|rid| (self.resolve)(rid));
        let text = part.as_deref().and_then(read_part);
        let (Some(part), Some(text)) = (part, text) else {
            self.warn(format!(
                "图表「{name}」：找不到图表部件（chartN.xml），已降级为占位"
            ));
            return None;
        };

        // 必须用 `parse_root`：`xml::parse` 返回的是承载根元素的虚拟节点，
        // 在它上面找 `c:chart`（其实是 `c:chartSpace` 的子节点）永远找不到
        let root = match ppt_core::xml::parse_root(&part, &text) {
            Ok(r) => r,
            Err(e) => {
                self.warn(format!("图表「{name}」：{e}，已降级为占位"));
                return None;
            }
        };

        let frame = Rect::new(0.0, 0.0, extent.w, extent.h);
        // 先克隆配色，避免后面 `self.warn` 与这里的不可变借用打架
        let scheme = self.inherit.color_scheme.clone();

        match crate::chart::parse(&root, frame, &scheme, self.resolve) {
            Ok(mut nodes) => {
                for (i, node) in nodes.iter_mut().enumerate() {
                    node.id = format!("{id}/{i}");
                    // 图表模块在框架局部坐标里布局（每个图元的 transform 只带平移），
                    // 这里把框架的绝对变换乘在**外侧** —— 组合子节点必须是绝对的
                    node.transform = abs_transform.multiply(&node.transform);
                }
                Some(nodes)
            }
            Err(reason) => {
                self.warn(format!("图表「{name}」：{reason}，已降级为占位"));
                None
            }
        }
    }

    /// 解析几何（`a:prstGeom` / `a:custGeom`）。
    fn parse_geometry(&mut self, sp_pr: &XmlNode, extent: Size) -> Geometry {
        if let Some(cust) = sp_pr.child("custGeom") {
            let g = preset::expand_custom(cust, extent);
            if g.is_degraded() {
                self.warn("自定义几何解析失败，已降级为占位");
            }
            return g;
        }

        if let Some(prst) = sp_pr.child("prstGeom") {
            let name = prst.attr("prst").unwrap_or("rect");
            let adj = AdjustValues::parse(prst.child("avLst"));

            // 少数常见预设直接用专用几何类型，渲染器可走更快的路径
            match name {
                "rect" => {
                    return Geometry::Rect;
                }
                "ellipse" => {
                    return Geometry::Ellipse;
                }
                "roundRect" => {
                    let r = adj.ratio("adj", 0.16667).clamp(0.0, 0.5);
                    return Geometry::RoundRect {
                        rx_pt: r * extent.w.min(extent.h),
                        ry_pt: r * extent.w.min(extent.h),
                    };
                }
                _ => {}
            }

            let g = preset::expand(name, &adj, extent);
            if g.is_degraded() {
                self.warn(format!("未支持的预设几何「{name}」，已降级为占位"));
            }
            return g;
        }

        // 没有任何几何元素：按规范视为矩形（多数工具生成的形状都带 prstGeom，
        // 缺失时给矩形比给空几何更接近原意）
        Geometry::Rect
    }

    /// 解析填充，含 `p:style/a:fillRef` 主题引用与 `a:grpFill` 继承。
    fn parse_fill_with_ref(
        &mut self,
        sp_pr: Option<&XmlNode>,
        style: Option<&XmlNode>,
        geometry: &Geometry,
    ) -> Fill {
        let own = sp_pr
            .map(|p| paint::parse_fill(p, Some(self.scheme()), self.resolve))
            .unwrap_or(Fill::None);

        let resolved = match own {
            // `a:grpFill` 沿用组合形状的填充
            Fill::Inherit => self
                .group_fill_stack
                .last()
                .cloned()
                .unwrap_or(Fill::None),
            other => other,
        };

        // 只有 `p:spPr` 里**根本没写填充**时才回退到主题样式引用。
        //
        // `a:noFill` 是显式的「不要填充」，不是「没写」——
        // 以前两者都是 `Fill::None`，于是显式的 noFill 也被回退成
        // `a:fillRef idx="1"`（accent1 蓝），透明的红框变蓝实心块。见 `has_fill_spec`。
        let wrote_fill = sp_pr.map(paint::has_fill_spec).unwrap_or(false);
        let chosen = if matches!(resolved, Fill::None) && !wrote_fill {
            self.theme_fill_ref(style).unwrap_or(Fill::None)
        } else {
            resolved
        };

        self.attach_image_native_size(chosen, geometry)
    }

    /// 给图片填充补上原生尺寸信息（用于解码降采样决策）。
    fn attach_image_native_size(&self, fill: Fill, geometry: &Geometry) -> Fill {
        match (fill, geometry) {
            (Fill::Image(mut img), Geometry::Rect) => {
                // 填充方式下图片铺满形状，原生尺寸未知时留空由渲染层按解码后尺寸处理
                img.image.native_size = None;
                Fill::Image(img)
            }
            (other, _) => other,
        }
    }

    /// 解析 `p:style/a:fillRef`。
    fn theme_fill_ref(&self, style: Option<&XmlNode>) -> Option<Fill> {
        let style = style?;
        let fill_ref = style.child("fillRef")?;
        let idx = fill_ref.attr_u32("idx").unwrap_or(0);
        if idx == 0 {
            return None;
        }

        let style_node = self.theme().fill_style(idx)?;

        // 用引用处的颜色替换主题样式里的 phClr 占位
        let substituted = match fill_ref.children.first() {
            Some(c) => paint::substitute_placeholder_color(style_node, c),
            None => style_node.clone(),
        };

        // 把替换后的填充节点包一层，复用 parse_fill 的解析逻辑
        let wrapper = XmlNode {
            name: "spPr".to_string(),
            attrs: Vec::new(),
            children: vec![substituted],
            text: String::new(),
        };
        let fill = paint::parse_fill(&wrapper, Some(self.scheme()), self.resolve);
        if matches!(fill, Fill::None) {
            None
        } else {
            Some(fill)
        }
    }

    /// 解析描边，含 `p:style/a:lnRef`。
    fn parse_stroke_with_ref(
        &mut self,
        sp_pr: Option<&XmlNode>,
        style: Option<&XmlNode>,
    ) -> Option<Stroke> {
        let own = paint::parse_stroke(
            sp_pr.and_then(|p| p.child("ln")),
            Some(self.scheme()),
            self.resolve,
        );
        if own.is_some() {
            return own;
        }

        // 主题线条样式引用
        let style = style?;
        let ln_ref = style.child("lnRef")?;
        let idx = ln_ref.attr_u32("idx").unwrap_or(0);
        if idx == 0 {
            return None;
        }
        let style_node = self.theme().line_style(idx)?;
        let substituted = match ln_ref.children.first() {
            Some(c) => paint::substitute_placeholder_color(style_node, c),
            None => style_node.clone(),
        };
        paint::parse_stroke(Some(&substituted), Some(self.scheme()), self.resolve)
    }

    /// 解析效果，含 `p:style/a:effectRef`。
    fn parse_effects_with_ref(
        &mut self,
        sp_pr: Option<&XmlNode>,
        style: Option<&XmlNode>,
    ) -> ppt_core::scene::Effects {
        let own = paint::parse_effects(
            sp_pr.and_then(|p| p.child("effectLst")),
            Some(self.scheme()),
        );
        // 写了 `a:effectLst` 就以它为准，**哪怕是空的** ——
        // 空的 effectLst 是显式的「不要效果」，与「没写」不是一回事
        // （跟 `a:noFill` 和「没写填充」的关系一样）。
        // 不回退才不会给已经清掉效果的形状硬加一层主题阴影。
        if sp_pr.map(|p| p.has_child("effectLst")).unwrap_or(false) {
            return own;
        }
        if !own.is_empty() {
            return own;
        }

        let Some(style) = style else {
            return own;
        };
        let Some(effect_ref) = style.child("effectRef") else {
            return own;
        };
        let idx = effect_ref.attr_u32("idx").unwrap_or(0);
        if idx == 0 {
            return own;
        }
        let Some(style_node) = self.theme().effect_style(idx) else {
            return own;
        };
        // `a:effectStyle` 里的 `a:effectLst` 才是效果列表
        let lst = style_node.child("effectLst");
        paint::parse_effects(lst, Some(self.scheme()))
    }

    /// 解析文本框。
    ///
    /// 取 `&self` 而非 `&mut self`：表格解析在持有解析器引用的同时
    /// 也要解析单元格文本，需要共享借用。
    fn parse_text_body(
        &self,
        tx_body: &XmlNode,
        ph_info: &Option<crate::inherit::Placeholder>,
        ph_type: Option<PlaceholderType>,
    ) -> ppt_core::scene::TextBox {
        // 1) 角色基线：母版 `p:txStyles` 给出该角色（标题/正文/其他）的
        //    各级字号、颜色、项目符号；母版缺失时退化为主题默认字符属性。
        let role_style = self.inherit.tx_style_for(ph_type);
        let base = if role_style.levels.is_empty() {
            TextStyles::parse(
                None,
                Some(self.scheme()),
                &crate::inherit::theme_seed_run(self.theme(), self.scheme()),
            )
        } else {
            role_style.clone()
        };

        // 2) 逐层叠加：版式占位符的 lstStyle → 形状自身的 lstStyle。
        //    注意是「叠加」而非「替换」—— 课件里常见空的 <a:lstStyle/>，
        //    替换会丢掉母版定义的全部样式。
        let own_lst = tx_body.child("lstStyle");
        let inherited_lst = ph_info.as_ref().and_then(|p| p.lst_style.as_ref());
        let styles = base
            .overlay(inherited_lst, Some(self.scheme()))
            .overlay(own_lst, Some(self.scheme()));

        let seed = styles.level(0).run.clone();

        let ctx = self.link_ctx();
        let mut tb = text::parse_text_box(tx_body, &styles, &seed, Some(self.scheme()), &ctx);

        // `+mj-lt` / `+mn-ea` 这类主题字体引用在解析阶段就要落成具体字体名，
        // 否则排版引擎会去查一个不存在的字体，丢掉课件的字体层次
        text::resolve_theme_fonts(&mut tb, &self.theme().font_scheme);

        // 占位符角色决定垂直锚点（标题通常居中）
        if let Some(t) = ph_type {
            if t.is_title() && tb.body.anchor == VerticalAnchor::Top {
                tb.body.anchor = VerticalAnchor::Middle;
            }
        }

        tb
    }

    /// 解析形状级超链接（点击形状本体跳转）。
    fn parse_shape_hyperlink(&self, c_nv_pr: &XmlNode) -> Option<Hyperlink> {
        let hl = c_nv_pr.child("hlinkClick")?;
        let ctx = self.link_ctx();
        // 复用文本超链接的解析逻辑：包一层 rPr 结构
        let wrapper = XmlNode {
            name: "rPr".to_string(),
            attrs: Vec::new(),
            children: vec![hl.clone()],
            text: String::new(),
        };
        text::parse_hyperlink(Some(&wrapper), &ctx)
    }
}

static EMPTY: XmlNode = XmlNode {
    name: String::new(),
    attrs: Vec::new(),
    children: Vec::new(),
    text: String::new(),
};

/// 递归提取节点树中的所有 `a:t` 文本。
fn extract_texts(node: &XmlNode) -> Vec<String> {
    let mut out = Vec::new();
    collect_texts(node, &mut out);
    out
}

fn collect_texts(node: &XmlNode, out: &mut Vec<String>) {
    if node.name == "t" {
        let t = node.deep_text();
        if !t.trim().is_empty() {
            out.push(t);
        }
        return;
    }
    for c in &node.children {
        collect_texts(c, out);
    }
}

/// 解析一整页幻灯片。
///
/// 返回的 [`Scene`] 已包含背景、形状与降级告警。
pub fn parse_slide(
    slide: &XmlNode,
    inherit: &SlideInheritance,
    resolve: RelResolver<'_>,
) -> Scene {
    parse_slide_with_parts(slide, inherit, resolve, None)
}

/// 部件读取回调：按部件名取出 XML 文本。
///
/// 图表、SmartArt 这类对象的正文**不在幻灯片里**，而在自己的部件中
/// （`ppt/charts/chartN.xml`）。幻灯片只留一个 `r:id` 指过去，
/// 因此解析这类对象必须能跨部件读取。
pub type PartReader<'a> = &'a dyn Fn(&str) -> Option<String>;

/// 同 [`parse_slide`]，但允许解析需要跨部件读取的对象（图表等）。
pub fn parse_slide_with_parts(
    slide: &XmlNode,
    inherit: &SlideInheritance,
    resolve: RelResolver<'_>,
    read_part: Option<PartReader<'_>>,
) -> Scene {
    let size = inherit.slide_size;
    let mut scene = Scene::new(size);

    // 背景：幻灯片自身优先，其次继承
    let scheme = &inherit.color_scheme;
    let own_bg = crate::inherit::parse_background(slide, scheme, resolve);
    scene.background = match own_bg {
        Some(f) => ppt_core::scene::SceneBackground::Fill(f),
        None => match &inherit.background {
            Some(f) => ppt_core::scene::SceneBackground::Fill(f.clone()),
            // 课件没指定背景时，默认白底（而不是透明），
            // 否则叠加到黑底窗口上会看不见内容
            None => ppt_core::scene::SceneBackground::Solid(Color::WHITE),
        },
    };

    let Some(tree) = slide.path(&["cSld", "spTree"]) else {
        scene.push_warning("幻灯片缺少 p:spTree，页面为空");
        return scene;
    };

    let mut parser = NodeParser::new(inherit, resolve);
    parser.read_part = read_part;
    scene.nodes = parser.parse_tree(tree);

    for w in parser.warnings {
        scene.push_warning(w);
    }

    // 动画时序：必须在节点树建好之后才处理 ——
    // 时序用 `spid` 指目标，得先有带 `shape_id` 的节点才认得出来；
    // 位置表达式（`#ppt_x-0.1`、`1+#ppt_h/2`）还要查目标形状的尺寸与位置
    let (anim, anim_warnings) = {
        let size = scene.size_pt;
        let lookup = |id: u32| -> Option<crate::timing::ShapeBox> {
            if size.w <= 0.0 || size.h <= 0.0 {
                return None;
            }
            let r = scene.node_bounds(id)?;
            Some(crate::timing::ShapeBox {
                w: r.w / size.w,
                h: r.h / size.h,
                cx: (r.x + r.w / 2.0) / size.w,
                cy: (r.y + r.h / 2.0) / size.h,
            })
        };
        crate::timing::parse_timing(slide, (size.w, size.h), &lookup)
    };
    scene.anim = anim;
    scene.apply_anim_sequence();
    for w in anim_warnings {
        scene.push_warning(w);
    }

    // 转场与动画时序互不相干，谁先谁后都行
    scene.transition = crate::transition::parse_transition(slide);

    scene.title = extract_title(&scene);
    scene
}

/// 从场景里推断标题（取标题占位符的文本），用于缩略图与导航。
fn extract_title(scene: &Scene) -> Option<String> {
    for node in scene.walk() {
        // 标题占位符在解析时被标记为 name 含「标题」或 Title，
        // 这里退化为「取最靠上的大字号文本」
        if let Some(tb) = &node.text {
            let text = tb.plain_text();
            if text.trim().is_empty() {
                continue;
            }
            let max_size = tb
                .paragraphs
                .iter()
                .flat_map(|p| p.runs.iter())
                .map(|r| r.props.size_pt)
                .fold(0.0f32, f32::max);
            if max_size >= 28.0 {
                return Some(text.trim().to_string());
            }
        }
    }
    None
}

/// 解析备注页（`ppt/notesSlides/notesSlideN.xml`）中的正文文本。
pub fn parse_notes(notes_slide: &XmlNode) -> Option<String> {
    let tree = notes_slide.path(&["cSld", "spTree"])?;

    let mut parts = Vec::new();
    for sp in tree.children_named("sp") {
        // 备注正文位于 `p:ph type="body"` 的占位符里
        let ph = sp.path(&["nvSpPr", "nvPr", "ph"]);
        let is_body = ph
            .and_then(|p| p.attr("type"))
            .map(|t| t == "body")
            .unwrap_or(false);
        if !is_body {
            continue;
        }
        let Some(tx) = sp.child("txBody") else {
            continue;
        };
        let text = text::extract_plain_text(tx);
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            parts.push(trimmed.to_string());
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

/// 未使用导入守卫：这些类型经由 `Node` 字段间接使用。
#[allow(dead_code)]
fn _assert_types(_: BodyProps, _: Insets, _: RunProps, _: StyleRole) {}

#[cfg(test)]
mod tests {
    use super::*;
    use ppt_core::xml;

    fn n(xml_text: &str) -> XmlNode {
        xml::parse_root("t", xml_text).unwrap()
    }

    fn no_rel(_: &str) -> Option<String> {
        None
    }

    fn inherit() -> SlideInheritance {
        SlideInheritance::build(
            &Theme::default(),
            None,
            None,
            None,
            Size::new(960.0, 540.0),
            &no_rel,
        )
    }

    fn slice(xml_text: &str) -> Scene {
        let inh = inherit();
        parse_slide(&n(xml_text), &inh, &no_rel)
    }

    #[test]
    fn parses_simple_rect_shape() {
        let scene = slice(
            r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><p:spTree>
                 <p:sp>
                   <p:nvSpPr><p:cNvPr id="2" name="矩形 1"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
                   <p:spPr>
                     <a:xfrm><a:off x="914400" y="457200"/><a:ext cx="1828800" cy="914400"/></a:xfrm>
                     <a:prstGeom prst="rect"><a:avLst/></a:prstGeom>
                     <a:solidFill><a:srgbClr val="FF0000"/></a:solidFill>
                   </p:spPr>
                 </p:sp>
               </p:spTree></p:cSld></p:sld>"#,
        );
        assert_eq!(scene.nodes.len(), 1);
        let node = &scene.nodes[0];
        assert_eq!(node.name.as_deref(), Some("矩形 1"));
        assert_eq!(node.geometry, Geometry::Rect);
        assert_eq!(node.fill, Fill::Solid(Color::rgb(255, 0, 0)));
        assert_eq!(node.canvas_bounds(), Rect::new(72.0, 36.0, 144.0, 72.0));
    }

    /// 翻转形状时文字**不能**跟着镜像。
    ///
    /// 出问题时的样子：`flipH="1"` 的文本框里写 REPORT，渲染出来是反写的。
    /// PowerPoint 的语义是形状翻过去、文字仍然正着
    /// （「文本在翻转的对象里不会被自动翻转」），所以解析阶段要另存一份
    /// 抵消掉翻转的变换给文字用。
    #[test]
    fn flipped_shape_keeps_its_text_upright() {
        let scene = slice(
            r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><p:spTree>
                 <p:sp>
                   <p:nvSpPr><p:cNvPr id="2" name="REPORT"/></p:nvSpPr>
                   <p:spPr>
                     <a:xfrm flipH="1"><a:off x="914400" y="457200"/><a:ext cx="1828800" cy="914400"/></a:xfrm>
                     <a:prstGeom prst="rect"><a:avLst/></a:prstGeom>
                   </p:spPr>
                   <p:txBody><a:bodyPr/><a:p><a:r><a:t>REPORT</a:t></a:r></a:p></p:txBody>
                 </p:sp>
               </p:spTree></p:cSld></p:sld>"#,
        );
        let node = &scene.nodes[0];
        let extent = Size::new(144.0, 72.0);

        // 形状本身确实镜像了：局部左上角跑到了画布右边
        let tl = node.transform.apply(Point::ZERO);
        let br = node.transform.apply(Point::new(extent.w, extent.h));
        assert!(tl.x > br.x, "形状应当被水平镜像");

        // 文字那份等于「同一个形状但不翻转」
        let text = node
            .text_transform
            .expect("翻转的形状应当带一份文字专用变换");
        let plain = Transform::from_ooxml(Point::new(72.0, 36.0), extent, 0.0, false, false);
        for p in [Point::ZERO, Point::new(144.0, 0.0), Point::new(20.0, 31.0)] {
            let (a, b) = (text.apply(p), plain.apply(p));
            assert!(
                (a.x - b.x).abs() < 0.01 && (a.y - b.y).abs() < 0.01,
                "文字变换在 {p:?} 处对不上：{a:?} vs {b:?}"
            );
        }

        // 位置一点没动：文字框映射出来还是同一个矩形，只是左右对调
        let (a, b) = (text.apply(Point::ZERO), text.apply(Point::new(extent.w, extent.h)));
        assert!((tl.x.min(br.x) - a.x.min(b.x)).abs() < 0.01);
        assert!((tl.y.min(br.y) - a.y.min(b.y)).abs() < 0.01);
        assert!((tl.x.max(br.x) - a.x.max(b.x)).abs() < 0.01);
        assert!((tl.y.max(br.y) - a.y.max(b.y)).abs() < 0.01);
    }

    /// 翻转的**组合**里的文字同样不能跟着镜像。
    ///
    /// 出问题时的样子：一页卡片全在 `<p:grpSp flipH="1">` 里，
    /// 卡片上的「输入标题」整句反写（课件第 15 页就是这个）。
    ///
    /// 上一版只看形状自己的 `flipH`，组合这一层漏了 —— 所以判据改成
    /// 「最终矩阵有没有镜像」。
    #[test]
    fn text_inside_a_flipped_group_stays_upright() {
        let scene = slice(
            r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><p:spTree>
                 <p:grpSp>
                   <p:nvGrpSpPr><p:cNvPr id="10" name="组合 1"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
                   <p:grpSpPr>
                     <a:xfrm flipH="1">
                       <a:off x="914400" y="457200"/><a:ext cx="5486400" cy="1828800"/>
                       <a:chOff x="0" y="0"/><a:chExt cx="5486400" cy="1828800"/>
                     </a:xfrm>
                   </p:grpSpPr>
                   <p:sp>
                     <p:nvSpPr><p:cNvPr id="11" name="输入标题"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
                     <p:spPr>
                       <a:xfrm><a:off x="0" y="0"/><a:ext cx="1828800" cy="914400"/></a:xfrm>
                       <a:prstGeom prst="rect"><a:avLst/></a:prstGeom>
                     </p:spPr>
                     <p:txBody><a:bodyPr/><a:p><a:r><a:t>输入标题</a:t></a:r></a:p></p:txBody>
                   </p:sp>
                 </p:grpSp>
               </p:spTree></p:cSld></p:sld>"#,
        );
        let shape = &scene.nodes[0].children[0];

        // 组合的翻转真的传到了子形状上（否则这个测试什么也没验证）
        assert!(shape.transform.is_mirrored(), "子形状应当是镜像的");

        // 而文字那份不是镜像的，且位置一点没动
        let text = shape
            .text_transform
            .expect("组合翻转时子形状应当带一份文字专用变换");
        assert!(!text.is_mirrored(), "文字变换不该是镜像的");

        // 文字框映射出来的矩形和形状完全重合（框里的点本来就该左右对调）
        let extent = Size::new(144.0, 72.0);
        let bounds = |tf: &Transform| {
            let a = tf.apply(Point::ZERO);
            let b = tf.apply(Point::new(extent.w, extent.h));
            [a.x.min(b.x), a.y.min(b.y), a.x.max(b.x), a.y.max(b.y)]
        };
        let (was, now) = (bounds(&shape.transform), bounds(&text));
        for i in 0..4 {
            assert!(
                (was[i] - now[i]).abs() < 0.01,
                "文字框位置被挪动了：{was:?} vs {now:?}"
            );
        }

        // 文字正立：基线方向回到 +x
        let [a, b, _, _, _, _] = text.m;
        assert!(b.atan2(a).to_degrees().abs() < 0.01, "文字应当是正立的");
    }

    /// 内外各翻一次是负负得正，这时候文字本来就正着，不该多此一举。
    #[test]
    fn group_flip_cancelled_by_shape_flip_needs_no_text_transform() {
        let scene = slice(
            r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><p:spTree>
                 <p:grpSp>
                   <p:nvGrpSpPr><p:cNvPr id="10" name="组合 1"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
                   <p:grpSpPr>
                     <a:xfrm flipH="1">
                       <a:off x="914400" y="457200"/><a:ext cx="5486400" cy="1828800"/>
                       <a:chOff x="0" y="0"/><a:chExt cx="5486400" cy="1828800"/>
                     </a:xfrm>
                   </p:grpSpPr>
                   <p:sp>
                     <p:nvSpPr><p:cNvPr id="11" name="输入标题"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
                     <p:spPr>
                       <a:xfrm flipH="1"><a:off x="0" y="0"/><a:ext cx="1828800" cy="914400"/></a:xfrm>
                       <a:prstGeom prst="rect"><a:avLst/></a:prstGeom>
                     </p:spPr>
                     <p:txBody><a:bodyPr/><a:p><a:r><a:t>输入标题</a:t></a:r></a:p></p:txBody>
                   </p:sp>
                 </p:grpSp>
               </p:spTree></p:cSld></p:sld>"#,
        );
        let shape = &scene.nodes[0].children[0];
        assert!(!shape.transform.is_mirrored());
        assert!(shape.text_transform.is_none());
    }

    #[test]
    fn unflipped_shape_needs_no_text_transform() {
        let scene = slice(
            r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><p:spTree>
                 <p:sp>
                   <p:nvSpPr><p:cNvPr id="2" name="标题"/></p:nvSpPr>
                   <p:spPr>
                     <a:xfrm><a:off x="914400" y="457200"/><a:ext cx="1828800" cy="914400"/></a:xfrm>
                     <a:prstGeom prst="rect"><a:avLst/></a:prstGeom>
                   </p:spPr>
                   <p:txBody><a:bodyPr/><a:p><a:r><a:t>REPORT</a:t></a:r></a:p></p:txBody>
                 </p:sp>
               </p:spTree></p:cSld></p:sld>"#,
        );
        assert!(scene.nodes[0].text_transform.is_none());
    }

    #[test]
    fn ellipse_uses_dedicated_geometry() {
        let scene = slice(
            r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><p:spTree>
                 <p:sp>
                   <p:nvSpPr><p:cNvPr id="2" name="椭圆"/></p:nvSpPr>
                   <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm>
                     <a:prstGeom prst="ellipse"/></p:spPr>
                 </p:sp>
               </p:spTree></p:cSld></p:sld>"#,
        );
        assert_eq!(scene.nodes[0].geometry, Geometry::Ellipse);
    }

    #[test]
    fn round_rect_radius_scaled_to_extent() {
        // 短边 72pt
        let radius_of = |av: &str| {
            let xml = format!(
                r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><p:spTree>
                     <p:sp>
                       <p:nvSpPr><p:cNvPr id="2" name="圆角"/></p:nvSpPr>
                       <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm>
                         <a:prstGeom prst="roundRect">{av}</a:prstGeom></p:spPr>
                     </p:sp>
                   </p:spTree></p:cSld></p:sld>"#
            );
            match slice(&xml).nodes[0].geometry {
                Geometry::RoundRect { rx_pt, .. } => rx_pt,
                ref other => panic!("应为圆角矩形，实际 {other:?}"),
            }
        };

        // OOXML 的默认圆角是短边的 1/6：72 / 6 = 12pt
        let implicit = radius_of("<a:avLst/>");
        assert!(
            (implicit - 12.0).abs() < 0.1,
            "不写调整值时应取默认的 1/6，实际 {implicit}"
        );

        // 把默认值显式写出来，结果必须一模一样。
        //
        // 这一条正是当初漏掉的：`a:gd/@fmla="val N"` 里的 N 是**十万分之一**的百分数，
        // 不是自定义几何那套 21600 虚拟坐标系里的长度。混了之后圆角被放大约 4.6 倍，
        // 课件里方方正正的圆角标注框全变成了「胶囊」。
        let explicit = radius_of(r#"<a:avLst><a:gd name="adj" fmla="val 16667"/></a:avLst>"#);
        assert!(
            (explicit - implicit).abs() < 0.05,
            "显式写出默认值应与不写等价：{explicit} vs {implicit}"
        );
    }

    #[test]
    fn star_preset_expands_to_path() {
        let scene = slice(
            r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><p:spTree>
                 <p:sp>
                   <p:nvSpPr><p:cNvPr id="2" name="五角星"/></p:nvSpPr>
                   <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm>
                     <a:prstGeom prst="star5"/></p:spPr>
                 </p:sp>
               </p:spTree></p:cSld></p:sld>"#,
        );
        match &scene.nodes[0].geometry {
            Geometry::Path(p) => assert_eq!(p.subpaths.len(), 1),
            other => panic!("五角星应展开为路径，实际 {other:?}"),
        }
        assert!(scene.warnings.is_empty(), "已支持的预设不应产生告警");
    }

    #[test]
    fn unknown_preset_degrades_with_warning() {
        let scene = slice(
            r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><p:spTree>
                 <p:sp>
                   <p:nvSpPr><p:cNvPr id="2" name="怪形状"/></p:nvSpPr>
                   <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm>
                     <a:prstGeom prst="totallyMadeUp"/></p:spPr>
                 </p:sp>
               </p:spTree></p:cSld></p:sld>"#,
        );
        assert!(scene.nodes[0].geometry.is_degraded());
        assert!(!scene.warnings.is_empty(), "降级应产生告警");
    }

    #[test]
    fn text_body_parsed_with_master_style() {
        let scene = slice(
            r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><p:spTree>
                 <p:sp>
                   <p:nvSpPr><p:cNvPr id="2" name="标题"/></p:nvSpPr>
                   <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="9144000" cy="1143000"/></a:xfrm>
                     <a:prstGeom prst="rect"/></p:spPr>
                   <p:txBody><a:bodyPr/><a:lstStyle/>
                     <a:p><a:r><a:rPr sz="3200" b="1"/><a:t>课程标题</a:t></a:r></a:p>
                   </p:txBody>
                 </p:sp>
               </p:spTree></p:cSld></p:sld>"#,
        );
        let tb = scene.nodes[0].text.as_ref().expect("应解析出文本框");
        assert_eq!(tb.plain_text(), "课程标题");
        assert!((tb.paragraphs[0].runs[0].props.size_pt - 32.0).abs() < 1e-4);
        assert!(tb.paragraphs[0].runs[0].props.bold);
    }

    #[test]
    fn title_extracted_from_large_text() {
        let scene = slice(
            r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><p:spTree>
                 <p:sp>
                   <p:nvSpPr><p:cNvPr id="2" name="标题"/></p:nvSpPr>
                   <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="9144000" cy="1143000"/></a:xfrm>
                     <a:prstGeom prst="rect"/></p:spPr>
                   <p:txBody><a:bodyPr/>
                     <a:p><a:r><a:rPr sz="4000"/><a:t>光合作用</a:t></a:r></a:p>
                   </p:txBody>
                 </p:sp>
               </p:spTree></p:cSld></p:sld>"#,
        );
        assert_eq!(scene.title.as_deref(), Some("光合作用"));
    }

    #[test]
    fn group_bakes_child_transform() {
        // 组合位于 (100,100)，尺寸 200x200；子坐标系 100x100 → 子矩形应被映射到组合范围内
        let scene = slice(
            r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><p:spTree>
                 <p:grpSp>
                   <p:nvGrpSpPr><p:cNvPr id="1" name="组合 1"/></p:nvGrpSpPr>
                   <p:grpSpPr><a:xfrm>
                     <a:off x="914400" y="914400"/>
                     <a:ext cx="1828800" cy="1828800"/>
                     <a:chOff x="0" y="0"/>
                     <a:chExt cx="914400" cy="914400"/>
                   </a:xfrm></p:grpSpPr>
                   <p:sp>
                     <p:nvSpPr><p:cNvPr id="2" name="子矩形"/></p:nvSpPr>
                     <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm>
                       <a:prstGeom prst="rect"/>
                       <a:solidFill><a:srgbClr val="00FF00"/></a:solidFill>
                     </p:spPr>
                   </p:sp>
                 </p:grpSp>
               </p:spTree></p:cSld></p:sld>"#,
        );

        assert_eq!(scene.nodes.len(), 1);
        let group = &scene.nodes[0];
        assert_eq!(group.children.len(), 1);

        // 子形状（72pt 见方）在子坐标系里占满，应被映射到组合的 144pt 范围
        let child_bounds = group.children[0].canvas_bounds();
        assert!(
            (child_bounds.x - 72.0).abs() < 0.1,
            "子形状 x 应为 72pt，实际 {}",
            child_bounds.x
        );
        assert!(
            (child_bounds.w - 144.0).abs() < 0.1,
            "子形状宽度应被缩放为 144pt，实际 {}",
            child_bounds.w
        );
    }

    #[test]
    fn nested_group_composes_transforms() {
        let scene = slice(
            r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><p:spTree>
                 <p:grpSp>
                   <p:nvGrpSpPr><p:cNvPr id="1" name="外层"/></p:nvGrpSpPr>
                   <p:grpSpPr><a:xfrm>
                     <a:off x="914400" y="0"/><a:ext cx="914400" cy="914400"/>
                     <a:chOff x="0" y="0"/><a:chExt cx="914400" cy="914400"/>
                   </a:xfrm></p:grpSpPr>
                   <p:grpSp>
                     <p:nvGrpSpPr><p:cNvPr id="2" name="内层"/></p:nvGrpSpPr>
                     <p:grpSpPr><a:xfrm>
                       <a:off x="0" y="914400"/><a:ext cx="914400" cy="914400"/>
                       <a:chOff x="0" y="0"/><a:chExt cx="914400" cy="914400"/>
                     </a:xfrm></p:grpSpPr>
                     <p:sp>
                       <p:nvSpPr><p:cNvPr id="3" name="深层矩形"/></p:nvSpPr>
                       <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm>
                         <a:prstGeom prst="rect"/>
                         <a:solidFill><a:srgbClr val="0000FF"/></a:solidFill>
                       </p:spPr>
                     </p:sp>
                   </p:grpSp>
                 </p:grpSp>
               </p:spTree></p:cSld></p:sld>"#,
        );

        // 外层偏移 (72,0)，内层偏移 (0,72) → 最内层矩形应在 (72,72)
        let outer = &scene.nodes[0];
        let inner = &outer.children[0];
        let deep = &inner.children[0];
        let b = deep.canvas_bounds();
        assert!((b.x - 72.0).abs() < 0.1, "实际 x = {}", b.x);
        assert!((b.y - 72.0).abs() < 0.1, "实际 y = {}", b.y);
    }

    #[test]
    fn group_fill_inherited_by_child() {
        let scene = slice(
            r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><p:spTree>
                 <p:grpSp>
                   <p:nvGrpSpPr><p:cNvPr id="1" name="组合"/></p:nvGrpSpPr>
                   <p:grpSpPr>
                     <a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/>
                       <a:chOff x="0" y="0"/><a:chExt cx="914400" cy="914400"/></a:xfrm>
                     <a:solidFill><a:srgbClr val="ABCDEF"/></a:solidFill>
                   </p:grpSpPr>
                   <p:sp>
                     <p:nvSpPr><p:cNvPr id="2" name="子形状"/></p:nvSpPr>
                     <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm>
                       <a:prstGeom prst="rect"/><a:grpFill/></p:spPr>
                   </p:sp>
                 </p:grpSp>
               </p:spTree></p:cSld></p:sld>"#,
        );
        let child = &scene.nodes[0].children[0];
        assert_eq!(child.fill, Fill::Solid(Color::rgb(0xAB, 0xCD, 0xEF)));
    }

    #[test]
    fn picture_parsed_with_source_rect() {
        let resolve = |_: &str| Some("ppt/media/image1.png".to_string());
        let inh = inherit();
        let scene = parse_slide(
            &n(
                r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a" xmlns:r="urn:r"><p:cSld><p:spTree>
                     <p:pic>
                       <p:nvPicPr><p:cNvPr id="2" name="图片 1"/></p:nvPicPr>
                       <p:blipFill>
                         <a:blip r:embed="rId1"/>
                         <a:srcRect l="10000" t="0" r="0" b="0"/>
                         <a:stretch><a:fillRect/></a:stretch>
                       </p:blipFill>
                       <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm>
                         <a:prstGeom prst="rect"/></p:spPr>
                     </p:pic>
                   </p:spTree></p:cSld></p:sld>"#,
            ),
            &inh,
            &resolve,
        );

        assert_eq!(scene.nodes.len(), 1);
        match &scene.nodes[0].geometry {
            Geometry::Image(img) => {
                assert_eq!(img.part, "ppt/media/image1.png");
                assert!((img.src_rect.l - 0.1).abs() < 1e-6);
            }
            other => panic!("应为图片几何，实际 {other:?}"),
        }
    }

    #[test]
    fn embedded_video_is_parsed_onto_the_picture() {
        // 视频挂在图片形状上：blipFill 是封页图，nvPr 里的 p14:media 才是媒体。
        //
        // **媒体藏在 `p:extLst/p:ext` 里两层**，这是 PowerPoint 真正写出来的样子：
        // `a:videoFile` 只带 `r:link`（指向外部文件，课件里 Target 甚至是 "NULL"），
        // 真正的嵌入关系在 `p14:media@r:embed`。早先这个测试把 `p14:media` 摆在
        // `p:nvPr` 的直接子节点上，与真实文件不符 —— 于是「课件里所有视频和音频
        // 都点不动」这个 bug 一路溜过了测试。
        let resolve = |id: &str| match id {
            "rId1" => Some("ppt/media/image1.png".to_string()),
            "rId2" => Some("ppt/media/media1.mp4".to_string()),
            _ => None,
        };
        let inh = inherit();
        let scene = parse_slide(
            &n(
                r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a" xmlns:r="urn:r"><p:cSld><p:spTree>
                     <p:pic>
                       <p:nvPicPr>
                         <p:cNvPr id="2" name="视频 1"/>
                         <p:nvPr><a:videoFile r:link="rId9"/>
                           <p:extLst><p:ext uri="{DAF3E8A0-...}">
                             <p14:media xmlns:p14="urn:p14" r:embed="rId2" loop="1">
                               <p14:trim st="1633" end="6864"/>
                             </p14:media>
                           </p:ext></p:extLst>
                         </p:nvPr>
                       </p:nvPicPr>
                       <p:blipFill><a:blip r:embed="rId1"/></p:blipFill>
                       <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm>
                         <a:prstGeom prst="rect"/></p:spPr>
                     </p:pic>
                   </p:spTree></p:cSld></p:sld>"#,
            ),
            &inh,
            &resolve,
        );

        let node = &scene.nodes[0];
        // 封页图照旧走图片链路
        assert!(matches!(node.geometry, Geometry::Image(_)));
        let media = node.media.as_ref().expect("应解析出内嵌媒体");
        // 必须认 `p14:media@r:embed`，而不是只有 `r:link` 的 `a:videoFile`
        assert_eq!(media.part, "ppt/media/media1.mp4");
        assert_eq!(media.kind, MediaKind::Video);
        assert!(media.loop_play, "loop=1 应被识别");
        assert_eq!(
            media.trim,
            Some(MediaTrim {
                start_ms: 1633,
                end_ms: Some(6864)
            }),
            "「裁剪视频」的区间要带上，否则会把整段片长全放出去"
        );

        // 热区应可用，供前端叠播放器
        let spots = scene.media_hotspots();
        assert_eq!(spots.len(), 1);
        assert_eq!(spots[0].rect, Rect::new(0.0, 0.0, 72.0, 72.0));
    }

    #[test]
    fn embedded_audio_without_trim_has_no_trim_range() {
        // 音频挂在图片形状上，且没裁剪过 —— `trim` 必须是 None 而不是「0~0」
        let resolve = |id: &str| match id {
            "rId1" => Some("ppt/media/image1.png".to_string()),
            "rId2" => Some("ppt/media/media2.wav".to_string()),
            _ => None,
        };
        let scene = parse_slide(
            &n(
                r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a" xmlns:r="urn:r"><p:cSld><p:spTree>
                     <p:pic>
                       <p:nvPicPr><p:cNvPr id="2" name="音频 1"/>
                         <p:nvPr><a:audioFile r:link="rId9"/>
                           <p:extLst><p:ext uri="{DAA4B4D4-...}">
                             <p14:media xmlns:p14="urn:p14" r:embed="rId2"/>
                           </p:ext></p:extLst></p:nvPr></p:nvPicPr>
                       <p:blipFill><a:blip r:embed="rId1"/></p:blipFill>
                       <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm>
                         <a:prstGeom prst="rect"/></p:spPr>
                     </p:pic>
                   </p:spTree></p:cSld></p:sld>"#,
            ),
            &inherit(),
            &resolve,
        );

        let media = scene.nodes[0].media.as_ref().expect("应解析出内嵌音频");
        assert_eq!(media.part, "ppt/media/media2.wav");
        assert_eq!(media.kind, MediaKind::Audio);
        assert_eq!(media.trim, None);
        assert!(!media.loop_play);
    }

    #[test]
    fn linked_media_is_skipped_with_an_explanatory_warning() {
        // 只给了 r:link（指向课件旁的外部文件）：不做成播放按钮，但要说明原因
        let resolve = |id: &str| match id {
            "rId1" => Some("ppt/media/image1.png".to_string()),
            "rId2" => Some("movie.mp4".to_string()),
            _ => None,
        };
        let scene = parse_slide(
            &n(
                r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a" xmlns:r="urn:r"><p:cSld><p:spTree>
                     <p:pic>
                       <p:nvPicPr><p:cNvPr id="2" name="视频 1"/>
                         <p:nvPr><a:videoFile r:link="rId2"/></p:nvPr></p:nvPicPr>
                       <p:blipFill><a:blip r:embed="rId1"/></p:blipFill>
                       <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm>
                         <a:prstGeom prst="rect"/></p:spPr>
                     </p:pic>
                   </p:spTree></p:cSld></p:sld>"#,
            ),
            &inherit(),
            &resolve,
        );

        assert!(scene.nodes[0].media.is_none(), "外链媒体不应产出播放项");
        assert!(
            scene.warnings.iter().any(|w| w.contains("链接到文件")),
            "应给出可排查的告警，实际：{:?}",
            scene.warnings
        );
    }

    #[test]
    fn picture_without_media_has_no_media_field() {
        let resolve = |_: &str| Some("ppt/media/image1.png".to_string());
        let scene = parse_slide(
            &n(
                r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a" xmlns:r="urn:r"><p:cSld><p:spTree>
                     <p:pic>
                       <p:nvPicPr><p:cNvPr id="2" name="普通图片"/></p:nvPicPr>
                       <p:blipFill><a:blip r:embed="rId1"/></p:blipFill>
                       <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm>
                         <a:prstGeom prst="rect"/></p:spPr>
                     </p:pic>
                   </p:spTree></p:cSld></p:sld>"#,
            ),
            &inherit(),
            &resolve,
        );
        assert!(scene.nodes[0].media.is_none());
        assert!(scene.media_hotspots().is_empty());
    }

    #[test]
    fn picture_without_relationship_is_skipped_with_warning() {
        let scene = slice(
            r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a" xmlns:r="urn:r"><p:cSld><p:spTree>
                 <p:pic>
                   <p:nvPicPr><p:cNvPr id="2" name="坏图片"/></p:nvPicPr>
                   <p:blipFill><a:blip r:embed="rId999"/></p:blipFill>
                   <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm></p:spPr>
                 </p:pic>
               </p:spTree></p:cSld></p:sld>"#,
        );
        assert!(scene.nodes.is_empty(), "资源缺失的图片应被跳过");
        assert!(!scene.warnings.is_empty());
    }

    #[test]
    fn theme_fill_ref_applied_when_shape_has_no_fill() {
        let inh = themed_inherit();
        let scene = parse_slide(
            &n(
                r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><p:spTree>
                     <p:sp>
                       <p:nvSpPr><p:cNvPr id="2" name="样式形状"/></p:nvSpPr>
                       <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm>
                         <a:prstGeom prst="rect"/></p:spPr>
                       <p:style>
                         <a:fillRef idx="1"><a:schemeClr val="accent1"/></a:fillRef>
                       </p:style>
                     </p:sp>
                   </p:spTree></p:cSld></p:sld>"#,
            ),
            &inh,
            &no_rel,
        );

        assert_eq!(
            scene.nodes[0].fill,
            Fill::Solid(Color::rgb(0x44, 0x72, 0xC4)),
            "应从主题 fillRef 取到 accent1"
        );
    }

    /// 主题里带一套填充样式（`fillStyleLst` 第 0 项 = accent1 实色），
    /// 供「要不要回退到 `a:fillRef`」这类测试用。
    fn themed_inherit() -> SlideInheritance {
        let theme = Theme::parse(
            &n(
                r#"<a:theme><a:themeElements>
                     <a:clrScheme name="x">
                       <a:dk1><a:srgbClr val="000000"/></a:dk1><a:lt1><a:srgbClr val="FFFFFF"/></a:lt1>
                       <a:dk2><a:srgbClr val="111111"/></a:dk2><a:lt2><a:srgbClr val="222222"/></a:lt2>
                       <a:accent1><a:srgbClr val="4472C4"/></a:accent1>
                       <a:accent2><a:srgbClr val="ED7D31"/></a:accent2>
                       <a:accent3><a:srgbClr val="A5A5A5"/></a:accent3>
                       <a:accent4><a:srgbClr val="FFC000"/></a:accent4>
                       <a:accent5><a:srgbClr val="5B9BD5"/></a:accent5>
                       <a:accent6><a:srgbClr val="70AD47"/></a:accent6>
                       <a:hlink><a:srgbClr val="0563C1"/></a:hlink>
                       <a:folHlink><a:srgbClr val="954F72"/></a:folHlink>
                     </a:clrScheme>
                     <a:fmtScheme name="Office">
                       <a:fillStyleLst>
                         <a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
                       </a:fillStyleLst>
                       <a:effectStyleLst>
                         <a:effectStyle><a:effectLst/></a:effectStyle>
                         <a:effectStyle><a:effectLst>
                           <a:outerShdw blurRad="40000" dist="20000" dir="5400000" rot="0">
                             <a:srgbClr val="000000"><a:alpha val="40000"/></a:srgbClr>
                           </a:outerShdw>
                         </a:effectLst></a:effectStyle>
                       </a:effectStyleLst>
                     </a:fmtScheme>
                   </a:themeElements></a:theme>"#,
            ),
            None,
        );
        SlideInheritance::build(&theme, None, None, None, Size::new(960.0, 540.0), &no_rel)
    }

    #[test]
    fn explicit_no_fill_does_not_fall_back_to_theme_fill_ref() {
        // 老师用「透明 + 红边」的圆角矩形挖空答案时，形状写的是
        // `<a:noFill/>` 加 `p:style/a:fillRef idx="1"`。
        //
        // `a:noFill` 是**显式的不要填充**，不是「没写」——
        // 分不清这两者就会把透明红框画成 accent1 的**蓝色实心块**，
        // 一页答案连同文字整片被盖住。这正是「严重渲染错误」的根因。
        let scene = parse_slide(
            &n(
                r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><p:spTree>
                     <p:sp>
                       <p:nvSpPr><p:cNvPr id="2" name="圆角矩形 30"/></p:nvSpPr>
                       <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm>
                         <a:prstGeom prst="roundRect"/><a:noFill/>
                         <a:ln w="12700"><a:solidFill><a:srgbClr val="C00000"/></a:solidFill></a:ln>
                       </p:spPr>
                       <p:style>
                         <a:lnRef idx="1"><a:schemeClr val="accent1"/></a:lnRef>
                         <a:fillRef idx="1"><a:schemeClr val="accent1"/></a:fillRef>
                         <a:effectRef idx="0"><a:schemeClr val="accent1"/></a:effectRef>
                       </p:style>
                     </p:sp>
                   </p:spTree></p:cSld></p:sld>"#,
            ),
            &themed_inherit(),
            &no_rel,
        );

        assert_eq!(
            scene.nodes[0].fill,
            Fill::None,
            "写了 a:noFill 就不该再回退到 fillRef，否则透明框会被填成蓝块"
        );
    }

    #[test]
    fn explicit_empty_effect_list_does_not_fall_back_to_theme_effect_ref() {
        // 与 `a:noFill` 同理：空的 `<a:effectLst/>` 是「不要效果」，
        // 回退的话会给已经清掉效果的形状硬加一层主题阴影。
        let scene = parse_slide(
            &n(
                r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><p:spTree>
                     <p:sp>
                       <p:nvSpPr><p:cNvPr id="2" name="无阴影形状"/></p:nvSpPr>
                       <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm>
                         <a:prstGeom prst="rect"/><a:effectLst/>
                       </p:spPr>
                       <p:style>
                         <a:lnRef idx="0"><a:schemeClr val="accent1"/></a:lnRef>
                         <a:fillRef idx="0"><a:schemeClr val="accent1"/></a:fillRef>
                         <a:effectRef idx="2"><a:schemeClr val="accent1"/></a:effectRef>
                       </p:style>
                     </p:sp>
                     <p:sp>
                       <p:nvSpPr><p:cNvPr id="3" name="对照组：没写 effectLst"/></p:nvSpPr>
                       <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm>
                         <a:prstGeom prst="rect"/>
                       </p:spPr>
                       <p:style>
                         <a:lnRef idx="0"><a:schemeClr val="accent1"/></a:lnRef>
                         <a:fillRef idx="0"><a:schemeClr val="accent1"/></a:fillRef>
                         <a:effectRef idx="2"><a:schemeClr val="accent1"/></a:effectRef>
                       </p:style>
                     </p:sp>
                   </p:spTree></p:cSld></p:sld>"#,
            ),
            &themed_inherit(),
            &no_rel,
        );

        assert!(
            scene.nodes[0].effects.outer_shadow.is_none(),
            "显式空 effectLst 表示不要效果，不该回退到 effectRef"
        );
        // 对照组：这个形状**没写** effectLst，同样一条 effectRef 就应该生效。
        // 有它才能证明上面的 None 是「短路」的结果，而不是 theme 里根本没解析出阴影。
        assert!(
            scene.nodes[1].effects.outer_shadow.is_some(),
            "没写 effectLst 时应当照旧回退到 effectRef"
        );
    }

    #[test]
    fn connector_gets_default_stroke() {
        let scene = slice(
            r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><p:spTree>
                 <p:cxnSp>
                   <p:nvCxnSpPr><p:cNvPr id="2" name="直线连接符 1"/></p:nvCxnSpPr>
                   <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="0"/></a:xfrm>
                     <a:prstGeom prst="line"/></p:spPr>
                 </p:cxnSp>
               </p:spTree></p:cSld></p:sld>"#,
        );
        assert_eq!(scene.nodes.len(), 1);
        let n = &scene.nodes[0];
        assert!(n.stroke.as_ref().is_some_and(|s| s.is_visible()), "连接线应有可见描边");
        assert_eq!(n.fill, Fill::None);
    }

    #[test]
    fn chart_frame_degrades_to_placeholder_with_text() {
        let scene = slice(
            r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"
                    xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"><p:cSld><p:spTree>
                 <p:graphicFrame>
                   <p:nvGraphicFramePr><p:cNvPr id="3" name="图表 1"/></p:nvGraphicFramePr>
                   <p:xfrm><a:off x="914400" y="914400"/><a:ext cx="3657600" cy="2743200"/></p:xfrm>
                   <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/chart">
                     <c:chart r:id="rId1"/>
                     <a:t>季度销售额</a:t>
                   </a:graphicData></a:graphic>
                 </p:graphicFrame>
               </p:spTree></p:cSld></p:sld>"#,
        );

        assert_eq!(scene.nodes.len(), 1, "图表应降级为占位而不是消失");
        let node = &scene.nodes[0];
        assert!(node.geometry.is_degraded());
        assert!(node.is_visible(), "占位矩形应有可见填充");
        // 提取到的文本不应丢失
        assert_eq!(
            node.text.as_ref().map(|t| t.plain_text()).as_deref(),
            Some("季度销售额")
        );
        assert!(!scene.warnings.is_empty());
    }

    #[test]
    fn smartart_frame_degrades_gracefully() {
        let scene = slice(
            r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><p:spTree>
                 <p:graphicFrame>
                   <p:nvGraphicFramePr><p:cNvPr id="3" name="SmartArt 1"/></p:nvGraphicFramePr>
                   <p:xfrm><a:off x="0" y="0"/><a:ext cx="3657600" cy="2743200"/></p:xfrm>
                   <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/diagram">
                     <a:t>流程步骤一</a:t><a:t>流程步骤二</a:t>
                   </a:graphicData></a:graphic>
                 </p:graphicFrame>
               </p:spTree></p:cSld></p:sld>"#,
        );
        assert_eq!(scene.nodes.len(), 1);
        let text = scene.nodes[0]
            .text
            .as_ref()
            .map(|t| t.plain_text())
            .unwrap_or_default();
        assert!(text.contains("流程步骤一"), "SmartArt 文本应被保留，实际 {text:?}");
        assert!(text.contains("流程步骤二"));
    }

    #[test]
    fn graphic_frame_without_position_is_skipped() {
        let scene = slice(
            r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><p:spTree>
                 <p:graphicFrame>
                   <p:nvGraphicFramePr><p:cNvPr id="3" name="无位置"/></p:nvGraphicFramePr>
                 </p:graphicFrame>
               </p:spTree></p:cSld></p:sld>"#,
        );
        assert!(scene.nodes.is_empty());
        assert!(!scene.warnings.is_empty());
    }

    #[test]
    fn shape_without_position_or_placeholder_is_skipped() {
        let scene = slice(
            r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><p:spTree>
                 <p:sp>
                   <p:nvSpPr><p:cNvPr id="2" name="孤儿形状"/></p:nvSpPr>
                   <p:spPr><a:prstGeom prst="rect"/></p:spPr>
                 </p:sp>
               </p:spTree></p:cSld></p:sld>"#,
        );
        assert!(scene.nodes.is_empty());
        assert!(!scene.warnings.is_empty());
    }

    #[test]
    fn hidden_shape_marked_but_still_parsed() {
        let scene = slice(
            r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><p:spTree>
                 <p:sp>
                   <p:nvSpPr><p:cNvPr id="2" name="隐藏形状" hidden="1"/></p:nvSpPr>
                   <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm>
                     <a:prstGeom prst="rect"/><a:solidFill><a:srgbClr val="FF0000"/></a:solidFill>
                   </p:spPr>
                 </p:sp>
               </p:spTree></p:cSld></p:sld>"#,
        );
        assert_eq!(scene.nodes.len(), 1);
        assert!(scene.nodes[0].hidden);
        assert!(!scene.nodes[0].is_visible(), "隐藏形状不应被渲染");
    }

    #[test]
    fn placeholder_inherits_position_from_layout() {
        let master = n(
            r#"<p:sldMaster xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><p:spTree/></p:cSld>
                 <p:txStyles>
                   <p:titleStyle><a:lvl1pPr><a:defRPr sz="4400"/></a:lvl1pPr></p:titleStyle>
                   <p:bodyStyle><a:lvl1pPr><a:defRPr sz="2800"/></a:lvl1pPr></p:bodyStyle>
                   <p:otherStyle><a:defPPr><a:defRPr sz="1800"/></a:defPPr></p:otherStyle>
                 </p:txStyles>
               </p:sldMaster>"#,
        );
        let layout = n(
            r#"<p:sldLayout xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><p:spTree>
                 <p:sp>
                   <p:nvSpPr><p:cNvPr id="1" name="标题占位符"/>
                     <p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>
                   <p:spPr><a:xfrm>
                     <a:off x="914400" y="457200"/><a:ext cx="7315200" cy="1143000"/>
                   </a:xfrm></p:spPr>
                   <p:txBody><a:bodyPr/><a:lstStyle/></p:txBody>
                 </p:sp>
               </p:spTree></p:cSld></p:sldLayout>"#,
        );

        let inh = SlideInheritance::build(
            &Theme::default(),
            Some(&master),
            Some(&layout),
            None,
            Size::new(960.0, 540.0),
            &no_rel,
        );

        // 幻灯片上的占位符没有自己的 a:xfrm，也没有字号
        let scene = parse_slide(
            &n(
                r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><p:spTree>
                     <p:sp>
                       <p:nvSpPr><p:cNvPr id="2" name="标题"/>
                         <p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>
                       <p:txBody><a:bodyPr/>
                         <a:p><a:r><a:t>继承标题</a:t></a:r></a:p>
                       </p:txBody>
                     </p:sp>
                   </p:spTree></p:cSld></p:sld>"#,
            ),
            &inh,
            &no_rel,
        );

        assert_eq!(scene.nodes.len(), 1);
        let node = &scene.nodes[0];

        // 位置继承自版式
        assert_eq!(
            node.canvas_bounds(),
            Rect::new(72.0, 36.0, 576.0, 90.0),
            "位置应从版式占位符继承"
        );

        // 字号继承自母版 titleStyle
        let tb = node.text.as_ref().unwrap();
        assert!(
            (tb.paragraphs[0].runs[0].props.size_pt - 44.0).abs() < 1e-4,
            "字号应继承母版标题样式，实际 {}",
            tb.paragraphs[0].runs[0].props.size_pt
        );

        // 标题占位符的垂直锚点应变为居中
        assert_eq!(tb.body.anchor, VerticalAnchor::Middle);
    }

    #[test]
    fn notes_extraction_from_body_placeholder() {
        let notes = n(
            r#"<p:notes xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><p:spTree>
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
                     <a:p><a:r><a:t>这里讲重点：光合作用的两个阶段</a:t></a:r></a:p>
                     <a:p><a:r><a:t>提问：为什么晚上不能光合作用？</a:t></a:r></a:p>
                   </p:txBody>
                 </p:sp>
               </p:spTree></p:cSld></p:notes>"#,
        );
        let text = parse_notes(&notes).expect("应提取到备注");
        assert!(text.contains("两个阶段"));
        assert!(text.contains("提问"));
    }

    #[test]
    fn notes_without_body_placeholder_is_none() {
        let notes = n(
            r#"<p:notes xmlns:p="urn:p"><p:cSld><p:spTree>
                 <p:sp><p:nvSpPr><p:cNvPr id="2" name="x"/></p:nvSpPr></p:sp>
               </p:spTree></p:cSld></p:notes>"#,
        );
        assert!(parse_notes(&notes).is_none());
    }

    #[test]
    fn background_falls_back_to_white() {
        let scene = slice(
            r#"<p:sld xmlns:p="urn:p"><p:cSld><p:spTree/></p:cSld></p:sld>"#,
        );
        assert_eq!(
            scene.background,
            ppt_core::scene::SceneBackground::Solid(Color::WHITE)
        );
    }

    #[test]
    fn slide_background_overrides_inherited() {
        let scene = slice(
            r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld>
                 <p:bg><p:bgPr><a:solidFill><a:srgbClr val="123456"/></a:solidFill></p:bgPr></p:bg>
                 <p:spTree/></p:cSld></p:sld>"#,
        );
        assert_eq!(
            scene.background,
            ppt_core::scene::SceneBackground::Fill(Fill::Solid(Color::rgb(
                0x12, 0x34, 0x56
            )))
        );
    }

    #[test]
    fn empty_sp_tree_yields_warning_not_panic() {
        let scene = slice(r#"<p:sld xmlns:p="urn:p"><p:cSld/></p:sld>"#);
        assert!(scene.nodes.is_empty());
        assert!(scene.warnings.iter().any(|w| w.contains("spTree")));
    }

    #[test]
    fn shape_hyperlink_parsed() {
        let resolve = |id: &str| {
            if id == "rId9" {
                Some("https://example.com/lesson".to_string())
            } else {
                None
            }
        };
        let inh = inherit();
        let scene = parse_slide(
            &n(
                r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a" xmlns:r="urn:r"><p:cSld><p:spTree>
                     <p:sp>
                       <p:nvSpPr><p:cNvPr id="2" name="按钮">
                         <a:hlinkClick r:id="rId9"/></p:cNvPr></p:nvSpPr>
                       <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm>
                         <a:prstGeom prst="rect"/><a:solidFill><a:srgbClr val="FF0000"/></a:solidFill>
                       </p:spPr>
                     </p:sp>
                   </p:spTree></p:cSld></p:sld>"#,
            ),
            &inh,
            &resolve,
        );

        let link = scene.nodes[0].hyperlink.as_ref().expect("应解析出形状超链接");
        assert_eq!(
            link.target,
            ppt_core::scene::HyperlinkTarget::Url("https://example.com/lesson".to_string())
        );
        // 链接热区应可用
        let spots = scene.link_hotspots();
        assert_eq!(spots.len(), 1);
    }

    #[test]
    fn many_shapes_all_parsed() {
        let mut xml_text = String::from(
            r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><p:spTree>"#,
        );
        for i in 0..50 {
            xml_text.push_str(&format!(
                r#"<p:sp>
                     <p:nvSpPr><p:cNvPr id="{i}" name="形状{i}"/></p:nvSpPr>
                     <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm>
                       <a:prstGeom prst="rect"/><a:solidFill><a:srgbClr val="FF0000"/></a:solidFill>
                     </p:spPr>
                   </p:sp>"#
            ));
        }
        xml_text.push_str("</p:spTree></p:cSld></p:sld>");

        let scene = slice(&xml_text);
        assert_eq!(scene.nodes.len(), 50);
        assert!(scene.warnings.is_empty());
    }

    #[test]
    fn node_ids_are_unique() {
        let scene = slice(
            r#"<p:sld xmlns:p="urn:p" xmlns:a="urn:a"><p:cSld><p:spTree>
                 <p:sp>
                   <p:nvSpPr><p:cNvPr id="1" name="同名形状"/></p:nvSpPr>
                   <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm>
                     <a:prstGeom prst="rect"/><a:solidFill><a:srgbClr val="FF0000"/></a:solidFill>
                   </p:spPr>
                 </p:sp>
                 <p:sp>
                   <p:nvSpPr><p:cNvPr id="2" name="同名形状"/></p:nvSpPr>
                   <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm>
                     <a:prstGeom prst="rect"/><a:solidFill><a:srgbClr val="00FF00"/></a:solidFill>
                   </p:spPr>
                 </p:sp>
               </p:spTree></p:cSld></p:sld>"#,
        );
        assert_ne!(scene.nodes[0].id, scene.nodes[1].id, "节点 id 必须唯一");
    }
}
