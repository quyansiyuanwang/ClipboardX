# 提交前门禁：格式 → clippy（警告即错误）→ 测试
#
# 用法：
#   pwsh scripts/check.ps1              # 全部
#   pwsh scripts/check.ps1 -SkipTest    # 只做静态检查
#
# 退出码：0 = 全部通过；1 = 有失败项（会打印失败清单）。
# 之所以需要它：曾经在 cargo check 失败的情况下提交并推送，导致 CI 六个 job 全红。
[CmdletBinding()]
param(
    [switch]$SkipTest,
    [switch]$Quiet
)

$ErrorActionPreference = 'Continue'
$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

$steps = [System.Collections.Generic.List[object]]::new()
$steps.Add(@{ Name = 'cargo fmt --all -- --check'; Cmd = { cargo fmt --all -- --check } })
$steps.Add(@{ Name = 'cargo clippy --workspace --all-targets -D warnings'; Cmd = { cargo clippy --workspace --all-targets -- -D warnings } })
if (-not $SkipTest) {
    $steps.Add(@{ Name = 'cargo test --workspace'; Cmd = { cargo test --workspace } })
}

# 跨平台 lint：Windows 上编不出 Unix 的 cfg 分支，像"函数在 Unix 上是 dead code"
# 这类问题只会在 CI 的 Linux/macOS 作业里炸出来。本机装了 linux 目标就顺带查一遍。
$hostIsWindows = $IsWindows -or ($PSVersionTable.PSEdition -eq 'Desktop')
if ($hostIsWindows) {
    $installed = (rustup target list --installed 2>$null) -split "`n" | ForEach-Object { $_.Trim() }
    if ($installed -contains 'x86_64-unknown-linux-gnu') {
        $steps.Add(@{
                Name = 'cargo clippy --target x86_64-unknown-linux-gnu -D warnings'
                Cmd  = { cargo clippy --workspace --all-targets --target x86_64-unknown-linux-gnu -- -D warnings }
                # 目标平台缺 C 工具链时（ring 要编 C/汇编）不算门禁失败：
                # 真正的 Linux 检查由 CI 的 ubuntu 作业负责，这里只求抓到 cfg 分支的 lint。
                AllowToolchainMiss = $true
            })
    }
    else {
        Write-Host '提示：未安装 x86_64-unknown-linux-gnu 目标，跳过跨平台 clippy' -ForegroundColor Yellow
        Write-Host '      rustup target add x86_64-unknown-linux-gnu' -ForegroundColor Yellow
    }
}

$failed = [System.Collections.Generic.List[string]]::new()
foreach ($step in $steps) {
    Write-Host "==> $($step.Name)"
    $output = & $step.Cmd 2>&1
    $code = $LASTEXITCODE
    if ($code -ne 0) {
        $text = ($output | Out-String)
        $toolchainMiss = $text -match 'failed to find tool|error occurred in cc-rs|linker .* not found'
        if ($step.AllowToolchainMiss -and $toolchainMiss) {
            Write-Host '    跳过：本机缺少目标平台的 C 工具链（CI 的对应平台作业会检查）' -ForegroundColor Yellow
            continue
        }
        $failed.Add($step.Name)
        Write-Host "    失败（exit $code），输出末尾：" -ForegroundColor Red
        $output | Select-Object -Last 25 | ForEach-Object { Write-Host "      $_" }
    }
    else {
        if (-not $Quiet) { $output | Select-Object -Last 3 | ForEach-Object { Write-Host "    $_" } }
        Write-Host '    通过' -ForegroundColor Green
    }
}

if ($failed.Count -gt 0) {
    Write-Host ''
    Write-Host "门禁未通过，共 $($failed.Count) 项：" -ForegroundColor Red
    $failed | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}

Write-Host ''
Write-Host '门禁全部通过' -ForegroundColor Green
exit 0
