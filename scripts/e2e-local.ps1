# 端到端验收：守护进程 + CLI（+ 可选的真实剪贴板）
#
# 用法：
#   pwsh scripts/e2e-local.ps1                 # 完整模式：需要图形会话（读写真实剪贴板）
#   pwsh scripts/e2e-local.ps1 -NoClipboard    # 无头模式：守护进程以 --no-clipboard 运行，CI 用
#   pwsh scripts/e2e-local.ps1 -KeepTemp       # 保留临时目录便于排障
#
# 说明：
# - 全程使用临时目录作为配置/数据/缓存（CX_*_DIR），不碰真实的 ~/.config/clipboardx；
# - 完整模式会临时改写系统剪贴板，结束时恢复原内容；
# - 带断言，任一条失败即非零退出，并打印守护进程日志尾部。
[CmdletBinding()]
param(
    [switch]$NoClipboard,
    [switch]$KeepTemp,
    [int]$WaitSeconds = 3
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$root = Join-Path ([IO.Path]::GetTempPath()) ("cx-e2e-" + [guid]::NewGuid().ToString('N').Substring(0, 8))

$env:CX_CONFIG_DIR = Join-Path $root 'config'
$env:CX_DATA_DIR = Join-Path $root 'data'
$env:CX_CACHE_DIR = Join-Path $root 'cache'
$env:CX_LOG = 'debug'

$isWindowsHost = $IsWindows -or ($PSVersionTable.PSEdition -eq 'Desktop')
$cx = if ($isWindowsHost) { Join-Path $repo 'target\release\cx.exe' } else { Join-Path $repo 'target/release/cx' }

$failures = [System.Collections.Generic.List[string]]::new()
$originalClipboard = $null
$daemonProc = $null

function Step([string]$name) { Write-Host "==> $name" -ForegroundColor Cyan }
function Ok([string]$message) { Write-Host "    [ok] $message" -ForegroundColor Green }
function Fail([string]$message) {
    Write-Host "    [fail] $message" -ForegroundColor Red
    $failures.Add($message)
}
function Assert-True([bool]$condition, [string]$message) {
    if ($condition) { Ok $message } else { Fail $message }
}
function Invoke-Cx {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Arguments)
    $output = & $cx @Arguments 2>&1
    return @{ Output = ($output -join "`n"); Code = $LASTEXITCODE }
}
# 把输出写进文件再读回来：避免"守护进程继承输出管道"导致的挂起
function Invoke-CxToFile {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Arguments)
    $path = Join-Path ([IO.Path]::GetTempPath()) ("cx-out-" + [guid]::NewGuid().ToString('N') + ".txt")
    & $cx @Arguments *> $path
    $code = $LASTEXITCODE
    $raw = if (Test-Path $path) { Get-Content $path -Raw } else { '' }
    $text = if ($null -eq $raw) { '' } else { [string]$raw }
    Remove-Item $path -Force -ErrorAction SilentlyContinue
    $daemonPid = $null
    if ($text) {
        $pidMatch = [regex]::Match($text, 'pid (\d+)')
        if ($pidMatch.Success) { $daemonPid = [int]$pidMatch.Groups[1].Value }
    }
    return @{
        Output    = $text.Trim()
        Code      = $code
        DaemonPid = $daemonPid
    }
}
function AsJson([string]$text) { return ($text | ConvertFrom-Json) }
function Get-ClipboardText {
    try { return (Get-Clipboard -Raw) } catch { return $null }
}
function Set-ClipboardText([string]$text) {
    if (Get-Command Set-Clipboard -ErrorAction SilentlyContinue) { Set-Clipboard -Value $text }
    else { $text | clip }
}

Write-Host "临时目录：$root" -ForegroundColor DarkGray
Write-Host ("模式：" + $(if ($NoClipboard) { '无头（--no-clipboard）' } else { '完整（真实剪贴板）' })) -ForegroundColor DarkGray

