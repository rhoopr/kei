# Offline qualification only. No login, sync, import or provider operations.
param(
    [Parameter(Mandatory=$true)][string]$CompanionRoot,
    [Parameter(Mandatory=$true)][string]$Exe,
    [Parameter(Mandatory=$true)][string]$FixtureRoot,
    [Parameter(Mandatory=$true)][string]$ProofPath
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Assert-Fixture {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw $Message }
}

function Assert-Refused {
    param([scriptblock]$Action, [string]$Message)
    $refused = $false
    try { & $Action | Out-Null } catch { $refused = $true }
    Assert-Fixture $refused $Message
}

Assert-Fixture ([IO.Path]::IsPathRooted($FixtureRoot)) 'Fixture root must be absolute'
Assert-Fixture (-not (Test-Path -LiteralPath $FixtureRoot)) 'Fixture root must be new'
foreach ($name in @('reporter-tools.ps1', 'qualify-helper.ps1')) {
    $tokens = $null
    $parseErrors = $null
    [System.Management.Automation.Language.Parser]::ParseFile((Join-Path $CompanionRoot $name), [ref]$tokens, [ref]$parseErrors) | Out-Null
    Assert-Fixture ($parseErrors.Count -eq 0) "PowerShell parse error in $name"
}
. (Join-Path $CompanionRoot 'reporter-tools.ps1')
$testRoot = Join-Path $FixtureRoot ('unicode space-' + [char]0x00e9)
$username = 'offline-' + [char]0x00e9 + '@example.invalid'
$case = New-KeiCase -TestRoot $testRoot -Name 'native-output' -Username $username
$config = Get-Content -LiteralPath $case.Config -Raw -Encoding UTF8
Assert-Fixture ($config.Contains($username) -and $config.Contains($case.Media)) 'UTF8 config did not preserve exact values'
$configBytes = [IO.File]::ReadAllBytes($case.Config)
Assert-Fixture (-not ($configBytes[0] -eq 0xef -and $configBytes[1] -eq 0xbb -and $configBytes[2] -eq 0xbf)) 'Config unexpectedly has a BOM'
Assert-Refused { New-KeiCase -TestRoot $testRoot -Name 'native-output' -Username $username } 'Existing case was reused'
Assert-Refused { Set-KeiTestBinary -Exe $Exe -ExpectedSHA256 ('0' * 64) } 'Wrong executable hash was accepted'
$exeHash = (Get-FileHash -LiteralPath $Exe -Algorithm SHA256).Hash
Set-KeiTestBinary -Exe $Exe -ExpectedSHA256 $exeHash

# Clap handles only these top-level version/invalid-option arguments before
# config loading or authentication. The fixture account has no credentials.
Invoke-KeiCase -Case $case -Exe $Exe -RunLabel 'version' -KeiArgs @('--version') | Out-Null
Assert-Fixture ((Get-Content (Join-Path $case.Logs 'version.exit-code.txt') -Raw).Trim() -eq '0') 'Version exit evidence is wrong'
Assert-Fixture ((Get-Content (Join-Path $case.Logs 'version.log') -Raw) -match 'kei 0.24.2-dev') 'Version output missing'
Invoke-KeiCase -Case $case -Exe $Exe -RunLabel 'parse-failure' -KeiArgs @('--reporter-fixture-invalid') -AllowFailure | Out-Null
Assert-Fixture ((Get-Content (Join-Path $case.Logs 'parse-failure.exit-code.txt') -Raw).Trim() -eq '2') 'Native error exit code was lost'
Assert-Fixture ((Get-Content (Join-Path $case.Logs 'parse-failure.log') -Raw) -match 'reporter-fixture-invalid') 'Native stderr was lost'
Assert-Refused { Invoke-KeiCase -Case $case -Exe $Exe -RunLabel 'parse-stop' -KeiArgs @('--reporter-fixture-invalid') } 'Nonzero exit was silently accepted'
Assert-Refused { Invoke-KeiCase -Case $case -Exe $Exe -RunLabel 'version' -KeiArgs @('--version') } 'Existing log was overwritten'

$originalConfig = [IO.File]::ReadAllText($case.Config)
try {
    [IO.File]::WriteAllText($case.Config, $originalConfig.Replace("data_dir = '$($case.Data)'", "data_dir = 'C:\wrong-fixture-path'"), [Text.UTF8Encoding]::new($false))
    Assert-Refused { Invoke-KeiCase -Case $case -Exe $Exe -RunLabel 'wrong-path' -KeiArgs @('--version') } 'Changed state path was accepted'
} finally {
    [IO.File]::WriteAllText($case.Config, $originalConfig, [Text.UTF8Encoding]::new($false))
}

