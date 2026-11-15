<#
.SYNOPSIS
    Idempotently registers or updates the `teams` launcher function in PowerShell $PROFILE.

.DESCRIPTION
    Ensures the directory for $PROFILE exists and inserts (or updates in place) the
    `# >>> teams launcher >>> ... # <<< teams launcher <<<` block pointing to
    <RepoRoot>\target\release\teams.exe.

.EXAMPLE
    pwsh -File .\scripts\install-profile.ps1
#>
[CmdletBinding(SupportsShouldProcess)]
param(
    [string]$ProfilePath = $PROFILE,
    [string]$RepoRoot = 'C:\Users\user\source\teams'
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

$escapedRoot = $resolvedRoot.Replace("'", "''")
$startMarker = '# >>> teams launcher >>>'
$endMarker = '# <<< teams launcher <<<'

$block = @"
$startMarker
# Opens the Microsoft Teams terminal client (CLI/TUI) from anywhere.
function teams {
    `$root = '$escapedRoot'
    `$exe = Join-Path `$root 'target\release\teams.exe'
    if (-not (Test-Path -LiteralPath `$exe)) {
        cargo build --release --manifest-path (Join-Path `$root 'Cargo.toml')
        if (`$LASTEXITCODE -ne 0) { return }
    }
    & `$exe @args
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

if ($updated -eq $existing) {
    Write-Output "teams launcher in $ProfilePath is already up to date."
    return
}

if ($PSCmdlet.ShouldProcess($ProfilePath, $action)) {
    $utf8NoBom = [System.Text.UTF8Encoding]::new($false)
    [System.IO.File]::WriteAllText($ProfilePath, $updated, $utf8NoBom)
    Write-Output "$action in $ProfilePath."
}
