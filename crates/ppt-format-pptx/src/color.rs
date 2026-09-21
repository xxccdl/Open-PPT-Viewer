//! 颜色解析：颜色值类型、主题色解析、颜色变换。
//!
//! OOXML 的颜色是「一个颜色值 + 一串变换」的组合，例如：
//!
//! ```xml
//! <a:solidFill>
//!   <a:schemeClr val="accent1">
//!     <a:lumMod val="75000"/>
//!     <a:lumOff val="25000"/>
//!   </a:schemeClr>
//! </a:solidFill>
//! ```
//!
//! 要得到最终 RGB，必须先解析 `schemeClr` 到主题色，再**按顺序**应用变换。
//! 变换顺序不可交换（先 `lumMod` 再 `lumOff` 与反过来结果不同），
//! 因此这里严格按 XML 中出现顺序累积。

use ppt_core::scene::{Color, GradientFlip, GradientKind, GradientStop, RelativeRect};
use ppt_core::XmlNode;

/// 主题的 12 个颜色槽。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SchemeSlot {
    Bg1,
    Tx1,
    Bg2,
    Tx2,
    Accent1,
    Accent2,
    Accent3,
    Accent4,
    Accent5,
    Accent6,
    Hlink,
    FolHlink,
}

impl SchemeSlot {
    /// 解析 `a:clrScheme` 里的子元素名（`dk1`/`lt1`/`accent1`…）。
    pub fn from_scheme_element(name: &str) -> Option<SchemeSlot> {
        Some(match name {
            "dk1" => SchemeSlot::Tx1,
            "lt1" => SchemeSlot::Bg1,
            "dk2" => SchemeSlot::Tx2,
            "lt2" => SchemeSlot::Bg2,
            "accent1" => SchemeSlot::Accent1,
            "accent2" => SchemeSlot::Accent2,
            "accent3" => SchemeSlot::Accent3,
            "accent4" => SchemeSlot::Accent4,
            "accent5" => SchemeSlot::Accent5,
            "accent6" => SchemeSlot::Accent6,
            "hlink" => SchemeSlot::Hlink,
            "folHlink" => SchemeSlot::FolHlink,
            _ => return None,
        })
    }

    /// 解析 `a:schemeClr/@val` 的取值。
    ///
    /// 注意 `bg1`/`tx1` 等是**映射后的**名称，
    /// 而 `a:clrMap` 负责把它们映射回 `dk1`/`lt1`。
    pub fn from_val(val: &str) -> Option<SchemeSlot> {
        Some(match val {
            "bg1" => SchemeSlot::Bg1,
            "tx1" => SchemeSlot::Tx1,
            "bg2" => SchemeSlot::Bg2,
            "tx2" => SchemeSlot::Tx2,
            "accent1" => SchemeSlot::Accent1,
            "accent2" => SchemeSlot::Accent2,
            "accent3" => SchemeSlot::Accent3,
            "accent4" => SchemeSlot::Accent4,
            "accent5" => SchemeSlot::Accent5,
            "accent6" => SchemeSlot::Accent6,
            "hlink" => SchemeSlot::Hlink,
            "folHlink" => SchemeSlot::FolHlink,
            // 少数课件直接写 dk1/lt1
            "dk1" => SchemeSlot::Tx1,
            "lt1" => SchemeSlot::Bg1,
            "dk2" => SchemeSlot::Tx2,
            "lt2" => SchemeSlot::Bg2,
            _ => return None,
        })
    }
}

/// 主题的 12 个颜色值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorScheme {
    pub bg1: Color,
    pub tx1: Color,
    pub bg2: Color,
    pub tx2: Color,
    pub accent1: Color,
    pub accent2: Color,
    pub accent3: Color,
    pub accent4: Color,
    pub accent5: Color,
    pub accent6: Color,
    pub hlink: Color,
    pub folhlink: Color,
}

impl Default for ColorScheme {
    /// Office 2013+ 默认主题色。
    ///
    /// 主题部件缺失或损坏时用它兜底，比返回全黑更接近课件原貌。
    fn default() -> Self {
        ColorScheme {
            bg1: Color::WHITE,
            tx1: Color::BLACK,
            bg2: Color::rgb(0xEE, 0xEC, 0xE1),
            tx2: Color::rgb(0x44, 0x54, 0x6A),
            accent1: Color::rgb(0x44, 0x72, 0xC4),
            accent2: Color::rgb(0xED, 0x7D, 0x31),
            accent3: Color::rgb(0xA5, 0xA5, 0xA5),
            accent4: Color::rgb(0xFF, 0xC0, 0x00),
            accent5: Color::rgb(0x5B, 0x9B, 0xD5),
            accent6: Color::rgb(0x70, 0xAD, 0x47),
            hlink: Color::rgb(0x05, 0x63, 0xC1),
            folhlink: Color::rgb(0x95, 0x4F, 0x72),
        }
    }
}

impl ColorScheme {
    pub fn get(&self, slot: SchemeSlot) -> Color {
        match slot {
            SchemeSlot::Bg1 => self.bg1,
            SchemeSlot::Tx1 => self.tx1,
            SchemeSlot::Bg2 => self.bg2,
            SchemeSlot::Tx2 => self.tx2,
            SchemeSlot::Accent1 => self.accent1,
            SchemeSlot::Accent2 => self.accent2,
            SchemeSlot::Accent3 => self.accent3,
            SchemeSlot::Accent4 => self.accent4,
            SchemeSlot::Accent5 => self.accent5,
            SchemeSlot::Accent6 => self.accent6,
            SchemeSlot::Hlink => self.hlink,
            SchemeSlot::FolHlink => self.folhlink,
        }
    }

    pub fn set(&mut self, slot: SchemeSlot, color: Color) {
        match slot {
            SchemeSlot::Bg1 => self.bg1 = color,
            SchemeSlot::Tx1 => self.tx1 = color,
            SchemeSlot::Bg2 => self.bg2 = color,
            SchemeSlot::Tx2 => self.tx2 = color,
            SchemeSlot::Accent1 => self.accent1 = color,
            SchemeSlot::Accent2 => self.accent2 = color,
            SchemeSlot::Accent3 => self.accent3 = color,
            SchemeSlot::Accent4 => self.accent4 = color,
            SchemeSlot::Accent5 => self.accent5 = color,
            SchemeSlot::Accent6 => self.accent6 = color,
            SchemeSlot::Hlink => self.hlink = color,
            SchemeSlot::FolHlink => self.folhlink = color,
        }
    }