# A real failed CreateProcess must not inherit a prior native success code.
$invalidExe = Join-Path $FixtureRoot 'not-an-executable.exe'
[IO.File]::WriteAllText($invalidExe, 'offline invalid PE fixture')
Set-KeiTestBinary -Exe $invalidExe -ExpectedSHA256 (Get-FileHash -LiteralPath $invalidExe -Algorithm SHA256).Hash
$global:LASTEXITCODE = 0
$launchError = $null
try { Invoke-KeiCase -Case $case -Exe $invalidExe -RunLabel 'launch-failure' -KeiArgs @('--version') | Out-Null } catch { $launchError = $_ }
Assert-Fixture ($null -ne $launchError) 'Failed launch recorded success'
$nativeFailure = $false
$exception = $launchError.Exception
while ($null -ne $exception) {
    if ($exception -is [System.ComponentModel.Win32Exception] -or
        $exception -is [System.Management.Automation.ApplicationFailedException]) { $nativeFailure = $true }
    $exception = $exception.InnerException
}
# Startup may throw before Tee starts, or arrive as redirected stderr followed
# by the helper's missing-exit refusal. Neither path must invent a success log.
$missingExit = $launchError.ToString().Contains('No native exit code; executable did not complete.')
$failedExit = $null -ne $global:LASTEXITCODE -and $global:LASTEXITCODE -ne 0
Assert-Fixture ($nativeFailure -or $missingExit -or $failedExit) 'Launch control did not reach native failure handling'
$launchExit = Join-Path $case.Logs 'launch-failure.exit-code.txt'
if (Test-Path -LiteralPath $launchExit) {
    Assert-Fixture ((Get-Content $launchExit -Raw).Trim() -ne '0') 'Failed launch wrote a stale zero exit code'
}
Set-KeiTestBinary -Exe $Exe -ExpectedSHA256 $exeHash

# Force log output failure using an ordinary file where a directory is needed.
$blockedLogs = Join-Path $FixtureRoot 'not-a-log-directory'
[IO.File]::WriteAllText($blockedLogs, 'offline log fixture')
$savedLogs = $case.Logs
try {
    $case.Logs = $blockedLogs
    Assert-Fixture (-not (Test-Path -LiteralPath (Join-Path $blockedLogs 'log-failure.log'))) 'Log failure fixture did not pass the nonreplacement guard'
    Assert-Refused { Invoke-KeiCase -Case $case -Exe $Exe -RunLabel 'log-failure' -KeiArgs @('--version') } 'Log write failure was silently accepted'
} finally { $case.Logs = $savedLogs }

# A locked late file must refuse the whole inventory before emitting any rows.
$first = Join-Path $case.Media 'a.bin'
$last = Join-Path $case.Media 'z.bin'
[IO.File]::WriteAllBytes($first, [byte[]](1,2,3))
[IO.File]::WriteAllBytes($last, [byte[]](4,5))
$inventory = @(Get-KeiMediaInventory -Directory $case.Media)
Assert-Fixture ($inventory.Count -eq 2 -and $inventory[0].Path -eq 'a.bin' -and $inventory[0].Bytes -eq 3) 'Complete inventory is wrong'
Assert-Fixture ($inventory[0].SHA256 -eq (Get-FileHash -LiteralPath $first -Algorithm SHA256).Hash.ToLowerInvariant()) 'Inventory hash is wrong'
$emitted = [Collections.Generic.List[object]]::new()
$lock = [IO.File]::Open($last, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
try {
    $refused = $false
    try { Get-KeiMediaInventory -Directory $case.Media | ForEach-Object { $emitted.Add($_) } } catch { $refused = $true }
    Assert-Fixture ($refused -and $emitted.Count -eq 0) 'Unreadable file yielded partial inventory proof'
} finally { $lock.Dispose() }

Assert-Fixture (@(Get-ChildItem -LiteralPath $case.Data -Force).Count -eq 0) 'Offline qualification created account data'
$proof = [ordered]@{
    result = 'passed'
    powershell = $PSVersionTable.PSVersion.ToString()
    edition = $PSVersionTable.PSEdition
    executable_sha256 = $exeHash.ToLowerInvariant()
    provider_operations = 0
    checks = @('AST', 'UTF8 paths/account/config', 'fresh case', 'hash guard', 'native version/exit/stderr', 'nonzero refusal', 'log nonreplacement', 'state path guard', 'failed launch/stale exit', 'log write failure', 'complete inventory', 'locked-file partial-output refusal', 'no account data')
}
$proof | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $ProofPath -Encoding UTF8
# Expected negative native controls must not become the host process exit code.
exit 0
