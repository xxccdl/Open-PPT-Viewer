//! 构建脚本：给安装程序挂上「以管理员身份运行」+「高 DPI 感知」的清单。
//!
//! 装到 `Program Files` 需要管理员权限，这一步必须由清单声明 ——
//! 不能靠运行时再 `ShellExecute runas`，那样会变成两个进程、进度还得靠 IPC 传。
//!
//! # 为什么留了一个「开发用」清单
//!
//! 带 `requireAdministrator` 的 exe 每次运行都会弹 UAC，自己没法在本机跑起来看效果。
//! 所以 `--no-default-features` 时挂另一份清单（去掉提权、保留 DPI），
//! 用来验证窗口、绘制、静默安装这些不碰系统目录的路径。

fn main() {
    #[cfg(windows)]
    {
        let dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let name = if std::env::var("CARGO_FEATURE_ADMIN_MANIFEST").is_ok() {
            "installer.manifest"
        } else {
            "installer-dev.manifest"
        };
        let manifest = dir.join(name);
        println!("cargo:rerun-if-changed={}", manifest.display());
        println!("cargo:rerun-if-changed=build.rs");
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
        // 别让链接器再塞一份自己的 UAC 段，否则和清单里的申明打架
        println!("cargo:rustc-link-arg=/MANIFESTUAC:NO");
    }
}
