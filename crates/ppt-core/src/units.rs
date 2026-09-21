//! 长度单位换算。
//!
//! 约定：**SceneGraph 内部统一使用「点（pt）」作为长度单位**。
//!
//! 理由：
//! - OOXML 用 EMU（914400 EMU = 1 英寸），字号用「百分之一磅」；
//!   换算到 pt 只需除以常数，且 pt 与字号同量纲，文本布局无需二次换算。
//! - 典型幻灯片为 960×540 pt（16:9），f32 在该量级有充足精度，
//!   比直接用 EMU（12192000）更省位宽也更快。

/// 1 英寸等于 914400 EMU（OOXML 定义）。
pub const EMU_PER_INCH: f64 = 914_400.0;

/// 1 点等于 12700 EMU（914400 / 72）。
pub const EMU_PER_POINT: f64 = 12_700.0;

/// 1 英寸等于 72 点。
pub const PT_PER_INCH: f32 = 72.0;

/// EMU → 点。
#[inline]
pub fn emu_to_pt(emu: f64) -> f32 {
    (emu / EMU_PER_POINT) as f32
}

/// 点 → EMU。
#[inline]
pub fn pt_to_emu(pt: f32) -> i64 {
    (pt as f64 * EMU_PER_POINT).round() as i64
}

/// 百分之一磅 → 点（OOXML 字号单位）。
#[inline]
pub fn centipoint_to_pt(cp: f64) -> f32 {
    (cp / 100.0) as f32
}

/// 点 → 指定 DPI 下的像素。
#[inline]
pub fn pt_to_px(pt: f32, dpi: f32) -> f32 {
    pt * dpi / PT_PER_INCH
}

/// 像素 → 点。
#[inline]
pub fn px_to_pt(px: f32, dpi: f32) -> f32 {
    px * PT_PER_INCH / dpi
}

/// OOXML 中的 1/100000 百分比 → 浮点比例（如 `50000` → 0.5）。
#[inline]
pub fn pct100k_to_f32(v: f64) -> f32 {
    (v / 100_000.0) as f32
}

/// OOXML 中的 1/1000 百分比 → 浮点比例（如 `100000` 之外的线条宽度用）。
#[inline]
pub fn pct1k_to_f32(v: f64) -> f32 {
    (v / 1_000.0) as f32
}

/// OOXML 角度单位（1/60000 度）→ 度。
#[inline]
pub fn ooxml_angle_to_deg(v: f64) -> f32 {
    (v / 60_000.0) as f32
}

/// 点 → 缩放后的像素尺寸（四舍五入并保证至少为 1）。
#[inline]
pub fn scaled_px(pt: f32, scale: f32) -> u32 {
    let v = (pt * scale).round();
    if v < 1.0 {
        1
    } else {
        v as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emu_point_roundtrip() {
        // 标准 16:9 幻灯片宽度 12192000 EMU 应为 960pt
        assert!((emu_to_pt(12_192_000.0) - 960.0).abs() < 0.001);
        assert_eq!(pt_to_emu(960.0), 12_192_000);
    }

    #[test]
    fn centipoint_conversion() {
        // 1800（18pt 字号）
        assert!((centipoint_to_pt(1800.0) - 18.0).abs() < 0.001);
    }

    #[test]
    fn pct_conversions() {
        assert!((pct100k_to_f32(50_000.0) - 0.5).abs() < 1e-6);
        assert!((pct100k_to_f32(-25_000.0) + 0.25).abs() < 1e-6);
    }

    #[test]
    fn scaled_px_never_zero() {
        assert_eq!(scaled_px(0.1, 1.0), 1);
        assert_eq!(scaled_px(10.0, 2.0), 20);
    }
}
