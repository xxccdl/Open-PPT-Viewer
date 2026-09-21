//! 开发期预览：把安装程序的每一屏渲染成 PNG。
//!
//! ```text
//! cargo run -p ppt-installer-ui --example preview -- <输出目录> [dpi 系数]
//! ```
//!
//! 安装程序的界面改一版就要重装一次才能看到，太慢；这个例子用**同一套绘制代码**
//! 把界面直接落成图片，几百毫秒就能看一遍，改配色和间距不用碰系统。

use ppt_installer_ui::ui::{Phase, Ui};
use ppt_installer_ui::{render, TextCtx};

fn main() {
    let mut args = std::env::args().skip(1);
    let out = args.next().unwrap_or_else(|| ".".to_string());
    let scale: f32 = args.next().and_then(|v| v.parse().ok()).unwrap_or(1.0);

    let mut text = TextCtx::new();

    let shots: Vec<(&str, Phase, f32, &str)> = vec![
        ("1-setup", Phase::Setup, 0.0, ""),
        ("2-options", Phase::Options, 0.0, ""),
        ("3-progress", Phase::Installing, 0.42, "正在复制文件…"),
        ("4-done", Phase::Done { launch: true }, 1.0, ""),
        ("5-uninstalled", Phase::Uninstalled, 1.0, ""),
        (
            "6-failed",
            Phase::Failed("无法写入 C:\\Program Files\\OpenPPTView：拒绝访问。".to_string()),
            0.0,
            "",
        ),
    ];

    for (name, phase, progress, step) in shots {
        let mut ui = Ui::new("0.1.0", "C:\\Program Files\\OpenPPTView");
        ui.phase = phase;
        ui.progress = progress;
        ui.step = step.to_string();
        if name == "1-setup" {
            // 勾上协议、鼠标停在主按钮上：这是最常见的一帧
            ui.license_agreed = true;
            ui.set_hover(Some(ppt_installer_ui::Hit::BigButton));
        }
        let pixmap = render(&ui, &mut text, scale);
        let path = format!("{out}/installer-{name}.png");
        match pixmap.encode_png() {
            Ok(png) => {
                if let Err(e) = std::fs::write(&path, png) {
                    eprintln!("写 {path} 失败：{e}");
                    std::process::exit(1);
                }
                println!("{path}");
            }
            Err(e) => {
                eprintln!("编码 PNG 失败：{e}");
                std::process::exit(1);
            }
        }
    }
}
