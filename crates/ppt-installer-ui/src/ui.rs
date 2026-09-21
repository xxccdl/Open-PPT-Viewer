//! 安装程序的界面。
//!
//! # 版式：照 WPS 那种「一屏主视觉」来做
//!
//! ```text
//!                                          ×
//!                    ┌──────────┐
//!                    │   标识    │          ← 居中大标识
//!                    └──────────┘
//!                     OpenPPTView           ← 产品名，大而粗
//!                  课 件 讲 演 器           ← 标语，字距拉开
//!                  (  立 即 安 装  )        ← 描边胶囊按钮，整屏唯一主角
//!                版本 0.1.0 · Windows 10/11
//!
//!  ☐ 我已阅读并同意《许可协议》   安装和存储位置 | 更多设置
//! ```
//!
//! 目标用户是不太会用电脑的老师，所以这一屏上只放一个决定：**点那个按钮**。
//! 「装到哪」「怎么操作」这类问题挪到「更多设置」里，默认值已经能用；
//! 许可协议放在左下角，勾一下就行。
//!
//! # 为什么不引 UI 框架
//!
//! 安装程序要在**没装 WebView2** 的干净系统上跑起来（它自己就负责装 WebView2），
//! 所以不能用网页界面；标准控件又拼不出这种版式。于是自己画：
//! 图元在 `paint`，本文件负责布局、状态与命中测试。

use tiny_skia::{Color, Pixmap};

use crate::paint::{self, Box2, TextCtx};
use crate::theme;

/// 老师选的「平时怎么操作电脑」，与安装后写进注册表的值一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Auto,
    Touch,
    Mouse,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Auto => "auto",
            Mode::Touch => "touch",
            Mode::Mouse => "mouse",
        }
    }
}

/// 当前处于哪一屏。
#[derive(Debug, Clone, PartialEq)]
pub enum Phase {
    /// 主视觉：标识 + 名字 + 一个大按钮
    Setup,
    /// 更多设置：装到哪、怎么操作
    Options,
    /// 正在安装
    Installing,
    /// 装好了
    Done { launch: bool },
    /// 卸载完成
    Uninstalled,
    /// 出错了
    Failed(String),
}

/// 鼠标落在哪个控件上。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Close,
    /// 主按钮（安装 / 完成 / 关闭）
    BigButton,
    /// 「更多设置」
    More,
    /// 「安装和存储位置」
    PathLink,
    /// 设置页的「返回」
    Back,
    /// 换一个文件夹
    Browse,
    /// 操作方式三选一
    Mode(Mode),
    /// 桌面快捷方式
    Desktop,
    /// 立即打开
    Launch,
    /// 同意许可协议
    License,
    /// 展开许可协议
    LicenseLink,
    /// 关掉许可协议浮层
    LicenseOk,
}

/// 界面向外抛出的意图。
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    None,
    /// 开始安装
    Install,
    /// 换一个安装目录
    Browse,
    /// 取消 / 关窗
    Close,
    /// 中途取消安装
    Cancel,
}

pub struct Ui {
    pub phase: Phase,
    pub path: String,
    pub mode: Mode,
    pub desktop_shortcut: bool,
    pub launch_after: bool,
    /// 勾了「我已阅读并同意许可协议」
    pub license_agreed: bool,
    /// 许可协议浮层是否打开
    pub license_shown: bool,
    /// 0.0 ~ 1.0
    pub progress: f32,
    /// 当前在做什么（进度屏上的一行字）
    pub step: String,
    pub version: String,
    /// 勾选框没勾时点安装，这里放一句提示
    pub warn: String,
    hover: Option<Hit>,
    pressed: Option<Hit>,
}

impl Ui {
    pub fn new(version: impl Into<String>, default_path: impl Into<String>) -> Ui {
        Ui {
            phase: Phase::Setup,
            path: default_path.into(),
            mode: Mode::Auto,
            desktop_shortcut: true,
            launch_after: true,
            license_agreed: false,
            license_shown: false,
            progress: 0.0,
            step: String::new(),
            version: version.into(),
            warn: String::new(),
            hover: None,
            pressed: None,
        }
    }

    // -----------------------------------------------------------------------
    // 布局（全部按 96 DPI 的设计坐标，画之前整体乘 DPI 系数）
    // -----------------------------------------------------------------------

