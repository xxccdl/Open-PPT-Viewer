<#
.SYNOPSIS
    合成单文件安装包 OpenPPTView-Setup.exe。

.DESCRIPTION
    主程序和安装器是两个 exe，交付出去得是一个文件：老师双击它就完事，
    不能让人先解压、再挑哪个是安装程序。

    做法是把主程序（和可选的 WebView2 引导器）追加到安装器 exe 的尾部，
    最后 16 字节放载荷长度和魔数。安装器运行时只认这个尾巴，
    解析代码在 crates/ppt-installer/src/install.rs：

        [安装器 exe][app_len u32][boot_len u32][主程序][WebView2 引导器][payload_len u64][魔数 u64]

    安装器本体放在最前面、原样搬运，所以它自己的清单（要求管理员权限）
    和高 DPI 感知都还在。

    注意 `cargo build` 直接产出的叫 `ppt-installer.exe`，那只是安装器的壳、
    尾部没有载荷，单独运行必然报错。**成品只有这一个**：`OpenPPTView-Setup.exe`。

.PARAMETER NoElevation
    打一个不提权的测试包，文件名带 `-dev` 后缀，放在 `out\dev\` 下。
    用途只有一个：本机自测安装流程 —— 正式包要求管理员权限，
    每次运行都弹 UAC，没法自动化验证。**别把它发给用户。**

.PARAMETER SkipWebView2
    不往包里塞 WebView2 引导器，安装包会小 1.8MB 左右。
    不塞也没关系：目标机器缺运行环境时，安装器会现去微软下一份，
    只是那一步得联网。学校机房里没网的话，建议塞进去。

.PARAMETER WebView2Path
    自己指定 WebView2 引导器（MicrosoftEdgeWebview2Setup.exe）的位置。

.PARAMETER OutDir
    产物目录，默认 packaging\out。

.EXAMPLE
    pwsh -File packaging\windows\make-setup.ps1
#>
[CmdletBinding()]
param(
    [switch]$NoElevation,
    [switch]$SkipWebView2,
    [string]$WebView2Path,
    [string]$OutDir
)

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
if (-not $OutDir) { $OutDir = Join-Path $root 'packaging\out' }

# 主程序在安装目录里叫什么，跟 install.rs 的 EXE_NAME 必须一致
$AppExeName = 'OpenPPTView.exe'
# 安装器 crate 的产物名：只是壳，跟成品故意不同名
$InstallerBin = 'ppt-installer.exe'
$SetupExeName = 'OpenPPTView-Setup.exe'
if ($NoElevation) {
    $SetupExeName = 'OpenPPTView-Setup-dev.exe'
    $OutDir = Join-Path $OutDir 'dev'
}

function Get-TargetDir {
    if ($env:CARGO_TARGET_DIR) { return $env:CARGO_TARGET_DIR }
    $cfg = Join-Path $root '.cargo\config.toml'
    if (Test-Path $cfg) {
        $m = Select-String -Path $cfg -Pattern '^\s*target-dir\s*=\s*"([^"]+)"' | Select-Object -First 1
        if ($m) {
            $dir = $m.Matches[0].Groups[1].Value
            if ([IO.Path]::IsPathRooted($dir)) { return $dir }
            return (Join-Path $root $dir)
        }
    }
    return (Join-Path $root 'target')
}

function Build([string[]]$CargoArgs, [string]$What) {
    Write-Host "==> $What" -ForegroundColor Cyan
    # cargo 的 warning 走 stderr；$ErrorActionPreference='Stop' 会把它当成致命错误，
    # 所以这一段单独放开，只看退出码
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    & cargo build --release @CargoArgs
    $code = $LASTEXITCODE
    $ErrorActionPreference = $prev
    if ($code -ne 0) { throw "$What 失败（cargo 退出码 $code）" }
}

$targetDir = Get-TargetDir
$release = Join-Path $targetDir 'release'

# ---------------------------------------------------------------------------
# 1. 主程序
# ---------------------------------------------------------------------------
Build @('-p', 'ppt-app') '构建主程序'

# Tauri 用 productName 当成品名，cargo 用 crate 名，两个都可能出现
$appExe = @(
    (Join-Path $release 'ppt-app.exe'),
    (Join-Path $release "$AppExeName")
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $appExe) { throw "找不到主程序（看过了 $release 下的 ppt-app.exe / $AppExeName）" }

# ---------------------------------------------------------------------------
# 2. 安装器（带「以管理员身份运行」的清单）
# ---------------------------------------------------------------------------
if ($NoElevation) {
    Build @('-p', 'ppt-installer', '--no-default-features') '构建安装器（不提权，仅供本机自测）'
} else {
    Build @('-p', 'ppt-installer', '--features', 'admin-manifest') '构建安装器'
}

$installerExe = Join-Path $release $InstallerBin
if (-not (Test-Path $installerExe)) { throw "找不到安装器 $installerExe" }

# ---------------------------------------------------------------------------
# 3. WebView2 引导器（可选）
# ---------------------------------------------------------------------------
$bootExe = $null
if (-not $SkipWebView2) {
    $redist = Join-Path $PSScriptRoot 'redist'
    $cached = Join-Path $redist 'MicrosoftEdgeWebview2Setup.exe'
    if ($WebView2Path) {
        if (-not (Test-Path $WebView2Path)) { throw "指定的 WebView2 引导器不存在：$WebView2Path" }
        $bootExe = $WebView2Path
    } elseif (Test-Path $cached) {
        $bootExe = $cached
    } else {
        # 微软的常青引导器（Evergreen Bootstrapper）永久链接
        $url = 'https://go.microsoft.com/fwlink/p/?LinkId=2124703'
        Write-Host "==> 下载 WebView2 引导器（只需一次，之后缓存在 packaging\windows\redist）" -ForegroundColor Cyan
        try {
            New-Item -ItemType Directory -Force -Path $redist | Out-Null
            Invoke-WebRequest -Uri $url -OutFile $cached -UseBasicParsing
            $bootExe = $cached
        } catch {
            Write-Warning "WebView2 引导器没下下来：$($_.Exception.Message)"
            Write-Warning "包里就不塞它了；目标机器缺运行环境时会现去微软下一份（需要联网）"
        }
    }
    if ($bootExe) {
        Write-Host ("    引导器 {0:N2} MB" -f ((Get-Item $bootExe).Length / 1MB))
    }
}

# ---------------------------------------------------------------------------
# 4. 拼载荷
# ---------------------------------------------------------------------------
$installerBytes = [IO.File]::ReadAllBytes($installerExe)
$appBytes = [IO.File]::ReadAllBytes($appExe)

# 千万别写成 `$bootBytes = if (...) { [IO.File]::ReadAllBytes(...) } else { $null }`。
#
# PowerShell 会把 `if` 表达式的输出**摊平成 Object[]**：长度看着是对的
# （`.Length` 仍是 1.8MB），但 `BinaryWriter.Write()` 绑不到重载，
# 于是**一个字节都没写进载荷**，头部却照实写了 boot_len ——
# 装的时候就成了「安装包中的程序数据不完整」。
# 而 dev 包没有引导器（boot_len = 0），自洽，所以一直是好的：
# 这正是「dev 能装、正式版不能」的全部原因。
# 显式标注 [byte[]] 并且分成两句赋值，类型才不会被摊平。
[byte[]]$bootBytes = $null
if ($bootExe) {
    $bootBytes = [IO.File]::ReadAllBytes($bootExe)
}

$ms = New-Object IO.MemoryStream
$bw = New-Object IO.BinaryWriter($ms)
$bw.Write([uint32]$appBytes.Length)
$bw.Write([uint32]$(if ($bootBytes) { $bootBytes.Length } else { 0 }))
$bw.Write($appBytes)
if ($bootBytes) { $bw.Write($bootBytes) }
$bw.Flush()
$payload = $ms.ToArray()
$bw.Dispose()

# 末尾 16 字节：载荷长度 + 魔数（只认尾巴，不去全文件搜魔数）
$tail = New-Object IO.MemoryStream
$tw = New-Object IO.BinaryWriter($tail)
$tw.Write([uint64]$payload.Length)
$tw.Write([uint64]0x315941505650504F)   # 小端读出来就是 "OPPVPAY1"
$tw.Flush()
$tailBytes = $tail.ToArray()
$tw.Dispose()

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$outPath = Join-Path $OutDir $SetupExeName

# 先写临时文件，再整份换过去。
#
# 为什么不直接往 $outPath 写：老师（或杀软）可能正开着/扫着上一个版本，
# 这时候写进去的就是个「一半新一半旧」的 exe —— 双击只会得到
# 「安装包中缺少程序数据」，看着像打包坏了，其实是我们递了半成品出去。
# 同目录改名是原子的，读的人要么看到旧的完整包，要么看到新的完整包。
$tmpPath = "$outPath.tmp"
$fs = [IO.File]::Create($tmpPath)
try {
    $fs.Write($installerBytes, 0, $installerBytes.Length)
    $fs.Write($payload, 0, $payload.Length)
    $fs.Write($tailBytes, 0, $tailBytes.Length)
} finally {
    $fs.Dispose()
}

# 覆盖时仍可能撞上占用（杀软扫描、文件正被打开）：退避重试几次
$moved = $false
for ($i = 1; $i -le 10; $i++) {
    try {
        Move-Item -Force -Path $tmpPath -Destination $outPath -ErrorAction Stop
        $moved = $true
        break
    } catch {
        if ($i -eq 10) { break }
        Write-Host "    $outPath 暂时被占用，等一会儿再试（第 $i 次）"
        Start-Sleep -Milliseconds 500
    }
}
if (-not $moved) {
    Remove-Item -Force $tmpPath -ErrorAction SilentlyContinue
    throw "写不进 $outPath（可能被杀软或资源管理器占用）。关掉它再跑一次。"
}

# ---------------------------------------------------------------------------
# 5. 读回来对一遍（和安装器运行时的解析走同一套规则）
# ---------------------------------------------------------------------------
$ahead = [IO.File]::ReadAllBytes($outPath)
$readTail = $ahead[($ahead.Length - 16)..($ahead.Length - 1)]
$readLen = [BitConverter]::ToUInt64($readTail, 0)
$readMagic = [BitConverter]::ToUInt64($readTail, 8)
if ($readMagic -ne [uint64]0x315941505650504F) { throw '自检失败：魔数对不上' }
if ($readLen -ne $payload.Length) { throw "自检失败：载荷长度对不上（写的 $($payload.Length)，读回 $readLen）" }
$payloadStart = $ahead.Length - 16 - $payload.Length
$readAppLen = [BitConverter]::ToUInt32($ahead, $payloadStart)
$readBootLen = [BitConverter]::ToUInt32($ahead, $payloadStart + 4)
if ($readAppLen -ne $appBytes.Length) { throw '自检失败：主程序长度对不上' }

# 这一段必须查：曾经因为 `$bootBytes` 被 PowerShell 摊平成 Object[]，
# `BinaryWriter.Write()` 静默一个字节没写，头部却写着 boot_len ——
# 生成出来的包「看着」有 13MB、魔数也对，装的时候才报程序数据不完整。
# `$null` 上取 `.Length` 在不同 PowerShell 版本上行为不一致，先归一成 0
$bootLen = if ($bootBytes) { $bootBytes.Length } else { 0 }
if ($readBootLen -ne $bootLen) {
    throw "自检失败：运行环境长度对不上（写的 $bootLen，读回 $readBootLen）"
}
$need = 8 + $readAppLen + $readBootLen
if ($need -ne $payload.Length) {
    throw "自检失败：载荷内部对不上账（8 + $readAppLen + $readBootLen = $need，实际载荷 $($payload.Length)）"
}

Write-Host ''
Write-Host "安装包：$outPath" -ForegroundColor Green
Write-Host ("  共 {0:N2} MB（安装器 {1:N2} + 主程序 {2:N2}{3}）" -f `
    ($ahead.Length / 1MB), ($installerBytes.Length / 1MB), ($appBytes.Length / 1MB), `
    $(if ($bootBytes) { (" + 运行环境 {0:N2}" -f ($bootBytes.Length / 1MB)) } else { '' }))