try {
    Step '构建 release 二进制'
    Push-Location $repo
    & cargo build --release --bin cx 2>&1 | Select-Object -Last 1 | ForEach-Object { Write-Host "    $_" }
    $buildCode = $LASTEXITCODE
    Pop-Location
    if ($buildCode -ne 0) { throw "cargo build 失败（exit $buildCode）" }
    Assert-True (Test-Path $cx) "二进制就绪：$cx"

    if (-not $NoClipboard) { $originalClipboard = Get-ClipboardText }

    Step 'cx version'
    $result = Invoke-Cx version --json
    Assert-True ($result.Code -eq 0) '退出码 0'
    Assert-True ((AsJson $result.Output).name -eq 'cx') 'name = cx'

    Step '守护进程未运行时给出可操作提示'
    $result = Invoke-Cx status
    Assert-True ($result.Code -eq 3) "未运行时退出码 3（实际 $($result.Code)）"
    Assert-True ($result.Output -match 'cx start') '提示包含 cx start'

    Step 'cx doctor（未运行 → 本地检查）'
    $result = Invoke-Cx doctor --json
    $local = AsJson $result.Output
    $localNames = ($local.checks | ForEach-Object { $_.name }) -join ','
    Assert-True ($localNames -match '配置') "本地诊断包含配置检查（$localNames）"
    Assert-True ($localNames -match '历史存储') '本地诊断包含历史存储检查'

    Step 'cx start（后台分离进程，输出重定向到文件）'
    # 为什么把输出重定向到文件：Windows 上守护进程会继承父进程的可继承句柄，
    # 若调用方用管道捕获输出，管道会一直开着直到守护进程退出（脚本/CI 会挂住）。
    # 详见 docs/packaging.md §8「守护进程与输出管道」。
    $startArgs = @('start', '--silent')
    if ($NoClipboard) { $startArgs += '--no-clipboard' }
    $startResult = Invoke-CxToFile @startArgs
    Assert-True ($startResult.Code -eq 0) "cx start 退出码 0（$($startResult.Output)）"
    Assert-True (Test-Path (Join-Path $env:CX_DATA_DIR 'daemon.json')) '写入了 daemon.json'
    # --silent 时没有输出，因此改用 status 确认分离进程确实活着
    $startedStatus = AsJson (Invoke-Cx status --json).Output
    Assert-True ($startedStatus.pid -gt 0) "分离进程存活（pid $($startedStatus.pid)）"
    Assert-True ($null -ne (Get-Process -Id $startedStatus.pid -ErrorAction SilentlyContinue)) '进程确实在运行'
    Assert-True ((Invoke-Cx stop).Code -eq 0) 'cx stop 能停掉它'
    Start-Sleep -Seconds 2
    Assert-True ($null -eq (Get-Process cx -ErrorAction SilentlyContinue | Select-Object -First 1)) '分离进程已退出'

    Step '启动守护进程（受管后台进程，后续断言都用它）'
    $daemonArgs = @('daemon', '--foreground', '--quiet')
    if ($NoClipboard) { $daemonArgs += '--no-clipboard' }
    $startParams = @{
        FilePath     = $cx
        ArgumentList = $daemonArgs
        PassThru     = $true
    }
    if ($isWindowsHost) { $startParams['WindowStyle'] = 'Hidden' }
    $daemonProc = Start-Process @startParams
    for ($i = 0; $i -lt 60; $i++) {
        if (Test-Path (Join-Path $env:CX_DATA_DIR 'daemon.json')) { break }
        Start-Sleep -Milliseconds 100
    }
    Assert-True (Test-Path (Join-Path $env:CX_DATA_DIR 'daemon.json')) '守护进程就绪（daemon.json）'
    Assert-True ($null -ne (Get-Process -Id $daemonProc.Id -ErrorAction SilentlyContinue)) '受管进程存活'

    Step 'cx status'
    $result = Invoke-Cx status --json
    Assert-True ($result.Code -eq 0) 'cx status 退出码 0'
    $status = AsJson $result.Output
    Assert-True ($status.pid -gt 0) "报告了 pid（$($status.pid)）"
    Assert-True (-not $status.paused) '初始未暂停'
    if ($NoClipboard) {
        Assert-True ($status.no_clipboard) 'status 标记为纯中继模式'
    }
    else {
        Assert-True ($status.clipboard -match '剪贴板') "剪贴板后端：$($status.clipboard)"
    }

    Step 'cx pull（暂无后端也不应失败）'
    $result = Invoke-Cx pull
    Assert-True ($result.Code -eq 0) 'cx pull 退出码 0'

    Step 'cx push --text（不依赖剪贴板的发布路径）'
    $pushed = "e2e push " + [guid]::NewGuid().ToString('N').Substring(0, 8)
    $result = Invoke-Cx push --text $pushed
    Assert-True ($result.Code -eq 0) 'cx push 退出码 0'
    $result = Invoke-Cx show 0 --json
    $preview = AsJson $result.Output
    Assert-True ($preview.text -eq $pushed) "最新历史是刚推送的内容（$($preview.text)）"

    Step 'cx history 列表'
    $result = Invoke-Cx history --json
    $history = AsJson $result.Output
    Assert-True ($history.Count -ge 1) "至少 1 条（实际 $($history.Count)）"
    Assert-True ($history[0].kind -eq 'text') '最新一条是文本'

    if (-not $NoClipboard) {
        # ---------- 真实剪贴板相关 ----------
        $text1 = "ClipboardX e2e 第一条 🎉 " + [guid]::NewGuid().ToString('N').Substring(0, 8)
        Step "写入剪贴板并等待采集：$text1"
        Set-ClipboardText $text1
        Start-Sleep -Seconds $WaitSeconds

        $history1 = AsJson (Invoke-Cx history --json).Output
        Assert-True ($history1[0].kind -eq 'text') '剪贴板内容已被采集'
        $preview = AsJson (Invoke-Cx show 0 --json).Output
        Assert-True ($preview.text -eq $text1) "采集内容与剪贴板一致（$($preview.text)）"

        Step '相同内容不重复入库'
        Set-ClipboardText $text1
        Start-Sleep -Seconds $WaitSeconds
        $history2 = AsJson (Invoke-Cx history --json).Output
        Assert-True ($history2.Count -eq $history1.Count) "条数不变（$($history2.Count)）"

        $text2 = "ClipboardX e2e 第二条 " + [guid]::NewGuid().ToString('N').Substring(0, 8)
        Step "第二条内容：$text2"
        Set-ClipboardText $text2
        Start-Sleep -Seconds $WaitSeconds
        $history3 = AsJson (Invoke-Cx history --json).Output
        Assert-True ($history3.Count -eq ($history2.Count + 1)) "新增一条（$($history3.Count)）"

        Step 'cx copy 1 写回剪贴板且不产生重复上传'
        # 先记下位置 1 的内容，再改写剪贴板为无关内容，最后 copy 回来
        $target = AsJson (Invoke-Cx show 1 --json).Output
        $countBeforeCopy = (AsJson (Invoke-Cx history --json).Output).Count
        $result = Invoke-Cx copy 1
        Assert-True ($result.Code -eq 0) 'cx copy 退出码 0'
        Start-Sleep -Milliseconds 1200
        $clipboardBack = Get-ClipboardText
        Assert-True ($clipboardBack -eq $target.text) "剪贴板变回位置 1 的内容（$clipboardBack）"
        Start-Sleep -Seconds $WaitSeconds
        $countAfterCopy = (AsJson (Invoke-Cx history --json).Output).Count
        Assert-True ($countAfterCopy -eq $countBeforeCopy) "写回没有导致重复入库（$countAfterCopy）"

        Step 'cx pause 期间不采集'
        $result = Invoke-Cx pause
        Assert-True ($result.Code -eq 0) 'cx pause 退出码 0'
        $before = (AsJson (Invoke-Cx history --json).Output).Count
        Set-ClipboardText ("暂停期间的内容 " + [guid]::NewGuid().ToString('N').Substring(0, 8))
        Start-Sleep -Seconds $WaitSeconds
        Assert-True ((AsJson (Invoke-Cx status --json).Output).paused) 'status 报告已暂停'
        $afterPause = (AsJson (Invoke-Cx history --json).Output).Count
        Assert-True ($afterPause -eq $before) "暂停期间不新增（$afterPause = $before）"

        Step 'cx resume 后补发最新内容'
        $result = Invoke-Cx resume
        Assert-True ($result.Code -eq 0) 'cx resume 退出码 0'
        Start-Sleep -Seconds $WaitSeconds
        $afterResume = (AsJson (Invoke-Cx history --json).Output).Count
        Assert-True ($afterResume -eq ($before + 1)) "恢复后补发一条（$afterResume）"
    }
    else {
        Step '暂停 / 恢复（无头模式）'
        Assert-True ((Invoke-Cx pause).Code -eq 0) 'cx pause 退出码 0'
        Assert-True ((AsJson (Invoke-Cx status --json).Output).paused) 'status 报告已暂停'
        Assert-True ((Invoke-Cx resume).Code -eq 0) 'cx resume 退出码 0'
        Assert-True (-not (AsJson (Invoke-Cx status --json).Output).paused) '恢复后 status 不再暂停'
    }

    Step '重复启动不产生第二个实例'
    $pidBefore = (AsJson (Invoke-Cx status --json).Output).pid
    $result = Invoke-Cx start --silent
    Assert-True ($result.Code -eq 0) 'cx start 幂等'
    $pidAfter = (AsJson (Invoke-Cx status --json).Output).pid
    Assert-True ($pidBefore -eq $pidAfter) "pid 未变（$pidBefore）"

    Step 'cx doctor（守护进程运行中）'
    $result = Invoke-Cx doctor --json
    $daemonReport = AsJson $result.Output
    $daemonCheck = $daemonReport.checks | Where-Object { $_.name -eq '守护进程' }
    Assert-True ($null -eq $daemonCheck -or $daemonCheck.ok) '运行中不再报告"守护进程未运行"'
    $checkNames = ($daemonReport.checks | ForEach-Object { $_.name }) -join ','
    Assert-True ($checkNames -match '剪贴板') "诊断包含剪贴板项（$checkNames）"

    Step 'cx config / cx service status'
    Assert-True ((Invoke-Cx config check).Code -eq 0) 'cx config check 通过'
    Assert-True ((Invoke-Cx service status).Code -eq 0) 'cx service status 可查询'

    Step 'cx reload'
    Assert-True ((Invoke-Cx reload).Code -eq 0) 'cx reload 退出码 0'

    Step 'cx tui --snapshot（不需要终端：渲染一帧后退出）'
    $snapshot = Invoke-Cx tui --snapshot
    Assert-True ($snapshot.Code -eq 0) 'cx tui --snapshot 退出码 0'
    # 宽字符在快照里会带占位空格（测试/文本后端的行为），比较前先去掉空白
    $flat = ($snapshot.Output -replace '\s', '')
    Assert-True ($flat.Contains('ClipboardX')) '快照里有标题'
    Assert-True ($flat.Contains('历史')) '快照里有历史面板'
    Assert-True ($flat.Contains('写回剪贴板')) '快照里有快捷键提示'
    # 界面读的是守护进程实时状态：刚 push 的内容应当出现在预览里
    Assert-True ($flat.Contains(($pushed -replace '\s', ''))) "快照预览里能看到刚推送的内容（$pushed）"


    Step 'cx stop'
    $result = Invoke-Cx stop
    Assert-True ($result.Code -eq 0) 'cx stop 退出码 0'
    Start-Sleep -Seconds 2
    $result = Invoke-Cx status
    Assert-True ($result.Code -eq 3) "停止后 status 退出码 3（实际 $($result.Code)）"

    Step '可重复 start/stop'
    $again = @('start', '--silent')
    if ($NoClipboard) { $again += '--no-clipboard' }
    Assert-True ((Invoke-CxToFile @again).Code -eq 0) '再次启动成功'
    Start-Sleep -Seconds 2
    Assert-True ((Invoke-Cx stop).Code -eq 0) '再次停止成功'
}
catch {
    Fail "未预期的异常：$($_.Exception.Message)"
}
finally {
    if (-not $NoClipboard -and $null -ne $originalClipboard) {
        try { Set-ClipboardText $originalClipboard; Write-Host '已恢复原剪贴板内容' -ForegroundColor DarkGray } catch { }
    }
    # 先请守护进程优雅退出（状态落盘），再兜底杀受管进程
    try { & $cx stop *> $null } catch { }
    Start-Sleep -Milliseconds 800
    if ($null -ne $daemonProc) {
        $still = Get-Process -Id $daemonProc.Id -ErrorAction SilentlyContinue
        if ($null -ne $still) { Stop-Process -Id $daemonProc.Id -Force -ErrorAction SilentlyContinue }
    }
    Get-Process cx -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue

    $logPath = Join-Path $env:CX_CACHE_DIR 'logs/daemon.log'
    if (Test-Path $logPath) {
        Write-Host "`n--- 守护进程日志（末尾 20 行）---" -ForegroundColor DarkGray
        Get-Content $logPath -Tail 20 | ForEach-Object { Write-Host "    $_" -ForegroundColor DarkGray }
    }
    else {
        Write-Host "`n（没有守护进程日志：说明还没到 logging::init 就退出了）" -ForegroundColor DarkGray
    }

    if ($KeepTemp) { Write-Host "临时目录保留在：$root" -ForegroundColor DarkGray }
    else { Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue }
}

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host '端到端验收全部通过' -ForegroundColor Green
    exit 0
}
else {
    Write-Host "端到端验收有 $($failures.Count) 项失败：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}