    /// 从 `ppt/theme/themeN.xml` 的 `a:clrScheme` 解析。
    ///
    /// `clr_map` 是母版的 `a:clrMap`，用于把 `dk1`/`lt1` 映射到 `tx1`/`bg1`
    /// （深色主题的课件会把它反过来）。
    pub fn from_theme_xml(clr_scheme: &XmlNode, clr_map: Option<&ClrMap>) -> ColorScheme {
        let mut raw: [(SchemeSlot, Color); 12] = [
            (SchemeSlot::Tx1, Color::BLACK),
            (SchemeSlot::Bg1, Color::WHITE),
            (SchemeSlot::Tx2, Color::rgb(0x44, 0x54, 0x6A)),
            (SchemeSlot::Bg2, Color::rgb(0xEE, 0xEC, 0xE1)),
            (SchemeSlot::Accent1, Color::rgb(0x44, 0x72, 0xC4)),
            (SchemeSlot::Accent2, Color::rgb(0xED, 0x7D, 0x31)),
            (SchemeSlot::Accent3, Color::rgb(0xA5, 0xA5, 0xA5)),
            (SchemeSlot::Accent4, Color::rgb(0xFF, 0xC0, 0x00)),
            (SchemeSlot::Accent5, Color::rgb(0x5B, 0x9B, 0xD5)),
            (SchemeSlot::Accent6, Color::rgb(0x70, 0xAD, 0x47)),
            (SchemeSlot::Hlink, Color::rgb(0x05, 0x63, 0xC1)),
            (SchemeSlot::FolHlink, Color::rgb(0x95, 0x4F, 0x72)),
        ];

        // 记录主题里 dk1/lt1 等**原始槽位**的颜色
        let mut dk1 = Color::BLACK;
        let mut lt1 = Color::WHITE;
        let mut dk2 = Color::rgb(0x44, 0x54, 0x6A);
        let mut lt2 = Color::rgb(0xEE, 0xEC, 0xE1);

        for child in &clr_scheme.children {
            let Some(slot) = SchemeSlot::from_scheme_element(&child.name) else {
                continue;
            };
            let Some(color) = parse_color_value(child) else {
                continue;
            };
            match child.name.as_str() {
                "dk1" => dk1 = color,
                "lt1" => lt1 = color,
                "dk2" => dk2 = color,
                "lt2" => lt2 = color,
                _ => {
                    // accent/hlink 直接落位
                    if let Some(entry) = raw.iter_mut().find(|(s, _)| *s == slot) {
                        entry.1 = color;
                    }
                }
            }
        }

        // 应用 clrMap：把 dk1/lt1/dk2/lt2 映射到 bg/tx 语义槽
        let map = clr_map.cloned().unwrap_or_default();
        let resolve = |name: &str| -> Color {
            match name {
                "dk1" => dk1,
                "lt1" => lt1,
                "dk2" => dk2,
                "lt2" => lt2,
                "accent1" => raw[4].1,
                "accent2" => raw[5].1,
                "accent3" => raw[6].1,
                "accent4" => raw[7].1,
                "accent5" => raw[8].1,
                "accent6" => raw[9].1,
                "hlink" => raw[10].1,
                "folHlink" => raw[11].1,
                _ => Color::BLACK,
            }
        };

        let mut scheme = ColorScheme::default();
        scheme.bg1 = resolve(&map.bg1);
        scheme.tx1 = resolve(&map.tx1);
        scheme.bg2 = resolve(&map.bg2);
        scheme.tx2 = resolve(&map.tx2);
        scheme.accent1 = resolve(&map.accent1);
        scheme.accent2 = resolve(&map.accent2);
        scheme.accent3 = resolve(&map.accent3);
        scheme.accent4 = resolve(&map.accent4);
        scheme.accent5 = resolve(&map.accent5);
        scheme.accent6 = resolve(&map.accent6);
        scheme.hlink = resolve(&map.hlink);
        scheme.folhlink = resolve(&map.folhlink);
        scheme
    }
}

/// 母版的 `a:clrMap`：语义槽 → 主题槽。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClrMap {
    pub bg1: String,
    pub tx1: String,
    pub bg2: String,
    pub tx2: String,
    pub accent1: String,
    pub accent2: String,
    pub accent3: String,
    pub accent4: String,
    pub accent5: String,
    pub accent6: String,
    pub hlink: String,
    pub folhlink: String,
}

impl Default for ClrMap {
    fn default() -> Self {
        ClrMap {
            bg1: "lt1".into(),
            tx1: "dk1".into(),
            bg2: "lt2".into(),
            tx2: "dk2".into(),
            accent1: "accent1".into(),
            accent2: "accent2".into(),
            accent3: "accent3".into(),
            accent4: "accent4".into(),
            accent5: "accent5".into(),
            accent6: "accent6".into(),
            hlink: "hlink".into(),
            folhlink: "folHlink".into(),
        }
    }
}

impl ClrMap {
    /// 从 `a:clrMap` 元素解析。
    pub fn parse(node: &XmlNode) -> ClrMap {
        let d = ClrMap::default();
        let get = |name: &str, fallback: &str| -> String {
            node.attr(name).unwrap_or(fallback).to_string()
        };
        ClrMap {
            bg1: get("bg1", &d.bg1),
            tx1: get("tx1", &d.tx1),
            bg2: get("bg2", &d.bg2),
            tx2: get("tx2", &d.tx2),
            accent1: get("accent1", &d.accent1),
            accent2: get("accent2", &d.accent2),
            accent3: get("accent3", &d.accent3),
            accent4: get("accent4", &d.accent4),
            accent5: get("accent5", &d.accent5),
            accent6: get("accent6", &d.accent6),
            hlink: get("hlink", &d.hlink),
            folhlink: get("folHlink", &d.folhlink),
        }
    }
}

/// 解析一个「颜色容器」元素的最终颜色。
///
/// 容器指 `a:solidFill`、`a:bgClr` 等，其下第一个子元素是颜色值本身。
/// `scheme` 为 `None` 时主题色退化为黑色（并保持 alpha 等变换生效）。
pub fn parse_solid_fill(node: &XmlNode, scheme: Option<&ColorScheme>) -> Option<Color> {
    let child = node.children.first()?;
    resolve_color_node(child, scheme)
}