    fn center_box(&self, w: f32, y: f32, h: f32) -> Box2 {
        Box2::new((theme::WINDOW_W - w) / 2.0, y, w, h)
    }

    pub fn mark_box(&self) -> Box2 {
        self.center_box(118.0, 62.0, 118.0)
    }

    pub fn button_box(&self) -> Box2 {
        match self.phase {
            // 主视觉：胶囊按钮，居中
            Phase::Setup => self.center_box(258.0, 344.0, 64.0),
            // 设置页与结果页：整行的大按钮，落在底部
            _ => Box2::new(
                theme::MARGIN,
                theme::WINDOW_H - theme::MARGIN - theme::BIG_BUTTON_H,
                theme::WINDOW_W - theme::MARGIN * 2.0,
                theme::BIG_BUTTON_H,
            ),
        }
    }

    fn close_box(&self) -> Box2 {
        Box2::new(theme::WINDOW_W - 20.0 - 34.0, 20.0, 34.0, 34.0)
    }

    fn license_box(&self) -> Box2 {
        Box2::new(theme::MARGIN, theme::WINDOW_H - 52.0, 24.0, 24.0)
    }

    fn license_link_box(&self) -> Box2 {
        Box2::new(theme::MARGIN + 34.0, theme::WINDOW_H - 52.0, 210.0, 24.0)
    }

    fn bottom_links(&self) -> (Box2, Box2) {
        let h = 24.0;
        let y = theme::WINDOW_H - 52.0;
        let more = Box2::new(theme::WINDOW_W - theme::MARGIN - 76.0, y, 76.0, h);
        let path = Box2::new(theme::WINDOW_W - theme::MARGIN - 76.0 - 12.0 - 116.0, y, 116.0, h);
        (path, more)
    }

    fn back_box(&self) -> Box2 {
        Box2::new(theme::MARGIN, 36.0, 76.0, 30.0)
    }

    /// 设置页：安装位置那一行
    fn path_field(&self) -> Box2 {
        let browse_w = 118.0;
        Box2::new(
            theme::MARGIN,
            190.0,
            theme::WINDOW_W - theme::MARGIN * 2.0 - browse_w - 12.0,
            theme::FIELD_H,
        )
    }

    fn browse_box(&self) -> Box2 {
        let f = self.path_field();
        Box2::new(f.x + f.w + 12.0, f.y, 118.0, f.h)
    }

    fn chip_box(&self, m: Mode) -> Box2 {
        let gap = 12.0;
        let total = theme::WINDOW_W - theme::MARGIN * 2.0;
        let w = (total - gap * 2.0) / 3.0;
        let idx = match m {
            Mode::Auto => 0.0,
            Mode::Touch => 1.0,
            Mode::Mouse => 2.0,
        };
        Box2::new(theme::MARGIN + (w + gap) * idx, 302.0, w, theme::CHIP_H)
    }

    fn desktop_box(&self) -> Box2 {
        Box2::new(theme::MARGIN, 396.0, 26.0, 26.0)
    }

    fn launch_box(&self) -> Box2 {
        Box2::new(theme::MARGIN, 396.0, 26.0, 26.0)
    }

    /// 许可协议浮层里的「知道了」按钮
    fn license_ok_box(&self) -> Box2 {
        self.center_box(180.0, theme::WINDOW_H - 116.0, 52.0)
    }

    // -----------------------------------------------------------------------
    // 命中测试与交互
    // -----------------------------------------------------------------------

