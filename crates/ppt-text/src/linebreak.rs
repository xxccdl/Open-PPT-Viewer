//! 断行：UAX#14 + 中文排版禁则。
//!
//! # 为什么不能只用 UAX#14
//!
//! `unicode-linebreak` 已实现 UAX#14，覆盖了大部分场景（不在闭标点前断、
//! 不在开标点后断、不拆断英文单词与数字）。但中文排版还有几类它不管的禁则：
//!
//! - `%`、`℃`、`°`、`′` 等「后置符号」不能出现在行首（UAX#14 归为 PO，允许断）；
//! - 长音符 `ー`、小假名 `ゃゅょ` 等日文规则同样要求不在行首；
//! - `$`、`￥` 等「前置符号」不能出现在行尾。
//!
//! 这些在本模块里以**附加禁则表**的形式补齐，避免改动 UAX#14 的通用逻辑。

/// 一个断行机会。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BreakOpportunity {
    /// 字节偏移（断行后新行的起点）。
    pub offset: usize,
    /// 是否为强制断行（`\n`、段落结束）。
    pub mandatory: bool,
}

/// 中文排版中「不能出现在行首」的字符（附加于 UAX#14 之上）。
///
/// UAX#14 已覆盖绝大多数闭标点，这里只补它漏掉的。
const NO_LINE_START: &[char] = &[
    // 后置符号
    '%', '‰', '‱', '°', '℃', '℉', '′', '″', '‴', '¢', '〞', '〟',
    // 日文长音符与小假名
    'ー', '々', '〻', 'ぁ', 'ぃ', 'ぅ', 'ぇ', 'ぉ', 'っ', 'ゃ', 'ゅ', 'ょ', 'ゎ', 'ァ', 'ィ', 'ゥ',
    'ェ', 'ォ', 'ッ', 'ャ', 'ュ', 'ョ', 'ヮ', 'ヵ', 'ヶ', 'ヽ', 'ヾ', 'ゝ', 'ゞ',
    // 中文里常被误断的收尾符号
    '～', '·', '、', '。', '，', '；', '：', '！', '？', '）', '】', '》', '」', '』', '〕', '｝',
    '〉', '］', '｀',
];

/// 中文排版中「不能出现在行尾」的字符。
const NO_LINE_END: &[char] = &[
    '$', '￥', '€', '£', '₩', '＄', '〝', '（', '【', '《', '「', '『', '〔', '｛', '〈', '［', '｟',
];

/// 不可拆分的组合字符（附加于 UAX#14 之上）。
///
/// 这些字符必须与其前一个字符保持在同一行内。
const NON_BREAKING_AFTER: &[char] = &['\u{3000}', '\u{00A0}', '\u{202F}'];

/// 判断某字符是否禁止出现在行首。
pub fn cannot_start_line(ch: char) -> bool {
    NO_LINE_START.contains(&ch)
}

/// 判断某字符是否禁止出现在行尾。
pub fn cannot_end_line(ch: char) -> bool {
    NO_LINE_END.contains(&ch)
}

/// 计算文本中的断行机会（字节偏移，已应用中文禁则）。
///
/// 返回的偏移量均为「新行的起点」，不含 0 与 `text.len()`。
/// 强制断行（`\n`）的位置也会被包含，并由 `mandatory` 标记。
pub fn break_opportunities(text: &str) -> Vec<BreakOpportunity> {
    use unicode_linebreak::BreakOpportunity as U;

    let mut raw: Vec<BreakOpportunity> = Vec::new();

    for (offset, kind) in unicode_linebreak::linebreaks(text) {
        if offset == 0 || offset >= text.len() {
            continue;
        }
        raw.push(BreakOpportunity {
            offset,
            mandatory: kind == U::Mandatory,
        });
    }

    apply_strict_rules(text, raw)
}

