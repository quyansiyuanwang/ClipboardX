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

$failed = [System.Collections.Generic.List[string]]::new()
foreach ($step in $steps) {
    Write-Host "==> $($step.Name)"
    $output = & $step.Cmd 2>&1
    $code = $LASTEXITCODE
    if ($code -ne 0) {
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
