//! OPC（Open Packaging Conventions）容器层：ZIP 按需读取。
//!
//! # 为什么不用「解压到临时目录」或「整体读入内存」
//!
//! 打开课件时唯一必须做的工作是解析 ZIP 的**中央目录**（几百字节到几十 KB），
//! 之后只有被渲染页引用的部件才会真正解压。这样：
//!
//! - 打开 100 页课件的磁盘读取量只有课件体积的一小部分（不触碰未访问页的 XML 与媒体）；
//! - 常驻内存不随课件页数线性增长；
//! - 对「95% 体积集中在少数几页大图」的课件尤其有效。
//!
//! 通道由 [`MmapReader`] 提供：它把文件内存映射后直接对切片读取，
//! 省去逐次 `read()` 系统调用；同时用原子计数器统计**实际读取字节数**，
//! 供性能验收脚本核对「磁盘读放大」指标。

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use memmap2::Mmap;

use crate::error::{Error, Result};

/// 基于内存映射的 `Read + Seek` 通道。
///
/// 自持有 [`Mmap`]，因此不存在自引用生命周期问题；
/// 读取时直接对映射切片做拷贝，并累计读取量。
pub struct MmapReader {
    mmap: Mmap,
    pos: u64,
    counter: Arc<AtomicU64>,
    /// 是否统计读取量。基准测试时开启，正常运行时也几乎无成本。
    count_reads: bool,
}

impl MmapReader {
    /// 打开文件并建立内存映射。
    pub fn open(path: &Path) -> Result<(MmapReader, Arc<AtomicU64>)> {
        let file = File::open(path).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let len = file
            .metadata()
            .map_err(|source| Error::Io {
                path: path.to_path_buf(),
                source,
            })?
            .len();
        if len == 0 {
            return Err(Error::Container("文件为空".into()));
        }
        // SAFETY: 我们不对映射做写操作；文件被截断属于外部异常，
        // 这种情况下降级为读取错误而非未定义行为（不会越界写）。
        let mmap = unsafe { Mmap::map(&file) }.map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let counter = Arc::new(AtomicU64::new(0));
        Ok((
            MmapReader {
                mmap,
                pos: 0,
                counter: Arc::clone(&counter),
                count_reads: true,
            },
            counter,
        ))
    }

    /// 文件总长度。
    #[inline]
    pub fn len(&self) -> u64 {
        self.mmap.len() as u64
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.mmap.is_empty()
    }

    /// 在不移动读写位置的前提下窥探起始字节（用于魔数探测）。
    pub fn peek(&self, offset: usize, buf: &mut [u8]) -> usize {
        if offset >= self.mmap.len() {
            return 0;
        }
        let end = (offset + buf.len()).min(self.mmap.len());
        let n = end - offset;
        buf[..n].copy_from_slice(&self.mmap[offset..end]);
        n
    }
}

impl Read for MmapReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let start = self.pos as usize;
        if start >= self.mmap.len() {
            return Ok(0);
        }
        let end = (start + buf.len()).min(self.mmap.len());
        let n = end - start;
        buf[..n].copy_from_slice(&self.mmap[start..end]);
        self.pos = end as u64;
        if self.count_reads {
            self.counter.fetch_add(n as u64, Ordering::Relaxed);
        }
        Ok(n)
    }
}

impl Seek for MmapReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::End(n) => self.mmap.len() as i64 + n,
            SeekFrom::Current(n) => self.pos as i64 + n,
        };
        if new_pos < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek 到负偏移",
            ));
        }
        self.pos = new_pos as u64;
        Ok(self.pos)
    }
}