/// 应用中文禁则，修正断行位置。
fn apply_strict_rules(text: &str, raw: Vec<BreakOpportunity>) -> Vec<BreakOpportunity> {
    let mut out: Vec<BreakOpportunity> = Vec::with_capacity(raw.len());

    for mut opp in raw {
        if opp.mandatory {
            out.push(opp);
            continue;
        }

        // 规则 1：断点后第一个字符若禁止出现在行首，则把断点前移，
        // 让该字符留在上一行（这就是中文排版的「避头点」）。
        if let Some(ch) = char_at(text, opp.offset) {
            if cannot_start_line(ch) {
                match previous_opportunity(&out, text) {
                    Some(prev) => {
                        opp.offset = prev;
                    }
                    None => {
                        // 没有更早的断点可用：放弃在此处断行，继续向后寻找。
                        // 若整段都找不到，调用方会退化为「硬切」并给出告警。
                        continue;
                    }
                }
            }
        }

        // 规则 2：断点前一个字符若禁止出现在行尾，则把断点后移，
        // 让该字符落到下一行（「避尾点」）。
        if let Some(ch) = char_before(text, opp.offset) {
            if cannot_end_line(ch) {
                continue;
            }
        }

        // 规则 3：不破坏不可拆分组合（如不换行空格）。
        if let Some(ch) = char_before(text, opp.offset) {
            if NON_BREAKING_AFTER.contains(&ch) {
                continue;
            }
        }

        // 规则 4：断点前后若构成「数字 + 后置符号」整体，不应拆开。
        if let (Some(prev), Some(next)) = (char_before(text, opp.offset), char_at(text, opp.offset))
        {
            if prev.is_ascii_digit() && cannot_start_line(next) {
                continue;
            }
        }

        // 去重并保持单调递增
        if out.last().map(|p| p.offset) != Some(opp.offset) {
            out.push(opp);
        }
    }

    out
}

/// 取指定字节偏移处的字符。
fn char_at(text: &str, offset: usize) -> Option<char> {
    text.get(offset..)?.chars().next()
}

/// 取指定字节偏移前一个字符。
fn char_before(text: &str, offset: usize) -> Option<char> {
    text.get(..offset)?.chars().next_back()
}

/// 在已确定的断点里，找出 `offset` 之前最近的一个。
fn previous_opportunity(existing: &[BreakOpportunity], text: &str) -> Option<usize> {
    // 这里按字符向前退一步：既保证「把禁则字符留在上一行」，
    // 又不至于一次退太多导致行太短。
    let last = existing.last().map(|o| o.offset)?;
    // 至少要让上一行多容纳一个字符
    let mut probe = last;
    if probe >= text.len() {
        return None;
    }
    // 从 last 起向前找一个「合法断点」，即其后的字符可以出现在行首
    loop {
        let next = char_at(text, probe)?;
        if !cannot_start_line(next) && !NON_BREAKING_AFTER.contains(&char_before(text, probe).unwrap_or(' ')) {
            return Some(probe);
        }
        // 继续向前推进一个字符
        let mut it = text.get(..probe)?.char_indices();
        probe = it.next_back().map(|(i, _)| i)?;
    }
}

/// 在 `limit` 字节内寻找最后一个可断行位置。
///
/// 返回 `None` 表示在限制内没有任何断行机会（例如一个超长的英文单词），
/// 调用方需要决定是「硬切」还是「允许溢出」。
pub fn last_break_within(opportunities: &[BreakOpportunity], limit: usize) -> Option<usize> {
    // 断点表按偏移递增，直接二分
    let idx = opportunities.partition_point(|o| o.offset <= limit);
    if idx == 0 {
        None
    } else {
        Some(opportunities[idx - 1].offset)
    }
}

