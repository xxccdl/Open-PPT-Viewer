//! 两级缓存：内存 LRU + 磁盘缓存。
//!
//! # 为什么要两级
//!
//! - **内存**负责「翻页即时响应」：已渲染页保持在内存里，回翻时零延迟。
//! - **磁盘**负责「第二次打开同一课件也快」：跨会话复用，尤其是缩略图 ——
//!   100 页课件的缩略图批量生成要几百毫秒，缓存后打开即见。
//!
//! # 场景图也缓存
//!
//! 同一页在不同缩放档（适应窗口 / 100% / 缩略图）都要渲染，
//! 但**解析只需一次**。因此内存里额外维护一份「页 → 场景图」的 LRU：
//! 改变缩放时只重新光栅化，不重新解析 XML、不重新走继承链。
//! 对含大量文本的课件，这一步能省掉一半以上的翻页耗时。

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lru::LruCache;

use ppt_core::scene::{PlayState, Scene, PLAY_ALL};
use ppt_core::{Bitmap, Error, PixelFormat, Result};

/// 缓存来源（用于诊断与性能分析）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheSource {
    /// 内存命中。
    Memory,
    /// 磁盘命中（已回填内存）。
    Disk,
    /// 实际渲染。
    Rendered,
}

/// 缓存键：页索引 + 缩放档 + 动画播放状态。
///
/// 播放状态**必须**进键：同一页在不同步是两张不同的图，
/// 少了这一维就会出现「点了一下没反应」（拿到的是上一步的缓存）。
/// 用掩码而不是步号，是因为触发器动画允许跳着播 —— 先点按钮播第 3 步、
/// 再点空白播第 1 步，两种状态必须各存一份。
type BitmapKey = (usize, u32, PlayState);

/// 内存缓存。
pub struct MemoryCache {
    bitmaps: LruCache<BitmapKey, Arc<Bitmap>>,
    scenes: LruCache<usize, Arc<Scene>>,
    /// 已占用的位图字节数（用于按内存预算而非页数淘汰）。
    bitmap_bytes: usize,
    /// 位图缓存的内存上限。
    max_bitmap_bytes: usize,
}

