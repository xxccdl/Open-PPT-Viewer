//! OpenPPTView 安装程序的界面层。
//!
//! 这一层只做三件事：**布局、绘制、命中测试**。它不碰文件、注册表、窗口，
//! 也不依赖任何 UI 框架 —— 因此可以离线把任意一屏渲染成 PNG（见
//! `examples/preview.rs`），在真正打包之前先把界面看一遍。
//!
//! 分开的理由很实际：安装程序的界面是最难改的部分（改完要装一次才能看到），
//! 而渲染成图片只要几百毫秒。

pub mod paint;
pub mod theme;
pub mod ui;

pub use paint::{Box2, TextCtx};
pub use ui::{Action, Hit, Mode, Phase, Ui};

/// 把一屏渲染成一张图片。
///
/// `scale` 是 DPI 系数（1.0 = 96 DPI）。窗口、打印预览、开发预览都用它，
/// 保证「看到的就是装出来的样子」。
pub fn render(ui: &Ui, text: &mut TextCtx, scale: f32) -> tiny_skia::Pixmap {
    let w = (theme::WINDOW_W * scale).round().max(1.0) as u32;
    let h = (theme::WINDOW_H * scale).round().max(1.0) as u32;
    let mut pixmap = tiny_skia::Pixmap::new(w, h).expect("创建画布失败");
    ui.draw(&mut pixmap, text, scale);
    pixmap
}