    /// 鼠标坐标与 `s` 都是设备像素 / 系数；`s` 用来把设计坐标换算过去。
    pub fn hit(&self, x: f32, y: f32, s: f32) -> Option<Hit> {
        let scaled = |b: Box2| Box2::new(b.x * s, b.y * s, b.w * s, b.h * s);
        let hit_box = |b: Box2| scaled(b).contains(x, y);

        // 许可协议浮层压在一切之上
        if self.license_shown {
            if hit_box(self.license_ok_box()) {
                return Some(Hit::LicenseOk);
            }
            return None;
        }

        if hit_box(self.button_box()) {
            return Some(Hit::BigButton);
        }
        if hit_box(self.close_box()) {
            return Some(Hit::Close);
        }

        match self.phase {
            Phase::Setup => {
                let (p, m) = self.bottom_links();
                if hit_box(p) {
                    return Some(Hit::PathLink);
                }
                if hit_box(m) {
                    return Some(Hit::More);
                }
                if hit_box(self.license_box()) || hit_box(self.license_link_box()) {
                    return Some(Hit::License);
                }
                // 协议链接单独一块（点字看全文）
                if x > (theme::MARGIN + 34.0) * s && hit_box(self.license_link_box()) {
                    return Some(Hit::LicenseLink);
                }
                None
            }
            Phase::Options => {
                if hit_box(self.back_box()) {
                    return Some(Hit::Back);
                }
                if hit_box(self.path_field()) {
                    return Some(Hit::Browse);
                }
                if hit_box(self.browse_box()) {
                    return Some(Hit::Browse);
                }
                for m in [Mode::Auto, Mode::Touch, Mode::Mouse] {
                    if hit_box(self.chip_box(m)) {
                        return Some(Hit::Mode(m));
                    }
                }
                let row = Box2::new(
                    theme::MARGIN,
                    self.desktop_box().y - 4.0,
                    theme::WINDOW_W - theme::MARGIN * 2.0,
                    34.0,
                );
                if hit_box(row) {
                    return Some(Hit::Desktop);
                }
                None
            }
            Phase::Done { .. } => {
                let row = Box2::new(
                    theme::MARGIN,
                    self.launch_box().y - 4.0,
                    theme::WINDOW_W - theme::MARGIN * 2.0,
                    34.0,
                );
                if hit_box(row) {
                    return Some(Hit::Launch);
                }
                None
            }
            Phase::Installing | Phase::Uninstalled | Phase::Failed(_) => None,
        }
    }

    pub fn set_hover(&mut self, h: Option<Hit>) -> bool {
        if self.hover != h {
            self.hover = h;
            true
        } else {
            false
        }
    }

    pub fn hover(&self) -> Option<Hit> {
        self.hover
    }

    pub fn set_pressed(&mut self, h: Option<Hit>) {
        self.pressed = h;
    }

    pub fn pressed(&self) -> Option<Hit> {
        self.pressed
    }

    pub fn click(&mut self, h: Hit) -> Action {
        match h {
            Hit::Close => Action::Close,
            Hit::LicenseOk => {
                self.license_shown = false;
                Action::None
            }
            Hit::BigButton => match self.phase.clone() {
                Phase::Setup | Phase::Options => {
                    if !self.license_agreed {
                        // 没勾协议就先别装：把话说清楚，别让人点了没反应
                        self.warn = "请先勾选左下角的「我已阅读并同意《用户许可协议》」".to_string();
                        return Action::None;
                    }
                    Action::Install
                }
                Phase::Done { .. } | Phase::Uninstalled => Action::Close,
                Phase::Failed(_) => Action::Close,
                _ => Action::None,
            },
            Hit::More => {
                self.phase = Phase::Options;
                Action::None
            }
            Hit::Back => {
                self.phase = Phase::Setup;
                Action::None
            }
            Hit::PathLink => {
                self.phase = Phase::Options;
                Action::None
            }
            Hit::Browse => Action::Browse,
            Hit::Mode(m) => {
                self.mode = m;
                Action::None
            }
            Hit::Desktop => {
                self.desktop_shortcut = !self.desktop_shortcut;
                Action::None
            }
            Hit::Launch => {
                self.launch_after = !self.launch_after;
                Action::None
            }
            Hit::License => {
                self.license_agreed = !self.license_agreed;
                if self.license_agreed {
                    self.warn.clear();
                }
                Action::None
            }
            Hit::LicenseLink => {
                self.license_shown = true;
                Action::None
            }
        }
    }

    pub fn wants_hand(&self) -> bool {
        self.hover.is_some()
    }

    // -----------------------------------------------------------------------
    // 绘制
    // -----------------------------------------------------------------------

