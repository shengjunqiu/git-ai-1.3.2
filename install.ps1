$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# Native git-ai output is UTF-8. Windows PowerShell commonly inherits a legacy
# console code page, which otherwise renders symbols such as ✓ and ⚠ as mojibake.
try {
    $utf8Encoding = New-Object System.Text.UTF8Encoding($false)
    [Console]::InputEncoding = $utf8Encoding
    [Console]::OutputEncoding = $utf8Encoding
    $OutputEncoding = $utf8Encoding
} catch { }

function Write-ErrorAndExit {
    param(
        [Parameter(Mandatory = $true)][string]$Message
    )
    Write-Host "Error: $Message" -ForegroundColor Red
    # This installer is commonly invoked with `irm ... | iex`. Calling `exit`
    # here would terminate the user's PowerShell host and make prerequisite
    # failures look like a window crash. A terminating error stops the install
    # while returning control to the existing PowerShell session.
    throw [System.InvalidOperationException]::new($Message)
}

function Write-Success {
    param(
        [Parameter(Mandatory = $true)][string]$Message
    )
    Write-Host $Message -ForegroundColor Green
}

function Write-Warning {
    param(
        [Parameter(Mandatory = $true)][string]$Message
    )
    Write-Host $Message -ForegroundColor Yellow
}

function Normalize-PathString {
    param(
        [Parameter(Mandatory = $true)][string]$Path
    )

    try {
        return ([IO.Path]::GetFullPath($Path.Trim())).TrimEnd('\').ToLowerInvariant()
    } catch {
        return ($Path.Trim()).TrimEnd('\').ToLowerInvariant()
    }
}

function Test-FileAvailable {
    param(
        [Parameter(Mandatory = $true)][string]$Path
    )

    try {
        $stream = [System.IO.File]::Open($Path, 'Open', 'Write', 'None')
        $stream.Close()
        return $true
    } catch {
        return $false
    }
}

function Stop-GitAiBackgroundService {
    param(
        [Parameter(Mandatory = $true)][string]$GitAiExe,
        [Parameter(Mandatory = $false)][switch]$Hard
    )

    if (-not (Test-Path -LiteralPath $GitAiExe)) {
        return $false
    }

    $args = @('bg', 'shutdown')
    if ($Hard) {
        $args += '--hard'
    }

    try {
        & $GitAiExe @args *> $null
        return $LASTEXITCODE -eq 0
    } catch {
        return $false
    }
}

function Get-GitAiManagedProcesses {
    param(
        [Parameter(Mandatory = $true)][string]$InstallDir
    )

    $targetPaths = @(
        (Normalize-PathString (Join-Path $InstallDir 'git-ai.exe')),
        (Normalize-PathString (Join-Path $InstallDir 'git.exe'))
    )

    $processes = @(Get-CimInstance Win32_Process -ErrorAction SilentlyContinue | Where-Object {
            $_.ProcessId -ne $PID -and
            $_.ExecutablePath -and
            ($targetPaths -contains (Normalize-PathString $_.ExecutablePath))
        })

    return $processes
}

function Stop-GitAiManagedProcesses {
    param(
        [Parameter(Mandatory = $true)][string]$InstallDir
    )

    $processes = @(Get-GitAiManagedProcesses -InstallDir $InstallDir)
    if ($processes.Count -eq 0) {
        return $false
    }

    $pids = @($processes | Sort-Object ProcessId -Unique | Select-Object -ExpandProperty ProcessId)
    Write-Warning ("Stopping lingering git-ai processes: {0}" -f ($pids -join ', '))

    foreach ($processId in $pids) {
        try {
            Stop-Process -Id $processId -Force -ErrorAction Stop
        } catch { }
    }

    return $true
}

function Wait-ForFileAvailable {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$InstallDir,
        [Parameter(Mandatory = $false)][int]$MaxWaitSeconds = 300,
        [Parameter(Mandatory = $false)][int]$RetryIntervalSeconds = 5,
        [Parameter(Mandatory = $false)][int]$ForceKillAfterSeconds = 20
    )

    $elapsed = 0
    $gitAiExe = Join-Path $InstallDir 'git-ai.exe'

    [void](Stop-GitAiBackgroundService -GitAiExe $gitAiExe)

    while ($elapsed -lt $MaxWaitSeconds) {
        if (Test-FileAvailable -Path $Path) {
            return $true
        }

        if ($elapsed -ge $ForceKillAfterSeconds) {
            [void](Stop-GitAiBackgroundService -GitAiExe $gitAiExe -Hard)
            [void](Stop-GitAiManagedProcesses -InstallDir $InstallDir)
        }

        if (-not (Test-FileAvailable -Path $Path)) {
            if ($elapsed -eq 0) {
                Write-Host "Waiting for file to be available: $Path" -ForegroundColor Yellow
            }
            Start-Sleep -Seconds $RetryIntervalSeconds
            $elapsed += $RetryIntervalSeconds
        }
    }
    return $false
}

function Verify-Checksum {
    param(
        [Parameter(Mandatory = $true)][string]$File,
        [Parameter(Mandatory = $true)][string]$BinaryName
    )

    # Skip verification if no checksums are embedded
    if ($EmbeddedChecksums -eq '__CHECKSUMS_PLACEHOLDER__') {
        return
    }

    # Extract expected checksum for this binary
    $expected = $null
    $entries = $EmbeddedChecksums -split '\|'
    foreach ($entry in $entries) {
        if ($entry -match "^([0-9a-fA-F]+)\s+$([regex]::Escape($BinaryName))$") {
            $expected = $Matches[1]
            break
        }
    }

    if (-not $expected) {
        Write-ErrorAndExit "No checksum found for $BinaryName"
    }

    # Calculate actual checksum
    $hashCommand = Get-Command Get-FileHash -ErrorAction SilentlyContinue
    if ($null -ne $hashCommand) {
        $actual = (Get-FileHash -Path $File -Algorithm SHA256).Hash.ToLower()
    } else {
        $stream = [System.IO.File]::OpenRead($File)
        try {
            $sha256 = [System.Security.Cryptography.SHA256]::Create()
            $hashBytes = $sha256.ComputeHash($stream)
            $actual = ([System.BitConverter]::ToString($hashBytes)).Replace('-', '').ToLower()
        } finally {
            $stream.Dispose()
            if ($sha256) {
                $sha256.Dispose()
            }
        }
    }

    if ($expected -ne $actual) {
        Remove-Item -Force -ErrorAction SilentlyContinue $File
        Write-ErrorAndExit "Checksum verification failed for $BinaryName`nExpected: $expected`nActual:   $actual"
    }

    Write-Success "Checksum verified for $BinaryName"
}

# GitHub repository details
# Replaced during release builds with the actual repository (e.g., "git-ai-project/git-ai")
# When set to __REPO_PLACEHOLDER__, defaults to "git-ai-project/git-ai"
$Repo = '__REPO_PLACEHOLDER__'
if ($Repo -eq '__REPO_PLACEHOLDER__') {
    $Repo = 'git-ai-project/git-ai'
}

# Version placeholder - replaced during release builds with actual version (e.g., "v1.0.24")
# When set to __VERSION_PLACEHOLDER__, defaults to "latest"
$PinnedVersion = '__VERSION_PLACEHOLDER__'

# Embedded checksums - replaced during release builds with actual SHA256 checksums
# Format: "hash  filename|hash  filename|..." (pipe-separated)
# When set to __CHECKSUMS_PLACEHOLDER__, checksum verification is skipped
$EmbeddedChecksums = '__CHECKSUMS_PLACEHOLDER__'

# Enterprise release source placeholders. Enterprise release generation replaces
# both values; public GitHub release generation leaves them untouched.
$EnterpriseReleaseBaseUrl = '__ENTERPRISE_RELEASE_BASE_URL_PLACEHOLDER__'
$EnterpriseReleaseChannel = '__ENTERPRISE_RELEASE_CHANNEL_PLACEHOLDER__'

# Enterprise API endpoint. Every install and upgrade enforces this value,
# replacing any api_base_url previously saved by the user.
$EnterpriseApiBaseUrl = 'http://117.147.213.234:38080'

# Ensure TLS 1.2 for GitHub downloads on older PowerShell versions
try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
} catch { }

