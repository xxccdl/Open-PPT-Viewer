# OpenPPTView 安装程序美术资源生成
#
# # 为什么要用脚本生成，而不是塞两个 .bmp 进仓库
#
# NSIS 的 MUI 只认 BMP（24 位），而 BMP 是不可读的二进制、改一版就得重画。
# 用脚本从配色和文字生成，改品牌色只改这里一处，而且资源可复现。
#
# # 画的是什么
#
#   sidebar.bmp  164×314  欢迎页与完成页左侧的竖幅（深色底）
#   header.bmp   150×57   内页顶部的小横幅（浅色底，要和页面同色才没有接缝）
#
# 标识（那枚「蓝底 + 白卡片 + 珊瑚点」的方块）是照着 `crates/ppt-app/icons/icon.png`
# 重画的：安装包、任务栏、开始菜单里必须是同一个符号，老师才认得出来。
# 配色就是从 icon.png 上取的实测值。
#
# 用法：powershell -ExecutionPolicy Bypass -File make-art.ps1
#
# 注意：本文件必须存成 UTF-8 **带 BOM**。Windows PowerShell 5.1 会把不带 BOM
# 的 .ps1 按 ANSI 解码，中文注释会被拆成乱码并导致语法错误。

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$out = Join-Path $here 'art'
New-Item -ItemType Directory -Force -Path $out | Out-Null

# ---------------------------------------------------------------------------
# 品牌色（取自 icons/icon.png）
# ---------------------------------------------------------------------------

$brandTop = [System.Drawing.Color]::FromArgb(255, 58, 130, 221)   # 底板渐变：上
$brandBot = [System.Drawing.Color]::FromArgb(255, 33, 89, 174)    # 底板渐变：下
$cardLine = [System.Drawing.Color]::FromArgb(255, 111, 160, 230)  # 卡片上的文字条
$cardBar = [System.Drawing.Color]::FromArgb(255, 174, 194, 226)   # 卡片下方的托条
$coral = [System.Drawing.Color]::FromArgb(255, 255, 90, 80)       # 那颗圆点：录制 / 待讲

# 竖幅底色：比页面深得多的一档蓝黑，让浅色页面上的标识跳出来
$deepTop = [System.Drawing.Color]::FromArgb(255, 15, 17, 22)
$deepBot = [System.Drawing.Color]::FromArgb(255, 28, 34, 48)

$ink = [System.Drawing.Color]::FromArgb(255, 29, 29, 31)          # 正文墨色
$paper = [System.Drawing.Color]::FromArgb(255, 255, 255)          # 页面底色
$sideTitle = [System.Drawing.Color]::FromArgb(255, 246, 247, 251)
$sideMuted = [System.Drawing.Color]::FromArgb(255, 138, 147, 168)
$sideFaint = [System.Drawing.Color]::FromArgb(255, 92, 101, 122)

# ---------------------------------------------------------------------------
# 小工具
# ---------------------------------------------------------------------------

function New-RoundedPath([single]$x, [single]$y, [single]$w, [single]$h, [single]$r) {
    $p = [System.Drawing.Drawing2D.GraphicsPath]::new()
    $d = $r * 2
    $p.AddArc($x, $y, $d, $d, 180, 90)
    $p.AddArc(($x + $w - $d), $y, $d, $d, 270, 90)
    $p.AddArc(($x + $w - $d), ($y + $h - $d), $d, $d, 0, 90)
    $p.AddArc($x, ($y + $h - $d), $d, $d, 90, 90)
    $p.CloseFigure()
    return $p
}

# 优先用系统里真的装了的字体，缺了就退回一个接近的，别让脚本直接崩
function Get-FirstFont([string[]]$names) {
    $have = @()
    try { $have = [System.Drawing.FontFamily]::Families | ForEach-Object { $_.Name } } catch { }
    foreach ($n in $names) {
        if ($have -contains $n) { return $n }
    }
    return $names[-1]
}

$uiFont = Get-FirstFont @('Microsoft YaHei UI', 'Microsoft YaHei', 'SimSun')
$latinFont = Get-FirstFont @('Segoe UI Semibold', 'Segoe UI', 'Microsoft YaHei UI')

# ---------------------------------------------------------------------------
# 标识：照着应用图标重画
# ---------------------------------------------------------------------------

<#
.SYNOPSIS
    画一枚 OpenPPTView 标识，返回指定边长的 32bpp 位图。

.DESCRIPTION
    曲线和圆角在几十像素的尺寸下直接画会有锯齿，所以一律放大 6 倍画完再降采样 ——
    这是让「小尺寸矢量图形」干净的唯一办法。

    画的内容与应用图标逐项对应：
      · 圆角方块，对角渐变
      · 居中的白色卡片（带一点投影，卡片才浮得起来）
      · 卡片里三条圆头文字条
      · 卡片右下角一颗珊瑚色圆点
      · 卡片下方一条浅蓝托条
