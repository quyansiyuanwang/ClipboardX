# 构建 Windows MSI 安装包
#
# 依赖：

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
        Write-Host "==> 使用 UI 扩展：$($extDll.FullName)"
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

    $size = [math]::Round((Get-Item $out).Length / 1KB, 1)
    Write-Host "==> 完成：$out ($size KB)"
    $out
}
finally {
    Pop-Location
}