function Get-Architecture {
    try {
        $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
        switch ($arch) {
            'X64' { return 'x64' }
            'Arm64' { return 'arm64' }
            default { return $null }
        }
    } catch {
        $pa = $env:PROCESSOR_ARCHITECTURE
        if ($pa -match 'ARM64') { return 'arm64' }
        elseif ($pa -match '64') { return 'x64' }
        else { return $null }
    }
}

function Resolve-StdGitCandidate {
    param(
        [Parameter(Mandatory = $false)][AllowNull()][string]$Candidate
    )

    if ([string]::IsNullOrWhiteSpace($Candidate)) {
        return $null
    }

    try {
        if (-not (Test-Path -LiteralPath $Candidate -PathType Leaf)) {
            return $null
        }

        $fullPath = (Get-Item -LiteralPath $Candidate -ErrorAction Stop).FullName
        $blockedPaths = @(
            (Normalize-PathString (Join-Path $HOME '.git-ai\bin\git.exe')),
            (Normalize-PathString (Join-Path $HOME '.git-ai\bin\git-ai.exe'))
        )
        if ($blockedPaths -contains (Normalize-PathString $fullPath)) {
            return $null
        }
        if ([IO.Path]::GetFileName($fullPath) -ieq 'git-ai.exe') {
            return $null
        }

        & $fullPath --version *> $null
        if ($LASTEXITCODE -ne 0) {
            return $null
        }
        return $fullPath
    } catch {
        return $null
    }
}

