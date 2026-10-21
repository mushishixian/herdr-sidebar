#Requires -Version 5.1
<#
install-herdr-ide.ps1 — Windows-side bootstrap for the herdr + Fresh IDE.

The workbench itself lives in WSL (Ubuntu): apt packages, symlinks into the repo and
the bash keymap scripts all stay there. This script provisions the Windows HOST and
then hands over to the real installer inside the distro — it is NOT a native-Windows
port.

  .\docs\install-herdr-ide.ps1                       host bits, then the WSL installer
  .\docs\install-herdr-ide.ps1 -DryRun               print every action, change nothing
  .\docs\install-herdr-ide.ps1 -Distro Ubuntu-24.04
  .\docs\install-herdr-ide.ps1 -RepoPath '~/code/herdr-sidebar'
  .\docs\install-herdr-ide.ps1 -NoTerminalSettings -NoFont -NoInstaller
  .\docs\install-herdr-ide.ps1 -AddToPath            also put herdr.cmd on the user PATH

What it does, in order:
  1. finds a WSL distro (or takes -Distro)
  2. winget-installs Windows Terminal and the Nerd Font (skipped: -NoFont)
  3. removes the two Windows Terminal chords that never reach herdr
     (Ctrl+Shift+A select-all, Ctrl+Tab next-tab) from settings.json, after a backup
     (skipped: -NoTerminalSettings; chords: -Chords)
  4. writes %LOCALAPPDATA%\Programs\herdr-ide\herdr.cmd (launches herdr in the distro)
  5. runs docs/install-herdr-ide.sh inside the distro — the same installer, verify gate
     included (-DryRun forwards --dry-run; -NoInstaller skips this step)

NOTE: this script has been reviewed but not executed on a Windows host by its author.
Run it with -DryRun first; every mutating step names the file or package it touches.
#>
[CmdletBinding()]
param(
    [string]$Distro = '',
    [string]$RepoPath = '',
    [switch]$DryRun,
    [switch]$NoTerminalSettings,
    [switch]$NoFont,
    [switch]$NoInstaller,
    [switch]$AddToPath,
    [string[]]$Chords = @('ctrl+shift+a', 'ctrl+tab')
)

$ErrorActionPreference = 'Stop'

$WingetTerminal = 'Microsoft.WindowsTerminal'
$WingetFont = 'DEVCOM.JetBrainsMonoNerdFont'
$FontName = 'JetBrainsMono Nerd Font'
$LauncherDir = Join-Path $env:LOCALAPPDATA 'Programs\herdr-ide'

function Write-Step { param([string]$Text) Write-Host ''; Write-Host "==> $Text" -ForegroundColor Cyan }
function Write-Info { param([string]$Text) Write-Host "    $Text" }
function Write-Ok { param([string]$Text) Write-Host "    [ok] $Text" -ForegroundColor Green }
function Write-Warn { param([string]$Text) Write-Host "    [!] $Text" -ForegroundColor Yellow }
function Write-Bad { param([string]$Text) Write-Host "    [x] $Text" -ForegroundColor Red }

# Runs a command, or only prints it under -DryRun.
function Invoke-OrShow {
    param([string]$Exe, [string[]]$Arguments)
    $line = "$Exe " + ($Arguments -join ' ')
    if ($DryRun) { Write-Host "    would run: $line"; return 0 }
    Write-Host "    $line"
    & $Exe @Arguments
    return $LASTEXITCODE
}

# ── 1. WSL distro ──────────────────────────────────────────────────────────────
function Get-WslDistros {
    if (-not (Get-Command wsl.exe -ErrorAction SilentlyContinue)) { return @() }
    # wsl.exe writes UTF-16: strip the NULs PowerShell keeps in the captured strings.
    $raw = & wsl.exe -l -q 2>$null
    if ($LASTEXITCODE -ne 0) { return @() }
    return @($raw | ForEach-Object { ($_ -replace "`0", '').Trim() } | Where-Object { $_ })
}

# ── 2. winget packages ────────────────────────────────────────────────────────
function Test-WingetPackage {
    param([string]$Id)
    if (-not (Get-Command winget.exe -ErrorAction SilentlyContinue)) { return $false }
    $out = & winget.exe list --id $Id -e --accept-source-agreements 2>$null
    return (($out -join "`n") -match [regex]::Escape($Id))
}

function Install-WingetPackage {
    param([string]$Id, [string]$Label)
    if (Test-WingetPackage $Id) { Write-Ok "$Label present"; return }
    if (-not (Get-Command winget.exe -ErrorAction SilentlyContinue)) {
        Write-Warn "winget is missing: install $Label by hand"
        return
    }
    $code = Invoke-OrShow 'winget.exe' @('install', '--id', $Id, '-e',
        '--accept-package-agreements', '--accept-source-agreements', '--disable-interactivity')
    if ($DryRun) { return }
    if ($code -eq 0) { Write-Ok "$Label installed" } else { Write-Warn "$Label: winget exited $code (install by hand)" }
}

