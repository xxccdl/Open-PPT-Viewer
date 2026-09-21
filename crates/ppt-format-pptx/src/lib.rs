//! # ppt-format-pptx
//!
//! PPTX（Office Open XML PresentationML）解析：把 `.pptx` 翻译成 [`ppt_core::scene::Scene`]。

pub mod chart;
pub mod color;
pub mod inherit;
pub mod paint;
pub mod preset;
pub mod shape;
pub mod source;
pub mod table;
pub mod text;
pub mod theme;
pub mod timing;
pub mod transition;

pub use source::PptxSource;