impl MemoryCache {
    /// `pages` 是「期望缓存的页数」，用于反推内存上限。
    ///
    /// 不直接按页数限制，是因为页面像素数差异很大：
    /// 缩略图每页几十 KB，主视图每页可能几 MB。
    /// 按字节限制才能真正约束老机器的内存占用。
    pub fn new(pages: usize, max_bitmap_bytes: usize) -> MemoryCache {
        let capacity = pages.max(1);
        MemoryCache {
            bitmaps: LruCache::new(NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::MIN)),
            scenes: LruCache::new(
                NonZeroUsize::new(capacity * 2).unwrap_or(NonZeroUsize::MIN),
            ),
            bitmap_bytes: 0,
            max_bitmap_bytes,
        }
    }

    /// 取「动画全部播完」的完整帧。
    pub fn get_bitmap(&mut self, page: usize, bucket: u32) -> Option<Arc<Bitmap>> {
        self.get_bitmap_at(page, bucket, PLAY_ALL)
    }

    /// 取指定动画播放状态的帧。
    pub fn get_bitmap_at(
        &mut self,
        page: usize,
        bucket: u32,
        state: PlayState,
    ) -> Option<Arc<Bitmap>> {
        self.bitmaps.get(&(page, bucket, state)).cloned()
    }

    /// 存「动画全部播完」的完整帧。
    pub fn put_bitmap(&mut self, page: usize, bucket: u32, bitmap: Arc<Bitmap>) {
        self.put_bitmap_at(page, bucket, PLAY_ALL, bitmap);
    }

    /// 存指定动画播放状态的帧。
    pub fn put_bitmap_at(
        &mut self,
        page: usize,
        bucket: u32,
        state: PlayState,
        bitmap: Arc<Bitmap>,
    ) {
        let bytes = bitmap.memory_bytes();
        // 单个位图就超过总额度时直接不缓存，否则会把缓存瞬间清空
        if bytes > self.max_bitmap_bytes {
            return;
        }
        if let Some(old) = self.bitmaps.put((page, bucket, state), Arc::clone(&bitmap)) {
            self.bitmap_bytes = self.bitmap_bytes.saturating_sub(old.memory_bytes());
        }
        self.bitmap_bytes += bytes;
        self.evict_to_budget();
    }

    /// 按内存预算淘汰最久未使用的位图。
    fn evict_to_budget(&mut self) {
        while self.bitmap_bytes > self.max_bitmap_bytes {
            match self.bitmaps.pop_lru() {
                Some((_, old)) => {
                    self.bitmap_bytes = self.bitmap_bytes.saturating_sub(old.memory_bytes());
                }
                None => break,
            }
        }
    }

    pub fn get_scene(&mut self, page: usize) -> Option<Arc<Scene>> {
        self.scenes.get(&page).cloned()
    }

    pub fn put_scene(&mut self, page: usize, scene: Arc<Scene>) {
        self.scenes.put(page, scene);
    }

    /// 丢弃某页的所有缓存（如渲染参数变化后需要强制重渲）。
    pub fn invalidate_page(&mut self, page: usize) {
        let keys: Vec<BitmapKey> = self
            .bitmaps
            .iter()
            .filter(|(k, _)| k.0 == page)
            .map(|(k, _)| *k)
            .collect();
        for k in keys {
            if let Some(old) = self.bitmaps.pop(&k) {
                self.bitmap_bytes = self.bitmap_bytes.saturating_sub(old.memory_bytes());
            }
        }
        self.scenes.pop(&page);
    }

    pub fn clear(&mut self) {
        self.bitmaps.clear();
        self.scenes.clear();
        self.bitmap_bytes = 0;
    }

    #[inline]
    pub fn bitmap_count(&self) -> usize {
        self.bitmaps.len()
    }

    #[inline]
    pub fn scene_count(&self) -> usize {
        self.scenes.len()
    }

    #[inline]
    pub fn bitmap_bytes(&self) -> usize {
        self.bitmap_bytes
    }
}

/// 磁盘缓存。
///
/// # 为什么存原始像素而不是 PNG
///
/// 位图是**预乘 RGBA8**，而 PNG 规范是直通 alpha。
/// 若走 PNG 往返，需要在写入时反预乘、读取时再预乘 ——
/// 两次全图浮点运算，且在 alpha=0 的像素上会丢信息（反预乘需要除以 0）。
/// 直接存原始字节加一个 16 字节头部，正确性无歧义，读写也更快。
/// 代价是文件更大，由磁盘预算上限约束。
pub struct DiskCache {
    root: PathBuf,
    /// 容量上限（字节）。
    budget_bytes: u64,
    /// 当前占用（启动时扫描一次，之后增量维护）。
    used_bytes: u64,
}