    pub fn draw(&self, pixmap: &mut Pixmap, t: &mut TextCtx, s: f32) {
        pixmap.fill(theme::page());
        paint::wash_background(pixmap, s, theme::WINDOW_W, theme::WINDOW_H);

        match self.phase {
            Phase::Setup => self.draw_setup(pixmap, t, s),
            Phase::Options => self.draw_options(pixmap, t, s),
            Phase::Installing => self.draw_progress(pixmap, t, s),
            Phase::Done { .. } => self.draw_done(pixmap, t, s),
            Phase::Uninstalled => self.draw_uninstalled(pixmap, t, s),
            Phase::Failed(ref msg) => self.draw_failed(pixmap, t, s, msg),
        }

        if self.license_shown {
            self.draw_license(pixmap, t, s);
        }
    }

    /// 主视觉。
    fn draw_setup(&self, pixmap: &mut Pixmap, t: &mut TextCtx, s: f32) {
        let mark = self.mark_box();
        paint::draw_mark(pixmap, mark.x * s, mark.y * s, mark.w * s);

        // 产品名
        let w = t.measure_w("OpenPPTView", 38.0, true) * s;
        t.draw(
            pixmap,
            "OpenPPTView",
            (theme::WINDOW_W * s - w) / 2.0,
            206.0 * s,
            38.0,
            true,
            theme::ink(),
            700.0 * s,
            s,
        );

        // 标语：字距拉开（靠插入空格实现，和 WPS 的做法一样）
        let tag = "课 件 讲 演 器";
        let w = t.measure_w(tag, 16.0, false) * s;
        t.draw(
            pixmap,
            tag,
            (theme::WINDOW_W * s - w) / 2.0,
            272.0 * s,
            16.0,
            false,
            theme::ink_dim(),
            700.0 * s,
            s,
        );

        self.draw_button(pixmap, t, "立 即 安 装", s);

        // 版本行
        let ver = format!("版本 {} · 适用于 Windows 10 / 11", self.version);
        let w = t.measure_w(&ver, 12.0, false) * s;
        t.draw(
            pixmap,
            &ver,
            (theme::WINDOW_W * s - w) / 2.0,
            428.0 * s,
            12.0,
            false,
            theme::ink_faint(),
            700.0 * s,
            s,
        );

        // 没勾协议时的那句提示
        if !self.warn.is_empty() {
            let w = t.measure_w(&self.warn, 12.0, false) * s;
            t.draw(
                pixmap,
                &self.warn,
                (theme::WINDOW_W * s - w) / 2.0,
                462.0 * s,
                12.0,
                false,
                theme::coral(),
                700.0 * s,
                s,
            );
        }

        self.draw_bottom_bar(pixmap, t, s);
        self.draw_close(pixmap, s);
    }

    /// 底部一行：左边协议勾选，右边两个链接。
    fn draw_bottom_bar(&self, pixmap: &mut Pixmap, t: &mut TextCtx, s: f32) {
        let cb = self.license_box();
        paint::draw_checkbox(
            pixmap,
            Box2::new(cb.x * s, cb.y * s, cb.w * s, cb.h * s),
            self.license_agreed,
            self.hover == Some(Hit::License),
            s,
        );
        t.draw(
            pixmap,
            "我已阅读并同意《用户许可协议》",
            (cb.x + 34.0) * s,
            (cb.y + 1.0) * s,
            13.0,
            false,
            theme::ink_dim(),
            400.0 * s,
            s,
        );

        let (path, more) = self.bottom_links();
        let links = [
            ("安装和存储位置", path, Hit::PathLink, "  |  "),
            ("更多设置", more, Hit::More, ""),
        ];
        for (text, b, hit, sep) in links {
            if !sep.is_empty() {
                t.draw(
                    pixmap,
                    sep,
                    b.x * s - 6.0 * s,
                    (b.y + 2.0) * s,
                    13.0,
                    false,
                    theme::line(),
                    40.0 * s,
                    s,
                );
            }
            let color = if self.hover == Some(hit) {
                theme::link()
            } else {
                theme::ink_dim()
            };
            t.draw(
                pixmap,
                text,
                b.x * s,
                (b.y + 2.0) * s,
                13.0,
                false,
                color,
                b.w * s,
                s,
            );
        }
    }

