<div align="center">

# OpenPPTView

**课件一打开，就能开始讲。**

教室那台电脑上的本地 PPT / PDF 放映器：首屏约 2 秒、翻页不卡、动画照旧、触屏能写。
Windows 10 / 11 · 全程离线 · 一个安装包装完就能用

[![release](https://img.shields.io/github/v/release/xxccdl/Open-PPT-Viewer?label=release&color=2ea043)](https://github.com/xxccdl/Open-PPT-Viewer/releases/latest)
[![downloads](https://img.shields.io/github/downloads/xxccdl/Open-PPT-Viewer/total?label=downloads&color=1f6feb)](https://github.com/xxccdl/Open-PPT-Viewer/releases)
[![license](https://img.shields.io/github/license/xxccdl/Open-PPT-Viewer?color=blue)](LICENSE)
[![platform](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-0078d4)](https://github.com/xxccdl/Open-PPT-Viewer/releases/latest)
[![stack](https://img.shields.io/badge/Tauri%202-Rust-f97316)](https://tauri.app)

[**⬇ 下载安装包**](https://github.com/xxccdl/Open-PPT-Viewer/releases/latest) ·
[功能](#-功能) · [为什么快](#-为什么快) · [机房运维](#-机房运维加分项) · [English](README.en.md)

</div>

---

## 🎯 它解决什么

老师在教室的那台电脑上，通常只有两分钟：插 U 盘、找到课件、打开、放映。
现有的工具在这两分钟里总会掉链子：

| 常见做法 | 教室里实际发生的事 |
|---|---|
| 在线放映器 | 要联网、要登录、要先把课件传上去；机房网一卡就没法上课 |
| 通用阅读器 | 把 `.pptx` 当文档排，动画、触发器、内嵌视频全丢 |
| 自己写渲染内核 | 永远在追排版长尾：字体 cmap、行高模型、组合坐标系、阴影层级……修好一批又冒一批 |

**OpenPPTView 的取舍是：画面让作者工具自己出，语义我们自己解析。**

| 问题 | 谁来回答 |
|---|---|
| 这一页长什么样 | 本机 WPS / PowerPoint 导出的逐页位图 |
| 哪里能点（超链接、动作按钮） | 原始 OOXML |
| 哪里有视频、字节在哪 | 原始 OOXML |
| 这一页怎么一步步弹出来 | 原始 OOXML 的 `p:timing`，让办公软件**按步出图** |
| 备注写了什么 | 原始 OOXML |

自研渲染内核退到**兜底**：本机没装办公软件、或某几页导不出来时顶上，
并如实告诉老师「当前用的是内置渲染」，绝不假装保真。

---

## ⚡ 为什么快

实测一份 **174 MB / 39 页** 的真实课件（release 构建）：

| 动作 | 耗时 |
|---|---|
| `Presentations.Open`（打开文档） | **1.8 s** |
| 导出第一页 | **0.4 s** |
| 导出整本矢量 PDF（后台） | 7.2 s |
| 打开课件到看见第一页 | **≈ 2 s** |

两个关键设计：

**① 逐页出图，不是一次导出整本。** 老师打开课件时只看得到第一页，
所以首屏只做 `Open + 一页`，剩下的页在后台排队（每页约 0.4 秒，比翻页还快）。

**② 按动画步出图。** 一张拍平的整页图上什么都有，包括这一步还不该露面的答案。
出图时先把「这一步还不该出现」的东西藏掉 —— 形状用 `Shape.Visible`，
段落（「项目符号逐条弹出」）用 `TextFrame2` 把文字填充设成全透明。
每一步一张图，每一张都是办公软件画的。

---

## ✨ 功能

**放映**

- `.pptx` / `.pptm` / `.ppsx` / `.ppsm` / `.potx` / `.potm` / `.pdf`，拖进去或双击都能开
- 旧版 `.ppt` 自动转成 `.pptx`（约 0.9 s，转换结果有缓存，第二次直接命中）
- 单击翻页、右键回翻、Esc 退出；放映计时、页码提示
- 逐元素弹出、转场、触发器、自动前进

**讲课**

- 画笔 / 荧光笔 / 橡皮 / 激光笔，压感与掌托抑制（触控一体机上跟手）
- 黑屏、聚光灯、放大镜、计时器
- 超链接与动作按钮可用；内嵌视频 / 音频尊重作者设的裁剪区间与循环
- 缩略图侧栏、演讲者备注、缩放平移、按页跳转

**课后**

- 标注旁挂存成 `<课件>.oppv-annot.json`，**不改老师的原文件**，课件拷走标注跟着走
- 自绘界面的原生安装包：可选安装位置、关联文件类型、桌面快捷方式

---

## 🧑‍🏫 机房运维（加分项）

学校机房的麻烦不在讲台上，而在「那台机器又蓝屏了，老师说不清」。

- **蓝屏诊断**：下次开机自动捡起 Windows 留下的现场（错误码、内核转储、当时的驱动与系统版本），
  跑一遍只读修复（系统文件 / 组件存储 / 磁盘扫描 / 驱动体检，每一项都能单独关掉省时间），
  再把报告打包送到管理后台。分析窗口停留多久由老师定（默认 5 秒后缩到右下角）。
- **远程协助**：管理员在后台能看到在线机器、打开它的屏幕、在它上面跑 `cmd`、点鼠标。
  方向是**机器主动连出来** —— 学校没有公网 IP，也不该为此开端口。
- **学校 / 班级隔离**：平台 → 学校 → 班级三层。每台电脑在「设置 → 班级归属」点登录，
  浏览器选班授权后，它的报告与远程只对**本班管理员**可见；没登录的机器只会进「未分类」。

> 全部可按需关闭：关掉远程协助即刻断开心跳；关掉蓝屏诊断就只当一个放映器。

---

## 🧱 架构

10 个 crate，依赖方向自上而下：

| crate | 职责 |
|---|---|
| `ppt-core` | OPC/ZIP 按需读取、`DocumentSource` 抽象、SceneGraph 中间表示 |
| `ppt-text` | 字体解析、文本整形、CJK 断行与文本布局 |
| `ppt-format-pptx` | PPTX 解析：OOXML → SceneGraph |
| `ppt-format-pdf` | PDF 支持：页面栅格化与 PageSource 适配 |
| `ppt-render` | SceneGraph 光栅化渲染器（自研兜底内核） |
| `ppt-pipeline` | 两级缓存与优先级预渲染调度 |
| `ppt-convert` | 借用本机 WPS / Office 内核出画面（逐页位图 / 按步出图） |
| `ppt-app` | Tauri 应用：窗口、命令、自定义协议、文件关联、蓝屏诊断、远程协助 |
| `ppt-installer-ui` | 安装程序界面：自绘控件与排版（不依赖 WebView2） |
| `ppt-installer` | 安装程序本体（不依赖 NSIS） |

「画面来源 + 语义来源」是两条独立的路，由 `Pipeline::with_interaction` 组装：

```text
                        ┌──────────────────┐
   本机办公软件 ──逐页出图──▶ │  画面来源（位图） │──┐
                        └──────────────────┘  │
                                              ├──▶  Pipeline  ──▶  前端 <canvas>
                        ┌──────────────────┐  │
   课件原文件  ──解析────▶ │ 语义来源（OOXML） │──┘
                        └──────────────────┘
```

前端是原生 HTML / CSS / JS，**没有构建步骤**（`crates/ppt-app/web`）。
后台是零依赖的 Node 服务（`server/crash`），只用自带模块。

---

## 🚀 30 秒上手

1. 从 [Releases](https://github.com/xxccdl/Open-PPT-Viewer/releases/latest) 下载 `OpenPPTView_x.y.z_x64-setup.exe`，双击安装。
2. 双击课件（或把文件拖进窗口）就能讲；`F5` 进入放映，`Esc` 退出。
3. 触屏机器上直接用手指写；激光笔、黑屏、计时器都在工具条上。
4. 想让这台电脑归到某个班：设置 → 蓝屏诊断 → **班级归属** → 「登录」→ 浏览器选班 → 授权，浏览器会自动跳回来。

> 想拿到最佳保真度，机器上装 WPS 或 PowerPoint（要的是 `KWPP.Application` / `PowerPoint.Application`，Word 不行）。
> 没装也能用，走内置渲染，并且界面会说明。

---

## 🛠 从源码构建

需要 Windows 与 Rust stable 工具链。

```powershell
# 跑起来
cargo run -p ppt-app --release

# 打安装包（产物在 target\...\bundle\nsis\ 下）
cargo tauri build --bundles nsis

# 跑测试（ppt-installer 的测试要管理员权限，会真的写安装目录，所以排除它）
cargo test --workspace --exclude ppt-installer
```

### 找得着的地方

- **日志**：`%LOCALAPPDATA%\OpenPPTView\logs\openpptview.log`
  （记录画面来源是办公软件还是自研渲染、导出失败原因、超过 60 ms 的慢渲染）
- **缓存**：`%LOCALAPPDATA%\OpenPPTView\cache\`
  - `wps\v<N>\<课件指纹>\<档位>\`：办公软件出的逐页 PNG 与动画分帧
  - `convert\v<N>\`：整本矢量 PDF
  - 换出图方式时 `<N>` 会加一，旧缓存自然作废

### 排查渲染问题

```powershell
# 把某一页画出来看，并打印节点清单、动画步数、链接与媒体热区
cargo run -p ppt-render --example dump_page -- "课件.pptx" "out.png" 0 2.0

# 逐缩放档量每一页的渲染延迟（看最慢的那几页，别只看平均）
cargo run -p ppt-pipeline --example page_latency -- "课件.pptx"
```

### 目录

```text
crates/            10 个 crate，见上表
  ppt-app/web/     前端（原生 HTML/CSS/JS，无构建步骤）
server/crash/      蓝屏报告接收与后台（零依赖 Node 服务）
packaging/windows/ 安装包脚本与资源
.trae/specs/       设计文档：需求、任务清单、验收清单
```

---

## ⚠️ 已知限制

- **只支持 Windows**：按步出图、格式转换、安装器、文件关联都依赖 Windows。
- **强调动画是近似**：变色、放大这类动画不改变可见性，静态帧表达不了，这一步由自研内核近似合成。
- **旧版 `.ppt` 需要本机装有 WPS 或 PowerPoint**；没装会退回「交给系统默认程序打开」并说明原因。
- **磁盘缓存会占空间**：某份 39 页含动画的课件约 180 MB。分帧只出一次，
  但「办公软件出的图」目前没有自动淘汰。
- 复杂课件上，自研兜底内核的个别排版可能与原稿有出入 —— 所以它只当兜底，并且会明确告诉老师。

---

## 📄 许可

MIT，见 [LICENSE](LICENSE)。

第三方组件（Tauri、Rust、hayro、`image`、`windows` 等）各自遵循其原许可。

<div align="center">

**如果这个项目让你的课堂省了两分钟，欢迎点一个 ⭐**

</div>