/// 文件头魔数与版本。
const DISK_MAGIC: &[u8; 4] = b"OPPV";
/// 缓存格式版本。
///
/// # 什么时候必须 +1
///
/// 只改文件布局（这里）不够 —— **渲染结果本身变化时也必须 +1**。
/// 磁盘缓存只按「课件指纹 + 页码 + 缩放档」定位，
/// 光栅化算法一变，旧像素依然「格式合法」，会被原样取出来显示，
/// 于是修好的渲染看起来像没修（曾因此把已修复的文字镜像当成未修复）。
///
/// 版本号写进目录名，旧版本整目录一起失效，不会与新缓存混在一起。
///
/// 版本 3：字体面选择、行高模型（1.2em）、折行规则、
/// 中文字体名解析一共四处改动，页面上每一行的位置都会变。
///
/// 版本 4：翻转形状（`flipH` / `flipV`）里的文字不再跟着镜像，
/// 「REPORT」这类被翻转的形状上的文字从反写变成正着。
///
/// 版本 5：翻转**组合**（`p:grpSp` 的 `a:xfrm/@flipH`）里的文字同样不再镜像
/// —— 上一版只处理了形状自己的翻转，组合里的那批文字还是反写的。
///
/// 版本 6：显式的 `<a:noFill/>` 不再被回退成主题 `a:fillRef`。
/// 改了之后老师用「透明 + 红边」挖空的答案框才不会再变成蓝色实心块。
///
/// 版本 7：行高模型改为与字体无关的 1.2em、`spcPct` 小于 100% 也照样收紧，
/// 再加上「组合子形状的局部单位不再缩放字号/线宽/图片解码尺寸」。
/// 这几处一起改了之后，**每一页**的排版都会变，旧像素必须整体作废。
///
/// 版本 8：预设调整值（`a:gd`）改用十万分之一的口径（圆角不再鼓成胶囊）、
/// 阴影改到填充之前绘制（白框不再变灰框）、悬挂缩进不再重复扣宽度。
///
/// 版本 9：线条端点箭头开始真正绘制、补上竖卷形/离页连接符/云朵标注/动作按钮。
///
/// 版本 10：自带光栅化来源（PDF）改为「一次解释出原生档位图，其余尺寸由它缩下去」，
/// 并让预取统一写原生桶。小幅缩放下的画面由「原生解释」变成「双线性缩小」，
/// 像素会略有不同，旧缓存必须作废，否则同一页在不同机器上看到两种画法。
const DISK_VERSION: u32 = 10;
/// 头部长度：magic(4) + version(4) + width(4) + height(4)。
const DISK_HEADER_LEN: usize = 16;

