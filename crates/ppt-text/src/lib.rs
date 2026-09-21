//! # ppt-text
//!
//! 文本排版引擎：字体解析与回退、文本整形、中文断行、字形定位。
//!
//! ## 职责边界
//!
//! 本 crate 只做「把 [`ppt_core::scene::TextBox`] 排成定位字形」，
//! **不负责绘制**。字形轮廓的提取与光栅化在 `ppt-render`。
//! 这样切分的好处是排版逻辑可以脱离像素单独做快照测试，
//! 而渲染器也不必理解 OOXML 的文本继承规则。
//!
//! ## 典型用法
//!
//! ```no_run
//! use ppt_text::{FontContext, LayoutOptions, TextLayouter};
//! use ppt_core::scene::Size;
//!
//! # fn demo(text_box: &ppt_core::scene::TextBox) {
//! // 字体索引只需建一次，之后在多个渲染线程间共享
//! let fonts = FontContext::new();
//! let layouter = TextLayouter::new(&fonts);
//!
//! // area 是已扣除内边距的可用文本区
//! let layout = layouter.layout(text_box, Size::new(400.0, 200.0), LayoutOptions::default());
//! for glyph in layout.glyphs() {
//!     // 交给渲染器取轮廓并绘制
//!     let _ = (glyph.glyph_id, glyph.font.id, glyph.x, glyph.y, glyph.size_pt);
//! }
//! # }
//! ```

pub mod fonts;
pub mod layout;
pub mod linebreak;

pub use fonts::{
    is_cjk_char, is_complex_script, is_symbol_char, is_theme_font_ref, normalize_family_name,
    FontContext, FontId, LoadedFont, ThemeFontRef,
};
pub use layout::{
    GlyphStyle, LaidOutLine, LayoutOptions, PositionedGlyph, TextLayout, TextLayouter,
};
pub use linebreak::{break_opportunities, BreakOpportunity};

/// 排版引擎版本，写入缓存键以便字体或算法升级后自动失效旧缓存。
pub const LAYOUT_ENGINE_VERSION: u32 = 1;
