//! 字体解析、系统字体索引与回退链。
//!
//! # 设计要点
//!
//! 1. **字体数据用 `Arc` 共享**：一次加载，多个排版任务并发读取。
//! 2. **按字符做覆盖检测**：课件里常出现「中文字体 + 英文单词 + 特殊符号」混排，
//!    而 OOXML 只为拉丁/东亚/复杂文种各指定一个字体名，
//!    真正的逐字符回退必须由我们自己完成，否则会出现豆腐块。
//! 3. **结果全部缓存**：`(家族, 粗体, 斜体) → 字体` 与 `(字体, 字符) → 是否覆盖`
//!    的查询在一次讲课中会重复上万次，缓存后开销可忽略。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use fontdb::{Database, Family, Query, Style, Weight};

use ppt_core::scene::FontSet;
use ppt_core::Error;

/// 字体在 [`FontContext`] 内的稳定标识。
pub type FontId = u32;

/// 字体字节的来源。
///
/// # 为什么要做这一层
///
/// 系统字体索引后会得到几百个字体面（本机是 395 个）。
/// `fontdb` **只保存路径**（`Source::File`），并不持有映射 ——
/// 若我们按路径 `fs::read` 把字节读进堆，实测会占掉 **973MB 私有内存**
/// （系统字体目录本身就有近 1GB），这对「适合老电脑」是致命的。
///
/// 改为 mmap 后，只有被真正访问到的表（`head`/`hhea`/`cmap` 等，几 KB）
/// 才会由操作系统换入物理内存；未被用到的字体几乎零占用。
#[derive(Clone)]
pub enum FontData {
    /// 内存映射（生产路径）。
    Mapped(Arc<memmap2::Mmap>),
    /// 直接持有的字节（测试注入、或 mmap 不可用时的兜底）。
    Inline(Arc<Vec<u8>>),
}

impl std::fmt::Debug for FontData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FontData::Mapped(m) => write!(f, "FontData::Mapped({} 字节)", m.len()),
            FontData::Inline(v) => write!(f, "FontData::Inline({} 字节)", v.len()),
        }
    }
}

impl AsRef<[u8]> for FontData {
    fn as_ref(&self) -> &[u8] {
        match self {
            FontData::Mapped(m) => m,
            FontData::Inline(v) => v,
        }
    }
}

impl FontData {
    #[inline]
    pub fn len(&self) -> usize {
        self.as_ref().len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.as_ref().is_empty()
    }
}

/// 字体文件的映射缓存。
///
/// 同一个文件可能注册成多个面（`.ttc` 集合），
/// 因此按**路径**缓存映射，多个面共用同一份。
pub struct FontDataStore {
    entries: RwLock<HashMap<PathBuf, FontData>>,
    /// 打开失败的文件也记下来，避免反复尝试（例如权限受限的系统字体）。
    failed: RwLock<std::collections::HashSet<PathBuf>>,
}

impl Default for FontDataStore {
    fn default() -> Self {
        FontDataStore::new()
    }
}

impl FontDataStore {
    pub fn new() -> FontDataStore {
        FontDataStore {
            entries: RwLock::new(HashMap::new()),
            failed: RwLock::new(std::collections::HashSet::new()),
        }
    }

    /// 取（或建立）某个字体文件的映射。
    pub fn get(&self, path: &Path) -> Option<FontData> {
        if let Ok(cache) = self.entries.read() {
            if let Some(hit) = cache.get(path) {
                return Some(hit.clone());
            }
        }
        if let Ok(failed) = self.failed.read() {
            if failed.contains(path) {
                return None;
            }
        }

        let data = Self::map_file(path);

        match &data {
            Some(d) => {
                if let Ok(mut cache) = self.entries.write() {
                    cache.insert(path.to_path_buf(), d.clone());
                }
            }
            None => {
                if let Ok(mut failed) = self.failed.write() {
                    failed.insert(path.to_path_buf());
                }
            }
        }
        data
    }

    fn map_file(path: &Path) -> Option<FontData> {
        let file = std::fs::File::open(path).ok()?;
        // SAFETY: 字体文件在应用运行期间不会被写入；映射仅用于只读解析。
        // 若文件被外部程序替换/截断，最坏情况是解析失败并降级，
        // 不会造成内存安全问题（我们只做只读切片访问）。
        match unsafe { memmap2::Mmap::map(&file) } {
            Ok(m) if !m.is_empty() => Some(FontData::Mapped(Arc::new(m))),
            // mmap 失败（如网络盘、特殊文件系统）时退化为读入内存。
            // 这种情况极少，且只针对单个文件，不会拖垮整体内存。
            _ => std::fs::read(path)
                .ok()
                .filter(|v| !v.is_empty())
                .map(|v| FontData::Inline(Arc::new(v))),
        }
    }

    /// 已映射的文件数（诊断用）。
    pub fn mapped_count(&self) -> usize {
        self.entries.read().map(|c| c.len()).unwrap_or(0)
    }
}

impl std::fmt::Debug for FontDataStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FontDataStore")
            .field("mapped", &self.mapped_count())
            .finish()
    }
}

/// 已加载的字体。
pub struct LoadedFont {
    pub id: FontId,
    /// 注册到系统的家族名。
    pub family: String,
    /// 字体数据（按需映射）。
    data: FontData,
    /// 字体集合（.ttc）中的面索引。
    pub face_index: u32,
    /// 归一化度量：em 方框为 1.0。
    pub units_per_em: f32,
    pub ascender: f32,
    pub descender: f32,
    pub line_gap: f32,
    pub cap_height: f32,
    pub x_height: f32,
    /// 是否为东亚字体（用于排版时的标点挤压等策略）。
    pub is_cjk: bool,
}

