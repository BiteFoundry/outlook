<#
.SYNOPSIS
    Idempotently installs `teams`, `outlook`, and `onenote` terminal launchers on any Windows PC.

.DESCRIPTION
    1. Ensures <RepoRoot>\target\release\teams.exe exists (downloads the prebuilt
       release binary from GitHub `shinick-dev/teams` via `gh` when missing, so
       Rust/Cargo is NOT required on target PCs).
    2. Registers/updates `# >>> teams launcher >>>` (`function teams` + `function outlook` + `function onenote`)
       in PowerShell $PROFILE. All three commands share the single `teams.exe` binary,
       with `outlook` and `onenote` setting `TEAMS_PERSONA` for the invocation.
    3. If $HOME\Tools\bin exists, also writes `teams.cmd`, `outlook.cmd`, and `onenote.cmd`
       there so already-open shells find all three commands.

.EXAMPLE
    pwsh -File .\scripts\install-profile.ps1
#>
[CmdletBinding(SupportsShouldProcess)]
param(
    [string]$ProfilePath = $PROFILE,
    [string]$RepoRoot = (Split-Path -Parent $PSScriptRoot)
)

$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrWhiteSpace($ProfilePath)) {
    throw 'ProfilePath is empty. Pass -ProfilePath explicitly or run inside a PowerShell host where $PROFILE is defined.'
}

$resolvedRoot = if (Test-Path -LiteralPath $RepoRoot) {
    (Resolve-Path -LiteralPath $RepoRoot).Path
} else {
    $RepoRoot
}

$releaseDir = Join-Path $resolvedRoot 'target\release'
$releaseExe = Join-Path $releaseDir 'teams.exe'

if (-not (Test-Path -LiteralPath $releaseExe)) {
    $gh = Get-Command gh -ErrorAction SilentlyContinue
    if ($gh) {
        Write-Output "[teams] Downloading prebuilt teams.exe from shinick-dev/teams GitHub Release..."
        New-Item -ItemType Directory -Force -Path $releaseDir | Out-Null
        & $gh.Source release download -R 'shinick-dev/teams' --pattern 'teams.exe' --dir $releaseDir --clobber
    }
}

$escapedRoot = $resolvedRoot.Replace("'", "''")
$startMarker = '# >>> teams launcher >>>'
$endMarker = '# <<< teams launcher <<<'

$block = @"
$startMarker
# Opens the unified Microsoft Teams + Outlook + OneNote terminal client (CLI/TUI) from anywhere.
function teams {
    `$root = '$escapedRoot'
    `$exe = Join-Path `$root 'target\release\teams.exe'
    if (-not (Test-Path -LiteralPath `$exe)) {
        if (Get-Command gh -ErrorAction SilentlyContinue) {
            `$relDir = Split-Path -Parent `$exe
            New-Item -ItemType Directory -Force -Path `$relDir | Out-Null
            gh release download -R 'shinick-dev/teams' --pattern 'teams.exe' --dir `$relDir --clobber 2>`$null
        }
    }
    if (-not (Test-Path -LiteralPath `$exe)) {
        cargo build --release --manifest-path (Join-Path `$root 'Cargo.toml')
        if (`$LASTEXITCODE -ne 0) { return }
    }
    & `$exe @args
}

# Opens the unified binary in Outlook persona (Mail, Calendar, Contacts, To Do).
function outlook {
    `$prevPersona = `$env:TEAMS_PERSONA
    `$env:TEAMS_PERSONA = 'outlook'
    try {
        teams @args
    } finally {
        if (`$null -eq `$prevPersona) {
            Remove-Item Env:TEAMS_PERSONA -ErrorAction SilentlyContinue
        } else {
            `$env:TEAMS_PERSONA = `$prevPersona
        }
    }
}

