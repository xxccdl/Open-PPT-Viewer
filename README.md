# OpenPPTView

**极速课件讲演器** —— 为学校老师讲课场景打造的本地 PPT / PDF 放映器。

打开就能讲，翻页不卡，动画照旧，超链接和视频都能用。为 Windows 桌面打造，全程离线。

---

## 为什么做这个

用现有工具上课，总会撞上这些事：

- 在线放映器要联网、要登录、把课件传上去；
- 通用阅读器把 `.pptx` 当文档排版，动画、触发器、媒体全丢；
- 自己写渲染内核，永远在追「无穷尽的排版长尾」：符号字体 cmap、行高模型、
  组合坐标系的局部单位、预设几何的调整值单位、阴影层级……修好一批，下一份
  课件又冒出一批。

所以这个项目的核心取舍是：**画面让作者工具自己出，语义我们自己解析。**

| 问题 | 谁来回答 |
|---|---|
| 这一页长什么样 | 本机 WPS / PowerPoint 导出的逐页位图 |
| 哪里能点（超链接、动作按钮） | 原始 OOXML |
| 哪里有视频、字节在哪 | 原始 OOXML |
| 这一页怎么一步步弹出来 | 原始 OOXML 的 `p:timing`，交给办公软件**按步出图** |
| 备注写了什么 | 原始 OOXML |

自研渲染内核退居**兜底**：本机没装办公软件、或某几页办公软件导不出来时顶上，
并且如实告诉老师「当前用的是内置渲染」。

### 两个关键设计

**逐页出图，而不是一次导出整本。** 实测一份 174MB / 39 页的真实课件
（release）：`Presentations.Open` 1.8s、导出首页 0.4s、导出整本矢量 PDF 7.2s。
老师打开课件时只看得到第一页，所以首屏压到 `Open + 一页` ≈ 2 秒，
剩下的页在后台排队（每页约 0.4 秒，比翻页快）。

**按动画步出图。** 一张拍平的整页图上什么都有，包括这一步还不该露面的答案。
所以出图时会把「这一步还不该出现」的东西藏掉再导出 —— 形状用
`Shape.Visible`，段落（「项目符号逐条弹出」）用 `TextFrame2` 把文字填充设成全透明。
每一步一张图，每一张都是办公软件画的。

---

## 功能

- **打开**：`.pptx` / `.pptm` / `.ppsx` / `.ppsm` / `.potx` / `.potm` 与 `.pdf`，
  拖进去或双击都能开；旧版 `.ppt` 会交给本机默认程序并说明原因
- **放映**：单击翻页、右键回翻、Esc 退出；放映计时、页码提示
- **动画**：逐元素弹出、转场效果、触发器（点某个按钮才播某一步）、自动前进
- **交互**：超链接与动作按钮、内嵌视频/音频（尊重作者设的裁剪区间与循环）
- **讲课工具**：画笔/荧光笔/橡皮/激光笔标注、黑屏、聚光灯、放大镜、计时器
- **窗口模式**：缩略图侧栏、演讲者备注、缩放与平移、页内搜索式跳页
- **标注存档**：旁挂一个 `<课件>.oppv-annot.json`，**不修改老师的原文件**，
  课件拷贝到别的机器标注跟着走
- **安装**：自绘界面的原生安装包，可选择安装位置、关联文件类型、创建桌面快捷方式

---

## 架构

10 个 crate，依赖方向自上而下：

| crate | 职责 |
|---|---|
| `ppt-core` | OPC/ZIP 按需读取、`DocumentSource` 抽象、SceneGraph 中间表示 |
| `ppt-text` | 字体解析、文本整形、CJK 断行与文本布局 |
| `ppt-format-pptx` | PPTX 解析：OOXML → SceneGraph |
| `ppt-format-pdf` | PDF 支持：页面栅格化与 PageSource 适配 |
| `ppt-render` | SceneGraph 光栅化渲染器（自研内核） |
| `ppt-pipeline` | 两级缓存与优先级预渲染调度 |
| `ppt-convert` | 借用本机 WPS / Office 内核出画面（逐页位图 / 按步出图） |
| `ppt-app` | Tauri 应用：窗口、命令、自定义协议与文件关联 |
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

前端是原生 HTML/CSS/JS，**没有构建步骤**（`crates/ppt-app/web`）。

---

## 快速开始

需要 Windows 与 Rust 工具链（stable）。想拿到最佳保真度，本机装 WPS 或
PowerPoint（Word 不算，要的是 `KWPP.Application` / `PowerPoint.Application`）。

```powershell
# 跑起来
cargo run -p ppt-app --release

# 跑测试
cargo test --workspace --exclude ppt-installer

# 打安装包（产物在 packaging\out\OpenPPTView-Setup.exe）
powershell -NoProfile -ExecutionPolicy Bypass -File packaging\windows\make-setup.ps1
```

> `ppt-installer` 的测试需要管理员权限（它会真的去写安装目录），
> 所以上面排除了它；手动跑要开一个提权的终端。

### 目录

```text
crates/            10 个 crate，见上表
  ppt-app/web/     前端（原生 HTML/CSS/JS，无构建步骤）
packaging/windows/ 安装包脚本与资源
.trae/specs/       设计文档：需求、任务清单、验收清单
光合作用-测试课件.pptx  合成出来的测试课件
```

### 两个地方找得着

- 日志：`%LOCALAPPDATA%\OpenPPTView\logs\openpptview.log`（记录画面来源是
  办公软件出的还是自研渲染的、导出失败原因、超过 60ms 的慢渲染）
- 缓存：`%LOCALAPPDATA%\OpenPPTView\cache\`
  - `wps\v<N>\<课件指纹>\<档位>\` 办公软件出的逐页 PNG 与动画分帧
  - `convert\v<N>\` 整本矢量 PDF
  - 换出图方式时那个 `<N>` 会加一，旧缓存自然作废

### 排查渲染问题

```powershell
# 把某一页画出来看，并打印节点清单、动画步数、链接与媒体热区
cargo run -p ppt-render --example dump_page -- "课件.pptx" "out.png" 0 2.0

# 逐缩放档量每一页的渲染延迟（看最慢的那几页，别只看平均）
cargo run -p ppt-pipeline --example page_latency -- "课件.pptx"
```

---

## 已知限制

- **只支持 Windows**：按步出图、安装器、文件关联都依赖 Windows 的东西
- **强调动画是近似**：变色、放大这类动画不改变可见性，而静态帧表达不了它，
  所以这一步仍由自研内核合成
- **旧版 `.ppt`** 不能直接放映，会转交给本机默认程序打开
- **动画分帧占磁盘**：一页 N 步会多出 N 张整页 PNG（实测某份 39 页课件约 180MB）。
  分帧只出一次，但**缓存淘汰还没做**
- 自研内核仍在，复杂课件上个别排版可能与原稿有出入 —— 所以它只当兜底，
  并且会明确告诉老师

---

## 许可

MIT，见 [LICENSE](LICENSE)。

第三方组件（Tauri、hayro、`image`、`windows` 等）各自遵循其原许可。
