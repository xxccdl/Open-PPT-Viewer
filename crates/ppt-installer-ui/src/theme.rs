//! 安装程序的设计令牌：颜色、圆角、字号、间距。
//!
//! 数值按 96 DPI 的像素写死，运行时整体乘一个 DPI 系数 ——
//! 这样在 125% / 150% 的一体机上，按钮和字会跟着一起变大，
//! 而不是「窗口变大了、控件还是那么小」。
//!
//! 配色与应用本身一致（同一个蓝、同一个墨色），安装程序看起来才像
//! 这个产品的一部分，而不是随便套了个壳。

use tiny_skia::Color;

/// 按 96 DPI 设计出来的窗口尺寸。
pub const WINDOW_W: f32 = 780.0;
pub const WINDOW_H: f32 = 600.0;

/// 顶部品牌带的高度。
pub const BAND_H: f32 = 140.0;

/// 左右留白。
pub const MARGIN: f32 = 36.0;

/// 控件高度。
pub const FIELD_H: f32 = 52.0;
pub const CHIP_H: f32 = 52.0;
pub const BIG_BUTTON_H: f32 = 68.0;

/// 圆角。
pub const R_FIELD: f32 = 10.0;
pub const R_CHIP: f32 = 10.0;
pub const R_BUTTON: f32 = 12.0;
pub const R_BAND: f32 = 0.0;

fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::from_rgba8(r, g, b, 255)
}

/// 品牌蓝（顶部浅、底部深）—— 与应用图标同一族。
pub fn brand_top() -> Color {
    rgb(0x3A, 0x82, 0xDD)
}
pub fn brand_bottom() -> Color {
    rgb(0x21, 0x59, 0xAE)
}

/// 主按钮：比品牌带亮一点，才有「可以点」的感觉。
pub fn primary_top() -> Color {
    rgb(0x2E, 0x8B, 0xF0)
}
pub fn primary_bottom() -> Color {
    rgb(0x0A, 0x6C, 0xD8)
}
pub fn primary_hover_top() -> Color {
    rgb(0x43, 0x99, 0xF5)
}
pub fn primary_hover_bottom() -> Color {
    rgb(0x14, 0x77, 0xE4)
}

pub fn page() -> Color {
    rgb(0xFF, 0xFF, 0xFF)
}
pub fn ink() -> Color {
    rgb(0x1D, 0x1D, 0x1F)
}
pub fn ink_dim() -> Color {
    rgb(0x6E, 0x6E, 0x73)
}
pub fn ink_faint() -> Color {
    rgb(0xA1, 0xA1, 0xA6)
}
pub fn line() -> Color {
    rgb(0xE3, 0xE4, 0xEA)
}
pub fn chip_bg() -> Color {
    rgb(0xF3, 0xF5, 0xF8)
}
pub fn chip_bg_hover() -> Color {
    rgb(0xEA, 0xEE, 0xF4)
}
pub fn chip_on_bg() -> Color {
    rgb(0xEA, 0xF2, 0xFE)
}
pub fn chip_on_border() -> Color {
    rgb(0x0A, 0x84, 0xFF)
}
pub fn track() -> Color {
    rgb(0xEC, 0xEE, 0xF2)
}
pub fn coral() -> Color {
    rgb(0xFF, 0x5A, 0x50)
}
pub fn ok_green() -> Color {
    rgb(0x1F, 0xB1, 0x62)
}

/// 链接色：比主色深一点，落在浅底上才看得清。
pub fn link() -> Color {
    rgb(0x1A, 0x6F, 0xD6)
}

/// 主按钮悬停时的淡蓝填充（描边胶囊用）。
pub fn primary_soft() -> Color {
    Color::from_rgba8(0x2E, 0x8B, 0xF0, 26)
}

/// 半透明白（品牌带上的副标题）。
pub fn white_soft() -> Color {
    Color::from_rgba8(255, 255, 255, 220)
}
pub fn white() -> Color {
    rgb(0xFF, 0xFF, 0xFF)
}

// 字号（pt）。比 Windows 默认大一档 —— 目标用户是不太会用电脑的老师。
pub const FS_TITLE: f32 = 27.0;
pub const FS_H2: f32 = 15.0;
pub const FS_BODY: f32 = 14.0;
pub const FS_SMALL: f32 = 12.0;
pub const FS_BUTTON: f32 = 21.0;
pub const FS_BUTTON_SMALL: f32 = 14.0;

/// 界面上用到的字体名（中英文各一个，避免西文落到宋体上）。
pub const FONT_UI: &str = "Microsoft YaHei UI";
pub const FONT_UI_FALLBACK: &str = "Microsoft YaHei";
pub const FONT_LATIN: &str = "Segoe UI";
