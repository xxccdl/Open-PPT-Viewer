//! 统一错误类型。
//!
//! 面向课堂场景：任何错误都必须能翻译成老师看得懂的中文，
//! 并且尽量指出「哪一页 / 哪个部件」出了问题，而不是笼统的失败。

use std::path::PathBuf;

/// ppt-core 的统一错误类型。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("无法读取文件 {path}：{source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("无法读取文件：{0}")]
    IoPlain(#[from] std::io::Error),

    #[error("文件容器已损坏或不是有效的 Office 文档：{0}")]
    Container(String),

    #[error("文档缺少必需的部件：{0}")]
    MissingPart(String),

    #[error("XML 解析失败（部件 {part}）：{message}")]
    Xml { part: String, message: String },

    #[error("不支持的文件格式：{0}")]
    UnsupportedFormat(String),

    #[error("第 {page} 页解析失败：{message}")]
    PageParse { page: usize, message: String },

    #[error("渲染失败：{0}")]
    Render(String),

    /// 这一页**还没生成好**，但正在生成中 —— 不是错误。
    ///
    /// 「让本机办公软件出图」这条路是逐页产出的：第一页几百毫秒就有了，
    /// 后面的页要排队。老师翻到某一页时它可能刚好还没轮到，
    /// 这时应当告诉他「正在生成第 N 页」，而不是报一个红叉。
    ///
    /// 调用方看到这个错误应当**稍后重试**，而不是降级到别的渲染路径 ——
    /// 降到自研渲染就会把「画得不准」的画面亮出来，那正是我们要避免的。
    #[error("第 {page} 页正在生成中，请稍候")]
    NotReady { page: usize },

    #[error("{0}")]
    Other(String),
}

impl Error {
    /// 这一页是否只是「还没生成好」。
    #[inline]
    pub fn is_not_ready(&self) -> bool {
        matches!(self, Error::NotReady { .. })
    }

    pub fn other(msg: impl Into<String>) -> Self {
        Error::Other(msg.into())
    }

    pub fn container(msg: impl Into<String>) -> Self {
        Error::Container(msg.into())
    }

    pub fn xml(part: impl Into<String>, message: impl Into<String>) -> Self {
        Error::Xml {
            part: part.into(),
            message: message.into(),
        }
    }

    pub fn page_parse(page: usize, message: impl Into<String>) -> Self {
        Error::PageParse {
            page: page + 1,
            message: message.into(),
        }
    }

    pub fn render(msg: impl Into<String>) -> Self {
        Error::Render(msg.into())
    }

    /// 该错误是否属于「可跳过、不影响其它页」的局部错误。
    ///
    /// 用于容错策略：局部错误应当降级到单页错误提示，
    /// 而不是让整本课件打不开。
    pub fn is_recoverable(&self) -> bool {
        matches!(
            self,
            Error::PageParse { .. } | Error::MissingPart(_) | Error::Render(_)
        )
    }
}

pub type Result<T> = std::result::Result<T, Error>;