impl std::fmt::Debug for LoadedFont {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedFont")
            .field("id", &self.id)
            .field("family", &self.family)
            .field("face_index", &self.face_index)
            .finish()
    }
}

impl LoadedFont {
    /// 字体数据的字节切片。
    #[inline]
    pub fn bytes(&self) -> &[u8] {
        self.data.as_ref()
    }

    /// 占位字体：数据为空，所有查询都退化为默认度量。
    ///
    /// 用于「字体解析失败但仍需保持行结构完整」的降级路径 ——
    /// 让排版继续跑完，总好过整页文本消失。
    pub fn placeholder() -> LoadedFont {
        LoadedFont {
            id: 0,
            family: String::new(),
            data: FontData::Inline(Arc::new(Vec::new())),
            face_index: 0,
            units_per_em: 1000.0,
            ascender: 0.8,
            descender: -0.2,
            line_gap: 0.0,
            cap_height: 0.7,
            x_height: 0.5,
            is_cjk: false,
        }
    }

    /// 是否为占位字体（无实际字形数据）。
    #[inline]
    pub fn is_placeholder(&self) -> bool {
        self.data.is_empty()
    }

    /// 在字体数据上建立一个 `ttf-parser` 视图。
    pub fn with_face<T>(&self, f: impl FnOnce(&ttf_parser::Face<'_>) -> T) -> Option<T> {
        let face = ttf_parser::Face::parse(self.bytes(), self.face_index).ok()?;
        Some(f(&face))
    }

    /// 在字体数据上建立一个 `rustybuzz` 视图。
    pub fn with_shaping_face<T>(&self, f: impl FnOnce(&rustybuzz::Face<'_>) -> T) -> Option<T> {
        let face = ttf_parser::Face::parse(self.bytes(), self.face_index).ok()?;
        let rb = rustybuzz::Face::from_face(face);
        Some(f(&rb))
    }

    /// 该字体是否有某个字符的字形。
    pub fn covers(&self, ch: char) -> bool {
        self.with_face(|f| f.glyph_index(ch).is_some())
            .unwrap_or(false)
    }

    /// 以「em 的倍数」表示的字符前进宽度（不经过整形，仅用于快速估算）。
    pub fn advance_em(&self, ch: char) -> f32 {
        self.with_face(|f| {
            let gid = match f.glyph_index(ch) {
                Some(g) => g,
                None => return 0.5,
            };
            match f.glyph_hor_advance(gid) {
                Some(a) => a as f32 / self.units_per_em.max(1.0),
                None => 0.5,
            }
        })
        .unwrap_or(0.5)
    }

    /// 行高（em 的倍数）。
    ///
    /// # PowerPoint 的单倍行距就是固定 1.2 倍字号，与字体自身度量无关
    ///
    /// 拿课件里 `spAutoFit` 的文本框当标尺（它里面写的高度就是 PowerPoint
    /// 排版的实际高度）反推：**同一个 24pt 文本框，无论里面是纯拉丁文还是
    /// 中英混排，PowerPoint 写出来的高度都一样**（36.2pt = 29.0pt 正文 + 7.2pt 内边距），
    /// 也就是 1.21em 左右。字体自己的 (asc - desc + lineGap) 在这个差值里
    /// 根本没有出现。
    ///
    /// # 曾经错在哪
    ///
    /// 这里原先写的是 `(asc - desc + lineGap).max(1.2)`：对 Times New Roman
    /// （1.150）、宋体 / 黑体（1.141）这些**小于** 1.2 的字体是对的，
    /// 但对**大于** 1.2 的字体就闯祸了 —— 微软雅黑的 (asc - desc + lineGap)
    /// 是 1.332，于是每一行都比 PowerPoint 高出 13%。
    ///
    /// 后果不是「字大一点」这么轻：课件里那页 13 行的听力选项，
    /// 文本框声明 366pt、PowerPoint 正好排满，我们却排出 415pt ——
    /// 后两行直接顶出文本框、掉到页面外。老师看到的就是「内容溢出了」。
    ///
    /// 所以这里退回固定值。取 1.2 而不是实测的 1.21，是**刻意留 0.8% 的余量**：
    /// 行高偏小只会让文字略紧一点（看不出来），偏大就会像上面那样溢出页面。
    #[inline]
    pub fn line_height_em(&self) -> f32 {
        LINE_HEIGHT_EM
    }
}

/// 单倍行距相对字号的比例。
///
/// 取值依据见 [`LoadedFont::line_height_em`]。
pub const LINE_HEIGHT_EM: f32 = 1.2;

/// 字体查询键。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FontKey {
    family: String,
    bold: bool,
    italic: bool,
}

/// 字体文件的映射缓存。
///
/// 持有它以确保「已解析的字体」在 `FontContext` 存活期间不会失效。
pub struct FontContext {
    fonts: Vec<Arc<LoadedFont>>,
    /// 家族名（小写）→ 该家族下的所有面。
    families: HashMap<String, Vec<FontId>>,
    resolve_cache: RwLock<HashMap<FontKey, Option<FontId>>>,
    coverage_cache: RwLock<HashMap<(FontId, char), bool>>,
    /// 逐字符回退的最终结果缓存。
    fallback_cache: RwLock<HashMap<(u32, FontId), Option<FontId>>>,
    /// 全局兜底字体（覆盖 CJK 的那一个），避免每次都全库扫描。
    generic_cjk: RwLock<Option<FontId>>,
    generic_latin: RwLock<Option<FontId>>,
    /// 字体文件的映射缓存（`LoadedFont::data` 的生命周期由它保证）。
    #[allow(dead_code)]
    store: FontDataStore,
}

impl std::fmt::Debug for FontContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FontContext")
            .field("fonts", &self.fonts.len())
            .field("families", &self.families.len())
            .finish()
    }
}