    /// 更多设置页：装到哪 + 怎么操作。
    fn draw_options(&self, pixmap: &mut Pixmap, t: &mut TextCtx, s: f32) {
        let back = self.back_box();
        let color = if self.hover == Some(Hit::Back) {
            theme::link()
        } else {
            theme::ink_dim()
        };
        t.draw(pixmap, "← 返回", back.x * s, (back.y + 4.0) * s, 14.0, false, color, 200.0 * s, s);

        t.draw(
            pixmap,
            "安装设置",
            theme::MARGIN * s,
            74.0 * s,
            26.0,
            true,
            theme::ink(),
            500.0 * s,
            s,
        );

        t.draw(
            pixmap,
            "装到哪个文件夹：",
            theme::MARGIN * s,
            162.0 * s,
            14.0,
            true,
            theme::ink(),
            500.0 * s,
            s,
        );

        let field = self.path_field();
        let fb = Box2::new(field.x * s, field.y * s, field.w * s, field.h * s);
        paint::fill_round(pixmap, fb, theme::R_FIELD * s, theme::page());
        paint::stroke_round(pixmap, fb, theme::R_FIELD * s, theme::line(), 1.0 * s);
        t.draw(
            pixmap,
            &self.path,
            (field.x + 16.0) * s,
            (field.y + (field.h - 17.0) / 2.0) * s,
            14.0,
            false,
            theme::ink(),
            (field.w - 32.0) * s,
            s,
        );

        let bb = self.browse_box();
        let browse = Box2::new(bb.x * s, bb.y * s, bb.w * s, bb.h * s);
        paint::fill_round(
            pixmap,
            browse,
            theme::R_FIELD * s,
            if self.hover == Some(Hit::Browse) {
                theme::chip_bg_hover()
            } else {
                theme::chip_bg()
            },
        );
        paint::stroke_round(pixmap, browse, theme::R_FIELD * s, theme::line(), 1.0 * s);
        centered_text(t, pixmap, "换一个…", bb, 14.0, false, theme::ink(), s);

        t.draw(
            pixmap,
            "平时您怎么操作这台电脑？",
            theme::MARGIN * s,
            272.0 * s,
            14.0,
            true,
            theme::ink(),
            500.0 * s,
            s,
        );

        for (mode, text) in [
            (Mode::Auto, "说不准（推荐）"),
            (Mode::Touch, "用手指点屏幕"),
            (Mode::Mouse, "用鼠标键盘"),
        ] {
            let b = self.chip_box(mode);
            let on = self.mode == mode;
            let hovered = self.hover == Some(Hit::Mode(mode));
            let dev = Box2::new(b.x * s, b.y * s, b.w * s, b.h * s);
            paint::fill_round(
                pixmap,
                dev,
                theme::R_CHIP * s,
                if on {
                    theme::chip_on_bg()
                } else if hovered {
                    theme::chip_bg_hover()
                } else {
                    theme::chip_bg()
                },
            );
            paint::stroke_round(
                pixmap,
                dev,
                theme::R_CHIP * s,
                if on { theme::chip_on_border() } else { theme::line() },
                if on { 2.0 * s } else { 1.0 * s },
            );
            let dot = Box2::new(b.x + 16.0, b.y + (b.h - 20.0) / 2.0, 20.0, 20.0);
            if on {
                paint::fill_circle(
                    pixmap,
                    (dot.x + dot.w / 2.0) * s,
                    (dot.y + dot.h / 2.0) * s,
                    dot.w / 2.0 * s,
                    theme::chip_on_border(),
                );
                paint::fill_circle(
                    pixmap,
                    (dot.x + dot.w / 2.0) * s,
                    (dot.y + dot.h / 2.0) * s,
                    (dot.w / 2.0 - 4.5) * s,
                    theme::white(),
                );
            } else {
                paint::stroke_round(pixmap, Box2::new(dot.x * s, dot.y * s, dot.w * s, dot.h * s), dot.w / 2.0 * s, theme::ink_faint(), 1.5 * s);
            }
            t.draw(
                pixmap,
                text,
                (dot.x + dot.w + 10.0) * s,
                (b.y + (b.h - 17.0) / 2.0) * s,
                14.0,
                on,
                theme::ink(),
                (b.x + b.w - dot.x - dot.w - 14.0) * s,
                s,
            );
        }

        let cb = self.desktop_box();
        paint::draw_checkbox(
            pixmap,
            Box2::new(cb.x * s, cb.y * s, cb.w * s, cb.h * s),
            self.desktop_shortcut,
            self.hover == Some(Hit::Desktop),
            s,
        );
        t.draw(
            pixmap,
            "在桌面上放一个快捷方式（方便找）",
            (cb.x + cb.w + 12.0) * s,
            (cb.y + 1.0) * s,
            14.0,
            false,
            theme::ink(),
            420.0 * s,
            s,
        );

        self.draw_button(pixmap, t, "开 始 安 装", s);
    }

