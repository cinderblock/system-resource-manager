# Build and install to ~/.srm/bin, plus the two files that live under ~/.claude.
#
# Safe to run while the broker is in use. Windows won't overwrite a running executable
# but will rename one, so the old daemon moves aside and keeps serving until it's told
# to stop. Running work survives that and rejoins the new daemon by itself.
#
# Builds go through the machine's budget like everything else.

param(
    # Reuse what's in target/release.
    [switch] $NoBuild
)

$ErrorActionPreference = 'Stop'

$repo = Split-Path $PSScriptRoot -Parent
$srm = Join-Path $HOME '.srm'
$bin = Join-Path $srm 'bin'
$claude = Join-Path $HOME '.claude'
$legacy = Join-Path $srm 'legacy'
$broker = Join-Path $claude 'bin\cpu-slots.mjs'
$skill = Join-Path $claude 'skills\compute-budget\SKILL.md'

function Budgeted([string[]] $Command) {
    # The installed broker, whichever generation that is. This repo's copy would find no
    # srmctl on a first install and run unbudgeted.
    & node $broker run --slots 3 --low --label 'srm install' -- @Command
    if ($LASTEXITCODE -ne 0) { throw "failed: $Command" }
}

if (-not $NoBuild) {
    Push-Location (Join-Path $repo 'app')
    try {
        & bun install --frozen-lockfile
        if ($LASTEXITCODE -ne 0) { throw 'bun install failed' }
        & bun run build
        if ($LASTEXITCODE -ne 0) { throw 'frontend build failed' }
    } finally {
        Pop-Location
    }
    Push-Location $repo
    try {
        # The custom-protocol feature is what `tauri build` adds: it makes the app serve
        # its embedded frontend instead of the dev server's URL.
        Budgeted @('cargo', 'build', '--release', '-j', '3', '-p', 'srmd', '-p', 'srmctl', '-p', 'srm-tray', '--features', 'srm-tray/custom-protocol')
    } finally {
        Pop-Location
    }
}

New-Item -ItemType Directory -Force $bin, $legacy | Out-Null

# Anything left from an earlier install can go now that nothing is running it.
Get-ChildItem $bin -Filter '*.old-*' -ErrorAction SilentlyContinue |
    ForEach-Object { Remove-Item $_.FullName -Force -ErrorAction SilentlyContinue }

foreach ($name in 'srmd.exe', 'srmctl.exe', 'srm-tray.exe') {
    $from = Join-Path $repo "target\release\$name"
    $to = Join-Path $bin $name
    if (-not (Test-Path $from)) { throw "$from is missing" }
    if (Test-Path $to) {
        Rename-Item $to "$name.old-$PID" -Force
    }
    Copy-Item $from $to
}

# Only ever created, so a machine's own tuning survives a reinstall.
$config = Join-Path $srm 'config.toml'
if (-not (Test-Path $config)) {
    $old = Join-Path $claude 'compute\config.json'
    $slots = if (Test-Path $old) { (Get-Content $old -Raw | ConvertFrom-Json).slots } else { $null }
    if ($slots) {
        Set-Content $config "[resources.cpu]`ncapacity = $slots`n" -NoNewline
    }
}

# Keep what's being replaced: these live outside any repo.
foreach ($file in $broker, $skill) {
    $backup = Join-Path $legacy (Split-Path $file -Leaf)
    if ((Test-Path $file) -and -not (Test-Path $backup)) { Copy-Item $file $backup }
}
New-Item -ItemType Directory -Force (Split-Path $broker), (Split-Path $skill) | Out-Null
Copy-Item (Join-Path $repo 'clients\cpu-slots.mjs') $broker
Copy-Item (Join-Path $repo 'skills\compute-budget\SKILL.md') $skill

$ctl = Join-Path $bin 'srmctl.exe'
& $ctl daemon stop | Out-Null
& $ctl daemon start
& $ctl status
