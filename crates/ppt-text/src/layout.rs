//! 文本排版：把 [`TextBox`] 排成可绘制的定位字形序列。
//!
//! # 流程
//!
//! ```text
//! TextBox
//!   └─ 逐段落：
//!        ① 拼接段落文本（run 之间无分隔）
//!        ② 按字体覆盖把文本切成 styled chunk（逐字符回退）
//!        ③ 用 MonotoneCharacters 整形一次，得到「字形 + 累计宽度」
//!        ④ 用 UAX#14 + 中文禁则求断点，按可用宽度折行
//!        ⑤ 每行按 chunk 切片取字形，做水平对齐与基线定位
//!   └─ 汇总为 TextLayout
//! ```
//!
//! # 两个关键决策
//!
//! **① 整形用 `BufferClusterLevel::MonotoneCharacters`。**
//! 默认的 grapheme 级簇在断行时无法把「一个簇」拆到两行，而中文按字断行
//! 需要逐字符切分；同时字符级簇保证 `cluster == 该字符的字节偏移`，
//! 于是「按字节范围取字形」变成一次二分查找，折行时无需重复整形。
//! 代价是失去拉丁连字（fi/fl），在课件场景可忽略。
//!
//! **② 每个 chunk 只整形一次。**
//! 折行需要反复测量前缀宽度。若每次测量都重新整形，一个 200 字的段落
//! 会产生上百次整形调用。改为整形一次并预计算累计宽度后，
//! 任意前缀宽度都是 O(log n) 的二分。
//!
//! # 与解析层的契约
//!
//! [`RunProps`] 的字段是**具体值而非 Option**，因此「未设置」与
//! 「显式设为默认值」无法区分。这要求解析层在构造 [`TextRun`] 时
//! 已经从段落默认属性（`a:defRPr`）、版式与母版继承链中解析完毕，
//! 交给排版引擎的是**最终值**。排版引擎不再做属性继承。

use std::sync::Arc;

use ppt_core::scene::{
    Bullet, BulletKind, Color, FontSet, LineSpacing, Paragraph, RunProps, Size, StrikeStyle,
    TabAlign, TextAlign, TextBox, TextDirection, TextWrap, UnderlineStyle, VerticalAnchor,
};

use crate::fonts::{FontContext, FontId, LoadedFont};
use crate::linebreak;

/// 上下标的字形缩放。
///
/// PowerPoint 用「偏移 + 缩小」表现上下标，偏移量写在 `a:rPr/@baseline` 里，
/// 缩小比例则是渲染约定（规范没写死）。0.65 与 Office 的观感一致：
/// 上标看得清，又不至于顶到上一行。
const SUBSCRIPT_SCALE: f32 = 0.65;

/// 排版参数。
#[derive(Debug, Clone, Copy)]
pub struct LayoutOptions {
    /// 整体字号缩放（由 `normAutofit` 求解得出，或调用方直接指定）。
    pub font_scale: f32,
    /// 是否求解 `normAutofit`（当课件未预置 `fontScale` 时）。
    ///
    /// 求解需要多次试排，故缩略图等对精度不敏感的场景可关闭。
    pub solve_autofit: bool,
    /// 行数上限，防止异常课件产生海量排版结果。
    pub max_lines: usize,
    /// 动画播放状态（位掩码，见 [`ppt_core::scene::PlayState`]）。
    ///
    /// 没出场的段落**照样占位**、只是不产生字形 —— 这与 PowerPoint 一致：
    /// 逐条弹出的项目符号不会把后面的内容往上顶。
    pub reveal: ppt_core::scene::PlayState,
    /// 只给第 `st..=end` 段产生字形（`None` = 全部）。
    ///
    /// 动画可以只作用在文本框的**某一段**上（`p:txEl/p:pRg`），
    /// 渲染「这一步的图层」时要能把其它段落摘掉，否则整框一起飞。
    pub paragraph_only: Option<(u32, u32)>,
}

impl Default for LayoutOptions {
    fn default() -> Self {
        LayoutOptions {
            font_scale: 1.0,
            solve_autofit: true,
            max_lines: 4096,
            reveal: ppt_core::scene::PLAY_ALL,
            paragraph_only: None,
        }
    }
}

/// 一个已定位的字形。
#[derive(Debug, Clone)]
pub struct PositionedGlyph {
    pub glyph_id: u16,
    /// 所属字体。渲染器据此取字形轮廓。
    pub font: Arc<LoadedFont>,
    /// 相对文本区左边缘的 x。
    pub x: f32,
    /// 相对基线的 y（负值向上）。
    pub y: f32,
    /// 该字形的绘制字号（已含 `font_scale`）。
    pub size_pt: f32,
    pub color: Color,
    /// 前进宽度（用于下划线、删除线、选中框等的横向范围）。
    pub advance: f32,
    pub style: GlyphStyle,
    /// 该字形在原始 run 中的字节偏移（用于命中测试与诊断）。
    pub cluster: u32,
    /// 所属 run 在段落中的索引。
    pub run_index: usize,
}

/// 字形的装饰样式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GlyphStyle {
    pub underline: UnderlineStyle,
    pub strike: StrikeStyle,
    /// 字体本身没有粗体字重，需要「描边加粗」来模拟。
    pub synthetic_bold: bool,
    /// 字体本身没有斜体，需要「倾斜变换」来模拟。
    pub synthetic_italic: bool,
}

impl GlyphStyle {
    #[inline]
    pub fn is_plain(&self) -> bool {
        self.underline == UnderlineStyle::None
            && self.strike == StrikeStyle::None
            && !self.synthetic_bold
            && !self.synthetic_italic
    }
}

/// 一行已排版文本。
#[derive(Debug, Clone, Default)]
pub struct LaidOutLine {
    pub glyphs: Vec<PositionedGlyph>,
    /// 基线相对文本区顶部的 y（已含垂直锚点偏移）。
    pub baseline: f32,
    /// 行顶相对文本区顶部的 y。
    pub top: f32,
    pub height: f32,
    /// 该行的实际宽度（对齐前）。
    pub width: f32,
    /// 所属段落索引。
    pub paragraph: usize,
    /// 该行是否为段落的最后一行（影响两端对齐与悬挂缩进）。
    pub last_in_paragraph: bool,
    /// 该行是否由 `a:br` 强制换行产生。
    pub hard_break: bool,
    /// 行文本内容（用于提取、搜索与调试）。
    pub text: String,
    /// 该行是否有超宽且无法断行的内容（渲染器可据此提示）。
    pub overflowed: bool,
}

/// 排版结果。
#[derive(Debug, Clone, Default)]
pub struct TextLayout {
    pub lines: Vec<LaidOutLine>,
    /// 所有行的最大宽度。
    pub width: f32,
    /// 内容块高度（**不含**垂直锚点偏移）。
    ///
    /// 各行 `top`/`baseline` 已经加上了锚点偏移，因此
    /// 「最后一行底边」应通过 [`TextLayout::content_bottom`] 获取，
    /// 而不是 `height`。
    pub height: f32,
    /// 是否超出给定的可用高度。
    pub overflow: bool,
    /// 实际使用的字号缩放。
    pub font_scale: f32,
    /// 排版过程中产生的告警（如缺字、无法断行）。
    pub warnings: Vec<String>,
}

impl TextLayout {
    /// 内容块的底边（已含垂直锚点偏移）。
    pub fn content_bottom(&self) -> f32 {
        self.lines
            .last()
            .map(|l| l.top + l.height)
            .unwrap_or(self.height)
    }

    /// 内容块的顶边（已含垂直锚点偏移）。
    pub fn content_top(&self) -> f32 {
        self.lines.first().map(|l| l.top).unwrap_or(0.0)
    }

    /// 遍历所有字形。
    pub fn glyphs(&self) -> impl Iterator<Item = &PositionedGlyph> {
        self.lines.iter().flat_map(|l| l.glyphs.iter())
    }

    /// 纯文本（按行拼接，便于调试与文本提取）。
    pub fn plain_text(&self) -> String {
        self.lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    #[inline]
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }
}

/// 排版引擎。
pub struct TextLayouter<'a> {
    fonts: &'a FontContext,
}