/// 解析颜色值节点（`a:srgbClr` / `a:schemeClr` / …）并应用其变换子元素。
pub fn resolve_color_node(node: &XmlNode, scheme: Option<&ColorScheme>) -> Option<Color> {
    let base = match node.name.as_str() {
        "srgbClr" => Color::from_hex(node.attr("val")?)?,
        "schemeClr" => {
            let slot = SchemeSlot::from_val(node.attr("val")?)?;
            match scheme {
                Some(s) => s.get(slot),
                // 主题缺失时用默认主题，避免整块颜色变黑
                None => ColorScheme::default().get(slot),
            }
        }
        "prstClr" => preset_color(node.attr("val")?)?,
        "sysClr" => {
            // 优先用 lastClr（生成时固化的实际颜色），否则按系统语义给默认值
            match node.attr("lastClr").and_then(Color::from_hex) {
                Some(c) => c,
                None => system_color(node.attr("val")?),
            }
        }
        "scrgbClr" => {
            let r = pct_attr(node, "r")?;
            let g = pct_attr(node, "g")?;
            let b = pct_attr(node, "b")?;
            // scrgbClr 是线性 RGB，需转换到 sRGB 伽马空间
            Color::rgb(
                linear_to_srgb(r),
                linear_to_srgb(g),
                linear_to_srgb(b),
            )
        }
        "hslClr" => {
            let h = angle_attr(node, "hue")?;
            let s = pct_attr(node, "sat")? as f32 / 255.0;
            let l = pct_attr(node, "lum")? as f32 / 255.0;
            hsl_to_rgb(h, s, l)
        }
        _ => return None,
    };

    Some(apply_transforms(base, node))
}

/// 按顺序应用颜色变换子元素。
fn apply_transforms(mut color: Color, node: &XmlNode) -> Color {
    for t in &node.children {
        let val = || t.attr_f64("val").unwrap_or(0.0);
        match t.name.as_str() {
            // 透明度类：直接作用在 alpha 通道
            "alpha" => {
                let k = (val() / 100_000.0).clamp(0.0, 1.0);
                color.a = (color.a as f64 * k).round() as u8;
            }
            "alphaMod" => {
                let k = (val() / 100_000.0).clamp(0.0, 10.0);
                color.a = ((color.a as f64 * k).round().clamp(0.0, 255.0)) as u8;
            }
            "alphaOff" => {
                let d = (val() / 100_000.0) * 255.0;
                color.a = ((color.a as f64 + d).round().clamp(0.0, 255.0)) as u8;
            }

            // 亮度类：在 HSL 空间操作
            "lumMod" | "lumOff" | "satMod" | "satOff" | "hueMod" | "hueOff" => {
                let (mut h, mut s, mut l) = rgb_to_hsl(color);
                let v = (val() / 100_000.0) as f32;
                match t.name.as_str() {
                    "lumMod" => l = (l * v).clamp(0.0, 1.0),
                    "lumOff" => l = (l + v).clamp(0.0, 1.0),
                    "satMod" => s = (s * v).clamp(0.0, 1.0),
                    "satOff" => s = (s + v).clamp(0.0, 1.0),
                    "hueMod" => h = (h * v).rem_euclid(360.0),
                    "hueOff" => h = (h + v * 360.0).rem_euclid(360.0),
                    _ => unreachable!(),
                }
                let a = color.a;
                color = hsl_to_rgb(h, s, l);
                color.a = a;
            }

            // tint/shade：向白/向黑线性插值（ECMA-376 定义）
            "tint" => {
                let k = (val() / 100_000.0).clamp(0.0, 1.0);
                color.r = lerp_u8(color.r, 255, k);
                color.g = lerp_u8(color.g, 255, k);
                color.b = lerp_u8(color.b, 255, k);
            }
            "shade" => {
                let k = (val() / 100_000.0).clamp(0.0, 1.0);
                color.r = lerp_u8(color.r, 0, k);
                color.g = lerp_u8(color.g, 0, k);
                color.b = lerp_u8(color.b, 0, k);
            }

            // 色相环旋转
            "comp" => {
                let (h, s, l) = rgb_to_hsl(color);
                let a = color.a;
                color = hsl_to_rgb((h + 180.0).rem_euclid(360.0), s, l);
                color.a = a;
            }
            "inv" => {
                color.r = 255 - color.r;
                color.g = 255 - color.g;
                color.b = 255 - color.b;
            }

            // 单通道调整
            "red" | "redMod" | "redOff" | "green" | "greenMod" | "greenOff" | "blue"
            | "blueMod" | "blueOff" => {
                let v = val() / 100_000.0;
                let apply = |c: u8, kind: &str| -> u8 {
                    let f = c as f64 / 255.0;
                    let r = match kind {
                        "set" => v,
                        "mod" => f * v,
                        "off" => f + v,
                        _ => f,
                    };
                    (r.clamp(0.0, 1.0) * 255.0).round() as u8
                };
                match t.name.as_str() {
                    "red" => color.r = apply(color.r, "set"),
                    "redMod" => color.r = apply(color.r, "mod"),
                    "redOff" => color.r = apply(color.r, "off"),
                    "green" => color.g = apply(color.g, "set"),
                    "greenMod" => color.g = apply(color.g, "mod"),
                    "greenOff" => color.g = apply(color.g, "off"),
                    "blue" => color.b = apply(color.b, "set"),
                    "blueMod" => color.b = apply(color.b, "mod"),
                    "blueOff" => color.b = apply(color.b, "off"),
                    _ => {}
                }
            }

            // 伽马：OOXML 里的 gamma 是「近似 2.2 伽马」的整数近似，
            // 精确复刻意义不大，这里用标准 sRGB 曲线近似
            "gamma" => {
                color.r = srgb_gamma(color.r);
                color.g = srgb_gamma(color.g);
                color.b = srgb_gamma(color.b);
            }
            "invGamma" => {
                color.r = srgb_inv_gamma(color.r);
                color.g = srgb_inv_gamma(color.g);
                color.b = srgb_inv_gamma(color.b);
            }

            _ => {}
        }
    }
    color
}

/// 解析颜色值节点但**忽略变换**（用于主题色表本身，那里不应有变换）。
fn parse_color_value(node: &XmlNode) -> Option<Color> {
    let child = node.children.first()?;
    // 主题色表里的颜色本身不含变换，但为稳妥仍走一遍
    resolve_color_node(child, None)
}