    fn draw_progress(&self, pixmap: &mut Pixmap, t: &mut TextCtx, s: f32) {
        // 安装中：标识缩小居中，下面一条进度条
        let mark = Box2::new((theme::WINDOW_W - 76.0) / 2.0, 96.0, 76.0, 76.0);
        paint::draw_mark(pixmap, mark.x * s, mark.y * s, mark.w * s);

        let title = "正在安装";
        let w = t.measure_w(title, 26.0, true) * s;
        t.draw(
            pixmap,
            title,
            (theme::WINDOW_W * s - w) / 2.0,
            208.0 * s,
            26.0,
            true,
            theme::ink(),
            600.0 * s,
            s,
        );

        let step = if self.step.is_empty() {
            "准备好就开始"
        } else {
            self.step.as_str()
        };
        let w = t.measure_w(step, 14.0, false) * s;
        t.draw(
            pixmap,
            step,
            (theme::WINDOW_W * s - w) / 2.0,
            254.0 * s,
            14.0,
            false,
            theme::ink_dim(),
            640.0 * s,
            s,
        );

        let bar = Box2::new((theme::WINDOW_W - 520.0) / 2.0, 302.0, 520.0, 12.0);
        paint::fill_round(
            pixmap,
            Box2::new(bar.x * s, bar.y * s, bar.w * s, bar.h * s),
            bar.h / 2.0 * s,
            theme::track(),
        );
        let p = self.progress.clamp(0.0, 1.0);
        if p > 0.0 {
            let mut fill = bar;
            fill.w = (bar.w * p).max(bar.h);
            paint::fill_round_gradient(
                pixmap,
                Box2::new(fill.x * s, fill.y * s, fill.w * s, fill.h * s),
                bar.h / 2.0 * s,
                theme::primary_top(),
                theme::primary_bottom(),
            );
        }

        let pct = format!("{}%", (p * 100.0).round() as i32);
        let w = t.measure_w(&pct, 12.0, false) * s;
        t.draw(
            pixmap,
            &pct,
            (theme::WINDOW_W * s - w) / 2.0,
            330.0 * s,
            12.0,
            false,
            theme::ink_faint(),
            200.0 * s,
            s,
        );

        let hint = "几秒钟就好，请不要关掉这个窗口。";
        let w = t.measure_w(hint, 13.0, false) * s;
        t.draw(
            pixmap,
            hint,
            (theme::WINDOW_W * s - w) / 2.0,
            380.0 * s,
            13.0,
            false,
            theme::ink_dim(),
            600.0 * s,
            s,
        );
    }

    fn draw_done(&self, pixmap: &mut Pixmap, t: &mut TextCtx, s: f32) {
        self.draw_result_head(
            pixmap,
            t,
            s,
            true,
            "安装完成",
            "OpenPPTView 已经装到这台电脑上了。",
            "以后双击 .pptx 文件就能直接打开。\n放映的时候，按 F5 开始，按 Esc 退出。",
        );

        if let Phase::Done { launch } = self.phase {
            let cb = self.launch_box();
            paint::draw_checkbox(
                pixmap,
                Box2::new(cb.x * s, cb.y * s, cb.w * s, cb.h * s),
                launch,
                self.hover == Some(Hit::Launch),
                s,
            );
            t.draw(
                pixmap,
                "装好以后马上就打开一个试试",
                (cb.x + cb.w + 12.0) * s,
                (cb.y + 1.0) * s,
                14.0,
                false,
                theme::ink(),
                460.0 * s,
                s,
            );
        }
        self.draw_button(pixmap, t, "完 成", s);
    }

    fn draw_uninstalled(&self, pixmap: &mut Pixmap, t: &mut TextCtx, s: f32) {
        self.draw_result_head(
            pixmap,
            t,
            s,
            true,
            "卸载完成",
            "OpenPPTView 已经从这台电脑上删除。",
            "桌面和开始菜单的快捷方式也一起清掉了。",
        );
        self.draw_button(pixmap, t, "关 闭", s);
    }