/// 中文字体回退链（按优先级）。
///
/// 覆盖国内课堂电脑的实际字体分布：Windows 自带雅黑/黑体/宋体/等线，
/// 加上常见的开源与 macOS 字体，避免跨平台时缺字。
const CJK_FALLBACK: &[&str] = &[
    "microsoft yahei",
    "微软雅黑",
    "microsoft yahei ui",
    "dengxian",
    "等线",
    "simhei",
    "黑体",
    "simsun",
    "宋体",
    "nsimsun",
    "kaiti",
    "楷体",
    "fangsong",
    "仿宋",
    "noto sans cjk sc",
    "source han sans sc",
    "思源黑体",
    "noto serif cjk sc",
    "pingfang sc",
    "hiragino sans gb",
    "heiti sc",
    "songti sc",
    "malgun gothic",
    "yu gothic",
    "meiryo",
];

/// 西文字体回退链。
const LATIN_FALLBACK: &[&str] = &[
    "segoe ui",
    "arial",
    "helvetica",
    "calibri",
    "tahoma",
    "verdana",
    "trebuchet ms",
    "times new roman",
    "georgia",
    "dejavu sans",
    "liberation sans",
    "noto sans",
    "roboto",
];

/// 符号字体回退链（项目符号、几何图形等）。
const SYMBOL_FALLBACK: &[&str] = &[
    "segoe ui symbol",
    "segoe ui emoji",
    "segoe ui historic",
    "wingdings",
    "wingdings 2",
    "wingdings 3",
    "webdings",
    "symbol",
    "dejavu sans",
    "noto sans symbols",
    "noto sans symbols 2",
];

impl Default for FontContext {
    fn default() -> FontContext {
        FontContext::new()
    }
}

impl FontContext {
    /// 扫描并索引系统字体。
    ///
    /// 这一步在冷启动时会遍历系统字体目录（Windows 上通常 200~500 个文件）。
    /// `fontdb` 默认使用内存映射，不会把字体字节读进内存，
    /// 因此即使字体很多，索引本身的耗时与内存都可控。
    pub fn new() -> FontContext {
        let mut db = Database::new();
        db.load_system_fonts();
        FontContext::from_database(db)
    }

    /// 只加载指定目录下的字体（用于测试与嵌入式场景）。
    pub fn from_dir(dir: &std::path::Path) -> FontContext {
        let mut db = Database::new();
        db.load_fonts_dir(dir);
        FontContext::from_database(db)
    }

    /// 不加载任何系统字体（用于纯单元测试）。
    pub fn empty() -> FontContext {
        FontContext::from_database(Database::new())
    }

    fn from_database(db: Database) -> FontContext {
        let store = FontDataStore::new();
        let mut fonts: Vec<Arc<LoadedFont>> = Vec::new();
        let mut families: HashMap<String, Vec<FontId>> = HashMap::new();

        for info in db.faces() {
            let (family, is_cjk_hint) = match info.families.first() {
                Some((name, _)) => (
                    name.clone(),
                    // 只要**任一**名字像 CJK 家族就算 —— 中文名往往排在英文名之后
                    info.families
                        .iter()
                        .any(|(n, _)| looks_like_cjk_family(n)),
                ),
                None => continue,
            };

            // 取字体文件路径。`fontdb` 只保存路径（`Source::File`），
            // 数据靠我们自己的 mmap 缓存按需建立。
            let path: PathBuf = match &info.source {
                fontdb::Source::File(p) => p.clone(),
                fontdb::Source::SharedFile(p, _) => p.clone(),
                // `Source::Binary` 是调用方注入的内存字体（测试用），
                // 没有路径，只能直接持有字节
                fontdb::Source::Binary(_) => continue,
            };

            // 只映射 + 解析度量；**不读取整个字体文件**
            let Some(loaded) = read_metrics(&store, &path, info.index, family.clone(), is_cjk_hint)
            else {
                continue;
            };

            let id = fonts.len() as FontId;
            let mut loaded = loaded;
            loaded.id = id;
            // 这张字面的**所有**家族名都要进索引，包括本地化名。
            //
            // 课件里写的是「黑体」「宋体」「微软雅黑」，而字体文件里
            // 排在最前的一般是英文名（SimHei / SimSun / Microsoft YaHei）。
            // 只登记第一个名字，中文名就全部落空，解析失败后悄悄回退到
            // 另一款 CJK 字体 —— 字面看着没事，行高与字宽却整套变了，
            // 整页排版跟着往下偏，而且很难查。
            for (name, _lang) in &info.families {
                let key = name.to_ascii_lowercase();
                let slot = families.entry(key).or_default();
                if !slot.contains(&id) {
                    slot.push(id);
                }
            }
            fonts.push(Arc::new(loaded));
        }

        log::debug!(
            "字体索引完成：{} 个面 / {} 个家族，已映射 {} 个文件",
            fonts.len(),
            families.len(),
            store.mapped_count()
        );

        FontContext {
            fonts,
            families,
            resolve_cache: RwLock::new(HashMap::new()),
            coverage_cache: RwLock::new(HashMap::new()),
            fallback_cache: RwLock::new(HashMap::new()),
            generic_cjk: RwLock::new(None),
            generic_latin: RwLock::new(None),
            store,
        }
    }

    /// 已索引的字体数量。
    #[inline]
    pub fn font_count(&self) -> usize {
        self.fonts.len()
    }

    /// 已索引的家族数量。
    #[inline]
    pub fn family_count(&self) -> usize {
        self.families.len()
    }

