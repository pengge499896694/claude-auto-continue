param(
    [switch]$Remove
)

$ErrorActionPreference = 'Stop'
$appDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$launcher = Join-Path $appDir '启动监听器.cmd'
$startup = [Environment]::GetFolderPath('Startup')
$shortcutPath = Join-Path $startup 'Claude Auto Continue.lnk'

if ($Remove) {
    if (Test-Path $shortcutPath) { Remove-Item -LiteralPath $shortcutPath -Force }
    Write-Host "已取消开机启动：$shortcutPath"
    exit 0
}

$shell = New-Object -ComObject WScript.Shell
$shortcut = $shell.CreateShortcut($shortcutPath)
$shortcut.TargetPath = $launcher
$shortcut.WorkingDirectory = $appDir
$shortcut.Description = 'Claude Code 自动续跑监听器'
$shortcut.Save()
Write-Host "已添加开机启动：$shortcutPath"
