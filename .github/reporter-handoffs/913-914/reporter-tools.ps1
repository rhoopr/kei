# LOCAL DRAFT: dot-source only after the build and reporter plan are approved.
# No kei command, media copy, login, or cleanup runs when this file is loaded.
Set-StrictMode -Version Latest
$script:QualifiedKeiBinary = $null

function Set-KeiTestBinary {
    param(
        [Parameter(Mandatory=$true)][string]$Exe,
        [Parameter(Mandatory=$true)][string]$ExpectedSHA256
    )
    if (-not [IO.Path]::IsPathRooted($Exe) -or $ExpectedSHA256 -notmatch '^[a-fA-F0-9]{64}$') {
        throw 'Supply an absolute executable path and the qualified full SHA-256'
    }
    if (-not (Test-Path -LiteralPath $Exe -PathType Leaf -ErrorAction Stop)) { throw 'Executable not found' }
    $actual = (Get-FileHash -LiteralPath $Exe -Algorithm SHA256 -ErrorAction Stop).Hash
    if ($actual -ne $ExpectedSHA256) { throw 'Executable hash differs from the qualified build' }
    $script:QualifiedKeiBinary = [pscustomobject]@{
        Exe = [IO.Path]::GetFullPath($Exe)
        SHA256 = $ExpectedSHA256
    }
}

function New-KeiCase {
    param(
        [Parameter(Mandatory=$true)][string]$TestRoot,
        [Parameter(Mandatory=$true)][string]$Name,
        [Parameter(Mandatory=$true)][string]$Username,
        [ValidateSet('com','cn')][string]$Realm = 'com',
        [ValidateSet('as-is','prefer-raw','prefer-jpeg')][string]$RawPolicy = 'as-is'
    )
    if ($Name -notmatch '^[a-z0-9-]+$') { throw 'Use a simple unique case name' }
    if (-not [IO.Path]::IsPathRooted($TestRoot)) { throw 'TestRoot must be absolute' }
    if ($TestRoot.Contains("'") -or $Username.Contains("'") -or $Username -match '[\r\n]') {
        throw 'Use a manually reviewed TOML config for values containing quotes/newlines'
    }
    $root = Join-Path ([IO.Path]::GetFullPath($TestRoot)) $Name
    if (Test-Path -LiteralPath $root) { throw 'Case already exists; choose a new name' }
    $case = [pscustomobject]@{
        Root = $root
        Config = Join-Path $root 'config.toml'
        Data = Join-Path $root 'data'
        Media = Join-Path $root 'media'
        Logs = Join-Path $root 'logs'
        Temp = Join-Path $root 'temp'
    }
    New-Item -ItemType Directory -Path $case.Data, $case.Media, $case.Logs, $case.Temp -Force -ErrorAction Stop | Out-Null
    # Metadata settings are deliberately explicit to permit byte-for-byte checks.
    # Adapt filters/photo policies and all three folder templates to the source
    # archive before the #913 lane. Keep the absolute writable paths unchanged.
    $toml = @"
data_dir = '$($case.Data)'
log_level = 'info'
[auth]
username = '$Username'
domain = '$Realm'
[download]
directory = '$($case.Media)'
folder_structure = '%Y/%m'
threads = 10
temp_suffix = '.kei-tmp'
[filters]
libraries = ['primary']
albums = ['none']
smart_folders = ['none']
unfiled = true
[photos]
file_match_policy = 'name-id7'
resolution = 'original'
live_resolution = 'original'
live_photo_mode = 'both'
live_photo_mov_filename_policy = 'suffix'
edited = false
alternative = false
raw_policy = '$RawPolicy'
[metadata]
set_exif_datetime = false
set_exif_rating = false
set_exif_gps = false
set_exif_description = false
embed_xmp = false
xmp_sidecar = false
"@
    [IO.File]::WriteAllText($case.Config, $toml, [Text.UTF8Encoding]::new($false))
    return $case
}

function Invoke-KeiCase {
    param(
        [Parameter(Mandatory=$true)]$Case,
        [Parameter(Mandatory=$true)][string]$Exe,
        [Parameter(Mandatory=$true)][string]$RunLabel,
        [Parameter(Mandatory=$true)][string[]]$KeiArgs,
        [switch]$AllowFailure
    )
    if ($RunLabel -notmatch '^[a-z0-9-]+$') { throw 'Use a simple unique run label' }
    if (-not $script:QualifiedKeiBinary -or -not [IO.Path]::IsPathRooted($Exe) -or
        [IO.Path]::GetFullPath($Exe) -ne $script:QualifiedKeiBinary.Exe) {
        throw 'Run Set-KeiTestBinary with the qualified executable/hash first'
    }
    if ((Get-FileHash -LiteralPath $Exe -Algorithm SHA256 -ErrorAction Stop).Hash -ne $script:QualifiedKeiBinary.SHA256) {
        throw 'Qualified executable changed'
    }
    $log = Join-Path $Case.Logs ($RunLabel + '.log')
    if (Test-Path -LiteralPath $log) { throw 'Log already exists; choose a new label' }
    $configText = Get-Content -LiteralPath $Case.Config -Raw -Encoding UTF8 -ErrorAction Stop
    foreach ($expected in @("data_dir = '$($Case.Data)'", "directory = '$($Case.Media)'")) {
        if (-not $configText.Contains($expected)) { throw 'Reviewed scratch paths changed' }
    }
    $env:KEI_DATA_DIR = $Case.Data
    $env:TEMP = $Case.Temp
    $env:TMP = $Case.Temp
    $previousPreference = $ErrorActionPreference
    $global:LASTEXITCODE = $null
    $code = $null
    try {
        # Windows PowerShell 5.1 represents redirected native stderr as error
        # records. Continue long enough to preserve stderr and the real exit code.
        $ErrorActionPreference = 'Continue'
        & $Exe --config $Case.Config --log-level info @KeiArgs 2>&1 |
            ForEach-Object { $_.ToString() } | Tee-Object -FilePath $log -ErrorAction Stop
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previousPreference
    }
    if ($null -eq $code) { throw "No native exit code; executable did not complete. Retain $log and stop." }
    $code | Set-Content -LiteralPath (Join-Path $Case.Logs ($RunLabel + '.exit-code.txt')) -ErrorAction Stop
    if ($code -ne 0 -and -not $AllowFailure) { throw "kei exited $code; retain $log and stop this lane" }
}

function Get-KeiMediaInventory {
    param([Parameter(Mandatory=$true)][string]$Directory)
    $root = [IO.Path]::GetFullPath($Directory).TrimEnd('\')
    if ((Get-Item -LiteralPath $root -ErrorAction Stop).Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw 'Media root is a reparse point'
    }
    $items = @(Get-ChildItem -LiteralPath $root -Recurse -Force -ErrorAction Stop)
    if ($items | Where-Object { $_.Attributes -band [IO.FileAttributes]::ReparsePoint }) {
        throw 'Tree contains a reparse point; stop and use independent ordinary copies'
    }
    # Buffer the complete inventory. A failed traversal/hash returns no successful
    # partial rows for a caller to mistake for a complete before/after proof.
    $inventory = @(foreach ($file in ($items | Where-Object { -not $_.PSIsContainer } | Sort-Object FullName)) {
        [pscustomobject]@{
            Path = $file.FullName.Substring($root.Length + 1)
            Bytes = $file.Length
            SHA256 = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256 -ErrorAction Stop).Hash.ToLowerInvariant()
        }
    })
    return $inventory
}