    pub fn font(&self, id: FontId) -> Option<&Arc<LoadedFont>> {
        self.fonts.get(id as usize)
    }

    /// 按家族名解析字体，缺失时返回 `None`。
    ///
    /// 匹配策略：家族名大小写不敏感；同家族内按粗体/斜体接近程度挑选，
    /// 例如请求粗体但只有常规字重时，返回常规字体并由渲染器做「伪粗体」。
    pub fn resolve(&self, family: &str, bold: bool, italic: bool) -> Option<FontId> {
        let key = FontKey {
            family: family.trim().to_ascii_lowercase(),
            bold,
            italic,
        };
        if let Some(hit) = self.resolve_cache.read().ok()?.get(&key) {
            return *hit;
        }

        let result = self.resolve_uncached(&key);
        if let Ok(mut cache) = self.resolve_cache.write() {
            cache.insert(key, result);
        }
        result
    }

    fn resolve_uncached(&self, key: &FontKey) -> Option<FontId> {
        if key.family.is_empty() {
            return None;
        }

        // 先按原样查。"Segoe UI Light" / "Segoe UI Semibold" 这类
        // **本身就是合法家族名**，不能一上来就把后缀削掉
        if let Some(candidates) = self.families.get(&key.family) {
            return self.pick_best(candidates, key.bold, key.italic);
        }

        // 再按去掉字重/别名后缀的规范化名字查一次。
        //
        // 课件里带修饰的写法非常常见（`Times New Roman Regular`、
        // `宋体 (中文正文)`），原样查必定落空 —— 以前就是在这里落空的，
        // 于是整段文字退到通用字体（Segoe UI），**字宽差 11%**：
        // 本来一行放得下的句子多折一行，压到下面那个文本框上。
        let normalized = normalize_family_name(&key.family);
        if normalized != key.family {
            if let Some(candidates) = self.families.get(&normalized) {
                return self.pick_best(candidates, key.bold, key.italic);
            }
        }

        None
    }

    /// 在候选面中挑选最接近请求字重/字形的一个。
    ///
    /// 注意比较方向：`style_score` 是**代价**，越小越接近。
    /// 这里曾经写成 `s >= score => 保留`，等于「新来的更合适也不换」——
    /// 结果同家族里最后一张字面（Times New Roman 的粗斜体）总会赢，
    /// 于是所有拉丁文都按粗斜体的字宽排版：整体宽约 5%，
    /// 到处多折一行，表格长高顶出页面、文本框文字溢出框外。
    fn pick_best(&self, candidates: &[FontId], bold: bool, italic: bool) -> Option<FontId> {
        let mut best: Option<(i32, FontId)> = None;
        for &id in candidates {
            let Some(font) = self.font(id) else { continue };
            let score = self.style_score(font, bold, italic);
            match best {
                // 已有同样合适或更合适的（代价更小）→ 保留先到的那张
                Some((s, _)) if s <= score => {}
                _ => best = Some((score, id)),
            }
        }
        best.map(|(_, id)| id)
    }

    /// 打分：分数越低越接近请求的样式。
    ///
    /// 需要读取字体的 OS/2 与 head 表，故做一层缓存（复用 resolve_cache 的键空间不冲突）。
    fn style_score(&self, font: &LoadedFont, bold: bool, italic: bool) -> i32 {
        let (weight, is_italic) = font
            .with_face(|f| {
                let w = f.weight().to_number() as i32;
                let i = f.is_italic() || f.is_oblique();
                (w, i)
            })
            .unwrap_or((400, false));

        let weight_score = if bold {
            // 期望粗体：越粗越好，700 为标准粗体
            (700 - weight).abs()
        } else {
            // 期望常规：400 最佳，避免误选到粗体
            (400 - weight).abs()
        };
        let italic_score = if italic == is_italic { 0 } else { 200 };
        weight_score + italic_score
    }

    /// 从回退链里挑第一个能覆盖该字符的字体。
    fn first_covering(&self, chain: &[&str], bold: bool, italic: bool, ch: char) -> Option<FontId> {
        for family in chain {
            if let Some(id) = self.resolve(family, bold, italic) {
                if self.covers(id, ch) {
                    return Some(id);
                }
            }
        }
        None
    }

    /// 该字体是否覆盖某字符（带缓存）。
    pub fn covers(&self, id: FontId, ch: char) -> bool {
        if let Ok(cache) = self.coverage_cache.read() {
            if let Some(v) = cache.get(&(id, ch)) {
                return *v;
            }
        }
        let v = self
            .font(id)
            .map(|f| f.covers(ch))
            .unwrap_or(false);
        if let Ok(mut cache) = self.coverage_cache.write() {
            cache.insert((id, ch), v);
        }
        v
    }

    /// 为某个字符挑选实际使用的字体。
    ///
    /// 优先级：调用方指定的字体 → 通用 CJK 兜底 → 全库扫描。
    /// `hint` 是 OOXML 里为拉丁/东亚/复杂文种指定的字体名，
    /// 调用方按字符所属文种传入对应的那个。
    pub fn font_for_char(
        &self,
        hint: Option<&str>,
        bold: bool,
        italic: bool,
        ch: char,
    ) -> Option<FontId> {
        let cache_key = (
            ch as u32,
            hint.map(|h| hash_str(h, bold, italic)).unwrap_or(u32::MAX),
        );
        if let Ok(cache) = self.fallback_cache.read() {
            if let Some(v) = cache.get(&cache_key) {
                return *v;
            }
        }

        let result = self.font_for_char_uncached(hint, bold, italic, ch);
        if let Ok(mut cache) = self.fallback_cache.write() {
            cache.insert(cache_key, result);
        }
        result
    }