/// 一个已打开的 OPC 包。
///
/// 线程安全性：内部 ZIP 读取状态用 `Mutex` 保护，
/// 允许多个渲染线程并行请求不同部件（解压耗时占主导，锁竞争可忽略）。
pub struct Package {
    archive: Mutex<zip::ZipArchive<MmapReader>>,
    /// 归一化后的条目名，顺序与 ZIP 条目一致。
    names: Vec<String>,
    /// 归一化条目名 → ZIP 索引。
    index: HashMap<String, usize>,
    read_counter: Arc<AtomicU64>,
    /// 已实际读取过的部件名。
    ///
    /// 只记录 `read_part` 级别的调用（每次会话几十次），开销可忽略，
    /// 但让「惰性解析」这一核心性能约定可以被精确验证 ——
    /// 例如断言「打开课件后 slide2.xml 从未被读取」，
    /// 这比对比字节数更可靠（字节数会受关系表等零碎部件干扰）。
    read_parts: Mutex<std::collections::HashSet<String>>,
    path: PathBuf,
    file_len: u64,
    /// 打开阶段（解析中央目录）读取的字节数。
    open_read_bytes: u64,
}

impl std::fmt::Debug for Package {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Package")
            .field("path", &self.path)
            .field("entries", &self.names.len())
            .field("file_len", &self.file_len)
            .finish()
    }
}

/// 单个部件解压后的体积上限（512MB）。
///
/// 防御「ZIP 炸弹」：一个几十 KB 的恶意课件可以声明解压后为几十 GB，
/// 直接把老机器打爆。
const MAX_PART_SIZE: u64 = 512 * 1024 * 1024;

impl Package {
    /// 打开一个 OPC 包：建立内存映射、读取中央目录、索引条目名。
    ///
    /// 此过程**不会**解压任何部件。
    pub fn open(path: impl AsRef<Path>) -> Result<Package> {
        let path = path.as_ref().to_path_buf();
        let (reader, counter) = MmapReader::open(&path)?;
        let file_len = reader.len();

        let mut archive = zip::ZipArchive::new(reader)
            .map_err(|e| Error::Container(format!("无法读取压缩容器：{e}")))?;

        let entry_count = archive.len();
        let mut names = Vec::with_capacity(entry_count);
        let mut index = HashMap::with_capacity(entry_count);
        for i in 0..entry_count {
            let name = match archive.by_index_raw(i) {
                Ok(f) => f.name().to_string(),
                Err(_) => continue,
            };
            let normalized = normalize_part_name(&name);
            // 目录条目（以 / 结尾）不参与索引
            if normalized.is_empty() || name.ends_with('/') {
                continue;
            }
            index.entry(normalized.clone()).or_insert(i);
            names.push(normalized);
        }

        let open_read_bytes = counter.load(Ordering::Relaxed);

        if names.is_empty() {
            return Err(Error::Container("压缩容器中没有任何部件".into()));
        }

        Ok(Package {
            archive: Mutex::new(archive),
            names,
            index,
            read_counter: counter,
            read_parts: Mutex::new(std::collections::HashSet::new()),
            path,
            file_len,
            open_read_bytes,
        })
    }

    #[inline]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[inline]
    pub fn file_len(&self) -> u64 {
        self.file_len
    }

    /// 全部条目名（已归一化）。
    #[inline]
    pub fn entry_names(&self) -> &[String] {
        &self.names
    }

    #[inline]
    pub fn entry_count(&self) -> usize {
        self.names.len()
    }

    /// 是否存在该部件（大小写与首斜杠不敏感）。
    #[inline]
    pub fn contains(&self, part: &str) -> bool {
        self.index.contains_key(&normalize_part_name(part))
    }

    /// 读取并解压一个部件。
    pub fn read_part(&self, part: &str) -> Result<Vec<u8>> {
        let key = normalize_part_name(part);
        let idx = *self
            .index
            .get(&key)
            .ok_or_else(|| Error::MissingPart(key.clone()))?;

        if let Ok(mut log) = self.read_parts.lock() {
            log.insert(key.clone());
        }

        let mut archive = self
            .archive
            .lock()
            .map_err(|_| Error::other("内部锁已损坏"))?;

        let mut entry = archive.by_index(idx).map_err(|e| {
            Error::Container(format!("无法解压部件 {key}：{e}"))
        })?;

        let declared = entry.size();
        if declared > MAX_PART_SIZE {
            return Err(Error::Container(format!(
                "部件 {key} 解压后体积异常（{} MB），已拒绝解压",
                declared / 1024 / 1024
            )));
        }

        let mut buf = Vec::with_capacity(declared.min(MAX_PART_SIZE) as usize);
        entry
            .by_ref()
            .take(MAX_PART_SIZE + 1)
            .read_to_end(&mut buf)
            .map_err(|e| Error::Container(format!("读取部件 {key} 失败：{e}")))?;

        if buf.len() as u64 > MAX_PART_SIZE {
            return Err(Error::Container(format!(
                "部件 {key} 实际解压体积超出上限，已中止"
            )));
        }

        Ok(buf)
    }