fn lerp_u8(from: u8, to: u8, t: f64) -> u8 {
    let f = from as f64;
    let v = f + (to as f64 - f) * t;
    v.round().clamp(0.0, 255.0) as u8
}

fn pct_attr(node: &XmlNode, name: &str) -> Option<u8> {
    let v = node.attr_f64(name)?;
    // scrgbClr/hslClr 的通道用 1/100000 表示 0..1，也可能写成百分比
    let f = if v > 1000.0 { v / 100_000.0 } else { v / 100.0 };
    Some((f.clamp(0.0, 1.0) * 255.0).round() as u8)
}

fn angle_attr(node: &XmlNode, name: &str) -> Option<f32> {
    let v = node.attr_f64(name)?;
    // 角度用 1/60000 度表示
    Some((v / 60_000.0) as f32)
}

fn linear_to_srgb(v: u8) -> u8 {
    let f = v as f64 / 255.0;
    let s = if f <= 0.003_130_8 {
        12.92 * f
    } else {
        1.055 * f.powf(1.0 / 2.4) - 0.055
    };
    (s.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn srgb_gamma(c: u8) -> u8 {
    let f = c as f64 / 255.0;
    let s = f.powf(1.0 / 2.2);
    (s.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn srgb_inv_gamma(c: u8) -> u8 {
    let f = c as f64 / 255.0;
    let s = f.powf(2.2);
    (s.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// RGB → HSL（H 为 0..360，S/L 为 0..1）。
pub fn rgb_to_hsl(c: Color) -> (f32, f32, f32) {
    let r = c.r as f32 / 255.0;
    let g = c.g as f32 / 255.0;
    let b = c.b as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let d = max - min;

    if d.abs() < f32::EPSILON {
        return (0.0, 0.0, l);
    }

    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };

    let h = if (max - r).abs() < f32::EPSILON {
        60.0 * (((g - b) / d) % 6.0)
    } else if (max - g).abs() < f32::EPSILON {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };

    (h.rem_euclid(360.0), s, l)
}

/// HSL → RGB。
pub fn hsl_to_rgb(h: f32, s: f32, l: f32) -> Color {
    let h = h.rem_euclid(360.0);
    let s = s.clamp(0.0, 1.0);
    let l = l.clamp(0.0, 1.0);

    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;

    let (r1, g1, b1) = match h as u32 / 60 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };

    Color::rgb(
        ((r1 + m).clamp(0.0, 1.0) * 255.0).round() as u8,
        ((g1 + m).clamp(0.0, 1.0) * 255.0).round() as u8,
        ((b1 + m).clamp(0.0, 1.0) * 255.0).round() as u8,
    )
}

/// `a:prstClr` 的预设颜色表（ECMA-376 附录）。
fn preset_color(name: &str) -> Option<Color> {
    let hex = match name {
        "aliceBlue" => 0xF0F8FF,
        "antiqueWhite" => 0xFAEBD7,
        "aqua" => 0x00FFFF,
        "aquamarine" => 0x7FFFD4,
        "azure" => 0xF0FFFF,
        "beige" => 0xF5F5DC,
        "bisque" => 0xFFE4C4,
        "black" => 0x000000,
        "blanchedAlmond" => 0xFFEBCD,
        "blue" => 0x0000FF,
        "blueViolet" => 0x8A2BE2,
        "brown" => 0xA52A2A,
        "burlyWood" => 0xDEB887,
        "cadetBlue" => 0x5F9EA0,
        "chartreuse" => 0x7FFF00,
        "chocolate" => 0xD2691E,
        "coral" => 0xFF7F50,
        "cornflowerBlue" => 0x6495ED,
        "cornsilk" => 0xFFF8DC,
        "crimson" => 0xDC143C,
        "cyan" => 0x00FFFF,
        "darkBlue" => 0x00008B,
        "darkCyan" => 0x008B8B,
        "darkGoldenrod" => 0xB8860B,
        "darkGray" => 0xA9A9A9,
        "darkGreen" => 0x006400,
        "darkGrey" => 0xA9A9A9,
        "darkKhaki" => 0xBDB76B,
        "darkMagenta" => 0x8B008B,
        "darkOliveGreen" => 0x556B2F,
        "darkOrange" => 0xFF8C00,
        "darkOrchid" => 0x9932CC,
        "darkRed" => 0x8B0000,
        "darkSalmon" => 0xE9967A,
        "darkSeaGreen" => 0x8FBC8F,
        "darkSlateBlue" => 0x483D8B,
        "darkSlateGray" => 0x2F4F4F,
        "darkSlateGrey" => 0x2F4F4F,
        "darkTurquoise" => 0x00CED1,
        "darkViolet" => 0x9400D3,
        "deepPink" => 0xFF1493,
        "deepSkyBlue" => 0x00BFFF,
        "dimGray" => 0x696969,
        "dimGrey" => 0x696969,
        "dkBlue" => 0x00008B,
        "dkCyan" => 0x008B8B,
        "dkGoldenrod" => 0xB8860B,
        "dkGray" => 0xA9A9A9,
        "dkGreen" => 0x006400,
        "dkGrey" => 0xA9A9A9,
        "dkKhaki" => 0xBDB76B,
        "dkMagenta" => 0x8B008B,
        "dkOliveGreen" => 0x556B2F,
        "dkOrange" => 0xFF8C00,
        "dkOrchid" => 0x9932CC,
        "dkRed" => 0x8B0000,
        "dkSalmon" => 0xE9967A,
        "dkSeaGreen" => 0x8FBC8B,
        "dkSlateBlue" => 0x483D8B,
        "dkSlateGray" => 0x2F4F4F,
        "dkSlateGrey" => 0x2F4F4F,
        "dkTurquoise" => 0x00CED1,
        "dkViolet" => 0x9400D3,
        "dodgerBlue" => 0x1E90FF,
        "firebrick" => 0xB22222,
        "floralWhite" => 0xFFFAF0,
        "forestGreen" => 0x228B22,
        "fuchsia" => 0xFF00FF,
        "gainsboro" => 0xDCDCDC,
        "ghostWhite" => 0xF8F8FF,
        "gold" => 0xFFD700,
        "goldenrod" => 0xDAA520,
        "gray" => 0x808080,
        "green" => 0x008000,
        "greenYellow" => 0xADFF2F,
        "grey" => 0x808080,
        "honeydew" => 0xF0FFF0,
        "hotPink" => 0xFF69B4,
        "indianRed" => 0xCD5C5C,
        "indigo" => 0x4B0082,
        "ivory" => 0xFFFFF0,
        "khaki" => 0xF0E68C,
        "lavender" => 0xE6E6FA,
        "lavenderBlush" => 0xFFF0F5,
        "lawnGreen" => 0x7CFC00,
        "lemonChiffon" => 0xFFFACD,
        "lightBlue" => 0xADD8E6,
        "lightCoral" => 0xF08080,
        "lightCyan" => 0xE0FFFF,
        "lightGoldenrodYellow" => 0xFAFAD2,
        "lightGray" => 0xD3D3D3,
        "lightGreen" => 0x90EE90,
        "lightGrey" => 0xD3D3D3,
        "lightPink" => 0xFFB6C1,
        "lightSalmon" => 0xFFA07A,
        "lightSeaGreen" => 0x20B2AA,
        "lightSkyBlue" => 0x87CEFA,
        "lightSlateGray" => 0x778899,
        "lightSlateGrey" => 0x778899,
        "lightSteelBlue" => 0xB0C4DE,
        "lightYellow" => 0xFFFFE0,
        "lime" => 0x00FF00,
        "limeGreen" => 0x32CD32,
        "linen" => 0xFAF0E6,
        "ltBlue" => 0xADD8E6,
        "ltCoral" => 0xF08080,
        "ltCyan" => 0xE0FFFF,
        "ltGoldenrodYellow" => 0xFAFAD2,
        "ltGray" => 0xD3D3D3,
        "ltGreen" => 0x90EE90,
        "ltGrey" => 0xD3D3D3,
        "ltPink" => 0xFFB6C1,
        "ltSalmon" => 0xFFA07A,
        "ltSeaGreen" => 0x20B2AA,
        "ltSkyBlue" => 0x87CEFA,
        "ltSlateGray" => 0x778899,
        "ltSlateGrey" => 0x778899,
        "ltSteelBlue" => 0xB0C4DE,
        "ltYellow" => 0xFFFFE0,
        "magenta" => 0xFF00FF,
        "maroon" => 0x800000,
        "medAquamarine" => 0x66CDAA,
        "medBlue" => 0x0000CD,
        "medOrchid" => 0xBA55D3,
        "medPurple" => 0x9370DB,
        "medSeaGreen" => 0x3CB371,
        "medSlateBlue" => 0x7B68EE,
        "medSpringGreen" => 0x00FA9A,
        "medTurquoise" => 0x48D1CC,
        "medVioletRed" => 0xC71585,
        "mediumAquamarine" => 0x66CDAA,
        "mediumBlue" => 0x0000CD,
        "mediumOrchid" => 0xBA55D3,
        "mediumPurple" => 0x9370DB,
        "mediumSeaGreen" => 0x3CB371,
        "mediumSlateBlue" => 0x7B68EE,
        "mediumSpringGreen" => 0x00FA9A,
        "mediumTurquoise" => 0x48D1CC,
        "mediumVioletRed" => 0xC71585,
        "midnightBlue" => 0x191970,
        "mintCream" => 0xF5FFFA,
        "mistyRose" => 0xFFE4E1,
        "moccasin" => 0xFFE4B5,
        "navajoWhite" => 0xFFDEAD,
        "navy" => 0x000080,
        "oldLace" => 0xFDF5E6,
        "olive" => 0x808000,
        "oliveDrab" => 0x6B8E23,
        "orange" => 0xFFA500,
        "orangeRed" => 0xFF4500,
        "orchid" => 0xDA70D6,
        "paleGoldenrod" => 0xEEE8AA,
        "paleGreen" => 0x98FB98,
        "paleTurquoise" => 0xAFEEEE,
        "paleVioletRed" => 0xDB7093,
        "papayaWhip" => 0xFFEFD5,
        "peachPuff" => 0xFFDAB9,
        "peru" => 0xCD853F,
        "pink" => 0xFFC0CB,
        "plum" => 0xDDA0DD,
        "powderBlue" => 0xB0E0E6,
        "purple" => 0x800080,
        "red" => 0xFF0000,
        "rosyBrown" => 0xBC8F8F,
        "royalBlue" => 0x4169E1,
        "saddleBrown" => 0x8B4513,
        "salmon" => 0xFA8072,
        "sandyBrown" => 0xF4A460,
        "seaGreen" => 0x2E8B57,
        "seaShell" => 0xFFF5EE,
        "sienna" => 0xA0522D,
        "silver" => 0xC0C0C0,
        "skyBlue" => 0x87CEEB,
        "slateBlue" => 0x6A5ACD,
        "slateGray" => 0x708090,
        "slateGrey" => 0x708090,
        "snow" => 0xFFFAFA,
        "springGreen" => 0x00FF7F,
        "steelBlue" => 0x4682B4,
        "tan" => 0xD2B48C,
        "teal" => 0x008080,
        "thistle" => 0xD8BFD8,
        "tomato" => 0xFF6347,
        "turquoise" => 0x40E0D0,
        "violet" => 0xEE82EE,
        "wheat" => 0xF5DEB3,
        "white" => 0xFFFFFF,
        "whiteSmoke" => 0xF5F5F5,
        "yellow" => 0xFFFF00,
        "yellowGreen" => 0x9ACD32,
        _ => return None,
    };
    Some(Color::rgb(
        ((hex >> 16) & 0xFF) as u8,
        ((hex >> 8) & 0xFF) as u8,
        (hex & 0xFF) as u8,
    ))
}

/// `a:sysClr` 的系统颜色名 → 默认值。
///
/// 课件通常已用 `lastClr` 固化实际颜色；这里只是兜底。
fn system_color(name: &str) -> Color {
    match name {
        "windowText" => Color::BLACK,
        "window" => Color::WHITE,
        "scrollBar" | "buttonFace" | "btnFace" => Color::rgb(0xF0, 0xF0, 0xF0),
        "buttonText" | "btnText" => Color::BLACK,
        "highlight" => Color::rgb(0x00, 0x78, 0xD4),
        "highlightText" => Color::WHITE,
        "grayText" => Color::rgb(0x6D, 0x6D, 0x6D),
        "menuText" => Color::BLACK,
        _ => Color::BLACK,
    }
}

/// 从 `a:gsLst` 解析渐变色标。
pub fn parse_gradient_stops(node: &XmlNode, scheme: Option<&ColorScheme>) -> Vec<GradientStop> {
    let mut stops = Vec::new();
    let Some(list) = node.child("gsLst") else {
        return stops;
    };
    for gs in list.children_named("gs") {
        let pos = gs.attr_f64("pos").unwrap_or(0.0) / 100_000.0;
        let Some(color) = gs.children.first().and_then(|c| resolve_color_node(c, scheme)) else {
            continue;
        };
        stops.push(GradientStop {
            pos: (pos as f32).clamp(0.0, 1.0),
            color,
        });
    }
    stops.sort_by(|a, b| {
        a.pos
            .partial_cmp(&b.pos)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    stops
}

/// 解析 `a:lin` / `a:path` 得到渐变类型。
pub fn parse_gradient_kind(node: &XmlNode) -> Option<GradientKind> {
    if let Some(lin) = node.child("lin") {
        // OOXML 的角度单位是 1/60000 度，且 0 度指向正右方、顺时针为正；
        // 而绘图坐标系里 y 轴向下，两者方向一致，无需翻转。
        let ang = lin.attr_f64("ang").unwrap_or(0.0) / 60_000.0;
        return Some(GradientKind::Linear {
            angle_deg: ang as f32,
            scaled: lin.attr_bool_or("scaled", true),
        });
    }

    if let Some(path) = node.child("path") {
        let fill_to_rect = path.child("fillToRect").map(parse_relative_rect);
        let focus = path.child("fillToRect").map(|r| {
            (
                (r.attr_f64("l").unwrap_or(50_000.0) / 100_000.0) as f32,
                (r.attr_f64("t").unwrap_or(50_000.0) / 100_000.0) as f32,
            )
        });
        return Some(match path.attr("path") {
            Some("circle") => GradientKind::Radial {
                ellipse: false,
                focus,
                fill_to_rect,
            },
            Some("rect") => GradientKind::Rect { fill_to_rect },
            Some("shape") => GradientKind::Shape { fill_to_rect },
            // 默认按椭圆径向处理
            _ => GradientKind::Radial {
                ellipse: true,
                focus,
                fill_to_rect,
            },
        });
    }

    // 既无 lin 也无 path：按规范默认是水平线性渐变
    Some(GradientKind::Linear {
        angle_deg: 0.0,
        scaled: true,
    })
}

/// 解析 `a:tileRect` / `a:fillToRect` 等相对矩形（千分比 → 0..1）。
///
/// 语义是「矩形边界」：`l`/`t` 是左上角，`r`/`b` 是右下角，默认铺满。
pub fn parse_relative_rect(node: &XmlNode) -> RelativeRect {
    let get = |name: &str, default: f32| -> f32 {
        node.attr_f64(name)
            .map(|v| (v / 100_000.0) as f32)
            .unwrap_or(default)
    };
    RelativeRect {
        l: get("l", 0.0),
        t: get("t", 0.0),
        r: get("r", 1.0),
        b: get("b", 1.0),
    }
}

/// 解析 `a:srcRect`（图片裁剪）。
///
/// **注意与 [`parse_relative_rect`] 的语义差异**：
/// `a:srcRect` 的 `l/t/r/b` 是**从四边向内裁掉的量**，全部默认为 0；
/// 而 `a:fillToRect` 的四值是矩形边界，`r`/`b` 默认为 1。
/// 两者共用一套逻辑会导致「`r="0"` 被解读为右边框在 0 处」这种错误裁剪。
pub fn parse_src_rect(node: &XmlNode) -> RelativeRect {
    let inset = |name: &str| -> f32 {
        node.attr_f64(name)
            .map(|v| (v / 100_000.0) as f32)
            .unwrap_or(0.0)
            .clamp(0.0, 1.0)
    };
    let (l, t, r, b) = (inset("l"), inset("t"), inset("r"), inset("b"));
    RelativeRect {
        l,
        t,
        // 右边框 = 1 - 右侧内缩量
        r: (1.0 - r).max(l),
        b: (1.0 - b).max(t),
    }
}

/// 解析渐变翻转属性。
pub fn parse_gradient_flip(node: &XmlNode) -> GradientFlip {
    GradientFlip {
        x: node.attr_bool_or("flip", false) || node.attr("flip") == Some("x"),
        y: node.attr("flip") == Some("y"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ppt_core::xml;

    fn node(xml_text: &str) -> XmlNode {
        xml::parse_root("t", xml_text).unwrap()
    }

    #[test]
    fn parses_srgb_color() {
        let n = node(r#"<a:solidFill><a:srgbClr val="FF8000"/></a:solidFill>"#);
        assert_eq!(
            parse_solid_fill(&n, None),
            Some(Color::rgb(255, 128, 0))
        );
    }

    #[test]
    fn parses_scheme_color_with_default_scheme() {
        let n = node(r#"<a:solidFill><a:schemeClr val="accent1"/></a:solidFill>"#);
        let scheme = ColorScheme::default();
        assert_eq!(parse_solid_fill(&n, Some(&scheme)), Some(scheme.accent1));
    }

    #[test]
    fn scheme_color_without_scheme_falls_back_to_default() {
        // 主题部件缺失时不应变成黑色，否则整页配色会崩
        let n = node(r#"<a:solidFill><a:schemeClr val="accent2"/></a:solidFill>"#);
        assert_eq!(
            parse_solid_fill(&n, None),
            Some(ColorScheme::default().accent2)
        );
    }

    #[test]
    fn alpha_transform_applies() {
        let n = node(r#"<a:solidFill><a:srgbClr val="FF0000"><a:alpha val="50000"/></a:srgbClr></a:solidFill>"#);
        let c = parse_solid_fill(&n, None).unwrap();
        assert_eq!(c, Color::rgba(255, 0, 0, 128));
    }

    #[test]
    fn alpha_zero_makes_transparent() {
        let n = node(r#"<a:solidFill><a:srgbClr val="00FF00"><a:alpha val="0"/></a:srgbClr></a:solidFill>"#);
        assert!(parse_solid_fill(&n, None).unwrap().is_transparent());
    }

    #[test]
    fn lum_mod_and_off_are_applied_in_order() {
        // 白色（L=1.0）先 lumMod 50% 再 lumOff -25% → L=0.25
        let n = node(
            r#"<a:solidFill><a:srgbClr val="FFFFFF">
                 <a:lumMod val="50000"/><a:lumOff val="-25000"/>
               </a:srgbClr></a:solidFill>"#,
        );
        let c = parse_solid_fill(&n, None).unwrap();
        assert_eq!(c.r, c.g);
        assert_eq!(c.g, c.b);
        // L=0.25 的灰色约 64
        assert!((c.r as i32 - 64).abs() <= 2, "实际 {:?}", c);
    }

    #[test]
    fn tint_moves_toward_white() {
        let n = node(r#"<a:solidFill><a:srgbClr val="000000"><a:tint val="100000"/></a:srgbClr></a:solidFill>"#);
        assert_eq!(parse_solid_fill(&n, None).unwrap(), Color::WHITE);
    }

    #[test]
    fn shade_moves_toward_black() {
        let n = node(r#"<a:solidFill><a:srgbClr val="FFFFFF"><a:shade val="100000"/></a:srgbClr></a:solidFill>"#);
        assert_eq!(parse_solid_fill(&n, None).unwrap(), Color::BLACK);
    }

    #[test]
    fn inv_inverts_channels_but_keeps_alpha() {
        let n = node(r#"<a:solidFill><a:srgbClr val="FF0000"><a:inv/><a:alpha val="50000"/></a:srgbClr></a:solidFill>"#);
        let c = parse_solid_fill(&n, None).unwrap();
        assert_eq!((c.r, c.g, c.b, c.a), (0, 255, 255, 128));
    }

    #[test]
    fn comp_rotates_hue_180() {
        let n = node(r#"<a:solidFill><a:srgbClr val="FF0000"><a:comp/></a:srgbClr></a:solidFill>"#);
        let c = parse_solid_fill(&n, None).unwrap();
        // 红色的补色是青色
        assert!(c.g > 200 && c.b > 200 && c.r < 60, "实际 {:?}", c);
    }

    #[test]
    fn preset_color_lookup() {
        let n = node(r#"<a:solidFill><a:prstClr val="steelBlue"/></a:solidFill>"#);
        assert_eq!(
            parse_solid_fill(&n, None),
            Some(Color::rgb(0x46, 0x82, 0xB4))
        );
    }

    #[test]
    fn unknown_preset_color_returns_none() {
        let n = node(r#"<a:solidFill><a:prstClr val="notAColor"/></a:solidFill>"#);
        assert_eq!(parse_solid_fill(&n, None), None);
    }

    #[test]
    fn sys_color_prefers_last_clr() {
        let n = node(r#"<a:solidFill><a:sysClr val="windowText" lastClr="112233"/></a:solidFill>"#);
        assert_eq!(
            parse_solid_fill(&n, None),
            Some(Color::rgb(0x11, 0x22, 0x33))
        );
    }

    #[test]
    fn sys_color_without_last_clr_uses_semantic_default() {
        let n = node(r#"<a:solidFill><a:sysClr val="windowText"/></a:solidFill>"#);
        assert_eq!(parse_solid_fill(&n, None), Some(Color::BLACK));
    }

    #[test]
    fn scrgb_is_converted_from_linear() {
        // 线性 0.5 → sRGB 约 188
        let n = node(r#"<a:solidFill><a:scrgbClr r="50000" g="50000" b="50000"/></a:solidFill>"#);
        let c = parse_solid_fill(&n, None).unwrap();
        assert!((c.r as i32 - 188).abs() <= 2, "实际 {:?}", c);
    }

    #[test]
    fn hsl_color_parses_primary_hues() {
        let red = node(r#"<a:solidFill><a:hslClr hue="0" sat="100000" lum="50000"/></a:solidFill>"#);
        let c = parse_solid_fill(&red, None).unwrap();
        assert!(c.r > 200 && c.g < 60 && c.b < 60, "实际 {:?}", c);
    }

    #[test]
    fn hsl_rgb_roundtrip() {
        for c in [
            Color::rgb(255, 0, 0),
            Color::rgb(0, 255, 0),
            Color::rgb(0, 0, 255),
            Color::rgb(128, 64, 32),
            Color::rgb(255, 255, 255),
            Color::rgb(0, 0, 0),
        ] {
            let (h, s, l) = rgb_to_hsl(c);
            let back = hsl_to_rgb(h, s, l);
            assert!(
                (back.r as i32 - c.r as i32).abs() <= 1
                    && (back.g as i32 - c.g as i32).abs() <= 1
                    && (back.b as i32 - c.b as i32).abs() <= 1,
                "HSL 往返失败：{c:?} → {back:?}"
            );
        }
    }

    #[test]
    fn scheme_slot_parsing() {
        assert_eq!(SchemeSlot::from_val("accent1"), Some(SchemeSlot::Accent1));
        assert_eq!(SchemeSlot::from_val("bg1"), Some(SchemeSlot::Bg1));
        assert_eq!(SchemeSlot::from_val("tx2"), Some(SchemeSlot::Tx2));
        assert_eq!(SchemeSlot::from_val("folHlink"), Some(SchemeSlot::FolHlink));
        assert_eq!(SchemeSlot::from_val("nope"), None);
        // dk1/lt1 别名
        assert_eq!(SchemeSlot::from_val("dk1"), Some(SchemeSlot::Tx1));
        assert_eq!(SchemeSlot::from_val("lt1"), Some(SchemeSlot::Bg1));
    }

    #[test]
    fn color_scheme_from_theme_xml_with_standard_map() {
        let theme = node(
            r#"<a:clrScheme name="Office">
                 <a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1>
                 <a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1>
                 <a:dk2><a:srgbClr val="44546A"/></a:dk2>
                 <a:lt2><a:srgbClr val="E7E6E6"/></a:lt2>
                 <a:accent1><a:srgbClr val="4472C4"/></a:accent1>
                 <a:accent2><a:srgbClr val="ED7D31"/></a:accent2>
                 <a:accent3><a:srgbClr val="A5A5A5"/></a:accent3>
                 <a:accent4><a:srgbClr val="FFC000"/></a:accent4>
                 <a:accent5><a:srgbClr val="5B9BD5"/></a:accent5>
                 <a:accent6><a:srgbClr val="70AD47"/></a:accent6>
                 <a:hlink><a:srgbClr val="0563C1"/></a:hlink>
                 <a:folHlink><a:srgbClr val="954F72"/></a:folHlink>
               </a:clrScheme>"#,
        );
        let scheme = ColorScheme::from_theme_xml(&theme, None);
        assert_eq!(scheme.tx1, Color::BLACK);
        assert_eq!(scheme.bg1, Color::WHITE);
        assert_eq!(scheme.accent1, Color::rgb(0x44, 0x72, 0xC4));
        assert_eq!(scheme.accent6, Color::rgb(0x70, 0xAD, 0x47));
    }

    #[test]
    fn color_scheme_respects_inverted_clr_map() {
        // 深色主题会把 bg1 映射到 dk1
        let theme = node(
            r#"<a:clrScheme name="Dark">
                 <a:dk1><a:srgbClr val="FFFFFF"/></a:dk1>
                 <a:lt1><a:srgbClr val="000000"/></a:lt1>
                 <a:dk2><a:srgbClr val="000000"/></a:dk2>
                 <a:lt2><a:srgbClr val="111111"/></a:lt2>
                 <a:accent1><a:srgbClr val="4472C4"/></a:accent1>
                 <a:accent2><a:srgbClr val="ED7D31"/></a:accent2>
                 <a:accent3><a:srgbClr val="A5A5A5"/></a:accent3>
                 <a:accent4><a:srgbClr val="FFC000"/></a:accent4>
                 <a:accent5><a:srgbClr val="5B9BD5"/></a:accent5>
                 <a:accent6><a:srgbClr val="70AD47"/></a:accent6>
                 <a:hlink><a:srgbClr val="0563C1"/></a:hlink>
                 <a:folHlink><a:srgbClr val="954F72"/></a:folHlink>
               </a:clrScheme>"#,
        );
        let map = ClrMap {
            bg1: "dk1".into(),
            tx1: "lt1".into(),
            ..ClrMap::default()
        };
        let scheme = ColorScheme::from_theme_xml(&theme, Some(&map));
        assert_eq!(scheme.bg1, Color::WHITE);
        assert_eq!(scheme.tx1, Color::BLACK);
    }

    #[test]
    fn clr_map_parses_and_defaults_missing_attrs() {
        let n = node(r#"<a:clrMap bg1="dk1" tx1="lt1" accent1="accent1"/>"#);
        let m = ClrMap::parse(&n);
        assert_eq!(m.bg1, "dk1");
        assert_eq!(m.tx1, "lt1");
        // 未指定的属性用默认值补齐
        assert_eq!(m.accent2, "accent2");
        assert_eq!(m.folhlink, "folHlink");
    }

    #[test]
    fn gradient_stops_are_parsed_and_sorted() {
        let n = node(
            r#"<a:gradFill>
                 <a:gsLst>
                   <a:gs pos="100000"><a:srgbClr val="FFFFFF"/></a:gs>
                   <a:gs pos="0"><a:srgbClr val="000000"/></a:gs>
                 </a:gsLst>
                 <a:lin ang="5400000" scaled="1"/>
               </a:gradFill>"#,
        );
        let stops = parse_gradient_stops(&n, None);
        assert_eq!(stops.len(), 2);
        assert_eq!(stops[0].pos, 0.0);
        assert_eq!(stops[0].color, Color::BLACK);
        assert_eq!(stops[1].color, Color::WHITE);
    }

    #[test]
    fn gradient_kind_linear_angle_conversion() {
        let n = node(r#"<a:gradFill><a:lin ang="5400000" scaled="1"/></a:gradFill>"#);
        match parse_gradient_kind(&n) {
            Some(GradientKind::Linear { angle_deg, scaled }) => {
                assert!((angle_deg - 90.0).abs() < 0.01, "5400000/60000 应为 90 度");
                assert!(scaled);
            }
            other => panic!("应解析为线性渐变，实际 {other:?}"),
        }
    }

    #[test]
    fn gradient_kind_defaults_to_horizontal_when_unspecified() {
        let n = node(r#"<a:gradFill><a:gsLst/></a:gradFill>"#);
        assert!(matches!(
            parse_gradient_kind(&n),
            Some(GradientKind::Linear { angle_deg, .. }) if angle_deg.abs() < 0.01
        ));
    }

    #[test]
    fn gradient_kind_path_variants() {
        let radial = node(r#"<a:gradFill><a:path path="circle"><a:fillToRect l="50000" t="50000" r="50000" b="50000"/></a:path></a:gradFill>"#);
        assert!(matches!(
            parse_gradient_kind(&radial),
            Some(GradientKind::Radial { ellipse: false, .. })
        ));

        let rect = node(r#"<a:gradFill><a:path path="rect"/></a:gradFill>"#);
        assert!(matches!(
            parse_gradient_kind(&rect),
            Some(GradientKind::Rect { .. })
        ));
    }

    #[test]
    fn relative_rect_parsing() {
        let n = node(r#"<a:fillToRect l="10000" t="20000" r="30000" b="40000"/>"#);
        let r = parse_relative_rect(&n);
        assert!((r.l - 0.1).abs() < 1e-6);
        assert!((r.t - 0.2).abs() < 1e-6);
        assert!((r.r - 0.3).abs() < 1e-6);
        assert!((r.b - 0.4).abs() < 1e-6);
    }

    #[test]
    fn relative_rect_defaults_to_full() {
        let n = node(r#"<a:fillToRect/>"#);
        assert!(parse_relative_rect(&n).is_full());
    }

    #[test]
    fn gradient_flip_parsing() {
        assert_eq!(parse_gradient_flip(&node(r#"<a:tileRect flip="x"/>"#)).x, true);
        assert_eq!(parse_gradient_flip(&node(r#"<a:tileRect flip="y"/>"#)).y, true);
        let none = parse_gradient_flip(&node(r#"<a:tileRect/>"#));
        assert!(!none.x && !none.y);
    }

    #[test]
    fn multiple_transforms_chain_in_order() {
        // 红色 → alpha 50% → 转白 50%：两个变换互不干扰
        let n = node(
            r#"<a:solidFill><a:srgbClr val="FF0000">
                 <a:alpha val="50000"/><a:tint val="50000"/>
               </a:srgbClr></a:solidFill>"#,
        );
        let c = parse_solid_fill(&n, None).unwrap();
        assert_eq!(c.a, 128);
        assert!(c.g > 120 && c.b > 120, "tint 后应偏向白色：{c:?}");
    }

    #[test]
    fn missing_color_value_returns_none() {
        let n = node(r#"<a:solidFill/>"#);
        assert_eq!(parse_solid_fill(&n, None), None);
    }
}