    fn font_for_char_uncached(
        &self,
        hint: Option<&str>,
        bold: bool,
        italic: bool,
        ch: char,
    ) -> Option<FontId> {
        // 1) 课件指定的字体
        if let Some(name) = hint {
            if let Some(id) = self.resolve(name, bold, italic) {
                if self.covers(id, ch) {
                    return Some(id);
                }
            }
        }

        // 2) 按字符所属文种走对应的回退链
        let chain = if is_symbol_char(ch) {
            SYMBOL_FALLBACK
        } else if is_cjk_char(ch) {
            CJK_FALLBACK
        } else {
            LATIN_FALLBACK
        };
        if let Some(id) = self.first_covering(chain, bold, italic, ch) {
            return Some(id);
        }

        // 3) 交叉尝试另一条链（例如中文字体里带有的西文字形）
        let alt = if is_cjk_char(ch) { LATIN_FALLBACK } else { CJK_FALLBACK };
        if let Some(id) = self.first_covering(alt, bold, italic, ch) {
            return Some(id);
        }

        // 4) 通用兜底
        if let Some(id) = self.generic_for(ch, bold, italic) {
            return Some(id);
        }

        // 5) 全库扫描（结果已缓存，且只在极端缺字时触发）
        self.scan_any_covering(ch, bold, italic)
    }

    fn generic_for(&self, ch: char, bold: bool, italic: bool) -> Option<FontId> {
        let slot = if is_cjk_char(ch) {
            &self.generic_cjk
        } else {
            &self.generic_latin
        };
        if let Ok(guard) = slot.read() {
            if let Some(id) = *guard {
                if self.covers(id, ch) {
                    return Some(id);
                }
            }
        }
        let chain = if is_cjk_char(ch) { CJK_FALLBACK } else { LATIN_FALLBACK };
        let found = self.first_covering(chain, bold, italic, ch);
        if let (Some(id), Ok(mut guard)) = (found, slot.write()) {
            *guard = Some(id);
        }
        found
    }

    fn scan_any_covering(&self, ch: char, bold: bool, italic: bool) -> Option<FontId> {
        let mut best: Option<(i32, FontId)> = None;
        for font in &self.fonts {
            if !self.covers(font.id, ch) {
                continue;
            }
            let score = self.style_score(font, bold, italic);
            match best {
                Some((s, _)) if s <= score => {}
                _ => best = Some((score, font.id)),
            }
        }
        best.map(|(_, id)| id)
    }

    /// 按 [`FontSet`] 为某个字符挑字体。
    ///
    /// `FontSet` 里为拉丁/东亚/复杂文种分别指定了字体名，
    /// 这里根据字符所属文种选择对应项。
    ///
    /// `lang` 是这段文字的 `a:rPr/@lang`：弯引号、破折号这类「宽窄随语境变」
    /// 的标点要看它 —— 中文里是全角，西文里是窄形（见 [`is_context_sensitive_punct`]）。
    pub fn font_for_char_in_set(
        &self,
        set: &FontSet,
        bold: bool,
        italic: bool,
        lang: Option<&str>,
        ch: char,
    ) -> Option<FontId> {
        let hint = if is_symbol_char(ch) {
            set.symbol
                .as_deref()
                .or(set.ea.as_deref())
                .or(set.latin.as_deref())
        } else if is_cjk_char(ch) {
            set.ea
                .as_deref()
                .or(set.cs.as_deref())
                .or(set.latin.as_deref())
        } else if is_context_sensitive_punct(ch) && is_east_asian_lang(lang) {
            set.ea.as_deref().or(set.latin.as_deref())
        } else if is_complex_script(ch) {
            set.cs.as_deref().or(set.latin.as_deref())
        } else {
            set.latin.as_deref().or(set.ea.as_deref())
        };
        self.font_for_char(hint, bold, italic, ch)
    }

    /// 系统里是否存在某个家族（用于「缺失字体」提示与诊断）。
    pub fn has_family(&self, family: &str) -> bool {
        self.families.contains_key(&family.trim().to_ascii_lowercase())
    }
}

/// 读取一个字体面的度量。
///
/// # 关键：只 mmap，不读取整份文件
///
/// `ttf-parser::Face::parse` 只访问 `head`/`hhea`/`OS/2`/`cmap` 等少数表，
/// 因此 mmap 之后只有这几 KB 会被换入物理内存。
/// 若在这里 `fs::read` 整份文件（尤其 CJK 字体动辄 20~40MB），
/// 395 个面会把近 1GB 私有内存吃光 —— 这是实测踩过的坑。
fn read_metrics(
    store: &FontDataStore,
    path: &Path,
    face_index: u32,
    family: String,
    is_cjk_hint: bool,
) -> Option<LoadedFont> {
    let data = store.get(path)?;

    // 先在一个作用域内解析出度量，之后再移动 `data`，
    // 避免「借用了 data 又要把 data 移进结构体」的冲突
    let metrics = {
        let face = ttf_parser::Face::parse(data.as_ref(), face_index).ok()?;
        let upem = face.units_per_em() as f32;
        let upem = if upem <= 0.0 { 1000.0 } else { upem };
        (
            upem,
            face.ascender() as f32 / upem,
            face.descender() as f32 / upem,
            face.line_gap() as f32 / upem,
            face.capital_height().unwrap_or(face.ascender()) as f32 / upem,
            face.x_height().unwrap_or(face.ascender() / 2) as f32 / upem,
        )
    };

    Some(LoadedFont {
        id: 0,
        family,
        data,
        face_index,
        units_per_em: metrics.0,
        ascender: metrics.1,
        descender: metrics.2,
        line_gap: metrics.3,
        cap_height: metrics.4,
        x_height: metrics.5,
        is_cjk: is_cjk_hint,
    })
}

