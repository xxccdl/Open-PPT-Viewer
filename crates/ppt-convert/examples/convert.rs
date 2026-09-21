//! 用本机 WPS / Office 内核把一份 `.pptx` 转成矢量 PDF，并打印各阶段耗时。
//!
//! ```text
//! cargo run -p ppt-convert --example convert -- <课件.pptx> [输出.pdf]
//! ```
//!
//! 这是接入主管线之前的「单点验证」：只看这条路通不通、快不快。
//! 耗时会被拆成「创建引擎 / 打开+导出」两段 —— 前者是可以在 App 启动时
//! 预热掉的固定开销，后者才是用户真正要等的。

use std::path::PathBuf;
use std::time::Instant;

use ppt_convert::{detect_engine, Converter};

fn main() {
    // 让库里的阶段计时（Open / SaveAs 拆分）能打出来 ——
    // 这个拆分是判断「还能不能更快」的唯一依据
    struct PrintLogger;
    impl log::Log for PrintLogger {
        fn enabled(&self, m: &log::Metadata) -> bool {
            m.level() <= log::Level::Info
        }
        fn log(&self, r: &log::Record) {
            if self.enabled(r.metadata()) {
                println!("[{}] {}", r.level(), r.args());
            }
        }
        fn flush(&self) {}
    }
    static LOGGER: PrintLogger = PrintLogger;
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Info);

    let mut args = std::env::args().skip(1);
    let Some(input) = args.next() else {
        eprintln!("用法：convert <课件.pptx> [输出.pdf]");
        std::process::exit(2);
    };
    let input = PathBuf::from(input);
    let output = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| input.with_extension("pdf"));

    let Some(engine) = detect_engine() else {
        eprintln!("本机既没有 WPS 也没有 Microsoft Office，无法转换");
        std::process::exit(1);
    };
    println!("使用引擎：{}", engine.display_name());

    // 模拟「App 启动时预热」：这一步的耗时用户感知不到
    let t = Instant::now();
    let converter = Converter::spawn(engine);
    println!("转换线程已启动：{:.0?}", t.elapsed());

    let t = Instant::now();
    match converter.convert(&input, &output) {
        Ok(()) => {
            let size = std::fs::metadata(&output)
                .map(|m| m.len() as f64 / 1048576.0)
                .unwrap_or(0.0);
            println!(
                "转换成功：{:.1?} → {}（{:.1} MB）",
                t.elapsed(),
                output.display(),
                size
            );
        }
        Err(e) => {
            eprintln!("转换失败（耗时 {:.1?}）：{e}", t.elapsed());
            std::process::exit(1);
        }
    }
}