function Get-StdGitPath {
    $candidates = New-Object System.Collections.Generic.List[string]

    # A previous installation records the real Git in git-og.cmd. Recover it
    # before inspecting PATH, where our own git.exe shim normally comes first.
    $gitOgShim = Join-Path $HOME '.git-ai\bin\git-og.cmd'
    if (Test-Path -LiteralPath $gitOgShim -PathType Leaf) {
        try {
            $gitOgContent = Get-Content -LiteralPath $gitOgShim -Raw -ErrorAction Stop
            $gitOgMatch = [regex]::Match($gitOgContent, '(?m)^\s*"([^"\r\n]+)"\s+%\*\s*$')
            if ($gitOgMatch.Success) {
                $candidates.Add($gitOgMatch.Groups[1].Value) | Out-Null
            }
        } catch { }
    }

    # Recover from a persisted configuration when available.
    try {
        $cfgPath = Join-Path $HOME '.git-ai\config.json'
        if (Test-Path -LiteralPath $cfgPath -PathType Leaf) {
            $cfg = Get-Content -LiteralPath $cfgPath -Raw | ConvertFrom-Json
            if ($cfg -and $cfg.git_path) {
                $candidates.Add([string]$cfg.git_path) | Out-Null
            }
        }
    } catch { }

    # Search every Git application on PATH. Do not stop when the first result
    # is %USERPROFILE%\.git-ai\bin\git.exe.
    $commands = @(Get-Command git.exe -All -CommandType Application -ErrorAction SilentlyContinue)
    foreach ($command in $commands) {
        if ($command.Path) {
            $candidates.Add([string]$command.Path) | Out-Null
        }
    }

    # PATH may be minimal under MDM or enterprise deployment tools.
    $commonRoots = @(
        $env:ProgramW6432,
        $env:ProgramFiles,
        ${env:ProgramFiles(x86)},
        $env:LOCALAPPDATA
    ) | Where-Object { -not [string]::IsNullOrWhiteSpace($_) } | Select-Object -Unique
    foreach ($root in $commonRoots) {
        if ($root -eq $env:LOCALAPPDATA) {
            $candidates.Add((Join-Path $root 'Programs\Git\cmd\git.exe')) | Out-Null
            $candidates.Add((Join-Path $root 'Programs\Git\bin\git.exe')) | Out-Null
        } else {
            $candidates.Add((Join-Path $root 'Git\cmd\git.exe')) | Out-Null
            $candidates.Add((Join-Path $root 'Git\bin\git.exe')) | Out-Null
        }
    }
    if (-not [string]::IsNullOrWhiteSpace($HOME)) {
        $candidates.Add((Join-Path $HOME 'scoop\apps\git\current\cmd\git.exe')) | Out-Null
    }
    if (-not [string]::IsNullOrWhiteSpace($env:ChocolateyInstall)) {
        $candidates.Add((Join-Path $env:ChocolateyInstall 'bin\git.exe')) | Out-Null
    }

    $seen = New-Object 'System.Collections.Generic.HashSet[string]'
    foreach ($candidate in $candidates) {
        $normalized = Normalize-PathString $candidate
        if (-not $seen.Add($normalized)) {
            continue
        }
        $gitPath = Resolve-StdGitCandidate -Candidate $candidate
        if ($gitPath) {
            return $gitPath
        }
    }

    $missingGitMessage = @'
Git for Windows is required but was not found.

Install it with Windows Package Manager:
  winget install --id Git.Git -e --source winget

Or download it from:
  https://git-scm.com/download/win

After Git is installed, open a new PowerShell window and run the git-ai installer again.
'@
    Write-ErrorAndExit $missingGitMessage
}

# Ensure $PathToAdd is inserted before any PATH entry that contains "git" (case-insensitive)
# Updates Machine (system) PATH; if not elevated, emits a prominent error with instructions
function Set-PathPrependBeforeGit {
    param(
        [Parameter(Mandatory = $true)][string]$PathToAdd
    )

    $sep = ';'

    function NormalizePath([string]$p) {
        try { return ([IO.Path]::GetFullPath($p.Trim())).TrimEnd('\\').ToLowerInvariant() }
        catch { return ($p.Trim()).TrimEnd('\\').ToLowerInvariant() }
    }

    $normalizedAdd = NormalizePath $PathToAdd

    # Helper to build new PATH string with PathToAdd inserted before first 'git' entry
    function BuildPathWithInsert([string]$existingPath, [string]$toInsert) {
        $entries = @()
        if ($existingPath) { $entries = ($existingPath -split $sep) | Where-Object { $_ -and $_.Trim() -ne '' } }

        # De-duplicate and remove any existing instance of $toInsert
        $list = New-Object System.Collections.Generic.List[string]
        $seen = New-Object 'System.Collections.Generic.HashSet[string]'
        foreach ($e in $entries) {
            $n = NormalizePath $e
            if (-not $seen.Contains($n) -and $n -ne $normalizedAdd) {
                $seen.Add($n) | Out-Null
                $list.Add($e) | Out-Null
            }
        }

        # Find first index that matches 'git' anywhere (case-insensitive)
        $insertIndex = 0
        for ($i = 0; $i -lt $list.Count; $i++) {
            if ($list[$i] -match '(?i)git') { $insertIndex = $i; break }
        }

        $list.Insert($insertIndex, $toInsert)
        return ($list -join $sep)
    }

    $userStatus = 'Skipped'
    try {
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        $newUserPath = BuildPathWithInsert -existingPath $userPath -toInsert $PathToAdd
        if ($newUserPath -ne $userPath) {
            [Environment]::SetEnvironmentVariable('Path', $newUserPath, 'User')
            $userStatus = 'Updated'
        } else {
            $userStatus = 'AlreadyPresent'
        }
    } catch {
        $userStatus = 'Error'
    }

    # Try to update Machine PATH
    $machineStatus = 'Skipped'
    try {
        $machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
        $newMachinePath = BuildPathWithInsert -existingPath $machinePath -toInsert $PathToAdd
        if ($newMachinePath -ne $machinePath) {
            [Environment]::SetEnvironmentVariable('Path', $newMachinePath, 'Machine')
            $machineStatus = 'Updated'
        } else {
            # Nothing changed at Machine scope; still treat as Machine for reporting
            $machineStatus = 'AlreadyPresent'
        }
    } catch {
        # A non-elevated per-user install is supported. Machine PATH is optional,
        # but users should verify precedence because Windows can place machine
        # Git entries before the user PATH when creating a new process.
        $machineStatus = 'Error'
    }

    # Update current process PATH immediately for this session
    try {
        $procPath = $env:PATH
        $newProcPath = BuildPathWithInsert -existingPath $procPath -toInsert $PathToAdd
        if ($newProcPath -ne $procPath) { $env:PATH = $newProcPath }
    } catch { }

    return [PSCustomObject]@{
        UserStatus    = $userStatus
        MachineStatus = $machineStatus
    }
}