    /// 读取部件为 UTF-8 字符串（OOXML 部件均为 UTF-8）。
    pub fn read_part_str(&self, part: &str) -> Result<String> {
        let bytes = self.read_part(part)?;
        // OOXML 部件可能带 BOM
        let s = String::from_utf8(bytes)
            .map_err(|e| Error::xml(part, format!("部件不是合法 UTF-8：{e}")))?;
        Ok(s.trim_start_matches('\u{feff}').to_string())
    }

    /// 部件是否存在且非空（有些课件会留下 0 字节的占位部件）。
    pub fn has_nonempty_part(&self, part: &str) -> bool {
        self.read_part(part).map(|b| !b.is_empty()).unwrap_or(false)
    }

    /// 累计读取字节数（含解压前后）。
    #[inline]
    pub fn bytes_read(&self) -> u64 {
        self.read_counter.load(Ordering::Relaxed)
    }

    /// 打开阶段（仅解析中央目录）读取的字节数。
    #[inline]
    pub fn open_read_bytes(&self) -> u64 {
        self.open_read_bytes
    }

    /// 当前「打开 + 已读取」相对文件大小的放大倍数。
    ///
    /// 用于核对 spec 中「打开 100 页课件读取量 ≤ 体积的 30%」的预算。
    #[inline]
    pub fn read_amplification(&self) -> f64 {
        if self.file_len == 0 {
            return 0.0;
        }
        self.bytes_read() as f64 / self.file_len as f64
    }

    /// 重置读取计数器（供基准测试分段测量）。
    pub fn reset_read_counter(&self) {
        self.read_counter.store(0, Ordering::Relaxed);
        if let Ok(mut log) = self.read_parts.lock() {
            log.clear();
        }
    }

    /// 某个部件是否已被实际读取过。
    ///
    /// 用于验证惰性解析：例如「打开课件后不应读过任何 slideN.xml」。
    pub fn was_part_read(&self, part: &str) -> bool {
        let key = normalize_part_name(part);
        self.read_parts
            .lock()
            .map(|log| log.contains(&key))
            .unwrap_or(false)
    }

    /// 已读取过的部件名（排序后，便于输出诊断信息）。
    pub fn read_parts(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .read_parts
            .lock()
            .map(|log| log.iter().cloned().collect())
            .unwrap_or_default();
        v.sort();
        v
    }

    /// 按前缀与后缀筛选条目（如 `ppt/slides/` 下所有 `.xml`）。
    pub fn entries_with_prefix<'a>(
        &'a self,
        prefix: &'a str,
    ) -> impl Iterator<Item = &'a str> + 'a {
        let p = normalize_part_name(prefix);
        self.names
            .iter()
            .filter(move |n| n.starts_with(&p))
            .map(|s| s.as_str())
    }
}

/// 归一化部件名：统一分隔符、去掉前导 `/`、转为小写。
///
/// OPC 规范要求部件名大小写敏感，但现实中大量课件由第三方工具生成，
/// 存在大小写不一致的情况；统一小写可显著提升容错率。
pub fn normalize_part_name(name: &str) -> String {
    let s = name.replace('\\', "/");
    let s = s.trim_start_matches('/');
    s.to_ascii_lowercase()
}