impl DiskCache {
    /// 打开（或创建）磁盘缓存目录。
    ///
    /// `root` 可以是与别人共用的目录（本应用就是：办公软件出的逐页 PNG
    /// 住在同一个 `root` 下的 `wps/`）—— 本类型只读写自己那棵
    /// `v<N>` 子树，占用统计也只算它，见 [`DiskCache::version_root`]。
    pub fn open(root: impl AsRef<Path>, budget_bytes: u64) -> Result<DiskCache> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root).map_err(|e| {
            Error::other(format!("无法创建缓存目录 {}：{e}", root.display()))
        })?;

        // 顺手清掉旧版本号的目录（只认形如 `v<数字>` 的，别的一律不碰）
        purge_stale_versions(&root);

        // 占用只统计自己这棵子树：把邻居的体积算进来，会让我们一上来就
        // 觉得「超预算」，然后把刚写下去的缓存立刻删掉
        let version_root = root.join(format!("v{DISK_VERSION}"));
        let used_bytes = scan_dir_size(&version_root);
        Ok(DiskCache {
            root,
            budget_bytes,
            used_bytes,
        })
    }

    /// 本版本位图缓存自己的根：`<root>/v<N>`。
    ///
    /// # 为什么增删都必须限制在这棵子树里
    ///
    /// `root` 里不止住着位图缓存。以本应用为例，它还住着
    /// **「办公软件出的逐页 PNG」**（`<root>/wps/<版本>/<课件>/…`）——
    /// 那些不是缓存副产物，而是**画面来源**：删掉了，那几页就只能退回
    /// 自研渲染内核，老师看到的就是「和原稿不一样」。
    ///
    /// 这里踩过一次很深的坑：淘汰按修改时间从 `root` 往下删，于是
    /// 它把最早写的那些**页面图**当最旧文件删了（实测某份课件第 0~10 页
    /// 的整页图与 0~9 的分帧一起消失，第 11 页往后完好），
    /// 表现为「有的页面突然变成自研渲染」。
    fn version_root(&self) -> PathBuf {
        self.root.join(format!("v{DISK_VERSION}"))
    }

    /// 某个课件的缓存目录（带版本号，便于整体失效）。
    fn dir_for(&self, fingerprint: &str) -> PathBuf {
        self.version_root().join(fingerprint)
    }

    fn path_for(&self, fingerprint: &str, page: usize, bucket: u32) -> PathBuf {
        self.dir_for(fingerprint)
            .join(format!("{page}-{bucket}.bin"))
    }

    /// 读取缓存的位图。
    pub fn load(&self, fingerprint: &str, page: usize, bucket: u32) -> Option<Bitmap> {
        let path = self.path_for(fingerprint, page, bucket);
        let bytes = std::fs::read(&path).ok()?;
        decode_disk_bitmap(&bytes)
    }

    /// 写入位图。
    ///
    /// 写入失败不影响主流程（缓存只是加速手段），因此错误被吞掉。
    pub fn store(&mut self, fingerprint: &str, page: usize, bucket: u32, bitmap: &Bitmap) {
        let dir = self.dir_for(fingerprint);
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }

        let encoded = encode_disk_bitmap(bitmap);
        let path = self.path_for(fingerprint, page, bucket);

        // 先写临时文件再改名：避免写入中途崩溃留下半个文件，
        // 下次读取时被当成有效缓存
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, &encoded).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        if std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return;
        }

        self.used_bytes += encoded.len() as u64;
        self.enforce_budget();
    }

    /// 超出预算时按修改时间淘汰最旧的文件。
    ///
    /// 只在自己那棵子树（`<root>/v<N>`）里淘汰，见 [`DiskCache::version_root`]。
    fn enforce_budget(&mut self) {
        if self.used_bytes <= self.budget_bytes {
            return;
        }

        // 收集所有缓存文件及其修改时间
        let mut files: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
        collect_files(&self.version_root(), &mut files);
        files.sort_by_key(|(_, _, t)| *t);

        for (path, size, _) in files {
            if self.used_bytes <= self.budget_bytes {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                self.used_bytes = self.used_bytes.saturating_sub(size);
            }
        }
    }

    /// 清空位图磁盘缓存（**不碰 root 下的其它东西**，见 [`DiskCache::version_root`]）。
    pub fn clear(&mut self) -> Result<()> {
        let version_root = self.version_root();
        if version_root.exists() {
            std::fs::remove_dir_all(&version_root).map_err(|e| {
                Error::other(format!("无法清空缓存目录 {}：{e}", version_root.display()))
            })?;
        }
        std::fs::create_dir_all(&version_root).map_err(|e| {
            Error::other(format!("无法重建缓存目录 {}：{e}", version_root.display()))
        })?;
        self.used_bytes = 0;
        Ok(())
    }

    /// 删除某个课件的全部缓存（课件被删除或内容变化时调用）。
    pub fn purge_document(&mut self, fingerprint: &str) {
        let dir = self.dir_for(fingerprint);
        if let Ok(size) = dir_size(&dir) {
            if std::fs::remove_dir_all(&dir).is_ok() {
                self.used_bytes = self.used_bytes.saturating_sub(size);
            }
        }
    }

    #[inline]
    pub fn used_bytes(&self) -> u64 {
        self.used_bytes
    }

    #[inline]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// 把位图编码为磁盘格式。
fn encode_disk_bitmap(bitmap: &Bitmap) -> Vec<u8> {
    let mut out = Vec::with_capacity(DISK_HEADER_LEN + bitmap.data.len());
    out.extend_from_slice(DISK_MAGIC);
    out.extend_from_slice(&DISK_VERSION.to_le_bytes());
    out.extend_from_slice(&bitmap.width.to_le_bytes());
    out.extend_from_slice(&bitmap.height.to_le_bytes());
    out.extend_from_slice(&bitmap.data);
    out
}