impl<'a> TextLayouter<'a> {
    pub fn new(fonts: &'a FontContext) -> TextLayouter<'a> {
        TextLayouter { fonts }
    }

    /// 在给定区域内排版文本框。
    ///
    /// `area` 是**已扣除内边距**的可用文本区尺寸。
    pub fn layout(&self, text: &TextBox, area: Size, opts: LayoutOptions) -> TextLayout {
        let body = &text.body;

        // 竖排走独立路径：字形逐个纵向堆叠，规则与横排差异大
        if matches!(
            body.direction,
            TextDirection::Stacked | TextDirection::StackedRightToLeft
        ) {
            return self.layout_vertical(text, area, opts);
        }

        // 求解自动缩放：二分找最大的、能让文本不溢出的缩放比
        let scale = if opts.solve_autofit && body.auto_fit.needs_solving() {
            self.solve_autofit(text, area, opts)
        } else {
            opts.font_scale * body.auto_fit.font_scale()
        };

        let mut layout = self.layout_at_scale(text, area, scale, opts);

        // 按垂直锚点整体下移
        let offset = match body.anchor {
            VerticalAnchor::Top => 0.0,
            VerticalAnchor::Middle => ((area.h - layout.height) / 2.0).max(0.0),
            VerticalAnchor::Bottom => (area.h - layout.height).max(0.0),
        };
        if offset > 0.0 {
            for line in &mut layout.lines {
                line.top += offset;
                line.baseline += offset;
            }
        }

        layout
    }

    /// 在固定缩放比下排版一次。
    fn layout_at_scale(
        &self,
        text: &TextBox,
        area: Size,
        scale: f32,
        opts: LayoutOptions,
    ) -> TextLayout {
        let scale = scale.clamp(0.05, 8.0);
        let mut out = TextLayout {
            font_scale: scale,
            ..Default::default()
        };

        // `wrap="none"`：这一框不换行。
        //
        // PowerPoint 里这类形状（课件里常见的「答案词」小块）宽度是随文字
        // 自动长出来的，文件里记的就是长好之后的宽度。若不认这个属性照常换行，
        // 「while」会被拆成「whil / e」，一行变两行、整块高度翻倍。
        let avail_w = if matches!(text.body.wrap, TextWrap::None) {
            // 给一个「大到不会触发换行」的实数，避免 Inf 参与后续运算
            1_000_000.0
        } else {
            area.w.max(0.0)
        };
        let mut cursor_y = 0.0f32;

        for (pi, para) in text.paragraphs.iter().enumerate() {
            if out.lines.len() >= opts.max_lines {
                out.warnings
                    .push(format!("段落数超出上限（{} 行），已截断", opts.max_lines));
                break;
            }

            // 段前距
            cursor_y += para.space_before_pt * scale;

            let para_lines = self.layout_paragraph(para, pi, avail_w, scale, &mut out.warnings);

            let base_line_height = para_lines.base_line_height;
            let line_gap_extra = self.line_gap_extra(para, base_line_height, scale);
            // 行距撑高的那部分，文字上下各分一半 —— PowerPoint 的比例行距
            // 是把整行撑高、文字在行框里垂直居中。全塞在文字下方的话，
            // 150% 行距的一行会整整低 7pt：课件里那种「答案词压在下划线上」的
            // 小方块（位置是作者在 PowerPoint 里对着调好的）就会串到线下面去。
            // 收紧行距（`spcPct` < 100%）时不给负值：文字仍贴着行框顶部，
            // 否则第一行会顶出框外。
            let half_leading = (line_gap_extra * 0.5).max(0.0);

            for (li, mut line) in para_lines.lines.into_iter().enumerate() {
                let line_h = base_line_height + line_gap_extra;
                line.top = cursor_y;
                line.baseline =
                    cursor_y + half_leading + base_line_height * self.baseline_ratio(para);
                line.height = line_h;
                cursor_y += line_h;

                // 还没出场的段落：**占位但不画**。
                // 保留行高是必须的 —— 若让后面的段落顶上来，
                // 逐条弹出的项目符号就会一路往上窜，与课件里看到的不一样。
                if !para.build.visible_in(opts.reveal) {
                    line.glyphs.clear();
                }

                // 只要指定段落的字形（渲染「单段动画图层」时用）
                if let Some((st, end)) = opts.paragraph_only {
                    let pi = pi as u32;
                    if pi < st || pi > end {
                        line.glyphs.clear();
                    }
                }

                out.width = out.width.max(line.width);
                out.lines.push(line);

                if out.lines.len() >= opts.max_lines {
                    break;
                }
                let _ = li;
            }

            // 段后距
            cursor_y += para.space_after_pt * scale;
        }

        out.height = cursor_y;
        out.overflow = out.height > area.h + 0.5;
        out
    }

    /// 行距带来的额外高度。
    ///
    /// `a:spcPct` 是**乘数**，收紧时照样生效（返回负值）。
    /// 以前只认「撑高」不认「收紧」，小于 100% 一律按 100% 算 ——
    /// 而课件里 95%、80% 的行距相当常见，于是每一行都多出 5%~20%，
    /// 十几行下来就顶出文本框、溢到页面外去。
    fn line_gap_extra(&self, para: &Paragraph, base: f32, scale: f32) -> f32 {
        match para.line_spacing {
            // 百分比行距：在基准行高之上的差额（收紧时为负）
            //
            // 这里**不再乘 `scale`**：`base` 已经含了字体缩放，
            // 再乘一次会把已经缩小的字号又压一遍行距。
            LineSpacing::Percent(p) => base * (p - 1.0),
            // 固定磅值行距（`a:spcPts`）：一行就是这么多磅，多退少补。
            // 可能比字体自己的行高小 —— PowerPoint 允许这样压紧，
            // 压过头文字会互相叠，那是课件作者自己的选择，照做即可。
            LineSpacing::Points(pt) => pt * scale - base,
        }
    }

    /// 基线在行高内的比例。
    ///
    /// 用「上升部占 (上升部 + 下降部) 的比例」近似，
    /// 大多数拉丁与 CJK 字体落在 0.8 附近。
    fn baseline_ratio(&self, para: &Paragraph) -> f32 {
        let _ = para;
        0.8
    }

    /// 求解 `normAutofit`：二分最大可用缩放。
    fn solve_autofit(&self, text: &TextBox, area: Size, opts: LayoutOptions) -> f32 {
        let mut lo = 0.25f32;
        let mut hi = 1.0f32;

        // 先看 1.0 是否已经能放下，这是最常见的情况，避免无谓迭代
        let probe = self.layout_at_scale(
            text,
            area,
            hi,
            LayoutOptions {
                solve_autofit: false,
                ..opts
            },
        );
        if !probe.overflow {
            return hi;
        }

        // 二分 12 次，精度约 0.02%，远超人眼可辨
        for _ in 0..12 {
            let mid = (lo + hi) / 2.0;
            let r = self.layout_at_scale(
                text,
                area,
                mid,
                LayoutOptions {
                    solve_autofit: false,
                    ..opts
                },
            );
            if r.overflow {
                hi = mid;
            } else {
                lo = mid;
            }
        }

        // 留一点余量，避免因浮点误差在渲染时又溢出
        (lo * 0.995).clamp(0.25, 1.0)
    }

    /// 竖排排版。
    ///
    /// 覆盖 `vert="eaVert"` 场景：字形不旋转，逐字自上而下堆叠，
    /// 到达底边后另起一列。中文竖排标题常见此形态。
    fn layout_vertical(&self, text: &TextBox, area: Size, opts: LayoutOptions) -> TextLayout {
        let scale = opts.font_scale * text.body.auto_fit.font_scale();
        let mut out = TextLayout {
            font_scale: scale,
            ..Default::default()
        };

        let mut col_x = 0.0f32;
        let mut col_width = 0.0f32;

        for (pi, para) in text.paragraphs.iter().enumerate() {
            let para_text: String = para.runs.iter().map(|r| r.text.as_str()).collect();
            let chunks = self.build_chunks(para, &para_text);
            let metrics = self.paragraph_metrics(para, &chunks, scale);
            let col_w = metrics.max_size * 1.2;
            col_width = col_width.max(col_w);
            let mut cursor_y = 0.0f32;

            for run in &para.runs {
                for ch in run.text.chars() {
                    if ch == '\n' {
                        col_x += col_w;
                        cursor_y = 0.0;
                        continue;
                    }
                    if cursor_y + metrics.line_height > area.h {
                        col_x += col_w;
                        cursor_y = 0.0;
                    }
                    let Some(font_id) = self.fonts.font_for_char_in_set(
                        &run.props.font,
                        run.props.bold,
                        run.props.italic,
                        run.props.language.as_deref(),
                        ch,
                    ) else {
                        continue;
                    };
                    let Some(font) = self.fonts.font(font_id).cloned() else {
                        continue;
                    };
                    let size = run.props.size_pt * scale;
                    let glyphs = self.shape_text(&font, &ch.to_string(), size);
                    for g in glyphs {
                        out.lines.push(LaidOutLine {
                            glyphs: vec![PositionedGlyph {
                                glyph_id: g.glyph_id,
                                font: Arc::clone(&font),
                                x: col_x,
                                y: 0.0,
                                size_pt: size,
                                color: run.props.color.unwrap_or(Color::BLACK),
                                advance: g.advance,
                                style: self.glyph_style(&font, &run.props),
                                cluster: g.cluster,
                                run_index: 0,
                            }],
                            baseline: cursor_y + size,
                            top: cursor_y,
                            height: metrics.line_height,
                            width: col_w,
                            paragraph: pi,
                            last_in_paragraph: false,
                            hard_break: false,
                            text: ch.to_string(),
                            overflowed: false,
                        });
                    }
                    cursor_y += size * 1.05;
                }
            }
        }

        out.width = col_x + col_width;
        out.height = area.h;
        out
    }

    /// 排版单个段落。
    fn layout_paragraph(
        &self,
        para: &Paragraph,
        para_index: usize,
        avail_w: f32,
        scale: f32,
        warnings: &mut Vec<String>,
    ) -> ParagraphLayout {
        // 先拼接段落文本，再据此切 chunk —— chunk 的字节区间必须与段落文本同坐标系
        let mut text = String::new();
        let mut run_ranges: Vec<(usize, usize, usize)> = Vec::new();
        for (ri, run) in para.runs.iter().enumerate() {
            let start = text.len();
            text.push_str(&run.text);
            run_ranges.push((start, text.len(), ri));
        }

        let chunks = self.build_chunks(para, &text);
        let metrics = self.paragraph_metrics(para, &chunks, scale);

        let mut result = ParagraphLayout {
            lines: Vec::new(),
            base_line_height: metrics.line_height,
        };

        if text.is_empty() {
            // 空段落仍占一行（OOXML 中空段落是有高度的）
            result.lines.push(LaidOutLine {
                height: metrics.line_height,
                paragraph: para_index,
                last_in_paragraph: true,
                ..Default::default()
            });
            return result;
        }

        // 整形所有 chunk（每个只做一次）
        let shaped: Vec<ShapedChunk> = chunks
            .into_iter()
            .map(|c| self.shape_chunk(c, scale))
            .collect();

        // `a:pPr/@marL` 是**正文列**的左边界；`@indent` 是首行相对它的偏移，
        // 负数就是悬挂缩进（首行向左突出）。
        //
        // 有项目符号时，符号的右边缘对齐正文列（见 [`Self::build_bullet`]），
        // 所以首行正文也从正文列开始；没有符号时，首行才按 `@indent` 左移。
        //
        // 这两者都不该再额外扣一遍 `|indent|`。曾经把它当成「符号占的宽度」
        // 从可用宽度里又减了一次，悬挂缩进的段落凭空少掉 2×|indent|：
        // 《Unit 2》第 19 页那句「十位数11-19」本来一行放得下，
        // 被硬挤成两行，第二行「11-19」直接压到下面的表格上。
        let bullet = para.bullet.as_ref().filter(|b| b.is_visible());
        let hanging = para.indent_pt * scale;
        let mut text_column = (para.margin_left_pt * scale).max(0.0);

        // marL 与 indent 都是 0 却有符号时，给符号让出一块默认宽度，
        // 否则符号会和正文叠在一起
        if bullet.is_some() && text_column + hanging <= 0.0 {
            text_column = metrics.max_size * 1.5;
        }

        let first_line_start = if bullet.is_some() {
            text_column
        } else {
            (text_column + hanging).max(0.0)
        };

        let ops = linebreak::break_opportunities(&text);
        let ranges = self.wrap(
            &text,
            &ops,
            &shaped,
            (avail_w - text_column).max(1.0),
            warnings,
        );

        let line_count = ranges.len();
        for (li, (start, end)) in ranges.into_iter().enumerate() {
            let is_last = li + 1 == line_count;

            // 悬挂缩进只作用于段落首行
            let line_indent = if li == 0 { first_line_start } else { text_column };

            let mut line = self.build_line(
                &text,
                start,
                end,
                &shaped,
                &run_ranges,
                para,
                para_index,
                line_indent,
                text_column,
                metrics.line_height,
                scale,
            );
            line.last_in_paragraph = is_last;

            // 项目符号只画在段落首行
            if li == 0 {
                if let Some(b) = bullet {
                    let bullet_glyphs =
                        self.build_bullet(b, para, text_column, metrics, scale);
                    line.glyphs.splice(0..0, bullet_glyphs);
                }
            }

            self.align_line(&mut line, para, avail_w, is_last, first_line_start);

            result.lines.push(line);
        }

        result
    }

    /// 按可用宽度折行，返回每行的字节区间。
    ///
    /// 换行符（`a:br` 产生的 `\n`）本身不属于任何一行：
    /// 它只负责把文本切开，因此这里先把段落按强制断点切成若干硬段，
    /// 再在每个硬段内做软折行。
    fn wrap(
        &self,
        text: &str,
        ops: &[linebreak::BreakOpportunity],
        shaped: &[ShapedChunk],
        avail_w: f32,
        warnings: &mut Vec<String>,
    ) -> Vec<(usize, usize)> {
        // 硬段：不含换行符的 (start, end)
        let mut segments: Vec<(usize, usize)> = Vec::new();
        let mut seg_start = 0usize;
        for o in ops.iter().filter(|o| o.mandatory) {
            // unicode-linebreak 给出的偏移在换行符之后，回退定位换行符本身
            let nl = text[..o.offset].rfind('\n').unwrap_or(o.offset);
            segments.push((seg_start, nl));
            seg_start = o.offset;
        }
        segments.push((seg_start, text.len()));

        let mut lines: Vec<(usize, usize)> = Vec::new();

        // 软断点按绝对偏移收集，随消费推进以保持 O(n)
        let mut soft: Vec<usize> = ops
            .iter()
            .filter(|o| !o.mandatory)
            .map(|o| o.offset)
            .collect();

        for (seg_start, seg_end) in segments {
            if seg_start >= seg_end {
                // 空硬段代表一个空行（`a:br` 后无内容），仍需占一行
                lines.push((seg_start, seg_end));
                continue;
            }

            let seg_soft: Vec<usize> = {
                let i = soft.partition_point(|&o| o < seg_start);
                let j = soft.partition_point(|&o| o < seg_end);
                let v = soft[i..j].to_vec();
                soft.drain(i..j);
                v
            };

            let mut cursor = seg_start;
            while cursor < seg_end {
                // 末行末尾的空白不参与宽度判断：`after   before   ` 这种
                // 靠空格对齐的词表，尾巴上挂着的空白不该把下一行顶出去
                let w = self.measure_range_trimmed(text, shaped, cursor, seg_end);
                if w <= avail_w {
                    lines.push((cursor, seg_end));
                    break;
                }

                // 在剩余宽度内找最后一个软断点
                let limit = self.fit_offset(text, shaped, cursor, seg_end, avail_w);
                let candidate = seg_soft
                    .iter()
                    .copied()
                    .rfind(|&o| o > cursor && o <= cursor + limit);

                match candidate {
                    Some(brk) => {
                        lines.push((cursor, brk));
                        cursor = brk;
                    }
                    None => {
                        // 限制内无断点：说明遇到超长不可断片段（长英文单词/URL）。
                        // 硬切到能放下的位置，保证不溢出；同时记录告警。
                        let cut = self.char_boundary_at(text, cursor + limit.max(1));
                        if cut <= cursor {
                            // 连一个字符都放不下：放行整段，避免死循环
                            lines.push((cursor, seg_end));
                            warnings.push("文本行宽不足，已允许溢出".to_string());
                            break;
                        }
                        warnings.push(format!(
                            "「{}」过长无法断行，已按字符切分",
                            &text[cursor..cut.min(cursor + 24)]
                        ));
                        lines.push((cursor, cut));
                        cursor = cut;
                    }
                }
            }
        }

        if lines.is_empty() {
            lines.push((0, text.len()));
        }
        lines
    }

    /// 测量文本字节区间 `[start, end)` 的总宽度。
    fn measure_range(&self, shaped: &[ShapedChunk], start: usize, end: usize) -> f32 {
        let mut w = 0.0;
        for sc in shaped {
            let (lo, hi) = (start.max(sc.start), end.min(sc.end));
            if lo >= hi {
                continue;
            }
            w += sc.width_between(lo - sc.start, hi - sc.start);
        }
        w
    }

    /// 同 [`Self::measure_range`]，但**不计行尾空白**。
    ///
    /// 行尾的空格不占宽度 —— PowerPoint、浏览器、Word 都是这么算的。
    /// 课件里爱用空格把词对齐（`after      although` 这种），一行末尾常挂着
    /// 几段空白；照着原样量，就会在还放得下的时候提前换行，
    /// 一段本该一行的文字被挤成两行，整块随即长高、顶出画面。
    fn measure_range_trimmed(
        &self,
        text: &str,
        shaped: &[ShapedChunk],
        start: usize,
        end: usize,
    ) -> f32 {
        let mut e = end.min(text.len());
        // `end` 是二分探针给的字节偏移，可能落在字符中间，先退回边界
        while e > start && !text.is_char_boundary(e) {
            e -= 1;
        }
        while e > start {
            let ch = match text[..e].chars().next_back() {
                Some(c) => c,
                None => break,
            };
            if !ch.is_whitespace() {
                break;
            }
            e -= ch.len_utf8();
        }
        // 全是空白：宽度算 0，这一行仍然要占位（由调用方决定）
        self.measure_range(shaped, start, e.max(start))
    }

    /// 在 `[start, end)` 内，求「宽度不超过 `avail` 的最大字节数」。
    ///
    /// 用二分而不是逐字符累加：一次折行最多 12 次测量，而不是 N 次。
    fn fit_offset(
        &self,
        text: &str,
        shaped: &[ShapedChunk],
        start: usize,
        end: usize,
        avail: f32,
    ) -> usize {
        let mut lo = 0usize;
        let mut hi = end - start;
        while lo < hi {
            // 上取整的中点：保证 `lo = mid` 时区间一定收缩
            let mid = (lo + hi).div_ceil(2);
            // mid 必须落在字符边界上，否则测量会跨半个字符
            let probe = start + mid;
            let w = self.measure_range_trimmed(text, shaped, start, probe);
            if w <= avail {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        lo
    }

    /// 把字节偏移向下取整到字符边界。
    fn char_boundary_at(&self, text: &str, offset: usize) -> usize {
        let mut o = offset.min(text.len());
        while o > 0 && !text.is_char_boundary(o) {
            o -= 1;
        }
        o
    }

    /// 构造一行：切取字形、定位、处理制表位。
    #[allow(clippy::too_many_arguments)]
    fn build_line(
        &self,
        text: &str,
        start: usize,
        end: usize,
        shaped: &[ShapedChunk],
        run_ranges: &[(usize, usize, usize)],
        para: &Paragraph,
        para_index: usize,
        line_indent: f32,
        _bullet_width: f32,
        _line_height: f32,
        _scale: f32,
    ) -> LaidOutLine {
        let mut glyphs = Vec::new();
        let mut x = line_indent;
        let mut overflowed = false;

        for sc in shaped {
            let (lo, hi) = (start.max(sc.start), end.min(sc.end));
            if lo >= hi {
                continue;
            }
            let gi0 = sc.glyph_index_at(lo - sc.start);
            let gi1 = sc.glyph_index_at(hi - sc.start);

            for g in &sc.glyphs[gi0..gi1] {
                // 制表符：跳到下一个制表位，不绘制字形
                if let Some('\t') = sc.char_at(g.cluster as usize) {
                    x = next_tab_stop(x, &para.tab_stops);
                    continue;
                }
                let run_index = run_ranges
                    .iter()
                    .find(|(s, e, _)| g.cluster as usize >= *s && (g.cluster as usize) < *e)
                    .map(|(_, _, ri)| *ri)
                    .unwrap_or(0);

                glyphs.push(PositionedGlyph {
                    glyph_id: g.glyph_id,
                    font: Arc::clone(&sc.font),
                    x: x + g.x_offset,
                    // 上下标：`a:rPr/@baseline` 是相对字号的百分比，
                    // 正数上移，而 y 轴向下，所以要取负
                    y: g.y_offset + sc.baseline_shift,
                    size_pt: sc.size_pt,
                    color: sc.color,
                    advance: g.advance,
                    style: sc.style,
                    cluster: g.cluster,
                    run_index,
                });
                x += g.advance;
            }
        }

        let width = x - line_indent;
        if width > 1e9 {
            overflowed = true;
        }

        LaidOutLine {
            glyphs,
            baseline: 0.0,
            top: 0.0,
            height: 0.0,
            width,
            paragraph: para_index,
            last_in_paragraph: false,
            hard_break: false,
            text: text[start..end].to_string(),
            overflowed,
        }
    }

    /// 应用水平对齐。
    fn align_line(
        &self,
        line: &mut LaidOutLine,
        para: &Paragraph,
        avail_w: f32,
        is_last: bool,
        left_reserve: f32,
    ) {
        if line.glyphs.is_empty() {
            return;
        }
        let content_w = line.width;
        let free = avail_w - left_reserve - content_w;

        match para.align {
            TextAlign::Left => {}
            TextAlign::Center => {
                if free > 0.0 {
                    shift_glyphs(&mut line.glyphs, free / 2.0);
                }
            }
            TextAlign::Right => {
                if free > 0.0 {
                    shift_glyphs(&mut line.glyphs, free);
                }
            }
            TextAlign::Justify => {
                // 最后一行按左对齐处理，这是排版惯例
                if !is_last && free > 0.5 && line.glyphs.len() > 1 {
                    distribute(&mut line.glyphs, free);
                    line.width = content_w + free;
                }
            }
            TextAlign::JustifyAll | TextAlign::Distributed | TextAlign::ThaiDistributed => {
                if free > 0.5 && line.glyphs.len() > 1 {
                    distribute(&mut line.glyphs, free);
                    line.width = content_w + free;
                }
            }
        }
    }

    /// 构造项目符号的字形。
    fn build_bullet(
        &self,
        bullet: &Bullet,
        para: &Paragraph,
        text_column: f32,
        metrics: ParagraphMetrics,
        scale: f32,
    ) -> Vec<PositionedGlyph> {
        let text = match &bullet.kind {
            BulletKind::None => return Vec::new(),
            BulletKind::Char(c) => c.clone(),
            BulletKind::AutoNumber => {
                let n = bullet.start_at.unwrap_or(1);
                format_auto_number(n, bullet.number_type.as_deref())
            }
        };
        if text.is_empty() {
            return Vec::new();
        }

        // 符号字号默认与段落最大字号一致，可由 buSzPct 缩放
        let size = metrics.max_size * bullet.size_pct.unwrap_or(1.0).clamp(0.1, 4.0) * scale;
        let color = bullet.color.or_else(|| {
            para.runs
                .first()
                .and_then(|r| r.props.color)
        }).unwrap_or(Color::BLACK);

        // 符号字体：优先 buFont，其次与正文一致
        let font_set = match &bullet.font {
            Some(f) => FontSet::from_latin(f.clone()),
            None => para
                .runs
                .first()
                .map(|r| r.props.font.clone())
                .unwrap_or_default(),
        };

        let Some(font_id) = self.fonts.font_for_char_in_set(
            &font_set,
            false,
            false,
            para.runs.first().and_then(|r| r.props.language.as_deref()),
            text.chars().next().unwrap_or('•'),
        ) else {
            return Vec::new();
        };
        let Some(font) = self.fonts.font(font_id).cloned() else {
            return Vec::new();
        };

        let shaped = self.shape_text(&font, &text, size);
        let total: f32 = shaped.iter().map(|g| g.advance).sum();

        // 符号右边缘对齐正文列的起点（悬挂缩进的标准行为）
        let right_edge = text_column.max(0.0);
        let mut x = (right_edge - total).max(0.0);

        let mut out = Vec::with_capacity(shaped.len());
        for g in shaped {
            out.push(PositionedGlyph {
                glyph_id: g.glyph_id,
                font: Arc::clone(&font),
                x,
                y: 0.0,
                size_pt: size,
                color,
                advance: g.advance,
                style: GlyphStyle::default(),
                cluster: g.cluster,
                run_index: 0,
            });
            x += g.advance;
        }
        out
    }

    /// 把段落按「字体 + 属性」切成 chunk。
    ///
    /// 同一 run 内若出现当前字体不覆盖的字符，就切出一个新 chunk
    /// 并换用回退字体 —— 这是避免豆腐块的关键步骤。
    ///
    /// `para_text` 是各 run 文本按序拼接的结果，chunk 的 `text` 直接从它切片，
    /// 这样 chunk 的字节区间与段落文本坐标系统一，断行时可直接比较偏移。
    fn build_chunks(&self, para: &Paragraph, para_text: &str) -> Vec<StyledChunk> {
        let mut chunks: Vec<StyledChunk> = Vec::new();
        let mut offset = 0usize;

        for (ri, run) in para.runs.iter().enumerate() {
            let props = self.effective_props(run, para);
            let mut current: Option<(FontId, usize)> = None;

            for (bi, ch) in run.text.char_indices() {
                let abs = offset + bi;

                // 换行符不参与整形，断行逻辑会把它当作强制断点
                if ch == '\n' {
                    if let Some((fid, start)) = current.take() {
                        chunks.push(self.make_chunk(fid, start, abs, &props, ri, para_text));
                    }
                    continue;
                }

                let font_id = self.fonts.font_for_char_in_set(
                    &props.font,
                    props.bold,
                    props.italic,
                    props.language.as_deref(),
                    ch,
                );

                match (current, font_id) {
                    (Some((fid, start)), Some(new_id)) => {
                        if fid != new_id {
                            chunks.push(self.make_chunk(
                                fid, start, abs, &props, ri, para_text,
                            ));
                            current = Some((new_id, abs));
                        }
                    }
                    (Some((fid, start)), None) => {
                        // 找不到任何字体：保留原字体，让渲染器画 .notdef（豆腐块），
                        // 比静默丢字更容易发现问题
                        chunks.push(self.make_chunk(fid, start, abs, &props, ri, para_text));
                        current = None;
                    }
                    (None, Some(new_id)) => current = Some((new_id, abs)),
                    (None, None) => {}
                }
            }

            if let Some((fid, start)) = current.take() {
                let end = offset + run.text.len();
                chunks.push(self.make_chunk(fid, start, end, &props, ri, para_text));
            }

            offset += run.text.len();
        }

        // 相邻同属性 chunk 合并，减少后续整形次数
        merge_adjacent(chunks)
    }

    fn make_chunk(
        &self,
        font_id: FontId,
        start: usize,
        end: usize,
        props: &RunProps,
        run_index: usize,
        para_text: &str,
    ) -> StyledChunk {
        StyledChunk {
            start,
            end,
            font_id,
            props: props.clone(),
            run_index,
            text: para_text.get(start..end).unwrap_or_default().to_string(),
        }
    }

    /// 合并 run 属性与段落默认属性。
    ///
    /// 解析层已填好继承结果，这里只处理「run 的 FontSet 只指定了 latin，
    /// 但段落默认指定了 ea」这类局部缺失。
    fn effective_props(&self, run: &ppt_core::scene::TextRun, para: &Paragraph) -> RunProps {
        let mut p = run.props.clone();
        p.font = p.font.or(&para.default_run.font);
        if p.color.is_none() {
            p.color = para.default_run.color;
        }
        p
    }

    /// 计算段落的行高度量。
    fn paragraph_metrics(
        &self,
        para: &Paragraph,
        chunks: &[StyledChunk],
        scale: f32,
    ) -> ParagraphMetrics {
        let mut max_size = 0.0f32;
        let mut line_height = 0.0f32;

        for c in chunks {
            let size = c.props.size_pt * scale;
            max_size = max_size.max(size);
            if let Some(font) = self.fonts.font(c.font_id) {
                line_height = line_height.max(font.line_height_em() * size);
            }
        }

        // 段落没有可解析文本时，用默认字号兜底，保证空段落仍占位
        if max_size <= 0.0 {
            max_size = para.default_run.size_pt * scale;
        }
        if line_height <= 0.0 {
            line_height = max_size * 1.2;
        }

        // 固定磅值行距直接覆盖行高
        if let LineSpacing::Points(pt) = para.line_spacing {
            line_height = pt * scale;
        }

        ParagraphMetrics {
            max_size,
            line_height: line_height.max(max_size * 0.5),
        }
    }

    /// 判断是否需要合成粗体/斜体。
    fn glyph_style(&self, font: &LoadedFont, props: &RunProps) -> GlyphStyle {
        let (weight, is_italic) = font
            .with_face(|f| {
                (
                    f.weight().to_number(),
                    f.is_italic() || f.is_oblique(),
                )
            })
            .unwrap_or((400, false));

        GlyphStyle {
            underline: props.underline,
            strike: props.strike,
            synthetic_bold: props.bold && weight < 600,
            synthetic_italic: props.italic && !is_italic,
        }
    }

    /// 整形一段文本。
    fn shape_text(&self, font: &LoadedFont, text: &str, size_pt: f32) -> Vec<ShapedGlyph> {
        font.with_shaping_face(|face| {
            let mut buf = rustybuzz::UnicodeBuffer::new();
            buf.push_str(text);
            buf.set_direction(rustybuzz::Direction::LeftToRight);
            // 字符级簇：保证 cluster == 字节偏移，断行时可直接二分切片
            buf.set_cluster_level(rustybuzz::BufferClusterLevel::MonotoneCharacters);

            let out = rustybuzz::shape(face, &[], buf);
            let upem = font.units_per_em.max(1.0);
            let k = size_pt / upem;

            let infos = out.glyph_infos();
            let positions = out.glyph_positions();

            infos
                .iter()
                .zip(positions.iter())
                .map(|(i, p)| ShapedGlyph {
                    glyph_id: i.glyph_id as u16,
                    advance: p.x_advance as f32 * k,
                    x_offset: p.x_offset as f32 * k,
                    y_offset: p.y_offset as f32 * k,
                    cluster: i.cluster,
                })
                .collect()
        })
        .unwrap_or_default()
    }

    /// 整形一个 chunk 并预计算累计宽度。
    fn shape_chunk(&self, chunk: StyledChunk, scale: f32) -> ShapedChunk {
        let StyledChunk {
            start,
            end,
            font_id,
            props,
            text,
            ..
        } = chunk;
        let full_size = props.size_pt * scale;
        // 上下标在 PowerPoint 里还会被缩小 —— `a:rPr/@baseline` 只管偏移，
        // 缩小是渲染约定（否则「kg/m3」的上标 3 会和正文一样大，很怪）。
        // 偏移量按**原字号**算，缩小只作用在字形上。
        let size = if props.baseline_pct != 0.0 {
            full_size * SUBSCRIPT_SCALE
        } else {
            full_size
        };

        let (font, mut glyphs) = match self.fonts.font(font_id) {
            Some(f) => {
                let g = self.shape_text(f, &text, size);
                (Arc::clone(f), g)
            }
            None => (Arc::new(LoadedFont::placeholder()), Vec::new()),
        };

        // 中文标点挤压（`，。、` 这类收尾标点只占半个字宽）。
        //
        // PowerPoint / Word 对中日文默认就这么排：标点的墨迹挤在字的左半边，
        // 右半边本来就空着，后面跟字时就把它压掉。不认这一条，中文长句每行
        // 都会少放一个字 —— 句子的行数一多，文本框与表格就跟着长高，
        // 中文课件里「表格顶出画面底部」多半是这么来的。
        for g in glyphs.iter_mut() {
            let Some(ch) = text.get(g.cluster as usize..).and_then(|s| s.chars().next()) else {
                continue;
            };
            // 只压**全角**形态：已经是半角的标点不能再压一半
            if is_cjk_closing_punct(ch) && g.advance > size * 0.9 {
                g.advance -= size * 0.5;
            }
        }

        // 字间距（`a:rPr/@spc`，单位是 1/100 pt）。
        //
        // 课件里正负都会用：标题常用正值把字撑开（`spc="29"` 就是每个字加 0.29pt），
        // 塞不下的长句则用负值压紧。以前只把它读进 `RunProps` 却没用在排版上，
        // 于是加宽的字距全都丢了 —— 断行位置跟着和 PowerPoint 对不上。
        if props.spacing_pt != 0.0 {
            let extra = props.spacing_pt * scale;
            for g in glyphs.iter_mut() {
                g.advance += extra;
            }
        }

        let mut cum = Vec::with_capacity(glyphs.len() + 1);
        cum.push(0.0f32);
        let mut acc = 0.0;
        for g in &glyphs {
            acc += g.advance;
            cum.push(acc);
        }

        let style = self.glyph_style(&font, &props);
        let color = props.color.unwrap_or(Color::BLACK);

        ShapedChunk {
            start,
            end,
            font,
            size_pt: size,
            color,
            style,
            baseline_shift: -props.baseline_pct * full_size,
            text,
            glyphs,
            cum,
        }
    }
}

struct ParagraphLayout {
    lines: Vec<LaidOutLine>,
    base_line_height: f32,
}

#[derive(Debug, Clone, Copy)]
struct ParagraphMetrics {
    max_size: f32,
    line_height: f32,
}

/// 切分后的「同字体同属性」文本片段。
struct StyledChunk {
    /// 相对段落文本的字节区间。
    start: usize,
    end: usize,
    font_id: FontId,
    props: RunProps,
    run_index: usize,
    /// 该片段的文本（与 `start..end` 对应），整形时直接使用。
    text: String,
}

/// 已整形的片段。
struct ShapedChunk {
    start: usize,
    end: usize,
    font: Arc<LoadedFont>,
    size_pt: f32,
    color: Color,
    style: GlyphStyle,
    /// 基线偏移（pt，向上为正）—— 上下标靠它，`a:rPr/@baseline`。
    baseline_shift: f32,
    text: String,
    glyphs: Vec<ShapedGlyph>,
    /// `cum[i]` = 前 i 个字形累计宽度；长度 = glyphs.len() + 1。
    cum: Vec<f32>,
}

impl ShapedChunk {
    /// 取字节偏移 `local`（相对本 chunk 起点）对应的字形索引。
    ///
    /// 字符级簇下 `cluster` 单调不减，可二分。
    fn glyph_index_at(&self, local: usize) -> usize {
        self.glyphs
            .partition_point(|g| (g.cluster as usize) < local)
    }

    /// 测量 `[from, to)`（相对本 chunk 起点）的宽度。
    fn width_between(&self, from: usize, to: usize) -> f32 {
        let i0 = self.glyph_index_at(from);
        let i1 = self.glyph_index_at(to);
        if i1 <= i0 {
            return 0.0;
        }
        self.cum[i1.min(self.cum.len() - 1)] - self.cum[i0.min(self.cum.len() - 1)]
    }

    fn char_at(&self, local: usize) -> Option<char> {
        self.text.get(local..)?.chars().next()
    }
}

struct ShapedGlyph {
    glyph_id: u16,
    advance: f32,
    x_offset: f32,
    y_offset: f32,
    cluster: u32,
}

/// 中文的收尾标点：句读与右括号。
///
/// 这些标点的墨迹都在字的左半边，PowerPoint 会把它们的**前进宽度压到半个字**
/// （标点挤压）—— 行中后面跟字时压，行末则表现为「悬挂」出边界半个字宽。
/// 只收全角形态：西文的 `.` `,` 本身就窄，PowerPoint 也不压，
/// 混进来反而会让西文行宽算少（一行挤进一个词）。
fn is_cjk_closing_punct(ch: char) -> bool {
    matches!(
        ch,
        '、' | '。'
            | '，'
            | '．'
            | '；'
            | '：'
            | '！'
            | '？'
            | '）'
            | '】'
            | '》'
            | '」'
            | '』'
            | '〉'
            | '〕'
            | '］'
    )
}

/// 把相邻且属性相同的 chunk 合并。
fn merge_adjacent(chunks: Vec<StyledChunk>) -> Vec<StyledChunk> {
    let mut out: Vec<StyledChunk> = Vec::with_capacity(chunks.len());
    for c in chunks {
        match out.last_mut() {
            Some(prev)
                if prev.end == c.start
                    && prev.font_id == c.font_id
                    && prev.run_index == c.run_index
                    && same_props(&prev.props, &c.props) =>
            {
                prev.end = c.end;
                prev.text.push_str(&c.text);
            }
            _ => out.push(c),
        }
    }
    out
}

fn same_props(a: &RunProps, b: &RunProps) -> bool {
    a.size_pt == b.size_pt
        && a.bold == b.bold
        && a.italic == b.italic
        && a.underline == b.underline
        && a.strike == b.strike
        && a.color == b.color
        && a.spacing_pt == b.spacing_pt
        && a.baseline_pct == b.baseline_pct
        && a.font == b.font
}

fn shift_glyphs(glyphs: &mut [PositionedGlyph], dx: f32) {
    for g in glyphs.iter_mut() {
        g.x += dx;
    }
}

/// 两端对齐：把富余宽度均摊到字形之间。
///
/// 中文排版里这对应「均等分散」，也是 WPS/PowerPoint 对 `algn="just"` 的处理方式。
fn distribute(glyphs: &mut [PositionedGlyph], free: f32) {
    if glyphs.len() < 2 {
        return;
    }
    let per_gap = free / (glyphs.len() - 1) as f32;
    for (i, g) in glyphs.iter_mut().enumerate() {
        g.x += per_gap * i as f32;
    }
}

/// 下一个制表位。
fn next_tab_stop(x: f32, stops: &[ppt_core::scene::TabStop]) -> f32 {
    // 优先使用段落自定义制表位
    for s in stops {
        if s.position_pt > x + 0.01 {
            return match s.align {
                TabAlign::Left | TabAlign::Decimal => s.position_pt,
                TabAlign::Center | TabAlign::Right => s.position_pt,
            };
        }
    }
    // 默认制表位：每 72pt（1 英寸）一个，与 PowerPoint 一致
    const DEFAULT_TAB_PT: f32 = 72.0;
    ((x / DEFAULT_TAB_PT).floor() + 1.0) * DEFAULT_TAB_PT
}

/// 把自动编号格式化为字符串。
fn format_auto_number(n: u32, number_type: Option<&str>) -> String {
    match number_type.unwrap_or("arabicPeriod") {
        "arabicPeriod" => format!("{n}."),
        "arabicParenR" => format!("{n})"),
        "arabicParenBoth" => format!("({n})"),
        "arabicPlain" => format!("{n}"),
        "romanUcPeriod" => format!("{}.", to_roman(n, true)),
        "romanLcPeriod" => format!("{}.", to_roman(n, false)),
        "alphaUcPeriod" => format!("{}.", to_alpha(n, true)),
        "alphaLcPeriod" => format!("{}.", to_alpha(n, false)),
        "alphaUcParenR" => format!("{})", to_alpha(n, true)),
        "alphaLcParenR" => format!("{})", to_alpha(n, false)),
        // 中文编号
        "cjkIdeographPeriod" => format!("{}.", to_cjk_number(n)),
        "cjkIdeographPlain" => to_cjk_number(n),
        "circleNumDbPlain" | "circleNumWdBlackPlain" => to_circled_number(n),
        _ => format!("{n}."),
    }
}

fn to_roman(mut n: u32, upper: bool) -> String {
    if n == 0 || n > 3999 {
        return n.to_string();
    }
    const TABLE: [(u32, &str); 13] = [
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    let mut s = String::new();
    for (v, sym) in TABLE {
        while n >= v {
            s.push_str(sym);
            n -= v;
        }
    }
    if upper {
        s.to_uppercase()
    } else {
        s
    }
}

fn to_alpha(n: u32, upper: bool) -> String {
    if n == 0 {
        return String::new();
    }
    let mut n = n;
    let mut s = String::new();
    while n > 0 {
        let rem = ((n - 1) % 26) as u8;
        s.insert(0, (b'a' + rem) as char);
        n = (n - 1) / 26;
    }
    if upper {
        s.to_uppercase()
    } else {
        s
    }
}

/// 阿拉伯数字 → 中文数字（覆盖 1~99，够用于项目编号）。
fn to_cjk_number(n: u32) -> String {
    const DIGITS: [&str; 10] = ["零", "一", "二", "三", "四", "五", "六", "七", "八", "九"];
    match n {
        0 => "零".to_string(),
        1..=9 => DIGITS[n as usize].to_string(),
        10 => "十".to_string(),
        11..=19 => format!("十{}", DIGITS[(n % 10) as usize]),
        20..=99 => {
            let tens = n / 10;
            let ones = n % 10;
            if ones == 0 {
                format!("{}十", DIGITS[tens as usize])
            } else {
                format!("{}十{}", DIGITS[tens as usize], DIGITS[ones as usize])
            }
        }
        _ => n.to_string(),
    }
}

fn to_circled_number(n: u32) -> String {
    // ①..⑳ 连续区，之后回退到括号形式
    match n {
        1..=20 => char::from_u32(0x2460 + n - 1)
            .map(|c| c.to_string())
            .unwrap_or_else(|| format!("{n}")),
        _ => format!("({n})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ppt_core::scene::{AutoFit, BodyProps, Build, TextRun};

    fn ctx() -> FontContext {
        FontContext::empty()
    }

    fn chunk(start: usize, end: usize, font_id: FontId, props: RunProps, text: &str) -> StyledChunk {
        StyledChunk {
            start,
            end,
            font_id,
            props,
            run_index: 0,
            text: text.to_string(),
        }
    }

    fn simple_box(text: &str) -> TextBox {
        TextBox {
            body: BodyProps::default(),
            paragraphs: vec![Paragraph {
                runs: vec![TextRun::new(text)],
                ..Default::default()
            }],
        }
    }

    #[test]
    fn empty_font_context_yields_no_glyphs_but_does_not_panic() {
        let fonts = ctx();
        let layouter = TextLayouter::new(&fonts);
        let layout = layouter.layout(
            &simple_box("Hello 中文"),
            Size::new(400.0, 200.0),
            LayoutOptions::default(),
        );
        assert!(layout.glyphs().count() == 0);
        // 没有字体时仍应产生行结构，便于上层显示占位
        assert!(!layout.is_empty());
    }

    #[test]
    fn empty_paragraph_still_occupies_a_line() {
        let fonts = ctx();
        let layouter = TextLayouter::new(&fonts);
        let tb = TextBox {
            body: BodyProps::default(),
            paragraphs: vec![Paragraph::default()],
        };
        let layout = layouter.layout(&tb, Size::new(400.0, 200.0), LayoutOptions::default());
        assert_eq!(layout.line_count(), 1);
        assert!(layout.height > 0.0, "空段落应有行高");
    }

    #[test]
    fn multiple_paragraphs_stack_vertically() {
        let fonts = ctx();
        let layouter = TextLayouter::new(&fonts);
        let tb = TextBox {
            body: BodyProps::default(),
            paragraphs: vec![
                Paragraph {
                    runs: vec![TextRun::new("A")],
                    ..Default::default()
                },
                Paragraph {
                    runs: vec![TextRun::new("B")],
                    ..Default::default()
                },
            ],
        };
        let layout = layouter.layout(&tb, Size::new(400.0, 200.0), LayoutOptions::default());
        assert_eq!(layout.line_count(), 2);
        assert!(layout.lines[1].top > layout.lines[0].top);
        assert_eq!(layout.lines[0].paragraph, 0);
        assert_eq!(layout.lines[1].paragraph, 1);
    }

    #[test]
    fn superscript_is_lifted_and_scaled() {
        // 「kg/m3」里的 3 是上标：既要抬上去，也要缩小。
        // 只抬不缩会得到一个和正文一样大的 3，看起来像没渲染
        let fonts = FontContext::new();
        if fonts.font_count() == 0 {
            return;
        }
        let layouter = TextLayouter::new(&fonts);
        let tb = TextBox {
            body: BodyProps::default(),
            paragraphs: vec![Paragraph {
                runs: vec![
                    TextRun {
                        text: "m".into(),
                        props: RunProps {
                            size_pt: 28.0,
                            ..Default::default()
                        },
                        hyperlink: None,
                        field: None,
                    },
                    TextRun {
                        text: "3".into(),
                        props: RunProps {
                            size_pt: 28.0,
                            baseline_pct: 0.3,
                            ..Default::default()
                        },
                        hyperlink: None,
                        field: None,
                    },
                ],
                ..Default::default()
            }],
        };

        let layout = layouter.layout(&tb, Size::new(400.0, 200.0), LayoutOptions::default());
        let glyphs = &layout.lines[0].glyphs;
        assert_eq!(glyphs.len(), 2, "两个 run 各出一个字形");
        let base = &glyphs[0];
        // 上标是那个更小的字形
        let sup = glyphs
            .iter()
            .min_by(|a, b| a.size_pt.total_cmp(&b.size_pt))
            .expect("应有字形");

        assert!(
            sup.size_pt < base.size_pt * 0.8,
            "上标应缩小：{} vs {}",
            sup.size_pt,
            base.size_pt
        );
        assert!(
            sup.y < base.y - 1.0,
            "上标应抬高：y {} vs {}",
            sup.y,
            base.y
        );
    }

    #[test]
    fn hidden_paragraph_keeps_its_place() {
        // 项目符号「逐条弹出」：还没出场的那条不能把后面的内容往上顶。
        // 位置是排版层的职责，因此用空字体上下文也能验；
        // 「一个字都不画」需要真实字体才看得出来。
        let fonts = FontContext::new();
        let layouter = TextLayouter::new(&fonts);
        let tb = TextBox {
            body: BodyProps::default(),
            paragraphs: vec![
                Paragraph {
                    runs: vec![TextRun::new("第一条")],
                    ..Default::default()
                },
                Paragraph {
                    runs: vec![TextRun::new("第二条")],
                    build: Build {
                        appear: Some(1),
                        disappear: None,
                    },
                    ..Default::default()
                },
                Paragraph {
                    runs: vec![TextRun::new("第三条")],
                    ..Default::default()
                },
            ],
        };

        let all = layouter.layout(&tb, Size::new(400.0, 200.0), LayoutOptions::default());
        let none = layouter.layout(
            &tb,
            Size::new(400.0, 200.0),
            LayoutOptions {
                reveal: 0,
                ..Default::default()
            },
        );

        assert_eq!(none.line_count(), 3, "没出场的段落照样占一行");
        assert_eq!(
            none.lines[2].top, all.lines[2].top,
            "第三段的位置不该因为第二段没出场而变化"
        );

        // 字体不可用的环境里拿不到字形，这两条就跳过
        if fonts.font_count() == 0 {
            return;
        }
        assert!(
            !all.lines[1].glyphs.is_empty(),
            "全部显示时第二段必须有字形"
        );
        assert_eq!(none.lines[1].glyphs.len(), 0, "没出场的段落一个字都不该画");
        assert!(!none.lines[2].glyphs.is_empty(), "第三段照常显示");
    }

    #[test]
    fn vertical_anchor_offsets_lines() {
        let fonts = ctx();
        let layouter = TextLayouter::new(&fonts);
        let make = |anchor| TextBox {
            body: BodyProps {
                anchor,
                ..Default::default()
            },
            paragraphs: vec![Paragraph {
                runs: vec![TextRun::new("A")],
                ..Default::default()
            }],
        };

        let area = Size::new(400.0, 300.0);
        let top = layouter.layout(&make(VerticalAnchor::Top), area, LayoutOptions::default());
        let mid = layouter.layout(&make(VerticalAnchor::Middle), area, LayoutOptions::default());
        let bot = layouter.layout(&make(VerticalAnchor::Bottom), area, LayoutOptions::default());

        assert!(top.lines[0].top < mid.lines[0].top);
        assert!(mid.lines[0].top < bot.lines[0].top);
    }

    #[test]
    fn space_before_and_after_add_height() {
        let fonts = ctx();
        let layouter = TextLayouter::new(&fonts);
        let plain = TextBox {
            body: BodyProps::default(),
            paragraphs: vec![Paragraph {
                runs: vec![TextRun::new("A")],
                ..Default::default()
            }],
        };
        let spaced = TextBox {
            body: BodyProps::default(),
            paragraphs: vec![Paragraph {
                runs: vec![TextRun::new("A")],
                space_before_pt: 20.0,
                space_after_pt: 30.0,
                ..Default::default()
            }],
        };
        let a = layouter.layout(&plain, Size::new(400.0, 300.0), LayoutOptions::default());
        let b = layouter.layout(&spaced, Size::new(400.0, 300.0), LayoutOptions::default());
        assert!((b.height - a.height - 50.0).abs() < 0.5, "段前段后距应计入高度");
    }

    /// 行距百分比是乘数 —— 收紧也生效。
    ///
    /// 课件里 `spcPct` 用 95%、80% 的很多（作者用它在有限高度里塞进更多行）。
    /// 以前只认「撑高」不认「收紧」，小于 100% 一律按 100% 算，
    /// 于是每一行都多出 5%~20%，十几行下来就顶出文本框、溢到页面外。
    #[test]
    fn percent_line_spacing_tightens_as_well_as_stretches() {
        let fonts = ctx();
        let layouter = TextLayouter::new(&fonts);
        let one_line = |pct: f32| {
            let tb = TextBox {
                body: BodyProps::default(),
                paragraphs: vec![Paragraph {
                    runs: vec![TextRun::new("A")],
                    line_spacing: LineSpacing::Percent(pct),
                    ..Default::default()
                }],
            };
            layouter
                .layout(&tb, Size::new(400.0, 300.0), LayoutOptions::default())
                .height
        };

        let single = one_line(1.0);
        let tight = one_line(0.8);
        let loose = one_line(1.5);

        assert!(
            (tight - single * 0.8).abs() < 0.5,
            "80% 行距应把一行压到基准的八成：{tight} vs {single}"
        );
        assert!(
            (loose - single * 1.5).abs() < 0.5,
            "150% 行距应把一行撑到基准的一倍半：{loose} vs {single}"
        );
    }

    /// 悬挂缩进与字间距的宽度影响，用真实字体在 `tests/real_fonts.rs` 里验证 ——
    /// 这里的空字体库产不出字形，任何宽度都是 0，测了等于没测。

    #[test]
    fn fixed_line_spacing_overrides_font_metrics() {
        let fonts = ctx();
        let layouter = TextLayouter::new(&fonts);
        let tb = TextBox {
            body: BodyProps::default(),
            paragraphs: vec![Paragraph {
                runs: vec![TextRun::new("A")],
                line_spacing: LineSpacing::Points(50.0),
                ..Default::default()
            }],
        };
        let layout = layouter.layout(&tb, Size::new(400.0, 300.0), LayoutOptions::default());
        assert!((layout.height - 50.0).abs() < 0.5, "固定行距应为 50pt");
    }

    #[test]
    fn autofit_shrinks_when_overflowing() {
        let fonts = ctx();
        let layouter = TextLayouter::new(&fonts);
        let tb = TextBox {
            body: BodyProps {
                auto_fit: AutoFit::NormAutofit {
                    font_scale: 1.0,
                    line_space_reduction: 0.0,
                },
                ..Default::default()
            },
            paragraphs: (0..10)
                .map(|_| Paragraph {
                    runs: vec![TextRun::new("测试文本")],
                    ..Default::default()
                })
                .collect(),
        };
        // 无字体环境下行高按「字号 × 1.2」估算：10 行 × 21.6pt = 216pt，
        // 放进 150pt 高度必须缩小字号
        let tight = Size::new(200.0, 150.0);
        let layout = layouter.layout(&tb, tight, LayoutOptions::default());
        assert!(
            layout.font_scale < 1.0,
            "内容超出时应缩小字号，实际 {}",
            layout.font_scale
        );
        assert!(!layout.overflow, "缩小后应能放下");
    }

    #[test]
    fn autofit_keeps_scale_when_fits() {
        let fonts = ctx();
        let layouter = TextLayouter::new(&fonts);
        let tb = TextBox {
            body: BodyProps {
                auto_fit: AutoFit::NormAutofit {
                    font_scale: 1.0,
                    line_space_reduction: 0.0,
                },
                ..Default::default()
            },
            paragraphs: vec![Paragraph {
                runs: vec![TextRun::new("短")],
                ..Default::default()
            }],
        };
        let layout = layouter.layout(&tb, Size::new(400.0, 300.0), LayoutOptions::default());
        assert!((layout.font_scale - 1.0).abs() < 1e-6);
    }

    #[test]
    fn preset_font_scale_is_respected() {
        let fonts = ctx();
        let layouter = TextLayouter::new(&fonts);
        let tb = TextBox {
            body: BodyProps {
                auto_fit: AutoFit::NormAutofit {
                    font_scale: 0.6,
                    line_space_reduction: 0.0,
                },
                ..Default::default()
            },
            paragraphs: vec![Paragraph {
                runs: vec![TextRun::new("A")],
                ..Default::default()
            }],
        };
        let layout = layouter.layout(&tb, Size::new(400.0, 300.0), LayoutOptions::default());
        // 课件已预置 fontScale 时不应重新求解
        assert!((layout.font_scale - 0.6).abs() < 1e-6);
    }

    #[test]
    fn max_lines_limit_is_enforced() {
        let fonts = ctx();
        let layouter = TextLayouter::new(&fonts);
        let tb = TextBox {
            body: BodyProps::default(),
            paragraphs: (0..50)
                .map(|_| Paragraph {
                    runs: vec![TextRun::new("行")],
                    ..Default::default()
                })
                .collect(),
        };
        let layout = layouter.layout(
            &tb,
            Size::new(400.0, 300.0),
            LayoutOptions {
                max_lines: 10,
                ..Default::default()
            },
        );
        assert!(layout.line_count() <= 10, "应受行数上限约束");
        assert!(!layout.warnings.is_empty(), "截断应产生告警");
    }

    #[test]
    fn hard_break_splits_into_lines() {
        let fonts = ctx();
        let layouter = TextLayouter::new(&fonts);
        let tb = simple_box("第一行\n第二行");
        let layout = layouter.layout(&tb, Size::new(400.0, 300.0), LayoutOptions::default());
        assert_eq!(layout.line_count(), 2);
        assert_eq!(layout.lines[0].text, "第一行");
        assert_eq!(layout.lines[1].text, "第二行");
    }

    #[test]
    fn plain_text_roundtrips_lines() {
        let fonts = ctx();
        let layouter = TextLayouter::new(&fonts);
        let layout = layouter.layout(
            &simple_box("A\nB"),
            Size::new(400.0, 300.0),
            LayoutOptions::default(),
        );
        assert_eq!(layout.plain_text(), "A\nB");
    }

    #[test]
    fn overflow_flag_set_when_too_tall() {
        let fonts = ctx();
        let layouter = TextLayouter::new(&fonts);
        let tb = TextBox {
            body: BodyProps::default(),
            paragraphs: (0..30)
                .map(|_| Paragraph {
                    runs: vec![TextRun::new("行")],
                    ..Default::default()
                })
                .collect(),
        };
        let layout = layouter.layout(
            &tb,
            Size::new(400.0, 50.0),
            LayoutOptions {
                solve_autofit: false,
                ..Default::default()
            },
        );
        assert!(layout.overflow);
    }

    #[test]
    fn tab_stop_advances_position() {
        assert_eq!(next_tab_stop(0.0, &[]), 72.0);
        assert_eq!(next_tab_stop(10.0, &[]), 72.0);
        assert_eq!(next_tab_stop(72.0, &[]), 144.0);
        assert_eq!(next_tab_stop(100.0, &[]), 144.0);
    }

    #[test]
    fn custom_tab_stops_take_precedence() {
        let stops = vec![ppt_core::scene::TabStop {
            position_pt: 30.0,
            align: TabAlign::Left,
        }];
        assert_eq!(next_tab_stop(10.0, &stops), 30.0);
        // 超过自定义制表位后回落到默认网格
        assert_eq!(next_tab_stop(40.0, &stops), 72.0);
    }

    #[test]
    fn auto_number_formats() {
        assert_eq!(format_auto_number(1, None), "1.");
        assert_eq!(format_auto_number(3, Some("arabicParenR")), "3)");
        assert_eq!(format_auto_number(2, Some("romanUcPeriod")), "II.");
        assert_eq!(format_auto_number(1, Some("alphaLcPeriod")), "a.");
        assert_eq!(format_auto_number(26, Some("alphaUcPeriod")), "Z.");
        assert_eq!(format_auto_number(27, Some("alphaUcPeriod")), "AA.");
        assert_eq!(format_auto_number(5, Some("cjkIdeographPeriod")), "五.");
        assert_eq!(format_auto_number(15, Some("cjkIdeographPlain")), "十五");
        assert_eq!(format_auto_number(20, Some("cjkIdeographPlain")), "二十");
        assert_eq!(format_auto_number(21, Some("cjkIdeographPlain")), "二十一");
    }

    #[test]
    fn roman_numerals() {
        assert_eq!(to_roman(4, false), "iv");
        assert_eq!(to_roman(9, true), "IX");
        assert_eq!(to_roman(1994, true), "MCMXCIV");
        // 超范围回退为阿拉伯数字
        assert_eq!(to_roman(0, true), "0");
        assert_eq!(to_roman(4000, true), "4000");
    }

    #[test]
    fn alpha_numbering_rolls_over() {
        assert_eq!(to_alpha(1, false), "a");
        assert_eq!(to_alpha(26, false), "z");
        assert_eq!(to_alpha(27, false), "aa");
        assert_eq!(to_alpha(0, false), "");
    }

    #[test]
    fn distribute_spreads_glyphs_evenly() {
        let font = Arc::new(LoadedFont::placeholder());
        let mut glyphs: Vec<PositionedGlyph> = (0..3)
            .map(|i| PositionedGlyph {
                glyph_id: 0,
                font: Arc::clone(&font),
                x: i as f32 * 10.0,
                y: 0.0,
                size_pt: 12.0,
                color: Color::BLACK,
                advance: 10.0,
                style: GlyphStyle::default(),
                cluster: 0,
                run_index: 0,
            })
            .collect();
        distribute(&mut glyphs, 30.0);
        assert!((glyphs[0].x - 0.0).abs() < 1e-4);
        assert!((glyphs[1].x - 25.0).abs() < 1e-4);
        assert!((glyphs[2].x - 50.0).abs() < 1e-4);
    }

    #[test]
    fn distribute_is_noop_for_single_glyph() {
        let mut glyphs = vec![PositionedGlyph {
            glyph_id: 0,
            font: Arc::new(LoadedFont::placeholder()),
            x: 5.0,
            y: 0.0,
            size_pt: 12.0,
            color: Color::BLACK,
            advance: 10.0,
            style: GlyphStyle::default(),
            cluster: 0,
            run_index: 0,
        }];
        distribute(&mut glyphs, 30.0);
        assert_eq!(glyphs[0].x, 5.0);
    }

    #[test]
    fn merge_adjacent_combines_same_style_chunks() {
        let p = RunProps::default();
        let chunks = vec![
            chunk(0, 3, 1, p.clone(), "abc"),
            chunk(3, 6, 1, p.clone(), "def"),
        ];
        let merged = merge_adjacent(chunks);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].end, 6);
        assert_eq!(merged[0].text, "abcdef");
    }

    #[test]
    fn merge_adjacent_keeps_different_fonts_separate() {
        let p = RunProps::default();
        let chunks = vec![
            chunk(0, 3, 1, p.clone(), "abc"),
            chunk(3, 6, 2, p.clone(), "def"),
        ];
        assert_eq!(merge_adjacent(chunks).len(), 2);
    }

    #[test]
    fn merge_adjacent_keeps_different_styles_separate() {
        let a = RunProps::default();
        let b = RunProps {
            bold: true,
            ..Default::default()
        };
        let chunks = vec![chunk(0, 3, 1, a, "abc"), chunk(3, 6, 1, b, "def")];
        assert_eq!(merge_adjacent(chunks).len(), 2);
    }

    #[test]
    fn merge_adjacent_keeps_noncontiguous_separate() {
        let p = RunProps::default();
        let chunks = vec![
            chunk(0, 3, 1, p.clone(), "abc"),
            chunk(5, 8, 1, p.clone(), "fgh"),
        ];
        assert_eq!(merge_adjacent(chunks).len(), 2);
    }

    #[test]
    fn glyph_style_flags_default_for_plain_text() {
        assert!(GlyphStyle::default().is_plain());
        let s = GlyphStyle {
            underline: UnderlineStyle::Single,
            ..Default::default()
        };
        assert!(!s.is_plain());
    }

    #[test]
    fn char_boundary_snaps_down() {
        let fonts = ctx();
        let layouter = TextLayouter::new(&fonts);
        let text = "中文";
        assert_eq!(layouter.char_boundary_at(text, 3), 3);
        // 落在多字节字符中间时回退到字符起点
        assert_eq!(layouter.char_boundary_at(text, 1), 0);
        assert_eq!(layouter.char_boundary_at(text, 2), 0);
        assert_eq!(layouter.char_boundary_at(text, 999), text.len());
    }

    #[test]
    fn wrap_terminates_on_unbreakable_long_token() {
        let fonts = ctx();
        let layouter = TextLayouter::new(&fonts);
        let mut warnings = Vec::new();
        // 无字体环境下所有宽度为 0，因此不会触发折行；
        // 这里主要验证在极端输入下不会死循环
        let text = "a".repeat(500);
        let ops = linebreak::break_opportunities(&text);
        let ranges = layouter.wrap(&text, &ops, &[], 10.0, &mut warnings);
        assert!(!ranges.is_empty());
        // 区间必须连续且覆盖全文
        assert_eq!(ranges[0].0, 0);
        assert_eq!(ranges.last().unwrap().1, text.len());
    }

    #[test]
    fn wrap_handles_empty_text() {
        let fonts = ctx();
        let layouter = TextLayouter::new(&fonts);
        let mut warnings = Vec::new();
        let ranges = layouter.wrap("", &[], &[], 100.0, &mut warnings);
        assert_eq!(ranges, vec![(0, 0)]);
    }

    #[test]
    fn line_height_never_below_one_and_a_fifth_em() {
        // 占位字体的 (asc - desc) 是 1.0em；PowerPoint 的单倍行距按字号 × 1.2 算，
        // 这里必须抬到 1.2 —— 否则每一行都比 PowerPoint 矮一截，
        // 行数一多，正文就整体往上挤、框与表格的高度全对不上
        let f = LoadedFont::placeholder();
        assert!((f.line_height_em() - 1.2).abs() < 1e-6);
    }

    #[test]
    fn trailing_spaces_do_not_push_the_segment_to_a_new_line() {
        let fonts = ctx();
        let layouter = TextLayouter::new(&fonts);

        // 造一段「每字符 10pt」的假 chunk：`aaaa   ` 共 70pt，
        // 去掉行尾三个空格只剩 40pt
        let text = "aaaa   ";
        let glyphs: Vec<ShapedGlyph> = text
            .bytes()
            .enumerate()
            .map(|(i, _)| ShapedGlyph {
                glyph_id: 1,
                advance: 10.0,
                x_offset: 0.0,
                y_offset: 0.0,
                cluster: i as u32,
            })
            .collect();
        let mut cum = vec![0.0f32];
        for g in &glyphs {
            cum.push(cum.last().copied().unwrap_or(0.0) + g.advance);
        }
        let chunk = ShapedChunk {
            start: 0,
            end: text.len(),
            font: Arc::new(LoadedFont::placeholder()),
            size_pt: 10.0,
            color: Color::BLACK,
            style: GlyphStyle::default(),
            baseline_shift: 0.0,
            text: text.to_string(),
            glyphs,
            cum,
        };

        let ops = linebreak::break_opportunities(text);
        let mut warnings = Vec::new();
        // 可用宽 45pt：算上尾部空格（70pt）放不下，不算（40pt）放得下。
        // 课件里常用空格对齐词表，行尾挂着的空白不该把整段顶到下一行
        let ranges = layouter.wrap(
            text,
            &ops,
            std::slice::from_ref(&chunk),
            45.0,
            &mut warnings,
        );
        assert_eq!(
            ranges,
            vec![(0, text.len())],
            "行尾空格不该把这一段挤成两行"
        );
    }

    #[test]
    fn cjk_closing_punctuation_compresses_to_half_width() {
        // 中文句读与右括号要压（墨迹在左半边）；西文标点本身就窄，不能压，
        // 否则西文行宽会算少、一行挤进一个词
        for ch in ['。', '，', '、', '．', '；', '：', '！', '？', '）', '」', '】'] {
            assert!(is_cjk_closing_punct(ch), "「{ch}」应当按半个字宽算");
        }
        for ch in ['.', ',', '!', '?', ')', 'a', '中'] {
            assert!(!is_cjk_closing_punct(ch), "「{ch}」不该被压缩");
        }
    }

    #[test]
    fn extra_line_spacing_is_split_above_the_text_too() {
        let fonts = ctx();
        let layouter = TextLayouter::new(&fonts);
        let mk = |pct: f32| {
            let mut tb = simple_box("line");
            tb.paragraphs[0].line_spacing = LineSpacing::Percent(pct);
            layouter.layout(&tb, Size::new(400.0, 400.0), LayoutOptions::default())
        };
        let plain = mk(1.0);
        let loose = mk(2.0);

        let plain_h = plain.lines[0].height;
        let plain_b = plain.lines[0].baseline;
        let loose_b = loose.lines[0].baseline;

        // 撑高的那部分要上下各分一半：全塞在文字下方的话，
        // 150% 行距的文字会整段低半个行高，课件里压在横线上的答案词就串到线下面去
        assert!(
            loose_b > plain_b,
            "行距变大时第一行的基线必须下移（多出的行距也要分一半到文字上方）"
        );
        assert!(
            (loose_b - plain_b - plain_h * 0.5).abs() < 0.5,
            "下移量应当正好是「多出来的行距」的一半"
        );
    }

    #[test]
    fn vertical_layout_produces_stacked_lines() {
        let fonts = ctx();
        let layouter = TextLayouter::new(&fonts);
        let tb = TextBox {
            body: BodyProps {
                direction: TextDirection::Stacked,
                ..Default::default()
            },
            paragraphs: vec![Paragraph {
                runs: vec![TextRun::new("竖排")],
                ..Default::default()
            }],
        };
        let layout = layouter.layout(&tb, Size::new(100.0, 400.0), LayoutOptions::default());
        // 无字体环境下不会产生字形，但不应 panic，且应返回结构
        assert!(layout.width >= 0.0);
    }

    #[test]
    fn layout_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TextLayout>();
        assert_send_sync::<PositionedGlyph>();
    }
}