/// 把文本按强制断行（`\n`）切分为若干硬行。
pub fn split_mandatory_breaks(text: &str) -> Vec<&str> {
    if text.contains('\n') {
        text.split('\n').collect()
    } else {
        vec![text]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offsets(text: &str) -> Vec<usize> {
        break_opportunities(text).into_iter().map(|o| o.offset).collect()
    }

    #[test]
    fn no_break_inside_english_word() {
        let ops = offsets("hello world");
        // 唯一合法断点在空格之后
        assert_eq!(ops, vec![6]);
    }

    #[test]
    fn no_break_inside_number() {
        let ops = offsets("1234567");
        assert!(ops.is_empty(), "数字内部不应断行：{ops:?}");
    }

    #[test]
    fn breaks_between_cjk_ideographs() {
        let ops = offsets("春眠不觉晓");
        // 5 个汉字之间应有 4 个断点
        assert_eq!(ops.len(), 4);
    }

    #[test]
    fn never_break_before_closing_punctuation() {
        // 「。」前后都不应出现「断在句号之前」的机会
        let text = "今天下雨。";
        let ops = offsets(text);
        let closing_offset = text.find('。').unwrap();
        assert!(
            !ops.contains(&closing_offset),
            "不应在句号前断行：{ops:?}"
        );
    }

    #[test]
    fn never_break_before_comma() {
        let text = "苹果，香蕉";
        let ops = offsets(text);
        let comma = text.find('，').unwrap();
        assert!(!ops.contains(&comma), "不应在逗号前断行：{ops:?}");
    }

    #[test]
    fn never_break_after_opening_bracket() {
        let text = "（注释）";
        let ops = offsets(text);
        let after_open = "（".len();
        assert!(!ops.contains(&after_open), "不应在左括号后断行：{ops:?}");
    }

    #[test]
    fn percent_sign_not_at_line_start() {
        // `%` 属于 UAX#14 的 PO，默认允许在其前断行，需被附加禁则拦下
        let text = "增长50%";
        let ops = offsets(text);
        let pct = text.find('%').unwrap();
        assert!(!ops.contains(&pct), "% 不应出现在行首：{ops:?}");
    }

    #[test]
    fn degree_sign_not_at_line_start() {
        let text = "气温30℃";
        let ops = offsets(text);
        let pos = text.find('℃').unwrap();
        assert!(!ops.contains(&pos), "℃ 不应出现在行首：{ops:?}");
    }

    #[test]
    fn dollar_sign_not_at_line_end() {
        let text = "价格$100";
        let ops = offsets(text);
        let after_dollar = text.find('$').unwrap() + 1;
        assert!(!ops.contains(&after_dollar), "$ 不应出现在行尾：{ops:?}");
    }

    #[test]
    fn mandatory_break_at_newline() {
        let ops = break_opportunities("第一行\n第二行");
        assert!(ops.iter().any(|o| o.mandatory), "换行处应为强制断行");
    }

    #[test]
    fn mixed_cjk_latin_breaks_correctly() {
        // 字节布局：使用(0..6) 空格(6..7) Rust(7..11) 空格(11..12) 编写(12..18)
        let text = "使用 Rust 编写";
        let ops = offsets(text);
        // 断点出现在空格之后（UAX#14 禁止在空格之前断行）
        assert!(ops.contains(&7), "「使用 」之后应可断：{ops:?}");
        assert!(ops.contains(&12), "「Rust 」之后应可断：{ops:?}");
        // 英文单词内部不得断开
        for inside in 8..=11 {
            assert!(!ops.contains(&inside), "英文单词内部不应断行（偏移 {inside}）：{ops:?}");
        }
        // 汉字之间应可断
        assert!(ops.contains(&15), "汉字之间应可断：{ops:?}");
    }

    #[test]
    fn last_break_within_respects_limit() {
        let text = "春眠不觉晓处处闻啼鸟";
        let ops = break_opportunities(text);
        // 每个汉字 3 字节；限制 12 字节恰好容纳前 4 个字，断点应在 12
        assert_eq!(last_break_within(&ops, 12), Some(12));
        // 限制到 11 字节时只能退到第 3 个字之后
        assert_eq!(last_break_within(&ops, 11), Some(9));
        // 限制极小：没有可用断点
        assert_eq!(last_break_within(&ops, 0), None);
    }

    #[test]
    fn last_break_within_handles_empty_opportunities() {
        assert_eq!(last_break_within(&[], 100), None);
    }

    #[test]
    fn split_mandatory_breaks_splits_on_newline() {
        assert_eq!(split_mandatory_breaks("a\nb\nc"), vec!["a", "b", "c"]);
        assert_eq!(split_mandatory_breaks("abc"), vec!["abc"]);
        // 尾部换行会产生一个空行，符合 OOXML 的 a:br 语义
        assert_eq!(split_mandatory_breaks("a\n"), vec!["a", ""]);
    }

    #[test]
    fn opportunities_are_strictly_increasing() {
        let text = "《论语》曰：「学而时习之，不亦说乎？」";
        let ops = offsets(text);
        assert!(ops.windows(2).all(|w| w[0] < w[1]), "断点必须严格递增：{ops:?}");
        assert!(ops.iter().all(|&o| o > 0 && o < text.len()), "断点应在文本内部");
    }

    #[test]
    fn non_breaking_space_prevents_break() {
        let text = "abc\u{00A0}def";
        let ops = offsets(text);
        let after_nbsp = text.find('\u{00A0}').unwrap() + '\u{00A0}'.len_utf8();
        assert!(!ops.contains(&after_nbsp), "不换行空格后不应断行：{ops:?}");
    }

    #[test]
    fn long_unbreakable_token_has_no_opportunity() {
        let text = "supercalifragilisticexpialidocious";
        assert!(offsets(text).is_empty());
    }

    #[test]
    fn char_helpers_are_utf8_safe() {
        let text = "中文abc";
        assert_eq!(char_at(text, 0), Some('中'));
        assert_eq!(char_at(text, 3), Some('文'));
        assert_eq!(char_at(text, 6), Some('a'));
        assert_eq!(char_before(text, 3), Some('中'));
        assert_eq!(char_before(text, 0), None);
        assert_eq!(char_at(text, 999), None);
        // 落在多字节字符中间时应返回 None 而不是 panic
        assert_eq!(char_at(text, 1), None);
    }
}