# Detect standard Git early and validate (fail-fast behavior)
$stdGitPath = $null
try {
    $stdGitPath = Get-StdGitPath
} catch {
    Write-Host "Error: $($_.Exception.Message)" -ForegroundColor Red
    # When invoked via `irm ... | iex` from a non-interactive context
    # (e.g. Win+R, double-clicked .bat, or MDM deployment), the console
    # window would disappear before the user can read the error above.
    # Pause here so the prerequisite failure is visible.
    #
    # [Console]::IsInputRedirected detects CI / piped-stdin environments
    # where Read-Host would hang indefinitely. In those cases we skip the
    # pause so tests don't time out.
    if ($Host.Name -match 'ConsoleHost' -and -not [Console]::IsInputRedirected) {
        Write-Host ''
        Write-Host 'Press Enter to exit (auto-closing in 10 seconds)...' -ForegroundColor Yellow
        $timeout = [TimeSpan]::FromSeconds(10)
        $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
        while ($stopwatch.Elapsed -lt $timeout) {
            if ([Console]::KeyAvailable) {
                $key = [Console]::ReadKey($true)
                if ($key.Key -eq 'Enter') { break }
            }
            Start-Sleep -Milliseconds 100
        }
    }
    # Stop the install. Using throw (not exit) preserves the interactive
    # PowerShell session; in -Command contexts the host will exit after
    # the throw, but the pause above already gave the user time to read
    # the prerequisite failure.
    throw
}

# Detect architecture and OS
$arch = Get-Architecture
if (-not $arch) { Write-ErrorAndExit "Unsupported architecture: $([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture)" }
$os = 'windows'

# Determine binary name and download URLs
$binaryName = "git-ai-$os-$arch"

# Determine release tag
# Priority: 1. Local binary override, 2. Bundled binary, 3. Enterprise release
# source, 4. Pinned GitHub version, 5. Environment override, 6. GitHub latest.
$BundledBinary = $null
$InstallScriptPath = [string]$PSCommandPath
$InstallScriptDir = $null
if (-not [string]::IsNullOrWhiteSpace($InstallScriptPath) -and
    (Test-Path -LiteralPath $InstallScriptPath -PathType Leaf)) {
    $InstallScriptDir = Split-Path -Parent $InstallScriptPath
}
if (-not [string]::IsNullOrWhiteSpace($InstallScriptDir)) {
    $BundledCandidate = Join-Path $InstallScriptDir $binaryName
    if (-not (Test-Path -LiteralPath $BundledCandidate)) {
        $BundledCandidate = Join-Path $InstallScriptDir "$binaryName.exe"
    }
    if (Test-Path -LiteralPath $BundledCandidate) {
        $BundledBinary = $BundledCandidate
    }
}

if (-not [string]::IsNullOrWhiteSpace($env:GIT_AI_LOCAL_BINARY)) {
    $releaseTag = 'local'
} elseif ($BundledBinary) {
    $releaseTag = 'local'
    $env:GIT_AI_LOCAL_BINARY = $BundledBinary
} elseif ($EnterpriseReleaseBaseUrl -ne '__ENTERPRISE_RELEASE_BASE_URL_PLACEHOLDER__') {
    $releaseTag = if ($PinnedVersion -ne '__VERSION_PLACEHOLDER__') { $PinnedVersion } else { $EnterpriseReleaseChannel }
    $enterpriseDownloadBase = "$($EnterpriseReleaseBaseUrl.TrimEnd('/'))/worker/releases/$EnterpriseReleaseChannel/download"
    $downloadUrlExe = "$enterpriseDownloadBase/$binaryName.exe"
    $downloadUrlNoExt = "$enterpriseDownloadBase/$binaryName"
} elseif ($PinnedVersion -ne '__VERSION_PLACEHOLDER__') {
    # Version-pinned install script from a release
    $releaseTag = $PinnedVersion
    $downloadUrlExe = "https://github.com/$Repo/releases/download/$releaseTag/$binaryName.exe"
    $downloadUrlNoExt = "https://github.com/$Repo/releases/download/$releaseTag/$binaryName"
} elseif (-not [string]::IsNullOrWhiteSpace($env:GIT_AI_RELEASE_TAG) -and $env:GIT_AI_RELEASE_TAG -ne 'latest') {
    # Environment variable override
    $releaseTag = $env:GIT_AI_RELEASE_TAG
    $downloadUrlExe = "https://github.com/$Repo/releases/download/$releaseTag/$binaryName.exe"
    $downloadUrlNoExt = "https://github.com/$Repo/releases/download/$releaseTag/$binaryName"
} else {
    # Default to latest
    $releaseTag = 'latest'
    $downloadUrlExe = "https://github.com/$Repo/releases/latest/download/$binaryName.exe"
    $downloadUrlNoExt = "https://github.com/$Repo/releases/latest/download/$binaryName"
}