#>
function New-MarkBitmap([int]$size) {
    $ss = 6
    $u = [single]($size * $ss)

    $big = [System.Drawing.Bitmap]::new([int]$u, [int]$u, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($big)
    $g.SmoothingMode = 'AntiAlias'
    $g.PixelOffsetMode = 'HighQuality'
    $g.Clear([System.Drawing.Color]::Transparent)

    # 底板
    $plate = New-RoundedPath 0 0 $u $u ($u * 0.235)
    $rect = [System.Drawing.RectangleF]::new(0, 0, $u, $u)
    $grad = [System.Drawing.Drawing2D.LinearGradientBrush]::new($rect, $brandTop, $brandBot, [single]55)
    $g.FillPath($grad, $plate)
    $grad.Dispose()
    $plate.Dispose()

    # 卡片：先铺两层偏移的黑，凑出柔和的投影
    $cx = $u * 0.235; $cy = $u * 0.255; $cw = $u * 0.53; $ch = $u * 0.365
    foreach ($s in @(@(0.030, 26), @(0.014, 34))) {
        $sh = New-RoundedPath $cx ($cy + $u * $s[0]) $cw $ch ($u * 0.055)
        $b = [System.Drawing.SolidBrush]::new([System.Drawing.Color]::FromArgb([int]$s[1], 0, 0, 0))
        $g.FillPath($b, $sh)
        $b.Dispose(); $sh.Dispose()
    }

    $card = New-RoundedPath $cx $cy $cw $ch ($u * 0.055)
    $wb = [System.Drawing.SolidBrush]::new($paper)
    $g.FillPath($wb, $card)
    $card.Dispose()

    # 卡片里的文字条
    $lb = [System.Drawing.SolidBrush]::new($cardLine)
    $lh = $u * 0.042
    $bars = @(@(0.300, 0.585, 0.325), @(0.300, 0.645, 0.425), @(0.300, 0.500, 0.525))
    foreach ($bar in $bars) {
        $p = New-RoundedPath ($u * $bar[0]) ($u * $bar[2]) ($u * ($bar[1] - $bar[0])) $lh ($lh / 2)
        $g.FillPath($lb, $p)
        $p.Dispose()
    }
    $lb.Dispose()
    $wb.Dispose()

    # 珊瑚圆点
    $dr = $u * 0.085
    $cb = [System.Drawing.SolidBrush]::new($coral)
    $g.FillEllipse($cb, [single]($u * 0.675 - $dr), [single]($u * 0.545 - $dr), [single]($dr * 2), [single]($dr * 2))
    $cb.Dispose()

    # 卡片下方托条
    $bh = $u * 0.045
    $bz = New-RoundedPath ($u * 0.300) ($u * 0.715) ($u * 0.360) $bh ($bh / 2)
    $bb = [System.Drawing.SolidBrush]::new($cardBar)
    $g.FillPath($bb, $bz)
    $bb.Dispose(); $bz.Dispose()
    $g.Dispose()

    # 降采样
    $small = [System.Drawing.Bitmap]::new($size, $size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $sg = [System.Drawing.Graphics]::FromImage($small)
    $sg.InterpolationMode = 'HighQualityBicubic'
    $sg.PixelOffsetMode = 'HighQuality'
    $sg.CompositingQuality = 'HighQuality'
    $sg.SmoothingMode = 'HighQuality'
    $sg.DrawImage($big, [System.Drawing.Rectangle]::new(0, 0, $size, $size))
    $sg.Dispose()
    $big.Dispose()
    return $small
}

# 24 位无压缩 BMP：NSIS 的 BgImage 只认这个
function Save-Bmp24($bitmap, [string]$path) {
    $w = $bitmap.Width; $h = $bitmap.Height
    $flat = [System.Drawing.Bitmap]::new($w, $h, [System.Drawing.Imaging.PixelFormat]::Format24bppRgb)
    $g = [System.Drawing.Graphics]::FromImage($flat)
    $g.DrawImage($bitmap, 0, 0, $w, $h)
    $g.Dispose()
    $flat.Save($path, [System.Drawing.Imaging.ImageFormat]::Bmp)
    $flat.Dispose()
}

# ---------------------------------------------------------------------------
# sidebar.bmp —— 欢迎页 / 完成页左侧的竖幅
# ---------------------------------------------------------------------------

$w = 164; $h = 314
$side = [System.Drawing.Bitmap]::new($w, $h, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$g = [System.Drawing.Graphics]::FromImage($side)
$g.SmoothingMode = 'AntiAlias'
$g.TextRenderingHint = 'AntiAliasGridFit'   # 不用 ClearType：BMP 没有次像素信息，会出彩边
$g.PixelOffsetMode = 'HighQuality'

# 底：竖向渐变
$rect = [System.Drawing.Rectangle]::new(0, 0, $w, $h)
$bg = [System.Drawing.Drawing2D.LinearGradientBrush]::new($rect, $deepTop, $deepBot, [System.Drawing.Drawing2D.LinearGradientMode]::Vertical)
$g.FillRectangle($bg, $rect)
$bg.Dispose()

# 右上角一团品牌蓝的柔光：深色底上给一点层次，不至于是一块死板的黑
for ($i = 0; $i -lt 60; $i++) {
    $rad = 182 - $i * 2.8
    if ($rad -le 2) { continue }
    $alpha = [int](1 + $i * 0.46)
    if ($alpha -gt 255) { $alpha = 255 }
    $c = [System.Drawing.Color]::FromArgb($alpha, $brandTop)
    $b = [System.Drawing.SolidBrush]::new($c)
    $g.FillEllipse($b, [single]($w - 20 - $rad), [single](-46 - $rad), [single]($rad * 2), [single]($rad * 2))
    $b.Dispose()
}

# 左下角再压一点冷光：让下半部分也有细节，锁标那一片不至于糊在纯色里
for ($i = 0; $i -lt 38; $i++) {
    $rad = 126 - $i * 3.1
    if ($rad -le 2) { continue }
    $alpha = [int](1 + $i * 0.30)
    $c = [System.Drawing.Color]::FromArgb($alpha, 118, 162, 235)
    $b = [System.Drawing.SolidBrush]::new($c)
    $g.FillEllipse($b, [single](-36 - $rad), [single]($h - 30 - $rad), [single]($rad * 2), [single]($rad * 2))
    $b.Dispose()
}

# 顶边一道高光：竖幅贴着窗口边缘，有这道线才显得是「一块面板」而不是漏出来的底色
$hl = [System.Drawing.SolidBrush]::new([System.Drawing.Color]::FromArgb(16, 255, 255, 255))
$g.FillRectangle($hl, 0, 0, $w, 1)
$hl.Dispose()

# 标识
$mark = New-MarkBitmap 46
$g.DrawImage($mark, 22, 26, 46, 46)
$mark.Dispose()

# 名称与说明，贴着底部排
$fWord = [System.Drawing.Font]::new($latinFont, [single]16, [System.Drawing.FontStyle]::Bold, [System.Drawing.GraphicsUnit]::Pixel)
$fTag = [System.Drawing.Font]::new($uiFont, [single]11, [System.Drawing.FontStyle]::Regular, [System.Drawing.GraphicsUnit]::Pixel)
$fVer = [System.Drawing.Font]::new($latinFont, [single]9, [System.Drawing.FontStyle]::Regular, [System.Drawing.GraphicsUnit]::Pixel)

$bWord = [System.Drawing.SolidBrush]::new($sideTitle)
$bTag = [System.Drawing.SolidBrush]::new($sideMuted)
$bVer = [System.Drawing.SolidBrush]::new($sideFaint)

$g.DrawString('OpenPPTView', $fWord, $bWord, 21, 226)
# 名称下的一道短蓝线：分隔，也把品牌色再点一次
$rule = New-RoundedPath 23 254 26 2 1
$rb = [System.Drawing.SolidBrush]::new($brandTop)
$g.FillPath($rb, $rule)
$rule.Dispose(); $rb.Dispose()
$g.DrawString('课件讲演器', $fTag, $bTag, 22, 264)
$g.DrawString('v0.1', $fVer, $bVer, 22, 285)

$g.Dispose()
Save-Bmp24 $side (Join-Path $out 'sidebar.bmp')
$side.Dispose()

# ---------------------------------------------------------------------------
# header.bmp —— 内页顶部的小横幅
# ---------------------------------------------------------------------------

# 底色必须与页面的 MUI_BGCOLOR 一致（现在是白），否则横幅会和页面之间露出一条缝
$w = 150; $h = 57
$head = [System.Drawing.Bitmap]::new($w, $h, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$g = [System.Drawing.Graphics]::FromImage($head)
$g.SmoothingMode = 'AntiAlias'
$g.TextRenderingHint = 'AntiAliasGridFit'
$g.PixelOffsetMode = 'HighQuality'
$g.Clear($paper)

$mark = New-MarkBitmap 24
$g.DrawImage($mark, 10, 17, 24, 24)
$mark.Dispose()

$fWord = [System.Drawing.Font]::new($latinFont, [single]13, [System.Drawing.FontStyle]::Bold, [System.Drawing.GraphicsUnit]::Pixel)
$bWord = [System.Drawing.SolidBrush]::new($ink)
$g.DrawString('OpenPPTView', $fWord, $bWord, 40, 20)
$bWord.Dispose()

$g.Dispose()
Save-Bmp24 $head (Join-Path $out 'header.bmp')
$head.Dispose()

Write-Host "已生成：$out\sidebar.bmp (164x314), $out\header.bmp (150x57)"
