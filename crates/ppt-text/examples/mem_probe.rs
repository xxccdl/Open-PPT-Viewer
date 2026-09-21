//! 一次性诊断：测量字体索引的真实内存占用。
//!
//! 目的：确认「常驻内存是否被系统字体索引拖高」这个假设。
//! 结论会直接决定优化方向，避免在错误的地方花力气。
//!
//! 运行：`cargo run -p ppt-text --example mem_probe`

use ppt_text::FontContext;

#[cfg(windows)]
mod mem {
    // Windows 的内存计数需要系统 API；这里用最简方式读取。
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> isize;
    }

    #[link(name = "psapi")]
    extern "system" {
        fn GetProcessMemoryInfo(
            process: isize,
            counters: *mut ProcessMemoryCounters,
            size: u32,
        ) -> i32;
    }

    #[repr(C)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    /// `(工作集, 私有提交内存)`，单位 MB。
    pub fn report() -> (f64, f64) {
        let mut c = ProcessMemoryCounters {
            cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
            page_fault_count: 0,
            peak_working_set_size: 0,
            working_set_size: 0,
            quota_peak_paged_pool_usage: 0,
            quota_paged_pool_usage: 0,
            quota_peak_non_paged_pool_usage: 0,
            quota_non_paged_pool_usage: 0,
            pagefile_usage: 0,
            peak_pagefile_usage: 0,
        };
        let ok = unsafe {
            GetProcessMemoryInfo(
                GetCurrentProcess(),
                &mut c,
                std::mem::size_of::<ProcessMemoryCounters>() as u32,
            )
        };
        if ok == 0 {
            return (0.0, 0.0);
        }
        (
            c.working_set_size as f64 / 1048576.0,
            c.pagefile_usage as f64 / 1048576.0,
        )
    }
}

#[cfg(not(windows))]
mod mem {
    pub fn report() -> (f64, f64) {
        (0.0, 0.0)
    }
}

fn main() {
    let (ws0, pv0) = mem::report();
    println!("索引前：工作集 {ws0:.0} MB，私有 {pv0:.0} MB");

    let t = std::time::Instant::now();
    let ctx = FontContext::new();
    let elapsed = t.elapsed();

    let (ws1, pv1) = mem::report();
    println!("索引后：工作集 {ws1:.0} MB，私有 {pv1:.0} MB");
    println!("增量：工作集 +{:.0} MB，私有 +{:.0} MB", ws1 - ws0, pv1 - pv0);
    println!("耗时：{elapsed:?}");
    println!("字体面 {} 个，家族 {} 个", ctx.font_count(), ctx.family_count());

    // 再做一次真实的字体解析与整形，看是否会把页面换入
    let t = std::time::Instant::now();
    let mut hit = 0;
    for ch in "光合作用与呼吸作用 English 123".chars() {
        if ctx
            .font_for_char(Some("Microsoft YaHei"), false, false, ch)
            .is_some()
        {
            hit += 1;
        }
    }
    let (ws2, pv2) = mem::report();
    println!("解析 {hit} 个字符后：工作集 {ws2:.0} MB，私有 {pv2:.0} MB（耗时 {:?}）", t.elapsed());
    println!("总增量：工作集 +{:.0} MB，私有 +{:.0} MB", ws2 - ws0, pv2 - pv0);
}