fn hash_str(s: &str, bold: bool, italic: bool) -> u32 {
    // FNV-1a，足够稳定且无需引入哈希库
    let mut h: u32 = 0x811c_9dc5;
    for b in s.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h ^= (bold as u32) << 1;
    h ^= (italic as u32) << 2;
    h
}

/// 家族名是否看起来是东亚字体。
fn looks_like_cjk_family(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    const MARKERS: &[&str] = &[
        "yahei", "song", "sun", "sung", "hei", "kai", "fang", "ming", "jhenghei", "cjk", "han",
        "pingfang", "hiragino", "gothic", "mincho", "malgun", "batang", "gulim",
    ];
    if MARKERS.iter().any(|m| lower.contains(m)) {
        return true;
    }
    // 中文家族名（雅黑/黑体/宋体…）
    name.chars().any(is_cjk_char)
}

/// 是否属于需要按东亚规则排版的字符。
///
/// 覆盖 CJK 统一表意文字、扩展 A、兼容表意文字、中日韩标点、全角形式、
/// 平假名/片假名、谚文。
pub fn is_cjk_char(ch: char) -> bool {
    let c = ch as u32;
    matches!(c,
        0x1100..=0x11FF        // 谚文字母
        | 0x2E80..=0x2EFF      // CJK 部首补充
        | 0x2F00..=0x2FDF      // 康熙部首
        | 0x3000..=0x303F      // CJK 符号与标点
        | 0x3040..=0x309F      // 平假名
        | 0x30A0..=0x30FF      // 片假名
        | 0x3100..=0x312F      // 注音符号
        | 0x3130..=0x318F      // 谚文兼容字母
        | 0x31C0..=0x31EF      // CJK 笔画
        | 0x3200..=0x32FF      // 带圈字符
        | 0x3300..=0x33FF      // CJK 兼容
        | 0x3400..=0x4DBF      // 扩展 A
        | 0x4E00..=0x9FFF      // 基本区
        | 0xA960..=0xA97F      // 谚文字母扩展 A
        | 0xAC00..=0xD7AF      // 谚文音节
        | 0xF900..=0xFAFF      // 兼容表意文字
        | 0xFE10..=0xFE1F      // 竖排标点
        | 0xFE30..=0xFE4F      // CJK 兼容形式
        | 0xFF00..=0xFFEF      // 全角形式
        | 0x20000..=0x2FA1F    // 扩展 B 及以后
    )
}

/// 宽度随语境变化的标点：弯引号 `’ “ ”`、破折号 `—`、省略号 `…` 等（U+2000~U+206F）。
///
/// 它们在中文排版里占一个全角字宽，在西文排版里是窄形 —— 同一个字符两套宽度。
/// 所以这类字符单看字符本身决定不了用哪套字体，要看这段文字是中文还是西文
/// （`a:rPr/@lang`）：中文走东亚字体，西文走拉丁字体。
fn is_context_sensitive_punct(ch: char) -> bool {
    matches!(ch as u32, 0x2000..=0x206F)
}

/// 这段文字的 `a:rPr/@lang` 是不是东亚语言。
fn is_east_asian_lang(lang: Option<&str>) -> bool {
    match lang {
        Some(l) => {
            let l = l.trim().to_ascii_lowercase();
            l.starts_with("zh") || l.starts_with("ja") || l.starts_with("ko")
        }
        None => false,
    }
}

/// 是否属于复杂文种（阿拉伯、希伯来、天城文等），需要走 `cs` 字体与双向排版。
pub fn is_complex_script(ch: char) -> bool {
    let c = ch as u32;
    matches!(c,
        0x0590..=0x05FF        // 希伯来
        | 0x0600..=0x06FF      // 阿拉伯
        | 0x0700..=0x074F      // 叙利亚
        | 0x0750..=0x077F
        | 0x0900..=0x097F      // 天城文
        | 0x0980..=0x09FF      // 孟加拉
        | 0x0A00..=0x0A7F      // 古木基
        | 0x0A80..=0x0AFF      // 古吉拉特
        | 0x0B00..=0x0B7F      // 奥里亚
        | 0x0B80..=0x0BFF      // 泰米尔
        | 0x0C00..=0x0C7F      // 泰卢固
        | 0x0C80..=0x0CFF      // 卡纳达
        | 0x0D00..=0x0D7F      // 马拉雅拉姆
        | 0x0E00..=0x0E7F      // 泰文
        | 0x0E80..=0x0EFF      // 老挝文
        | 0x0F00..=0x0FFF      // 藏文
        | 0x1000..=0x109F      // 缅甸文
        | 0x1780..=0x17FF      // 高棉文
        | 0xFB1D..=0xFB4F      // 希伯来表现形式
        | 0xFE70..=0xFEFF      // 阿拉伯表现形式
    )
}