    fn draw_failed(&self, pixmap: &mut Pixmap, t: &mut TextCtx, s: f32, msg: &str) {
        self.draw_result_head(
            pixmap,
            t,
            s,
            false,
            "安装未完成",
            msg,
            "详细信息已记入安装日志，再次出现时可以把它提供给帮忙的人。",
        );
        self.draw_button(pixmap, t, "关 闭", s);
    }

    /// 结果页共用的头部：一个圆圆的标记 + 两句说明。
    fn draw_result_head(
        &self,
        pixmap: &mut Pixmap,
        t: &mut TextCtx,
        s: f32,
        ok: bool,
        title: &str,
        line1: &str,
        line2: &str,
    ) {
        let circle = self.center_box(88.0, 96.0, 88.0);
        let dev = Box2::new(circle.x * s, circle.y * s, circle.w * s, circle.h * s);
        if ok {
            paint::fill_round_gradient(
                pixmap,
                dev,
                circle.w / 2.0 * s,
                theme::ok_green(),
                Color::from_rgba8(0x18, 0x93, 0x50, 255),
            );
            paint::draw_tick(pixmap, dev.inset(20.0 * s), theme::white(), 6.0 * s);
        } else {
            paint::fill_round(pixmap, dev, circle.w / 2.0 * s, theme::coral());
            paint::draw_cross(pixmap, dev.inset(26.0 * s), theme::white(), 6.0 * s);
        }

        let w = t.measure_w(title, 30.0, true) * s;
        t.draw(
            pixmap,
            title,
            (theme::WINDOW_W * s - w) / 2.0,
            214.0 * s,
            30.0,
            true,
            theme::ink(),
            700.0 * s,
            s,
        );

        let w = t.measure_w(line1, 15.0, false) * s;
        t.draw(
            pixmap,
            line1,
            (theme::WINDOW_W * s - w) / 2.0,
            276.0 * s,
            15.0,
            false,
            theme::ink(),
            (theme::WINDOW_W - 120.0) * s,
            s,
        );
        let w = t.measure_w(line2, 13.0, false) * s;
        t.draw(
            pixmap,
            line2,
            (theme::WINDOW_W * s - w) / 2.0,
            308.0 * s,
            13.0,
            false,
            theme::ink_dim(),
            (theme::WINDOW_W - 120.0) * s,
            s,
        );
    }

    /// 主按钮。主视觉上是描边胶囊（照 WPS 的样子），设置页与结果页上是实心大按钮。
    fn draw_button(&self, pixmap: &mut Pixmap, t: &mut TextCtx, text: &str, s: f32) {
        let b = self.button_box();
        let dev = Box2::new(b.x * s, b.y * s, b.w * s, b.h * s);
        let hovered = self.hover == Some(Hit::BigButton);
        let pressed = self.pressed == Some(Hit::BigButton);
        let pill = self.phase == Phase::Setup;

        if pill {
            // 描边胶囊：白底蓝框蓝字，悬停时淡蓝填充、按下时实心
            if pressed {
                paint::fill_round_gradient(
                    pixmap,
                    dev,
                    dev.h / 2.0,
                    theme::primary_top(),
                    theme::primary_bottom(),
                );
            } else {
                paint::fill_round(pixmap, dev, dev.h / 2.0, theme::page());
                if hovered {
                    paint::fill_round(pixmap, dev, dev.h / 2.0, theme::primary_soft());
                }
                paint::stroke_round(pixmap, dev, dev.h / 2.0, theme::primary_bottom(), 2.0 * s);
            }
            centered_text(
                t,
                pixmap,
                text,
                b,
                theme::FS_BUTTON,
                true,
                if pressed { theme::white() } else { theme::primary_bottom() },
                s,
            );
        } else {
            let (top, bottom) = if hovered && !pressed {
                (theme::primary_hover_top(), theme::primary_hover_bottom())
            } else {
                (theme::primary_top(), theme::primary_bottom())
            };
            if !pressed {
                paint::soft_shadow(
                    pixmap,
                    Box2::new(dev.x, dev.y + 6.0 * s, dev.w, dev.h),
                    theme::R_BUTTON * s + 4.0 * s,
                    7.0 * s,
                    26,
                );
            }
            paint::fill_round_gradient(pixmap, dev, theme::R_BUTTON * s, top, bottom);
            centered_text(t, pixmap, text, b, theme::FS_BUTTON, true, theme::white(), s);
        }
    }