# Install directory: %USERPROFILE%\.git-ai\bin
$installDir = Join-Path $HOME ".git-ai\bin"
New-Item -ItemType Directory -Force -Path $installDir | Out-Null

Write-Host ("Downloading git-ai (release: {0})..." -f $releaseTag)
$tmpFile = Join-Path $installDir "git-ai.tmp.$PID.exe"

function Try-Download {
    param(
        [Parameter(Mandatory = $true)][string]$Url
    )
    try {
        # Disable progress bar to avoid extreme slowdown caused by PowerShell's
        # progress-stream rendering (can make downloads 10-50x slower).
        $oldProgressPreference = $ProgressPreference
        $ProgressPreference = 'SilentlyContinue'
        try {
            Invoke-WebRequest -Uri $Url -OutFile $tmpFile -UseBasicParsing -ErrorAction Stop
        } finally {
            $ProgressPreference = $oldProgressPreference
        }
        return $true
    } catch {
        return $false
    }
}

# Track which download URL succeeded for checksum verification
$downloadedBinaryName = $null
if (-not [string]::IsNullOrWhiteSpace($env:GIT_AI_LOCAL_BINARY)) {
    if (-not (Test-Path -LiteralPath $env:GIT_AI_LOCAL_BINARY)) {
        Remove-Item -Force -ErrorAction SilentlyContinue $tmpFile
        Write-ErrorAndExit "Local binary not found at $($env:GIT_AI_LOCAL_BINARY)"
    }
    Copy-Item -Force -Path $env:GIT_AI_LOCAL_BINARY -Destination $tmpFile
    $downloadedBinaryName = "$binaryName.exe"
} elseif (Try-Download -Url $downloadUrlExe) {
    $downloadedBinaryName = "$binaryName.exe"
} elseif (Try-Download -Url $downloadUrlNoExt) {
    $downloadedBinaryName = $binaryName
}

if (-not $downloadedBinaryName) {
    Remove-Item -Force -ErrorAction SilentlyContinue $tmpFile
    Write-ErrorAndExit 'Failed to download binary (HTTP error)'
}

try {
    if ((Get-Item $tmpFile).Length -le 0) {
        Remove-Item -Force -ErrorAction SilentlyContinue $tmpFile
        Write-ErrorAndExit 'Downloaded file is empty'
    }
} catch {
    Remove-Item -Force -ErrorAction SilentlyContinue $tmpFile
    Write-ErrorAndExit 'Download failed'
}

# Verify checksum if embedded (release builds only)
Verify-Checksum -File $tmpFile -BinaryName $downloadedBinaryName

$finalExe = Join-Path $installDir 'git-ai.exe'

# Wait for git-ai.exe to be available if it exists and is in use
if (Test-Path -LiteralPath $finalExe) {
    if (-not (Wait-ForFileAvailable -Path $finalExe -InstallDir $installDir -MaxWaitSeconds 300 -RetryIntervalSeconds 5)) {
        Remove-Item -Force -ErrorAction SilentlyContinue $tmpFile
        Write-ErrorAndExit "Timeout waiting for $finalExe to be available. Please close any running git-ai processes and try again."
    }
}

Move-Item -Force -Path $tmpFile -Destination $finalExe
try { Unblock-File -Path $finalExe -ErrorAction SilentlyContinue } catch { }

# Create a shim so calling `git` goes through git-ai by PATH precedence
$gitShim = Join-Path $installDir 'git.exe'

# Wait for git.exe shim to be available if it exists and is in use
if (Test-Path -LiteralPath $gitShim) {
    if (-not (Wait-ForFileAvailable -Path $gitShim -InstallDir $installDir -MaxWaitSeconds 300 -RetryIntervalSeconds 5)) {
        Write-ErrorAndExit "Timeout waiting for $gitShim to be available. Please close any running git processes and try again."
    }
}

Copy-Item -Force -Path $finalExe -Destination $gitShim
try { Unblock-File -Path $gitShim -ErrorAction SilentlyContinue } catch { }