/// 是否属于符号/图形字符（项目符号、箭头、几何图形等）。
///
/// 这些字符常见于 `Wingdings` 等符号字体，回退策略与正文不同：
/// 优先 `a:sym`，其次东亚字体。
///
/// # 为什么**不含** U+2000~U+206F（常用标点）
///
/// 弯引号 `’ “ ”`、破折号 `—`、省略号 `…` 都在这个区间里，但它们是**正文标点**，
/// PowerPoint 会拿拉丁字体去排（Times New Roman 里的 `’` 只有 0.25em）；
/// 一旦归到符号/东亚一类，就会落到宋体、黑体上取到**全角**字形（整整 1em），
/// 一句话里带几个弯引号，整行就宽出 2%~3% —— 正好够把最后那个词挤到下一行。
pub fn is_symbol_char(ch: char) -> bool {
    let c = ch as u32;
    matches!(c,
        0x2070..=0x209F      // 上下标
        | 0x20A0..=0x20CF      // 货币符号
        | 0x2100..=0x214F      // 字母式符号
        | 0x2150..=0x218F      // 数字形式
        | 0x2190..=0x21FF      // 箭头
        | 0x2200..=0x22FF      // 数学运算符
        | 0x2300..=0x23FF      // 杂项技术符号
        | 0x2460..=0x24FF      // 带圈字母数字
        | 0x25A0..=0x25FF      // 几何图形
        | 0x2600..=0x26FF      // 杂项符号
        | 0x2700..=0x27BF      // 装饰符号
        | 0x27C0..=0x27EF      // 杂项数学符号 A
        | 0x27F0..=0x27FF      // 补充箭头 A
        | 0x2800..=0x28FF      // 盲文
        | 0x2900..=0x297F      // 补充箭头 B
        | 0x2980..=0x29FF      // 杂项数学符号 B
        | 0x2A00..=0x2AFF      // 补充数学运算符
        | 0x2B00..=0x2BFF      // 杂项符号与箭头
        | 0xE000..=0xF8FF      // 私有使用区（Wingdings 常映射于此）
        | 0xF0000..=0xFFFFD    // 补充私有使用区
    )
}

/// 把 OOXML 的字体名规范化，便于与系统家族名匹配。
///
/// 现实中课件里常见 `Arial Bold`、`Times New Roman Regular`、`宋体 (中文正文)`
/// 这类带修饰的名字，直接查系统字体表会失败 —— 落空就会退到通用字体，
/// 字宽差一大截，整段文字的折行位置跟着变。
///
/// 后缀匹配**不区分大小写**：调用方往往已经把它转成小写再传进来
/// （见 [`FontContext::resolve`]），按原样比 `" Regular"` 会白比一场。
pub fn normalize_family_name(name: &str) -> String {
    let mut s = name.trim().to_string();
    // 去掉 WPS / Office 附加的中文别名括号
    if let Some(idx) = s.find('(') {
        let tail = &s[idx..];
        if tail.contains(')') && tail.chars().any(is_cjk_char) {
            s = s[..idx].trim().to_string();
        }
    }
    // 去掉常见的字重/字形后缀（可能叠着写，如 `Bold Italic`）
    for suffix in [
        " Bold Italic",
        " Bold",
        " Italic",
        " Regular",
        " Light",
        " Medium",
        " Semibold",
        " SemiBold",
        " Black",
        " Thin",
    ] {
        let Some(cut) = s.len().checked_sub(suffix.len()) else {
            continue;
        };
        // 多字节字符的边界不能切，切了就是非法 UTF-8
        if cut == 0 || !s.is_char_boundary(cut) {
            continue;
        }
        if s[cut..].eq_ignore_ascii_case(suffix) {
            s = s[..cut].trim_end().to_string();
        }
    }
    s
}

/// 该字体名是否代表「跟随主题」的占位（`+mj-lt` / `+mn-ea` 等）。
///
/// 这类名字必须由主题字体方案解析，不能当普通家族名查询。
pub fn is_theme_font_ref(name: &str) -> bool {
    name.starts_with('+')
}

/// 主题字体引用的类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeFontRef {
    /// `+mj-lt` 标题字体（拉丁）
    MajorLatin,
    /// `+mj-ea` 标题字体（东亚）
    MajorEa,
    /// `+mj-cs` 标题字体（复杂文种）
    MajorCs,
    /// `+mn-lt` 正文字体（拉丁）
    MinorLatin,
    /// `+mn-ea` 正文字体（东亚）
    MinorEa,
    /// `+mn-cs` 正文字体（复杂文种）
    MinorCs,
}

impl ThemeFontRef {
    /// 解析 `+mj-lt` 形式的名字。
    pub fn parse(name: &str) -> Option<ThemeFontRef> {
        let n = name.trim().to_ascii_lowercase();
        let body = n.strip_prefix('+')?;
        let (major_minor, script) = body.split_at(body.len().checked_sub(3)?);
        let major = match major_minor {
            "mj" => true,
            "mn" => false,
            _ => return None,
        };
        match (major, script) {
            (true, "-lt") => Some(ThemeFontRef::MajorLatin),
            (true, "-ea") => Some(ThemeFontRef::MajorEa),
            (true, "-cs") => Some(ThemeFontRef::MajorCs),
            (false, "-lt") => Some(ThemeFontRef::MinorLatin),
            (false, "-ea") => Some(ThemeFontRef::MinorEa),
            (false, "-cs") => Some(ThemeFontRef::MinorCs),
            _ => None,
        }
    }
}

/// 校验字体数据是否可用，返回可读的中文错误。
pub fn validate_font_data(data: &[u8], index: u32) -> Result<(), Error> {
    if data.is_empty() {
        return Err(Error::other("字体数据为空"));
    }
    ttf_parser::Face::parse(data, index)
        .map(|_| ())
        .map_err(|e| Error::other(format!("字体数据无法解析：{e}")))
}