# Opens the unified binary in OneNote persona (Personal + Team/Group Shared Notebooks, Sections, Pages).
function onenote {
    `$prevPersona = `$env:TEAMS_PERSONA
    `$env:TEAMS_PERSONA = 'onenote'
    try {
        teams @args
    } finally {
        if (`$null -eq `$prevPersona) {
            Remove-Item Env:TEAMS_PERSONA -ErrorAction SilentlyContinue
        } else {
            `$env:TEAMS_PERSONA = `$prevPersona
        }
    }
}
$endMarker
"@.TrimEnd("`r", "`n")

$parentDir = Split-Path -Parent $ProfilePath
if ($parentDir -and -not (Test-Path -LiteralPath $parentDir)) {
    if ($PSCmdlet.ShouldProcess($parentDir, 'Create PowerShell profile parent directory')) {
        New-Item -ItemType Directory -Force -Path $parentDir | Out-Null
    }
}

$existing = ''
if (Test-Path -LiteralPath $ProfilePath) {
    $existing = Get-Content -LiteralPath $ProfilePath -Raw -Encoding UTF8
    if ($null -eq $existing) { $existing = '' }
}

$pattern = '(?ms)^# >>> teams launcher >>>\r?\n.*?^# <<< teams launcher <<<\r?\n?'
if ([regex]::IsMatch($existing, $pattern)) {
    $updated = [regex]::Replace($existing, $pattern, { param($m) $block + [Environment]::NewLine }, 1)
    $action = 'Update teams launcher block'
} elseif ([string]::IsNullOrWhiteSpace($existing)) {
    $updated = $block + [Environment]::NewLine
    $action = 'Create profile with teams launcher block'
} else {
    $trimmed = $existing.TrimEnd("`r", "`n")
    $nl = [Environment]::NewLine
    $updated = $trimmed + $nl + $nl + $block + $nl
    $action = 'Append teams launcher block'
}

if ($updated -ne $existing) {
    if ($PSCmdlet.ShouldProcess($ProfilePath, $action)) {
        $utf8NoBom = [System.Text.UTF8Encoding]::new($false)
        [System.IO.File]::WriteAllText($ProfilePath, $updated, $utf8NoBom)
        Write-Output "$action in $ProfilePath."
    }
} else {
    Write-Output "teams launcher in $ProfilePath is already up to date."
}

$toolsBin = Join-Path $HOME 'Tools\bin'
if (Test-Path -LiteralPath $toolsBin) {
    $teamsShimPath = Join-Path $toolsBin 'teams.cmd'
    $teamsShimContent = "@echo off`r`nchcp 65001 >nul`r`n`"$resolvedRoot\teams.cmd`" %*`r`n"
    if ($PSCmdlet.ShouldProcess($teamsShimPath, 'Write PATH shim teams.cmd')) {
        [System.IO.File]::WriteAllText($teamsShimPath, $teamsShimContent, [System.Text.Encoding]::ASCII)
        Write-Output "Updated PATH shim: $teamsShimPath"
    }

    $outlookShimPath = Join-Path $toolsBin 'outlook.cmd'
    $outlookShimContent = "@echo off`r`nchcp 65001 >nul`r`n`"$resolvedRoot\outlook.cmd`" %*`r`n"
    if ($PSCmdlet.ShouldProcess($outlookShimPath, 'Write PATH shim outlook.cmd')) {
        [System.IO.File]::WriteAllText($outlookShimPath, $outlookShimContent, [System.Text.Encoding]::ASCII)
        Write-Output "Updated PATH shim: $outlookShimPath"
    }

    $onenoteShimPath = Join-Path $toolsBin 'onenote.cmd'
    $onenoteShimContent = "@echo off`r`nchcp 65001 >nul`r`nsetlocal`r`nset `"TEAMS_PERSONA=onenote`"`r`ncall `"$resolvedRoot\teams.cmd`" %*`r`nexit /b %errorlevel%`r`n"
    if ($PSCmdlet.ShouldProcess($onenoteShimPath, 'Write PATH shim onenote.cmd')) {
        [System.IO.File]::WriteAllText($onenoteShimPath, $onenoteShimContent, [System.Text.Encoding]::ASCII)
        Write-Output "Updated PATH shim: $onenoteShimPath"
    }
}