# Create a shim so calling `git-og` invokes the standard Git
$gitOgShim = Join-Path $installDir 'git-og.cmd'
$gitOgShimContent = "@echo off$([Environment]::NewLine)`"$stdGitPath`" %*$([Environment]::NewLine)"
Set-Content -Path $gitOgShim -Value $gitOgShimContent -Encoding ASCII -Force
try { Unblock-File -Path $gitOgShim -ErrorAction SilentlyContinue } catch { }

# Login user with install token if provided
$needLogin = $false
if ($env:INSTALL_NONCE -and $env:API_BASE) {
    try {
        & $finalExe exchange-nonce | Out-Host
        if ($LASTEXITCODE -ne 0) {
            $needLogin = $true
        }
    } catch {
        $needLogin = $true
    }
}

# Install hooks
Write-Host 'Setting up IDE/agent hooks...'
$previousDeferDaemonStart = $env:GIT_AI_INSTALLER_DEFER_DAEMON_START
try {
    # install-hooks normally restarts the daemon after writing trace2 config.
    # During installation the script owns startup and recovery, so defer it to
    # the single verified start near the end of this script.
    $env:GIT_AI_INSTALLER_DEFER_DAEMON_START = '1'
    & $finalExe install-hooks | Out-Host
    if ($LASTEXITCODE -ne 0) {
        throw "git-ai install-hooks exited with code $LASTEXITCODE"
    }
    Write-Success 'Successfully set up IDE/agent hooks'
} catch {
    Write-Warning "Failed to set up IDE/agent hooks: $($_.Exception.Message)"
} finally {
    if ($null -eq $previousDeferDaemonStart) {
        Remove-Item Env:GIT_AI_INSTALLER_DEFER_DAEMON_START -ErrorAction SilentlyContinue
    } else {
        $env:GIT_AI_INSTALLER_DEFER_DAEMON_START = $previousDeferDaemonStart
    }
}

# Update PATH so our shim takes precedence over any Git entries
$skipPathUpdate = $env:GIT_AI_SKIP_PATH_UPDATE -eq '1'
if ($skipPathUpdate) {
    Write-Warning 'Skipping PATH updates because GIT_AI_SKIP_PATH_UPDATE=1'
    $pathUpdate = [PSCustomObject]@{
        UserStatus    = 'Skipped'
        MachineStatus = 'Skipped'
    }
} else {
    $pathUpdate = Set-PathPrependBeforeGit -PathToAdd $installDir
}
if ($pathUpdate.UserStatus -eq 'Updated') {
    Write-Success 'Successfully added git-ai to the user PATH.'
} elseif ($pathUpdate.UserStatus -eq 'AlreadyPresent') {
    Write-Success 'git-ai already present in the user PATH.'
} elseif ($pathUpdate.UserStatus -eq 'Error') {
    Write-Host 'Failed to update the user PATH.' -ForegroundColor Red
}

if ($pathUpdate.MachineStatus -eq 'Updated') {
    Write-Success 'Successfully added git-ai to the system PATH.'
} elseif ($pathUpdate.MachineStatus -eq 'AlreadyPresent') {
    Write-Success 'git-ai already present in the system PATH.'
} elseif ($pathUpdate.MachineStatus -eq 'Error') {
    Write-Warning 'System PATH was not updated (administrator rights or system policy may prevent it).'
    if ($pathUpdate.UserStatus -eq 'Updated' -or $pathUpdate.UserStatus -eq 'AlreadyPresent') {
        Write-Warning 'User PATH is configured; PowerShell profile precedence will be configured automatically below.'
    }
}

# Windows merges Machine PATH before User PATH in new processes, so a system
# Git installation can still shadow the per-user git-ai shim. Configure both
# Windows PowerShell and PowerShell 7 user profiles to enforce precedence
# without requiring administrator rights.
$powerShellProfilesConfigured = New-Object System.Collections.Generic.List[string]
$profileRootOverride = $env:GIT_AI_TEST_PROFILE_ROOT
if (-not $skipPathUpdate -or $profileRootOverride) {
try {
    $profileCandidates = New-Object System.Collections.Generic.List[string]
    if (-not $profileRootOverride -and $PROFILE -and $PROFILE.CurrentUserAllHosts) {
        $profileCandidates.Add([string]$PROFILE.CurrentUserAllHosts) | Out-Null
    }
    $documentsDir = if ($profileRootOverride) {
        $profileRootOverride
    } else {
        [Environment]::GetFolderPath('MyDocuments')
    }
    if (-not [string]::IsNullOrWhiteSpace($documentsDir)) {
        $profileCandidates.Add((Join-Path $documentsDir 'WindowsPowerShell\profile.ps1')) | Out-Null
        $profileCandidates.Add((Join-Path $documentsDir 'PowerShell\profile.ps1')) | Out-Null
    }

    $profileMarker = '# >>> git-ai managed PATH >>>'
    $profileEndMarker = '# <<< git-ai managed PATH <<<'
    $profileBlock = @'

# >>> git-ai managed PATH >>>
$gitAiBin = Join-Path $HOME '.git-ai\bin'
if (Test-Path -LiteralPath $gitAiBin) {
    $normalizedGitAiBin = $gitAiBin.Trim().TrimEnd('\')
    $gitAiPathEntries = @($env:Path -split ';' | Where-Object {
        -not [string]::IsNullOrWhiteSpace($_) -and $_.Trim().TrimEnd('\') -ine $normalizedGitAiBin
    })
    $env:Path = (@($gitAiBin) + $gitAiPathEntries) -join ';'
    $gitAiGit = Join-Path $gitAiBin 'git.exe'
    if (Test-Path -LiteralPath $gitAiGit -PathType Leaf) {
        Set-Alias -Name git -Value $gitAiGit -Scope Global -Force
    }
}
# <<< git-ai managed PATH <<<
'@
    $seenProfiles = New-Object 'System.Collections.Generic.HashSet[string]'
    foreach ($profilePath in $profileCandidates) {
        if ([string]::IsNullOrWhiteSpace($profilePath) -or -not $seenProfiles.Add($profilePath)) {
            continue
        }
        $existingProfile = ''
        if (Test-Path -LiteralPath $profilePath -PathType Leaf) {
            $existingProfile = Get-Content -LiteralPath $profilePath -Raw -ErrorAction Stop
        }
        $profileDir = Split-Path -Parent $profilePath
        New-Item -ItemType Directory -Force -Path $profileDir | Out-Null
        $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
        if ($existingProfile -and $existingProfile.Contains($profileMarker)) {
            $managedPattern = '(?s)' + [regex]::Escape($profileMarker) + '.*?' + [regex]::Escape($profileEndMarker)
            $managedRegex = New-Object System.Text.RegularExpressions.Regex($managedPattern)
            $replacementText = $profileBlock.Trim()
            # Regex replacement strings treat `$_` as the entire input and
            # interpret other `$` sequences as substitutions. A MatchEvaluator
            # returns the managed PowerShell block literally, preserving all
            # of its dollar-prefixed variables across repeated installations.
            $updatedProfile = $managedRegex.Replace(
                $existingProfile,
                [System.Text.RegularExpressions.MatchEvaluator] {
                    param($match)
                    $replacementText
                },
                1
            )
            if ($updatedProfile -eq $existingProfile) {
                continue
            }
            [System.IO.File]::WriteAllText($profilePath, $updatedProfile, $utf8NoBom)
        } else {
            [System.IO.File]::AppendAllText($profilePath, $profileBlock, $utf8NoBom)
        }
        $powerShellProfilesConfigured.Add($profilePath) | Out-Null
    }
} catch {
    Write-Warning "Unable to configure PowerShell PATH precedence automatically: $($_.Exception.Message)"
}
}
if ($powerShellProfilesConfigured.Count -gt 0) {
    Write-Success 'Successfully configured PowerShell PATH precedence for git-ai.'
}
try {
    # Make the shim effective immediately for direct `irm ... | iex` installs.
    # Background self-updates cannot modify their parent shell, but the profile
    # block above applies the alias when the next PowerShell starts.
    if (Test-Path -LiteralPath $gitShim -PathType Leaf) {
        Set-Alias -Name git -Value $gitShim -Scope Global -Force
    }
} catch {
    Write-Warning "Unable to activate the git-ai PowerShell alias in this session: $($_.Exception.Message)"
}

try {
    $resolvedGit = Get-Command git -ErrorAction Stop
    $resolvedGitPath = [string]$resolvedGit.Definition
    if ((Normalize-PathString $resolvedGitPath) -ne (Normalize-PathString $gitShim)) {
        throw "PowerShell resolves 'git' to '$resolvedGitPath' instead of '$gitShim'."
    }
    Write-Success "PowerShell resolves 'git' through the git-ai shim."
} catch {
    Write-Warning "Unable to verify PowerShell git precedence: $($_.Exception.Message)"
    Write-Warning "Close and reopen PowerShell, then run: Get-Command git -All"
}

# Configure Git Bash shell profiles so git-ai takes precedence over /mingw64/bin/git
# Git Bash (MSYS2/MinGW) prepends its own directories to PATH, which shadows
# the Windows PATH entry we set above. Writing to ~/.bashrc ensures git-ai's
# bin directory is prepended after Git Bash's own PATH setup.
$gitBashConfigured = $false
$gitBashAlreadyConfigured = $false
try {
    $bashrcPath = Join-Path $HOME '.bashrc'
    $bashProfilePath = Join-Path $HOME '.bash_profile'
    $pathCmd = 'export PATH="$HOME/.git-ai/bin:$PATH"'
    $markerString = '.git-ai/bin'

    # Detect if Git Bash is installed
    $gitBashInstalled = $false
    $gitForWindowsPaths = @()
    if ($env:ProgramFiles) { $gitForWindowsPaths += Join-Path $env:ProgramFiles 'Git\bin\bash.exe' }
    if (${env:ProgramFiles(x86)}) { $gitForWindowsPaths += Join-Path ${env:ProgramFiles(x86)} 'Git\bin\bash.exe' }
    if ($env:LOCALAPPDATA) { $gitForWindowsPaths += Join-Path $env:LOCALAPPDATA 'Programs\Git\bin\bash.exe' }
    foreach ($p in $gitForWindowsPaths) {
        if ($p -and (Test-Path -LiteralPath $p)) {
            $gitBashInstalled = $true
            break
        }
    }

    if ($gitBashInstalled) {
        # Determine which config file to update (prefer .bashrc, fall back to .bash_profile)
        $targetBashConfig = $null
        if (Test-Path -LiteralPath $bashrcPath) {
            $targetBashConfig = $bashrcPath
        } elseif (Test-Path -LiteralPath $bashProfilePath) {
            $targetBashConfig = $bashProfilePath
        } else {
            # No existing config; create .bashrc
            $targetBashConfig = $bashrcPath
        }

        # Check if already configured
        $alreadyPresent = $false
        if (Test-Path -LiteralPath $targetBashConfig) {
            $content = Get-Content -LiteralPath $targetBashConfig -Raw -ErrorAction SilentlyContinue
            if ($content -and $content.Contains($markerString)) {
                $alreadyPresent = $true
            }
        }

        if ($alreadyPresent) {
            $gitBashAlreadyConfigured = $true
        } else {
            $timestamp = Get-Date -Format 'yyyy-MM-dd HH:mm:ss'
            $appendContent = "`n# Added by git-ai installer on $timestamp`n$pathCmd`n"
            $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
            [System.IO.File]::AppendAllText($targetBashConfig, $appendContent, $utf8NoBom)
            $gitBashConfigured = $true
        }
    }
} catch {
    Write-Host "Warning: Failed to configure Git Bash: $($_.Exception.Message)" -ForegroundColor Yellow
}