# ── 3. Windows Terminal keybindings (JSONC) ───────────────────────────────────
function Get-TerminalSettingsPath {
    $candidates = @(
        'Packages\Microsoft.WindowsTerminal_8wekyb3d8bbwe\LocalState\settings.json',
        'Packages\Microsoft.WindowsTerminalPreview_8wekyb3d8bbwe\LocalState\settings.json',
        'Packages\Microsoft.WindowsTerminalDev_8wekyb3d8bbwe\LocalState\settings.json',
        'Microsoft\Windows Terminal\settings.json',
        'Microsoft\Windows TerminalPreview\settings.json'
    )
    foreach ($rel in $candidates) {
        $p = Join-Path $env:LOCALAPPDATA $rel
        if (Test-Path -LiteralPath $p) { return $p }
    }
    return $null
}

# WT's settings.json is JSONC: // and /* */ comments, trailing commas. PS 5.1's
# ConvertFrom-Json accepts none of them, so strip them without touching string
# contents (a URL in there must survive).
function Remove-JsonComments {
    param([string]$Text)
    $sb = New-Object System.Text.StringBuilder
    $i = 0
    $inString = $false
    $escaped = $false
    while ($i -lt $Text.Length) {
        $c = $Text[$i]
        if ($inString) {
            [void]$sb.Append($c)
            if ($escaped) { $escaped = $false }
            elseif ($c -eq '\') { $escaped = $true }
            elseif ($c -eq '"') { $inString = $false }
            $i++
            continue
        }
        if ($c -eq '"') { $inString = $true; [void]$sb.Append($c); $i++; continue }
        if ($c -eq '/' -and ($i + 1) -lt $Text.Length) {
            $n = $Text[$i + 1]
            if ($n -eq '/') {
                while ($i -lt $Text.Length -and $Text[$i] -ne "`n") { $i++ }
                continue
            }
            if ($n -eq '*') {
                $i += 2
                while (($i + 1) -lt $Text.Length -and -not ($Text[$i] -eq '*' -and $Text[$i + 1] -eq '/')) { $i++ }
                $i += 2
                continue
            }
        }
        [void]$sb.Append($c)
        $i++
    }
    return ($sb.ToString() -replace ',\s*([}\]])', '$1')
}

# Windows Terminal keeps these chords for itself (select all / next tab), so they
# never reach herdr. Removing the bindings is user-config surgery: back up first,
# and say that the rewrite drops the file's comments.
function Update-TerminalKeybindings {
    param([string]$Path, [string[]]$Want)
    Write-Info $Path
    $raw = Get-Content -LiteralPath $Path -Raw
    try {
        $json = Remove-JsonComments $raw | ConvertFrom-Json
    }
    catch {
        Write-Bad "could not parse settings.json ($($_.Exception.Message)); leaving it alone"
        Write-Info 'remove the bindings for your chords by hand (Settings -> Actions)'
        return
    }
    if (-not $json.PSObject.Properties['keybindings']) { Write-Ok 'no keybindings section'; return }

    $kept = @()
    $removed = @()
    foreach ($kb in @($json.keybindings)) {
        $keys = @()
        if ($kb.PSObject.Properties['keys']) {
            if ($kb.keys -is [System.Array]) { $keys = @($kb.keys) } else { $keys = @([string]$kb.keys) }
        }
        $hit = $false
        foreach ($k in $keys) {
            foreach ($c in $Want) {
                if (([string]$k).Trim().ToLower() -eq $c) { $hit = $true }
            }
        }
        if ($hit) { $removed += (($keys | ForEach-Object { [string]$_ }) -join '+') } else { $kept += $kb }
    }

    if ($removed.Count -eq 0) { Write-Ok 'no conflicting bindings left to remove'; return }
    if ($DryRun) { Write-Host "    would remove: $($removed -join ', ')"; return }

    $backup = "$Path.herdr-ide.bak-" + (Get-Date -Format 'yyyyMMdd-HHmmss')
    Copy-Item -LiteralPath $Path -Destination $backup -Force
    $json.keybindings = $kept
    [IO.File]::WriteAllText($Path, ($json | ConvertTo-Json -Depth 100), (New-Object System.Text.UTF8Encoding($false)))
    Write-Ok "removed $($removed -join ', ')"
    Write-Warn "settings.json was rewritten as plain JSON (its comments are gone); backup: $backup"
}

# ── 4. herdr launcher ─────────────────────────────────────────────────────────
function Write-HerdrLauncher {
    param([string]$DistroName)
    $path = Join-Path $LauncherDir 'herdr.cmd'
    $body = "@echo off`r`nrem herdr + Fresh IDE inside WSL, written by docs/install-herdr-ide.ps1`r`nwsl.exe -d $DistroName -- bash -lc `"cd ~ && exec herdr`"`r`n"
    if ($DryRun) { Write-Host "    would write $path"; return }
    New-Item -ItemType Directory -Force -Path $LauncherDir | Out-Null
    [IO.File]::WriteAllText($path, $body, (New-Object System.Text.ASCIIEncoding))
    Write-Ok $path
    if ($AddToPath) {
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        if ($userPath -and ($userPath -split ';') -contains $LauncherDir) {
            Write-Ok 'launcher directory already on the user PATH'
        }
        else {
            $newPath = if ($userPath) { "$userPath;$LauncherDir" } else { $LauncherDir }
            [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
            Write-Ok "added $LauncherDir to the user PATH (new terminals see it)"
        }
    }
    else {
        Write-Info "add $LauncherDir to PATH (-AddToPath) to run herdr from anywhere"
    }
}

# ── 5. the WSL path of this checkout ──────────────────────────────────────────
function Resolve-WslRepoPath {
    param([string]$DistroName)
    if ($RepoPath) { return $RepoPath }
    $winRepo = Split-Path -Parent $PSScriptRoot    # docs\ -> repo root
    $converted = & wsl.exe -d $DistroName -e wslpath -a -u $winRepo 2>$null
    if ($LASTEXITCODE -eq 0 -and $converted) { return ([string]$converted).Trim() }
    return $null
}

# ── main ──────────────────────────────────────────────────────────────────────
Write-Host 'herdr + Fresh IDE: Windows host setup (the workbench itself lives in WSL)'
if ($DryRun) { Write-Warn 'dry run: nothing is installed, written or launched' }

Write-Step 'WSL distro'
$distros = Get-WslDistros
if (-not $distros -or $distros.Count -eq 0) {
    Write-Bad 'no WSL distro found'
    Write-Info 'install one first:  wsl --install -d Ubuntu   (then reopen this shell)'
    exit 1
}
if (-not $Distro) {
    $Distro = $distros[0]
    Write-Info "using $Distro (override with -Distro)"
}
if (-not ($distros -contains $Distro)) {
    Write-Bad "$Distro is not installed (have: $($distros -join ', '))"
    exit 1
}
Write-Ok "$Distro"

Write-Step 'Windows Terminal'
Install-WingetPackage $WingetTerminal 'Windows Terminal'

if (-not $NoFont) {
    Write-Step 'Nerd Font'
    Install-WingetPackage $WingetFont $FontName
    Write-Info "select '$FontName' in the Windows Terminal profile (the sidebar also probes for it)"
}

if (-not $NoTerminalSettings) {
    Write-Step 'Windows Terminal keybindings'
    $settingsPath = Get-TerminalSettingsPath
    if ($settingsPath) {
        Update-TerminalKeybindings -Path $settingsPath -Want $Chords
    }
    else {
        Write-Warn 'no Windows Terminal settings.json found (store or unpackaged install?)'
        Write-Info "unbind $($Chords -join ' and ') by hand (Settings -> Actions)"
    }
}

Write-Step 'herdr launcher'
Write-HerdrLauncher $Distro

if (-not $NoInstaller) {
    Write-Step 'installer inside WSL'
    $repo = Resolve-WslRepoPath $Distro
    if (-not $repo) {
        Write-Bad 'could not translate this checkout to a WSL path'
        Write-Info "pass it explicitly:  -RepoPath '~/code/herdr-sidebar'"
        exit 1
    }
    Write-Info "repo: $repo"
    $script = "cd '$repo' && docs/install-herdr-ide.sh"
    if ($DryRun) { $script += ' --dry-run' }
    $code = Invoke-OrShow 'wsl.exe' @('-d', $Distro, '--', 'bash', '-lc', $script)
    if (-not $DryRun -and $code -ne 0) {
        Write-Bad "the WSL installer exited $code (see its output above)"
        exit $code
    }
}

Write-Step 'next steps'
Write-Info 'restart Windows Terminal (the font needs a new window; keybindings apply live)'
Write-Info "start the IDE:  $LauncherDir\herdr.cmd   (or: wsl -d $Distro -- bash -lc 'cd ~ && exec herdr')"
Write-Info 'in a project pane: ctrl+b i for the IDE layout, ctrl+b ? for every key'
Write-Info 'docs: docs/herdr-ide.md'