/// 按 `fontdb` 的查询接口解析（供需要更复杂匹配策略的调用方使用）。
pub fn query_font(db: &Database, family: &str, bold: bool, italic: bool) -> Option<fontdb::ID> {
    db.query(&Query {
        families: &[Family::Name(family)],
        weight: if bold { Weight::BOLD } else { Weight::NORMAL },
        style: if italic { Style::Italic } else { Style::Normal },
        stretch: fontdb::Stretch::Normal,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_height_ignores_fonts_own_oversized_metrics() {
        // 微软雅黑这类字体的 (asc - desc + lineGap) 是 1.332em。
        //
        // 曾经行高取「字体度量与 1.2 的较大者」，于是雅黑的每一行都比
        // PowerPoint 高出 13% —— 13 行的听力选项排出 415pt 顶出 366pt 的文本框，
        // 后两行直接掉到页面外。PowerPoint 的单倍行距与字体无关，恒为 1.2em。
        let yahei_like = LoadedFont {
            ascender: 1.058,
            descender: -0.274,
            line_gap: 0.0,
            ..LoadedFont::placeholder()
        };
        assert!(
            (yahei_like.line_height_em() - 1.2).abs() < 1e-6,
            "行高不该被字体自带的大度量撑大，实际 {}",
            yahei_like.line_height_em()
        );

        // 反方向同样成立：度量比 1.2 小的字体（Times New Roman 1.150、
        // 宋体 1.141）也不该把行高压到 1.2 以下
        let times_like = LoadedFont {
            ascender: 0.891,
            descender: -0.216,
            line_gap: 0.0,
            ..LoadedFont::placeholder()
        };
        assert!((times_like.line_height_em() - 1.2).abs() < 1e-6);
    }

    #[test]
    fn cjk_detection_covers_common_chars() {
        assert!(is_cjk_char('中'));
        assert!(is_cjk_char('文'));
        assert!(is_cjk_char('，'));
        assert!(is_cjk_char('。'));
        assert!(is_cjk_char('あ'));
        assert!(is_cjk_char('ア'));
        assert!(is_cjk_char('한'));
        assert!(is_cjk_char('（'));
        assert!(!is_cjk_char('A'));
        assert!(!is_cjk_char('1'));
        assert!(!is_cjk_char(','));
    }

    #[test]
    fn complex_script_detection() {
        assert!(is_complex_script('ا'));
        assert!(is_complex_script('א'));
        assert!(is_complex_script('क'));
        assert!(is_complex_script('ก'));
        assert!(!is_complex_script('中'));
        assert!(!is_complex_script('A'));
    }

    #[test]
    fn symbol_detection() {
        assert!(is_symbol_char('→'));
        assert!(is_symbol_char('●'));
        assert!(is_symbol_char('★'));
        assert!(is_symbol_char('✓'));
        assert!(!is_symbol_char('A'));
        assert!(!is_symbol_char('中'));
    }

    #[test]
    fn normalize_strips_office_style_suffixes() {
        assert_eq!(normalize_family_name("Arial Bold"), "Arial");
        assert_eq!(normalize_family_name("Arial Bold Italic"), "Arial");
        assert_eq!(normalize_family_name("Segoe UI Semibold"), "Segoe UI");
        assert_eq!(normalize_family_name(" 宋体  "), "宋体");
        // 中文别名括号应被剥离
        assert_eq!(normalize_family_name("宋体 (中文正文)"), "宋体");
        // 不含中文的括号属于名字本身，保留
        assert_eq!(normalize_family_name("Font (Alt)"), "Font (Alt)");
    }

    #[test]
    fn theme_font_ref_parsing() {
        assert_eq!(ThemeFontRef::parse("+mj-lt"), Some(ThemeFontRef::MajorLatin));
        assert_eq!(ThemeFontRef::parse("+mj-ea"), Some(ThemeFontRef::MajorEa));
        assert_eq!(ThemeFontRef::parse("+mn-lt"), Some(ThemeFontRef::MinorLatin));
        assert_eq!(ThemeFontRef::parse("+MN-EA"), Some(ThemeFontRef::MinorEa));
        assert_eq!(ThemeFontRef::parse("Arial"), None);
        assert_eq!(ThemeFontRef::parse("+xx-lt"), None);
        assert_eq!(ThemeFontRef::parse("+"), None);
    }

    #[test]
    fn theme_font_ref_detection() {
        assert!(is_theme_font_ref("+mj-lt"));
        assert!(is_theme_font_ref("+mn-ea"));
        assert!(!is_theme_font_ref("Arial"));
    }

    #[test]
    fn empty_context_has_no_fonts() {
        let ctx = FontContext::empty();
        assert_eq!(ctx.font_count(), 0);
        assert_eq!(ctx.family_count(), 0);
        assert!(ctx.resolve("Arial", false, false).is_none());
    }

    #[test]
    fn empty_context_never_panics_on_lookup() {
        let ctx = FontContext::empty();
        assert!(ctx.font_for_char(Some("Arial"), false, false, 'A').is_none());
        assert!(ctx.font_for_char(None, false, false, '中').is_none());
        assert!(!ctx.has_family("Arial"));
    }

    #[test]
    fn hash_str_is_deterministic_and_sensitive() {
        assert_eq!(hash_str("Arial", false, false), hash_str("Arial", false, false));
        assert_ne!(hash_str("Arial", false, false), hash_str("Arial", true, false));
        assert_ne!(hash_str("Arial", false, false), hash_str("Arial", false, true));
        assert_ne!(hash_str("Arial", false, false), hash_str("Arial2", false, false));
    }

    #[test]
    fn validate_font_data_rejects_garbage() {
        assert!(validate_font_data(&[], 0).is_err());
        assert!(validate_font_data(&[0u8; 64], 0).is_err());
    }

    #[test]
    fn looks_like_cjk_family_detection() {
        assert!(looks_like_cjk_family("Microsoft YaHei"));
        assert!(looks_like_cjk_family("SimSun"));
        assert!(looks_like_cjk_family("Noto Sans CJK SC"));
        assert!(looks_like_cjk_family("宋体"));
        assert!(!looks_like_cjk_family("Arial"));
        assert!(!looks_like_cjk_family("Segoe UI"));
    }

    #[test]
    fn context_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<FontContext>();
    }
}