/// 从磁盘格式解码位图。
fn decode_disk_bitmap(bytes: &[u8]) -> Option<Bitmap> {
    if bytes.len() < DISK_HEADER_LEN {
        return None;
    }
    if &bytes[0..4] != DISK_MAGIC {
        return None;
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    if version != DISK_VERSION {
        // 版本不匹配说明是旧版缓存，丢弃而不是误读
        return None;
    }
    let width = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
    let height = u32::from_le_bytes(bytes[12..16].try_into().ok()?);
    let data = bytes[DISK_HEADER_LEN..].to_vec();

    // 长度必须与尺寸自洽，否则说明文件被截断
    if data.len() != width as usize * height as usize * 4 {
        return None;
    }

    Some(Bitmap {
        width,
        height,
        format: PixelFormat::Rgba8Premultiplied,
        data,
    })
}

/// 计算目录（含子目录）的总字节数。
fn dir_size(dir: &Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    if !dir.exists() {
        return Ok(0);
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_dir() {
            total += dir_size(&entry.path())?;
        } else {
            total += meta.len();
        }
    }
    Ok(total)
}

fn scan_dir_size(dir: &Path) -> u64 {
    dir_size(dir).unwrap_or(0)
}

/// 这个名字是不是「位图缓存的版本目录」（形如 `v2` / `v10`）。
///
/// 规则定义在这里，因为这是位图缓存**自己的**命名规则。淘汰旧版本要用它，
/// 应用的设置页统计占用也要用它 —— 分散成两份，改命名时一定会漏掉一处，
/// 而症状是「清理缓存清不干净」这种最难察觉的那种。
pub fn is_version_dir(name: &str) -> bool {
    name.strip_prefix('v')
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// 删除非当前版本号的缓存目录。
///
/// 版本号写在目录名里（`v2/<课件指纹>/`），所以升级后旧目录不会被读到；
/// 这里顺手清掉，避免它们一直占着磁盘预算。
fn purge_stale_versions(root: &Path) {
    let keep = format!("v{DISK_VERSION}");
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // 只动形如 `v<数字>` 的目录，其它内容一律不碰
        if is_version_dir(&name) && name != keep {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// 收集目录下所有文件及其大小、修改时间。
fn collect_files(dir: &Path, out: &mut Vec<(PathBuf, u64, std::time::SystemTime)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            collect_files(&path, out);
        } else {
            let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            out.push((path, meta.len(), mtime));
        }
    }
}

/// 由缩放比得到缓存档位。
///
/// 量化到 0.01 精度：`0.999` 与 `1.001` 归为同一档，
/// 避免因浮点抖动导致缓存永远不命中。
pub fn scale_bucket(scale: f32) -> u32 {
    (scale.max(0.01) * 100.0).round() as u32
}

/// 未使用导入守卫：`HashMap` 供后续扩展（如按页索引的缩略图集合）。
#[allow(dead_code)]
fn _assert_types(_: HashMap<usize, usize>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use ppt_core::scene::{Color, Size};

    fn bitmap(w: u32, h: u32) -> Arc<Bitmap> {
        Arc::new(Bitmap::new_filled(w, h, Color::rgb(1, 2, 3)))
    }

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn memory_cache_hits_and_misses() {
        let mut c = MemoryCache::new(4, 64 * 1024 * 1024);
        assert!(c.get_bitmap(0, 100).is_none());

        c.put_bitmap(0, 100, bitmap(10, 10));
        assert!(c.get_bitmap(0, 100).is_some());
        // 不同缩放档互不影响
        assert!(c.get_bitmap(0, 200).is_none());
        assert_eq!(c.bitmap_count(), 1);
    }

    #[test]
    fn memory_cache_evicts_by_page_capacity() {
        let mut c = MemoryCache::new(2, 64 * 1024 * 1024);
        c.put_bitmap(0, 100, bitmap(10, 10));
        c.put_bitmap(1, 100, bitmap(10, 10));
        c.put_bitmap(2, 100, bitmap(10, 10));
        // 容量 2，最早的应被淘汰
        assert!(c.get_bitmap(0, 100).is_none(), "最久未使用应被淘汰");
        assert!(c.get_bitmap(2, 100).is_some());
    }

    #[test]
    fn memory_cache_evicts_to_byte_budget() {
        // 每张 100×100 位图 40000 字节；预算只够一张半
        let mut c = MemoryCache::new(10, 60_000);
        c.put_bitmap(0, 100, bitmap(100, 100));
        assert_eq!(c.bitmap_count(), 1);
        c.put_bitmap(1, 100, bitmap(100, 100));
        // 超预算后应淘汰到不超过预算
        assert!(c.bitmap_bytes() <= 60_000, "实际 {}", c.bitmap_bytes());
        assert!(c.bitmap_count() <= 1, "实际 {}", c.bitmap_count());
    }

    #[test]
    fn memory_cache_skips_oversized_bitmap() {
        let mut c = MemoryCache::new(10, 1000);
        // 单张就超过预算：不应缓存，也不应把已有内容清空
        c.put_bitmap(0, 100, bitmap(10, 10));
        c.put_bitmap(1, 100, bitmap(100, 100));
        assert!(
            c.get_bitmap(0, 100).is_some(),
            "超大位图不应挤掉已有缓存"
        );
        assert!(c.get_bitmap(1, 100).is_none());
    }

    #[test]
    fn lru_order_is_refreshed_by_access() {
        let mut c = MemoryCache::new(2, 64 * 1024 * 1024);
        c.put_bitmap(0, 100, bitmap(10, 10));
        c.put_bitmap(1, 100, bitmap(10, 10));
        // 访问 0 使其成为最近使用
        assert!(c.get_bitmap(0, 100).is_some());
        // 插入 2 应淘汰 1
        c.put_bitmap(2, 100, bitmap(10, 10));
        assert!(c.get_bitmap(0, 100).is_some(), "被访问过的应保留");
        assert!(c.get_bitmap(1, 100).is_none(), "未被访问的应淘汰");
    }

    #[test]
    fn scene_cache_works() {
        let mut c = MemoryCache::new(4, 64 * 1024 * 1024);
        let scene = Arc::new(Scene::new(Size::new(960.0, 540.0)));
        c.put_scene(3, Arc::clone(&scene));
        assert!(c.get_scene(3).is_some());
        assert_eq!(c.scene_count(), 1);
    }

    #[test]
    fn invalidate_page_removes_all_buckets() {
        let mut c = MemoryCache::new(8, 64 * 1024 * 1024);
        c.put_bitmap(5, 100, bitmap(10, 10));
        c.put_bitmap(5, 200, bitmap(10, 10));
        c.put_bitmap(6, 100, bitmap(10, 10));
        c.put_scene(5, Arc::new(Scene::new(Size::new(1.0, 1.0))));

        c.invalidate_page(5);
        assert!(c.get_bitmap(5, 100).is_none());
        assert!(c.get_bitmap(5, 200).is_none());
        assert!(c.get_scene(5).is_none());
        assert!(c.get_bitmap(6, 100).is_some(), "其它页不受影响");
    }

    #[test]
    fn clear_empties_everything() {
        let mut c = MemoryCache::new(4, 64 * 1024 * 1024);
        c.put_bitmap(0, 100, bitmap(10, 10));
        c.clear();
        assert_eq!(c.bitmap_count(), 0);
        assert_eq!(c.bitmap_bytes(), 0);
    }

    #[test]
    fn disk_bitmap_roundtrip_is_lossless() {
        let bmp = Bitmap::new_filled(7, 5, Color::rgba(10, 20, 30, 128));
        let encoded = encode_disk_bitmap(&bmp);
        let decoded = decode_disk_bitmap(&encoded).expect("应能解码");
        assert_eq!(decoded.width, 7);
        assert_eq!(decoded.height, 5);
        assert_eq!(decoded.format, PixelFormat::Rgba8Premultiplied);
        assert_eq!(decoded.data, bmp.data, "往返应逐字节一致");
    }

    #[test]
    fn disk_decode_rejects_bad_magic() {
        let mut encoded = encode_disk_bitmap(&Bitmap::new_filled(2, 2, Color::WHITE));
        encoded[0] = b'X';
        assert!(decode_disk_bitmap(&encoded).is_none());
    }

    #[test]
    fn disk_decode_rejects_old_version() {
        let mut encoded = encode_disk_bitmap(&Bitmap::new_filled(2, 2, Color::WHITE));
        encoded[4..8].copy_from_slice(&99u32.to_le_bytes());
        assert!(decode_disk_bitmap(&encoded).is_none(), "版本不符应丢弃");
    }

    #[test]
    fn disk_decode_rejects_truncated_data() {
        let encoded = encode_disk_bitmap(&Bitmap::new_filled(10, 10, Color::WHITE));
        let truncated = &encoded[..encoded.len() - 10];
        assert!(decode_disk_bitmap(truncated).is_none(), "截断文件应被拒绝");
        assert!(decode_disk_bitmap(&[]).is_none());
        assert!(decode_disk_bitmap(&[0u8; 8]).is_none());
    }

    #[test]
    fn disk_cache_stores_and_loads() {
        let dir = temp_dir("openpptview-diskcache-basic");
        let mut c = DiskCache::open(&dir, 64 * 1024 * 1024).expect("应能打开缓存目录");

        let bmp = Bitmap::new_filled(16, 8, Color::rgb(200, 100, 50));
        c.store("fp123", 4, 100, &bmp);

        let loaded = c.load("fp123", 4, 100).expect("应能读取缓存");
        assert_eq!(loaded.width, 16);
        assert_eq!(loaded.height, 8);
        assert_eq!(loaded.data, bmp.data);

        // 不同键应未命中
        assert!(c.load("fp123", 5, 100).is_none());
        assert!(c.load("other", 4, 100).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_cache_tracks_used_bytes() {
        let dir = temp_dir("openpptview-diskcache-bytes");
        let mut c = DiskCache::open(&dir, 64 * 1024 * 1024).unwrap();
        assert_eq!(c.used_bytes(), 0);

        c.store("fp", 0, 100, &Bitmap::new_filled(10, 10, Color::WHITE));
        assert!(c.used_bytes() > 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_cache_enforces_budget_by_evicting() {
        let dir = temp_dir("openpptview-diskcache-budget");
        // 预算只够两页（每页 100×100×4 + 16 ≈ 40016 字节）
        let mut c = DiskCache::open(&dir, 100_000).unwrap();

        for page in 0..5 {
            c.store("fp", page, 100, &Bitmap::new_filled(100, 100, Color::WHITE));
        }

        assert!(
            c.used_bytes() <= 100_000,
            "应淘汰到预算内，实际 {}",
            c.used_bytes()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 淘汰与清空都只许动自己那棵 `v<N>` 子树。
    ///
    /// 这条钉住的是一个很深的坑：位图缓存与「办公软件出的逐页 PNG」
    /// （`<root>/wps/…`）共用一个 root，而淘汰曾经从 root 往下删最旧的
    /// 文件 —— 于是把**页面图**当缓存删掉了，那几页只能退回自研渲染，
    /// 老师看到的就是「有的页面和原稿不一样」。
    #[test]
    fn disk_cache_leaves_siblings_alone() {
        let dir = temp_dir("openpptview-diskcache-neighbour");

        // 邻居：模拟办公软件出的页面图
        let page = dir.join("wps").join("v1").join("fingerprint").join("0.png");
        std::fs::create_dir_all(page.parent().unwrap()).unwrap();
        std::fs::write(&page, b"page raster, not a bitmap cache file").unwrap();

        // 预算小到一写就超，逼出淘汰
        let mut c = DiskCache::open(&dir, 1).unwrap();
        for page_no in 0..3 {
            c.store("fp", page_no, 100, &Bitmap::new_filled(100, 100, Color::WHITE));
        }

        assert!(c.used_bytes() <= 1, "应淘汰到预算内");
        assert!(c.load("fp", 0, 100).is_none(), "自己的缓存该被淘汰掉");
        assert!(page.is_file(), "淘汰不该碰邻居的文件");

        c.clear().unwrap();
        assert!(page.is_file(), "清空也不该碰邻居的文件");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 占用统计只算自己那棵子树。
    ///
    /// 把邻居的体积算进来，会让我们一上来就觉得「超预算」，
    /// 然后把刚写下去的缓存立刻删掉 —— 缓存等于失效。
    #[test]
    fn disk_cache_size_ignores_siblings() {
        let dir = temp_dir("openpptview-diskcache-size");
        let neighbour = dir.join("wps").join("v1").join("big.png");
        std::fs::create_dir_all(neighbour.parent().unwrap()).unwrap();
        std::fs::write(&neighbour, vec![0u8; 100 * 1024]).unwrap();

        let c = DiskCache::open(&dir, 64 * 1024 * 1024).unwrap();
        assert_eq!(c.used_bytes(), 0, "邻居的体积不该算进来");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_cache_clear_removes_everything() {
        let dir = temp_dir("openpptview-diskcache-clear");
        let mut c = DiskCache::open(&dir, 64 * 1024 * 1024).unwrap();
        c.store("fp", 0, 100, &Bitmap::new_filled(4, 4, Color::WHITE));
        assert!(c.load("fp", 0, 100).is_some());

        c.clear().unwrap();
        assert_eq!(c.used_bytes(), 0);
        assert!(c.load("fp", 0, 100).is_none());
        assert!(dir.exists(), "清空后目录应仍存在");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_cache_purge_document_removes_only_that_document() {
        let dir = temp_dir("openpptview-diskcache-purge");
        let mut c = DiskCache::open(&dir, 64 * 1024 * 1024).unwrap();
        c.store("docA", 0, 100, &Bitmap::new_filled(4, 4, Color::WHITE));
        c.store("docB", 0, 100, &Bitmap::new_filled(4, 4, Color::WHITE));

        c.purge_document("docA");
        assert!(c.load("docA", 0, 100).is_none());
        assert!(c.load("docB", 0, 100).is_some(), "其它课件不应被牵连");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_cache_survives_reopen() {
        let dir = temp_dir("openpptview-diskcache-reopen");
        {
            let mut c = DiskCache::open(&dir, 64 * 1024 * 1024).unwrap();
            c.store("fp", 7, 150, &Bitmap::new_filled(8, 8, Color::rgb(9, 9, 9)));
        }
        // 重新打开：应能读到上次写入的内容，且占用统计正确
        let c = DiskCache::open(&dir, 64 * 1024 * 1024).unwrap();
        assert!(c.load("fp", 7, 150).is_some(), "跨会话应命中");
        assert!(c.used_bytes() > 0, "启动时应扫描出已占用空间");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_cache_isolates_renderer_versions() {
        let dir = temp_dir("openpptview-diskcache-version");
        let mut c = DiskCache::open(&dir, 64 * 1024 * 1024).unwrap();
        c.store("fp", 0, 100, &Bitmap::new_filled(4, 4, Color::WHITE));

        // 缓存文件必须落在带版本号的目录里 —— 渲染算法一变，旧像素就取不到了
        assert_eq!(c.dir_for("fp"), dir.join(format!("v{DISK_VERSION}")).join("fp"));
        assert!(c.load("fp", 0, 100).is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_cache_drops_stale_version_dirs() {
        let dir = temp_dir("openpptview-diskcache-stale");
        std::fs::create_dir_all(dir.join("v1").join("olddeck")).unwrap();
        std::fs::write(dir.join("v1").join("olddeck").join("0-100.bin"), b"stale").unwrap();
        // 名字不像版本号的目录不应被误删
        std::fs::create_dir_all(dir.join("notes")).unwrap();

        let c = DiskCache::open(&dir, 64 * 1024 * 1024).unwrap();
        assert!(!dir.join("v1").exists(), "旧版本目录应被清理");
        assert!(dir.join("notes").exists(), "其它目录不应被触碰");
        assert_eq!(c.used_bytes(), 0, "清理后占用统计应归零");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scale_bucket_quantizes_floats() {
        assert_eq!(scale_bucket(1.0), 100);
        assert_eq!(scale_bucket(0.999), 100, "接近 1.0 应归入同一档");
        assert_eq!(scale_bucket(1.001), 100);
        assert_eq!(scale_bucket(2.0), 200);
        assert_eq!(scale_bucket(0.15), 15);
        // 极小与负值不应产生 0 档（0 档与「未指定」混淆）
        assert!(scale_bucket(0.0) > 0);
        assert!(scale_bucket(-1.0) > 0);
    }

    #[test]
    fn cache_source_variants_are_distinct() {
        assert_ne!(CacheSource::Memory, CacheSource::Disk);
        assert_ne!(CacheSource::Disk, CacheSource::Rendered);
    }
}
