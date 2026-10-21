# 构建 Windows MSI 安装包
#
# 依赖：
#   - cargo / rustc
#   - .NET SDK（dotnet 10 已装）；WiX 通过仓库内 .config/dotnet-tools.json 固定为 v4
#
# 用法：
#   pwsh scripts/build-msi.ps1
#   pwsh scripts/build-msi.ps1 -Version 0.2.0 -NoUi
#
# 产物：dist\ClipboardX-<version>-x64.msi
[CmdletBinding()]
param(
    [string]$Version,
    [string]$Configuration = 'Release',
    [string]$Target = 'x86_64-pc-windows-msvc',
    [switch]$NoUi,
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
Push-Location $repo
try {
    if (-not $Version) {
        $cargoToml = Get-Content 'Cargo.toml' -Raw
        $m = [regex]::Match($cargoToml, '(?m)^\s*version\s*=\s*"([^"]+)"')
        if (-not $m.Success) { throw '无法从 Cargo.toml 解析 workspace 版本号' }
        $Version = $m.Groups[1].Value
    }
    Write-Host "==> ClipboardX MSI 版本 $Version (target $Target)"

    if (-not $SkipBuild) {
        Write-Host '==> cargo build --release'
        & cargo build --release --target $Target --bin cx --bin cx-server
        if ($LASTEXITCODE -ne 0) { throw "cargo build 失败 (exit $LASTEXITCODE)" }
    }

    $binDir = Join-Path $repo "target\$Target\release"
    $cxExe = Join-Path $binDir 'cx.exe'
    $cxServerExe = Join-Path $binDir 'cx-server.exe'
    if (-not (Test-Path $cxExe)) { throw "找不到 $cxExe，请先构建（去掉 -SkipBuild）" }
    if (-not (Test-Path $cxServerExe)) { throw "找不到 $cxServerExe，请先构建（去掉 -SkipBuild）" }

    # 只在 -SkipBuild 时校验载荷版本：这条路最容易把 target 里的旧二进制打进包
    # （v0.3.2 的 MSI 因此缺失 --key-stdin）。不带 -SkipBuild 时刚刚重新编译过，
    # 版本对不上是**故意的**——升级验收会用 0.0.9 这样的道具版本打同一个载荷。
    if ($SkipBuild) {
        $reportedVersion = (& $cxExe version 2>$null | Select-Object -First 1) -replace '^cx\s+', ''
        if ($reportedVersion -ne $Version) {
            throw ("载荷版本（cx $reportedVersion）与要打包的版本（$Version）不一致。" +
                "去掉 -SkipBuild 重新编译，或先 cargo build --release --target $Target --bin cx --bin cx-server")
        }
    }

    $distDir = Join-Path $repo 'dist'
    New-Item -ItemType Directory -Force -Path $distDir | Out-Null

    Write-Host '==> dotnet tool restore (WiX v4)'
    & dotnet tool restore
    if ($LASTEXITCODE -ne 0) { throw "dotnet tool restore 失败 (exit $LASTEXITCODE)" }

    $withUi = if ($NoUi) { 0 } else { 1 }
    $wixExtArgs = @()
    if (-not $NoUi) {
        # WixUI 对话框库来自 UI 扩展，且扩展必须与 WiX 工具版本严格对应
        # （4.x 落在 wixext4 目录，7.x 落在 wixext7，混用会报 WIX0144）。
        # 这里不使用 -ext <扩展ID>：无版本号的 ID 解析会跑到扩展缓存里的最高版本（7.0.0）
        # 再用本工具的 flavor 找目录，必然失败。改为显式传入 4.0.6/wixext4 下的 DLL 路径。
        $wixVersion = (Get-Content '.config/dotnet-tools.json' -Raw | ConvertFrom-Json).tools.wix.version
        $extRoot = Join-Path $env:USERPROFILE ".wix\extensions\WixToolset.UI.wixext\$wixVersion"
        $extDll = Get-ChildItem -Path $extRoot -Recurse -Filter 'WixToolset.UI.wixext.dll' `
            -ErrorAction SilentlyContinue | Select-Object -First 1

        if (-not $extDll) {
            Write-Host "==> wix extension add WixToolset.UI.wixext/$wixVersion"
            & dotnet tool run wix extension add --global "WixToolset.UI.wixext/$wixVersion" 2>&1 |
                ForEach-Object { Write-Host "    $_" }
            $extDll = Get-ChildItem -Path $extRoot -Recurse -Filter 'WixToolset.UI.wixext.dll' `
                -ErrorAction SilentlyContinue | Select-Object -First 1
        }
        if (-not $extDll) {
            throw "找不到 WixToolset.UI.wixext/$wixVersion。请联网后重试，或用 -NoUi 构建无向导的 MSI。"
        }
        # 把扩展的路径、大小、文件版本都打出来：CI 上一旦再出现"有扩展却没向导"，
        # 日志里就能看出到底加载的是哪一份（是不是 flavor/版本不匹配）
        Write-Host ("==> 使用 UI 扩展：{0}（{1} KB，文件版本 {2}）" -f $extDll.FullName,
            [math]::Round($extDll.Length / 1KB, 0), $extDll.VersionInfo.FileVersion)
        $wixExtArgs = @('-ext', $extDll.FullName)
    }

    $out = Join-Path $distDir "ClipboardX-$Version-x64.msi"
    if (Test-Path $out) { Remove-Item $out -Force }

    $wixArgs = @(
        'tool', 'run', 'wix', 'build',
        'packaging/wix/Package.wxs',
        '-arch', 'x64',
        "-d", "ProductVersion=$Version",
        "-d", "BinDir=$binDir",
        "-d", "WithUi=$withUi",
        '-out', $out
    )
    $wixArgs += $wixExtArgs

    Write-Host "==> wix build -> $out"
    & dotnet @wixArgs
    if ($LASTEXITCODE -ne 0) { throw "wix build 失败 (exit $LASTEXITCODE)" }

    # 构建后自检：要 UI 就必须真的带安装向导。
    # 出现过「扩展找到了、wix build 成功、产物却没有 Dialog 表」的情况（CI 上 0 个对话框，
    # 本机 21 个）：这种包能装能升级，验收脚本也全绿，只是双击时没有任何向导。
    # 与其等发版后靠人发现，不如这里直接失败。
    if (-not $NoUi) {
        $dialogs = 0
        try {
            # wix build 刚返回时文件句柄可能还没完全释放，先等一下再打开数据库
            Start-Sleep -Milliseconds 300
            $installer = New-Object -ComObject WindowsInstaller.Installer
            # 注意：COM 反射调用要求参数是**明确的**类型。`$out` 是从 Join-Path 来的
            # 普通变量（PSObject 包装），直接传会 DISP_E_TYPEMISMATCH，所以这里显式转型。
            $db = $installer.GetType().InvokeMember('OpenDatabase', 'InvokeMethod', $null, $installer, @([string]$out, [int]0))
            $view = $db.GetType().InvokeMember('OpenView', 'InvokeMethod', $null, $db, @('SELECT Dialog FROM Dialog'))
            $view.GetType().InvokeMember('Execute', 'InvokeMethod', $null, $view, $null)
            while ($view.GetType().InvokeMember('Fetch', 'InvokeMethod', $null, $view, $null)) { $dialogs++ }
        }
        catch {
            Write-Host "    [自检] 读 MSI 的 Dialog 表失败：$($_.Exception.Message)" -ForegroundColor Yellow
            $dialogs = 0
        }
        if ($dialogs -lt 5) {
            throw ("构建出来的 MSI 只有 $dialogs 个对话框——UI 扩展没有真正生效。" +
                "扩展路径：$($wixExtArgs -join ' ')。请检查扩展与 WiX 版本是否匹配，或用 -NoUi 显式构建无向导版本。")
        }
        Write-Host "==> 自检通过：含安装向导（$dialogs 个对话框）"
    }

    $size = [math]::Round((Get-Item $out).Length / 1KB, 1)
    Write-Host "==> 完成：$out ($size KB)"
    $out
}
finally {
    Pop-Location
}