/// 由「基准部件 + 相对目标」解析出绝对部件名。
///
/// 支持 `./`、`../` 与绝对路径 `/ppt/slides/slide1.xml`。
/// 返回 `None` 表示目标越出包根（非法）。
pub fn resolve_part_name(base_part: &str, target: &str) -> Option<String> {
    let target = target.replace('\\', "/");
    if target.starts_with('/') {
        return Some(normalize_part_name(&target));
    }

    let base_dir = match base_part.rfind('/') {
        Some(i) => &base_part[..=i],
        None => "",
    };

    let mut segments: Vec<&str> = Vec::new();
    for seg in base_dir.split('/').filter(|s| !s.is_empty()) {
        segments.push(seg);
    }

    for seg in target.split('/') {
        match seg {
            "" | "." => continue,
            ".." => {
                // 越过根目录的 `..` 说明路径非法，直接拒绝
                segments.pop()?;
            }
            other => segments.push(other),
        }
    }

    Some(segments.join("/").to_ascii_lowercase())
}

/// 判断关系目标是否为外部资源（如 `http://`、`file://`）。
#[inline]
pub fn is_external_target(target: &str) -> bool {
    let t = target.trim();
    let lower = t.to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("file:")
        || lower.starts_with("mailto:")
        || lower.starts_with("ftp:")
        || t.starts_with("//")
}

/// 取部件所在目录（含结尾 `/`）。
pub fn part_dir(part: &str) -> &str {
    match part.rfind('/') {
        Some(i) => &part[..=i],
        None => "",
    }
}

/// 取部件文件名（不含目录）。
pub fn part_file_name(part: &str) -> &str {
    match part.rfind('/') {
        Some(i) => &part[i + 1..],
        None => part,
    }
}

/// 取部件扩展名（小写，不含点）。
pub fn part_extension(part: &str) -> Option<String> {
    let name = part_file_name(part);
    let dot = name.rfind('.')?;
    Some(name[dot + 1..].to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_leading_slash_and_lowercases() {
        assert_eq!(normalize_part_name("/ppt/slides/Slide1.XML"), "ppt/slides/slide1.xml");
        assert_eq!(normalize_part_name("ppt\\slides\\slide1.xml"), "ppt/slides/slide1.xml");
        assert_eq!(normalize_part_name("ppt/slides/slide1.xml"), "ppt/slides/slide1.xml");
    }

    #[test]
    fn resolve_relative_same_dir() {
        assert_eq!(
            resolve_part_name("ppt/slides/slide1.xml", "slide2.xml").as_deref(),
            Some("ppt/slides/slide2.xml")
        );
    }

    #[test]
    fn resolve_relative_parent() {
        assert_eq!(
            resolve_part_name("ppt/slides/slide1.xml", "../slideLayouts/slideLayout1.xml").as_deref(),
            Some("ppt/slidelayouts/slidelayout1.xml")
        );
    }

    #[test]
    fn resolve_relative_double_parent() {
        assert_eq!(
            resolve_part_name("ppt/slides/slide1.xml", "../../docProps/core.xml").as_deref(),
            Some("docprops/core.xml")
        );
    }

    #[test]
    fn resolve_absolute_target() {
        assert_eq!(
            resolve_part_name("ppt/slides/slide1.xml", "/ppt/media/image1.png").as_deref(),
            Some("ppt/media/image1.png")
        );
    }

    #[test]
    fn resolve_current_dir_segment() {
        assert_eq!(
            resolve_part_name("ppt/slides/slide1.xml", "./notesSlide1.xml").as_deref(),
            Some("ppt/slides/notesslide1.xml")
        );
    }

    #[test]
    fn resolve_rejects_escaping_root() {
        assert_eq!(resolve_part_name("slide1.xml", "../a.xml"), None);
    }

    #[test]
    fn external_target_detection() {
        assert!(is_external_target("http://example.com/a.png"));
        assert!(is_external_target("HTTPS://example.com"));
        assert!(is_external_target("file:///c:/a.png"));
        assert!(is_external_target("mailto:a@b.c"));
        assert!(!is_external_target("media/image1.png"));
        assert!(!is_external_target("../media/image1.png"));
    }

    #[test]
    fn part_name_helpers() {
        assert_eq!(part_dir("ppt/slides/slide1.xml"), "ppt/slides/");
        assert_eq!(part_file_name("ppt/slides/slide1.xml"), "slide1.xml");
        assert_eq!(part_extension("ppt/media/Image1.PNG").as_deref(), Some("png"));
        assert_eq!(part_extension("ppt/media/noext"), None);
        assert_eq!(part_dir("slide1.xml"), "");
    }
}