if ($gitBashConfigured) {
    Write-Success "Successfully configured Git Bash ($targetBashConfig)"
} elseif ($gitBashAlreadyConfigured) {
    Write-Success "Git Bash already configured ($targetBashConfig)"
}

# Write JSON config at %USERPROFILE%\.git-ai\config.json (only if it doesn't exist)
try {
    $configDir = Join-Path $HOME '.git-ai'
    $configJsonPath = Join-Path $configDir 'config.json'
    New-Item -ItemType Directory -Force -Path $configDir | Out-Null

    if (-not (Test-Path -LiteralPath $configJsonPath)) {
        $cfg = @{
            git_path = $stdGitPath
            feature_flags = @{
                async_mode = $true
            }
        } | ConvertTo-Json -Depth 3 -Compress
        $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
        [System.IO.File]::WriteAllText($configJsonPath, $cfg, $utf8NoBom)
    }
} catch {
    Write-Host "Warning: Failed to write config.json: $($_.Exception.Message)" -ForegroundColor Yellow
}

try {
    & $finalExe config set api_base_url $EnterpriseApiBaseUrl | Out-Host
    if ($LASTEXITCODE -ne 0) {
        throw "git-ai config exited with code $LASTEXITCODE"
    }
    Write-Success "Configured enterprise API server: $EnterpriseApiBaseUrl"

    & $finalExe config set disable_version_checks false | Out-Host
    if ($LASTEXITCODE -ne 0) {
        throw "git-ai config exited with code $LASTEXITCODE"
    }

    & $finalExe config set disable_auto_updates true | Out-Host
    if ($LASTEXITCODE -ne 0) {
        throw "git-ai config exited with code $LASTEXITCODE"
    }
    Write-Success 'Configured automatic update checks with manual installation'
} catch {
    Write-ErrorAndExit "Failed to configure enterprise API server $EnterpriseApiBaseUrl`: $($_.Exception.Message)"
}