    fn draw_close(&self, pixmap: &mut Pixmap, s: f32) {
        let b = self.close_box();
        let hovered = self.hover == Some(Hit::Close);
        if hovered {
            paint::fill_round(
                pixmap,
                Box2::new(b.x * s, b.y * s, b.w * s, b.h * s),
                b.w / 2.0 * s,
                theme::chip_bg(),
            );
        }
        paint::draw_cross(
            pixmap,
            Box2::new((b.x + 9.0) * s, (b.y + 9.0) * s, (b.w - 18.0) * s, (b.h - 18.0) * s),
            if hovered { theme::ink() } else { theme::ink_faint() },
            2.0 * s,
        );
    }

    /// 许可协议浮层。
    fn draw_license(&self, pixmap: &mut Pixmap, t: &mut TextCtx, s: f32) {
        // 先把整屏压暗
        paint::fill_round(
            pixmap,
            Box2::new(0.0, 0.0, theme::WINDOW_W * s, theme::WINDOW_H * s),
            0.0,
            Color::from_rgba8(0x10, 0x14, 0x1C, 150),
        );

        let panel = Box2::new(theme::MARGIN, 60.0, theme::WINDOW_W - theme::MARGIN * 2.0, theme::WINDOW_H - 200.0);
        let dev = Box2::new(panel.x * s, panel.y * s, panel.w * s, panel.h * s);
        paint::fill_round(pixmap, dev, 14.0 * s, theme::page());

        t.draw(
            pixmap,
            "用户许可协议",
            (panel.x + 28.0) * s,
            (panel.y + 26.0) * s,
            18.0,
            true,
            theme::ink(),
            500.0 * s,
            s,
        );

        let body = "OpenPPTView 是给学校老师讲课用的课件讲演器，按 MIT 许可协议发布。\n\n\
您可以自由地在自己的电脑上安装、使用它，也可以把它装到学校的机房里。\n\n\
它会做这些事：把程序复制到您选择的文件夹；在桌面和开始菜单放一个快捷方式；\
在系统里登记一个卸载入口；如果系统里没有 WebView2 运行环境，会先把它装上。\n\n\
它不会上传您的课件，也不会收集任何使用数据。课件始终只在这台电脑上。\n\n\
卸载时可以在「设置 → 应用」里找到 OpenPPTView，点卸载即可；\
桌面和开始菜单的快捷方式、注册表里的登记项会一起清掉。";
        t.draw(
            pixmap,
            body,
            (panel.x + 28.0) * s,
            (panel.y + 66.0) * s,
            13.0,
            false,
            theme::ink_dim(),
            panel.w - 56.0,
            s,
        );

        let ok = self.license_ok_box();
        let dev = Box2::new(ok.x * s, ok.y * s, ok.w * s, ok.h * s);
        let hovered = self.hover == Some(Hit::LicenseOk);
        paint::fill_round_gradient(
            pixmap,
            dev,
            dev.h / 2.0,
            if hovered {
                theme::primary_hover_top()
            } else {
                theme::primary_top()
            },
            if hovered {
                theme::primary_hover_bottom()
            } else {
                theme::primary_bottom()
            },
        );
        centered_text(t, pixmap, "知道了", ok, 16.0, true, theme::white(), s);
    }
}

// ---------------------------------------------------------------------------
// 小助手
// ---------------------------------------------------------------------------

fn centered_y(t: &TextCtx, b: Box2, size: f32, s: f32) -> f32 {
    let lh = t.line_height(size) * s;
    b.y * s + (b.h * s - lh) * 0.5
}

fn centered_text(
    t: &mut TextCtx,
    pixmap: &mut Pixmap,
    text: &str,
    b: Box2,
    size: f32,
    bold: bool,
    color: Color,
    s: f32,
) {
    let y = centered_y(t, b, size, s);
    let w = t.measure_w(text, size, bold) * s;
    let x = b.x * s + ((b.w * s - w) * 0.5).max(0.0);
    t.draw(pixmap, text, x, y, size, bold, color, b.w, s);
}