# Confirm the newly installed binary can provide the background service before
# returning control to the user. `bg start` is a no-op when the service is
# healthy and automatically repairs an unhealthy Windows lock holder in current
# releases. The fallback handles upgrades from older releases that cannot
# perform that recovery themselves.
if (-not $env:GIT_AI_TEST_DB_PATH -and -not $env:GITAI_TEST_DB_PATH) {
    try {
        $firstStartOutput = @(& $finalExe bg start 2>&1)
        $firstStartExitCode = $LASTEXITCODE
        if ($firstStartExitCode -ne 0) {
            [void](Stop-GitAiManagedProcesses -InstallDir $installDir)
            $daemonDir = Join-Path $HOME '.git-ai\internal\daemon'
            Remove-Item -LiteralPath (Join-Path $daemonDir 'daemon.lock') -Force -ErrorAction SilentlyContinue
            Remove-Item -LiteralPath (Join-Path $daemonDir 'daemon.pid.json') -Force -ErrorAction SilentlyContinue
            $retryStartOutput = @(& $finalExe bg start 2>&1)
            $retryStartExitCode = $LASTEXITCODE
            if ($retryStartExitCode -ne 0) {
                $firstStartOutput | Out-Host
                $retryStartOutput | Out-Host
                throw "git-ai bg start exited with code $retryStartExitCode"
            }
            Write-Warning 'Recovered the background service after its first startup attempt did not become ready.'
        }
        Write-Success 'Background service is ready'
    } catch {
        Write-Warning "Warning: Background service did not start during installation: $($_.Exception.Message)"
        try {
            & $finalExe config set feature_flags.async_mode false | Out-Host
            if ($LASTEXITCODE -ne 0) {
                throw "git-ai config exited with code $LASTEXITCODE"
            }
            # Do not leave Git writing Trace2 events to unavailable named pipes.
            & $stdGitPath config --global --remove-section trace2 2>$null
            Write-Warning 'Background mode was disabled automatically. Local AI and human code tracking remains enabled.'
        } catch {
            Write-Warning "Failed to enable local tracking fallback automatically: $($_.Exception.Message)"
        }
    }
}

# If nonce exchange failed, run interactive login
if ($needLogin) {
    Write-Host ''
    Write-Host 'Launching login...'
    & $finalExe login
}

# Only report installation success after all configuration, background-service
# startup, fallback handling, and any required login have finished.
Write-Success "Successfully installed git-ai into $installDir"
Write-Success "You can now run 'git-ai' from your terminal"
Write-Success "To update later on Windows, run 'git-ai update' (not 'git ai update')"
Write-Host 'Close and reopen your terminal and IDE sessions to use git-ai.' -ForegroundColor Yellow
