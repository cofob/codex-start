#Requires -Version 5.1
<#
.SYNOPSIS
Install the latest stable codex-start release on Windows.

.DESCRIPTION
Downloads an exact artifact selected from release-manifest.json, verifies its
SHA-256 checksum, verifies its Sigstore bundle when Cosign is available, and
atomically installs codex-start.exe.
#>
[CmdletBinding()]
param(
    [string] $Version,
    [string] $InstallDir,
    [switch] $System,
    [switch] $AutoUpdates,
    [switch] $NoAutoUpdates,
    [switch] $Yes,
    [switch] $RequireSignature,
    [switch] $Force
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$Repository = if ($env:CODEX_START_REPOSITORY) { $env:CODEX_START_REPOSITORY } else { 'cofob/codex-start' }
$GitHubApi = if ($env:CODEX_START_GITHUB_API_URL) { $env:CODEX_START_GITHUB_API_URL.TrimEnd('/') } else { 'https://api.github.com' }
$ReleaseDownloadBase = if ($env:CODEX_START_RELEASE_DOWNLOAD_BASE) {
    $env:CODEX_START_RELEASE_DOWNLOAD_BASE.TrimEnd('/')
} else {
    "https://github.com/$Repository/releases/download"
}
$LatestReleaseUrl = if ($env:CODEX_START_LATEST_RELEASE_URL) {
    $env:CODEX_START_LATEST_RELEASE_URL
} else {
    "$GitHubApi/repos/$Repository/releases/latest"
}
$MaxMetadataBytes = 10MB
$MaxArtifactBytes = 1GB
$MaxExecutableBytes = 512MB
$TemporaryDirectory = $null
$StagedDestination = $null

function Write-WarningMessage {
    param([Parameter(Mandatory = $true)][string] $Message)
    [Console]::Error.WriteLine("codex-start installer: warning: $Message")
}

function Assert-SafeDownloadUri {
    param([Parameter(Mandatory = $true)][uri] $Uri)
    if ($Uri.Scheme -ne 'https' -and $Uri.Scheme -ne 'file') {
        throw "refusing non-HTTPS download URL: $Uri"
    }
}

function Receive-File {
    param(
        [Parameter(Mandatory = $true)][uri] $Uri,
        [Parameter(Mandatory = $true)][string] $Destination,
        [Parameter(Mandatory = $true)][long] $MaximumBytes
    )

    Assert-SafeDownloadUri -Uri $Uri
    $Partial = "$Destination.part"
    Remove-Item -LiteralPath $Partial -Force -ErrorAction SilentlyContinue
    $InputStream = $null
    $OutputStream = $null
    $Response = $null
    $Client = $null
    try {
        if ($Uri.IsFile) {
            $InputStream = [System.IO.File]::OpenRead($Uri.LocalPath)
            if ($InputStream.Length -gt $MaximumBytes) {
                throw "download exceeds the $MaximumBytes byte limit: $Uri"
            }
        } else {
            Add-Type -AssemblyName System.Net.Http
            $Handler = New-Object System.Net.Http.HttpClientHandler
            $Handler.AllowAutoRedirect = $true
            $Handler.MaxAutomaticRedirections = 5
            $Client = New-Object System.Net.Http.HttpClient($Handler)
            $Client.Timeout = [TimeSpan]::FromMinutes(5)
            $Client.DefaultRequestHeaders.UserAgent.ParseAdd('codex-start-installer')
            $Client.DefaultRequestHeaders.Accept.ParseAdd('application/vnd.github+json')
            $Client.DefaultRequestHeaders.Add('X-GitHub-Api-Version', '2022-11-28')
            $Response = $Client.GetAsync($Uri, [System.Net.Http.HttpCompletionOption]::ResponseHeadersRead).GetAwaiter().GetResult()
            $Response.EnsureSuccessStatusCode() | Out-Null
            if ($Response.RequestMessage.RequestUri.Scheme -ne 'https') {
                throw "download redirected to a non-HTTPS URL: $($Response.RequestMessage.RequestUri)"
            }
            if ($Response.Content.Headers.ContentLength -and $Response.Content.Headers.ContentLength.Value -gt $MaximumBytes) {
                throw "download exceeds the $MaximumBytes byte limit: $Uri"
            }
            $InputStream = $Response.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
        }

        $OutputStream = New-Object System.IO.FileStream(
            $Partial,
            [System.IO.FileMode]::CreateNew,
            [System.IO.FileAccess]::Write,
            [System.IO.FileShare]::None
        )
        $Buffer = New-Object byte[] 65536
        [long] $Total = 0
        while (($Read = $InputStream.Read($Buffer, 0, $Buffer.Length)) -gt 0) {
            $Total += $Read
            if ($Total -gt $MaximumBytes) {
                throw "download exceeds the $MaximumBytes byte limit: $Uri"
            }
            $OutputStream.Write($Buffer, 0, $Read)
        }
        $OutputStream.Flush($true)
        $OutputStream.Dispose()
        $OutputStream = $null
        Move-Item -LiteralPath $Partial -Destination $Destination -Force
    } catch {
        Remove-Item -LiteralPath $Partial -Force -ErrorAction SilentlyContinue
        throw
    } finally {
        if ($OutputStream) { $OutputStream.Dispose() }
        if ($InputStream) { $InputStream.Dispose() }
        if ($Response) { $Response.Dispose() }
        if ($Client) { $Client.Dispose() }
    }
}

function Get-ExpectedChecksum {
    param(
        [Parameter(Mandatory = $true)][string] $ChecksumFile,
        [Parameter(Mandatory = $true)][string] $Filename
    )
    $Found = @()
    foreach ($Line in [System.IO.File]::ReadLines($ChecksumFile)) {
        if ($Line -match '^([0-9A-Fa-f]{64})\s+\*?(.+)$' -and $Matches[2] -eq $Filename) {
            $Found += $Matches[1].ToLowerInvariant()
        }
    }
    if ($Found.Count -ne 1) {
        throw "SHA256SUMS must contain exactly one valid entry for $Filename"
    }
    return $Found[0]
}

function Confirm-Checksum {
    param(
        [Parameter(Mandatory = $true)][string] $Path,
        [Parameter(Mandatory = $true)][string] $Filename,
        [Parameter(Mandatory = $true)][string] $ChecksumFile
    )
    $Expected = Get-ExpectedChecksum -ChecksumFile $ChecksumFile -Filename $Filename
    $Actual = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($Actual -ne $Expected) {
        throw "SHA-256 mismatch for $Filename"
    }
    return $Actual
}

function Get-CosignCommand {
    if ($env:CODEX_START_COSIGN -eq 'none') { return $null }
    if ($env:CODEX_START_COSIGN) {
        if (-not (Test-Path -LiteralPath $env:CODEX_START_COSIGN -PathType Leaf)) {
            throw 'CODEX_START_COSIGN does not name an executable file'
        }
        return $env:CODEX_START_COSIGN
    }
    $Command = Get-Command cosign -CommandType Application -ErrorAction SilentlyContinue
    if ($Command) { return $Command.Source }
    return $null
}

function Confirm-SigstoreBundle {
    param(
        [Parameter(Mandatory = $true)][string] $Cosign,
        [Parameter(Mandatory = $true)][string] $Path,
        [Parameter(Mandatory = $true)][string] $Bundle,
        [Parameter(Mandatory = $true)][string] $Tag
    )
    $Identity = if ($env:CODEX_START_CERTIFICATE_IDENTITY) {
        $env:CODEX_START_CERTIFICATE_IDENTITY
    } else {
        "https://github.com/$Repository/.github/workflows/release.yml@refs/tags/$Tag"
    }
    & $Cosign verify-blob --bundle $Bundle --certificate-identity $Identity `
        --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' $Path | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "Sigstore verification failed for $([System.IO.Path]::GetFileName($Path))"
    }
}

function Assert-SafeAssetName {
    param([Parameter(Mandatory = $true)][string] $Name)
    if ($Name -notmatch '^[A-Za-z0-9][A-Za-z0-9._+-]*$') {
        throw "unsafe release asset name: $Name"
    }
}

function Move-FileAtomically {
    param(
        [Parameter(Mandatory = $true)][string] $Source,
        [Parameter(Mandatory = $true)][string] $Destination
    )
    if (Test-Path -LiteralPath $Destination -PathType Leaf) {
        $Backup = "$Destination.backup.$PID"
        Remove-Item -LiteralPath $Backup -Force -ErrorAction SilentlyContinue
        try {
            [System.IO.File]::Replace($Source, $Destination, $Backup, $true)
        } finally {
            Remove-Item -LiteralPath $Backup -Force -ErrorAction SilentlyContinue
        }
    } else {
        [System.IO.File]::Move($Source, $Destination)
    }
}

function Protect-PrivatePath {
    param(
        [Parameter(Mandatory = $true)][string] $Path,
        [Parameter(Mandatory = $true)][bool] $Directory
    )
    if ($env:CODEX_START_TEST_WINDOWS -eq '1') { return }
    $Identity = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
    if ($Directory) {
        $Security = New-Object System.Security.AccessControl.DirectorySecurity
        $Inheritance = [System.Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
    } else {
        $Security = New-Object System.Security.AccessControl.FileSecurity
        $Inheritance = [System.Security.AccessControl.InheritanceFlags]::None
    }
    $Security.SetAccessRuleProtection($true, $false)
    $Rule = New-Object System.Security.AccessControl.FileSystemAccessRule(
        $Identity,
        [System.Security.AccessControl.FileSystemRights]::FullControl,
        $Inheritance,
        [System.Security.AccessControl.PropagationFlags]::None,
        [System.Security.AccessControl.AccessControlType]::Allow
    )
    $Security.AddAccessRule($Rule)
    Set-Acl -LiteralPath $Path -AclObject $Security
}

function Get-InstallReceiptPath {
    $DataDirectory = if ($env:CODEX_START_INSTALLER_TEST_DATA_DIR) {
        $env:CODEX_START_INSTALLER_TEST_DATA_DIR
    } else {
        Join-Path $env:LOCALAPPDATA 'codex-start'
    }
    return Join-Path $DataDirectory 'install.json'
}

function Get-PreservedSignaturePolicy {
    param([Parameter(Mandatory = $true)][string] $Executable)
    $ReceiptPath = Get-InstallReceiptPath
    if (-not (Test-Path -LiteralPath $ReceiptPath)) { return $false }
    if (-not (Test-Path -LiteralPath $ReceiptPath -PathType Leaf)) {
        throw "installation receipt is not a regular file: $ReceiptPath"
    }
    $ReceiptItem = Get-Item -LiteralPath $ReceiptPath
    if ($ReceiptItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {
        throw "refusing a reparse-point installation receipt: $ReceiptPath"
    }
    $Receipt = Get-Content -LiteralPath $ReceiptPath -Raw | ConvertFrom-Json
    if ($Receipt.schema_version -ne 1 -or $Receipt.require_signature -isnot [bool]) {
        throw "installation receipt has an invalid signature policy: $ReceiptPath"
    }
    $RecordedExecutable = [System.IO.Path]::GetFullPath([string] $Receipt.executable)
    $ExpectedExecutable = [System.IO.Path]::GetFullPath($Executable)
    return $RecordedExecutable -ieq $ExpectedExecutable -and [bool] $Receipt.require_signature
}

function Write-InstallReceipt {
    param(
        [Parameter(Mandatory = $true)][string] $Method,
        [Parameter(Mandatory = $true)][string] $Target,
        [Parameter(Mandatory = $true)][string] $Executable
    )
    $ReceiptPath = Get-InstallReceiptPath
    $DataDirectory = Split-Path -Parent $ReceiptPath
    if ((Test-Path -LiteralPath $DataDirectory) -and -not (Test-Path -LiteralPath $DataDirectory -PathType Container)) {
        throw "application data path is not a directory: $DataDirectory"
    }
    [System.IO.Directory]::CreateDirectory($DataDirectory) | Out-Null
    Protect-PrivatePath -Path $DataDirectory -Directory $true
    $ReceiptTemporary = "$ReceiptPath.tmp.$PID"
    $Receipt = [ordered]@{
        schema_version = 1
        method = $Method
        target = $Target
        executable = [System.IO.Path]::GetFullPath($Executable)
        require_signature = [bool] $RequireSignature
    }
    [System.IO.File]::WriteAllText(
        $ReceiptTemporary,
        (($Receipt | ConvertTo-Json) + [Environment]::NewLine),
        (New-Object System.Text.UTF8Encoding($false))
    )
    Protect-PrivatePath -Path $ReceiptTemporary -Directory $false
    Move-FileAtomically -Source $ReceiptTemporary -Destination $ReceiptPath
    Protect-PrivatePath -Path $ReceiptPath -Directory $false
}

function Add-PathEntry {
    param(
        [Parameter(Mandatory = $true)][string] $Directory,
        [Parameter(Mandatory = $true)][ValidateSet('User', 'Machine')][string] $Scope
    )
    $FullDirectory = [System.IO.Path]::GetFullPath($Directory).TrimEnd('\')
    $PersistentPath = [Environment]::GetEnvironmentVariable('Path', $Scope)
    $Entries = @()
    if ($PersistentPath) { $Entries = @($PersistentPath.Split(';') | Where-Object { $_ }) }
    $Present = @($Entries | Where-Object { $_.TrimEnd('\') -ieq $FullDirectory }).Count -gt 0
    if (-not $Present) {
        $NewPath = (@($Entries) + $FullDirectory) -join ';'
        [Environment]::SetEnvironmentVariable('Path', $NewPath, $Scope)
    }
    $ProcessEntries = @($env:Path.Split(';') | Where-Object { $_ })
    if (@($ProcessEntries | Where-Object { $_.TrimEnd('\') -ieq $FullDirectory }).Count -eq 0) {
        $env:Path = "$env:Path;$FullDirectory"
    }
}

function Set-AutoUpdatePreference {
    param(
        [Parameter(Mandatory = $true)][string] $Executable,
        [Parameter(Mandatory = $true)][bool] $Enabled
    )
    $ConfigCommand = if ($env:CODEX_START_CONFIG_COMMAND) { $env:CODEX_START_CONFIG_COMMAND } else { $Executable }
    $PreferenceValue = $Enabled.ToString().ToLowerInvariant()
    & $ConfigCommand config set --global updates.enabled $PreferenceValue
    if ($LASTEXITCODE -ne 0) {
        throw 'codex-start was installed, but its auto-update preference could not be saved'
    }
}

if ($AutoUpdates -and $NoAutoUpdates) {
    throw 'choose only one of -AutoUpdates and -NoAutoUpdates'
}
if ($System -and $InstallDir) {
    throw '-System and -InstallDir cannot be combined'
}
$RunningOnWindows = $env:OS -eq 'Windows_NT'
$SimulatedWindowsForTests = $env:CODEX_START_TEST_WINDOWS -eq '1'
if (-not $RunningOnWindows -and -not $SimulatedWindowsForTests) {
    throw 'install.ps1 supports Windows; use install.sh on Linux or macOS'
}

$Architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToLowerInvariant()
switch ($Architecture) {
    'x64' { $PlatformArchitecture = 'x86_64'; $Target = 'x86_64-pc-windows-msvc' }
    'arm64' { $PlatformArchitecture = 'aarch64'; $Target = 'aarch64-pc-windows-msvc' }
    default { throw "unsupported architecture: $Architecture; supported architectures are x64 and ARM64" }
}

if ($System) {
    $Identity = [System.Security.Principal.WindowsIdentity]::GetCurrent()
    $Principal = New-Object System.Security.Principal.WindowsPrincipal($Identity)
    if (-not $Principal.IsInRole([System.Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw '-System requires an elevated PowerShell session'
    }
    $InstallDir = Join-Path $env:ProgramFiles 'codex-start\bin'
} elseif (-not $InstallDir) {
    $InstallDir = if ($env:CODEX_START_INSTALL_DIR) {
        $env:CODEX_START_INSTALL_DIR
    } else {
        Join-Path $env:LOCALAPPDATA 'Programs\codex-start\bin'
    }
}
$InstallDir = [System.IO.Path]::GetFullPath($InstallDir)
$Destination = Join-Path $InstallDir 'codex-start.exe'
$FreshInstall = -not (Test-Path -LiteralPath $Destination)
if (Get-PreservedSignaturePolicy -Executable $Destination) {
    $RequireSignature = $true
}

try {
    $TemporaryDirectory = Join-Path ([System.IO.Path]::GetTempPath()) ("codex-start-install-" + [Guid]::NewGuid().ToString('N'))
    [System.IO.Directory]::CreateDirectory($TemporaryDirectory) | Out-Null

    if ($Version) {
        if ($Version.StartsWith('v')) { $Tag = $Version; $ReleaseVersion = $Version.Substring(1) }
        else { $Tag = "v$Version"; $ReleaseVersion = $Version }
    } else {
        $LatestPath = Join-Path $TemporaryDirectory 'latest.json'
        Receive-File -Uri $LatestReleaseUrl -Destination $LatestPath -MaximumBytes $MaxMetadataBytes
        $Latest = Get-Content -LiteralPath $LatestPath -Raw | ConvertFrom-Json
        $Tag = [string] $Latest.tag_name
        if (-not $Tag.StartsWith('v')) { throw 'latest release metadata has no valid tag_name' }
        $ReleaseVersion = $Tag.Substring(1)
    }
    if ($Tag -notmatch '^v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$') {
        throw "release tag is not a v-prefixed semantic version: $Tag"
    }

    $ReleaseUrl = "$ReleaseDownloadBase/$Tag"
    $ChecksumsPath = Join-Path $TemporaryDirectory 'SHA256SUMS'
    Receive-File -Uri "$ReleaseUrl/SHA256SUMS" -Destination $ChecksumsPath -MaximumBytes $MaxMetadataBytes

    $Cosign = Get-CosignCommand
    if ($Cosign) {
        $ChecksumsBundle = Join-Path $TemporaryDirectory 'SHA256SUMS.bundle'
        Receive-File -Uri "$ReleaseUrl/SHA256SUMS.bundle" -Destination $ChecksumsBundle -MaximumBytes $MaxMetadataBytes
        Confirm-SigstoreBundle -Cosign $Cosign -Path $ChecksumsPath -Bundle $ChecksumsBundle -Tag $Tag
    } elseif ($RequireSignature) {
        throw '-RequireSignature was used, but cosign is not installed'
    } else {
        Write-WarningMessage 'cosign is unavailable; continuing with mandatory SHA-256 verification'
    }

    $ManifestPath = Join-Path $TemporaryDirectory 'release-manifest.json'
    Receive-File -Uri "$ReleaseUrl/release-manifest.json" -Destination $ManifestPath -MaximumBytes $MaxMetadataBytes
    Confirm-Checksum -Path $ManifestPath -Filename 'release-manifest.json' -ChecksumFile $ChecksumsPath | Out-Null
    $Manifest = Get-Content -LiteralPath $ManifestPath -Raw | ConvertFrom-Json
    if ([int] $Manifest.schema_version -ne 1) { throw "unsupported release manifest schema: $($Manifest.schema_version)" }
    if ([string] $Manifest.version -cne $ReleaseVersion -or [string] $Manifest.tag -cne $Tag) {
        throw "release manifest metadata does not match $Tag"
    }

    $Artifacts = @($Manifest.artifacts | Where-Object {
        $_.kind -ceq 'archive' -and $_.os -ceq 'windows' -and
        $_.arch -ceq $PlatformArchitecture -and $null -eq $_.libc
    })
    if ($Artifacts.Count -ne 1) {
        throw "release manifest does not contain exactly one Windows/$PlatformArchitecture archive"
    }
    $Artifact = $Artifacts[0]
    $ArtifactName = [string] $Artifact.filename
    $ArtifactBundleName = [string] $Artifact.bundle
    $ArtifactSbomName = [string] $Artifact.sbom
    Assert-SafeAssetName $ArtifactName
    Assert-SafeAssetName $ArtifactBundleName
    Assert-SafeAssetName $ArtifactSbomName
    [long] $ArtifactSize = $Artifact.size
    if ($ArtifactSize -le 0) { throw 'release manifest artifact size must be positive' }
    if ($ArtifactSize -gt $MaxArtifactBytes) { throw "release artifact exceeds the $MaxArtifactBytes byte safety limit" }
    $ManifestSha256 = [string] $Artifact.sha256
    if ($ManifestSha256 -notmatch '^[0-9A-Fa-f]{64}$') { throw 'release manifest artifact checksum is invalid' }

    $ArtifactPath = Join-Path $TemporaryDirectory $ArtifactName
    Receive-File -Uri "$ReleaseUrl/$ArtifactName" -Destination $ArtifactPath -MaximumBytes $ArtifactSize
    if ((Get-Item -LiteralPath $ArtifactPath).Length -ne $ArtifactSize) {
        throw "downloaded size does not match release manifest for $ArtifactName"
    }
    $VerifiedSha256 = Confirm-Checksum -Path $ArtifactPath -Filename $ArtifactName -ChecksumFile $ChecksumsPath
    if ($VerifiedSha256 -cne $ManifestSha256.ToLowerInvariant()) {
        throw "release manifest and SHA256SUMS disagree for $ArtifactName"
    }
    if ($Cosign) {
        $ArtifactBundlePath = Join-Path $TemporaryDirectory $ArtifactBundleName
        Receive-File -Uri "$ReleaseUrl/$ArtifactBundleName" -Destination $ArtifactBundlePath -MaximumBytes $MaxMetadataBytes
        Confirm-SigstoreBundle -Cosign $Cosign -Path $ArtifactPath -Bundle $ArtifactBundlePath -Tag $Tag
    }

    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $Archive = [System.IO.Compression.ZipFile]::OpenRead($ArtifactPath)
    try {
        $ExecutableEntries = @($Archive.Entries | Where-Object {
            $_.FullName -cmatch '^[A-Za-z0-9._+-]+/codex-start\.exe$' -and $_.Name -ceq 'codex-start.exe'
        })
        if ($ExecutableEntries.Count -ne 1) {
            throw 'archive must contain exactly one safe codex-start.exe path'
        }
        [System.IO.Directory]::CreateDirectory($InstallDir) | Out-Null
        $InstallDirectoryItem = Get-Item -LiteralPath $InstallDir
        if ($InstallDirectoryItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {
            throw "refusing a reparse-point installation directory: $InstallDir"
        }
        if ((Test-Path -LiteralPath $Destination) -and -not (Test-Path -LiteralPath $Destination -PathType Leaf)) {
            throw "destination is not a regular file: $Destination"
        }
        if (Test-Path -LiteralPath $Destination -PathType Leaf) {
            $Existing = Get-Item -LiteralPath $Destination
            if ($Existing.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {
                throw "refusing to replace a reparse-point destination: $Destination"
            }
            if ($Existing.IsReadOnly) {
                if (-not $Force) { throw "destination is read-only; use -Force to replace it: $Destination" }
                $Existing.IsReadOnly = $false
            }
        }
        $StagedDestination = "$Destination.tmp.$PID"
        $EntryStream = $ExecutableEntries[0].Open()
        try {
            if ($ExecutableEntries[0].Length -gt $MaxExecutableBytes) {
                throw "archive executable exceeds the $MaxExecutableBytes byte safety limit"
            }
            $DestinationStream = New-Object System.IO.FileStream(
                $StagedDestination,
                [System.IO.FileMode]::CreateNew,
                [System.IO.FileAccess]::Write,
                [System.IO.FileShare]::None
            )
            try {
                $CopyBuffer = New-Object byte[] 65536
                [long] $Copied = 0
                while (($CopyRead = $EntryStream.Read($CopyBuffer, 0, $CopyBuffer.Length)) -gt 0) {
                    $Copied += $CopyRead
                    if ($Copied -gt $MaxExecutableBytes) { throw 'archive executable exceeded the size safety limit' }
                    $DestinationStream.Write($CopyBuffer, 0, $CopyRead)
                }
                $DestinationStream.Flush($true)
            }
            finally { $DestinationStream.Dispose() }
        } finally {
            $EntryStream.Dispose()
        }
    } finally {
        $Archive.Dispose()
    }
    Move-FileAtomically -Source $StagedDestination -Destination $Destination
    $StagedDestination = $null

    $AutoUpdateChoice = $null
    if ($AutoUpdates) { $AutoUpdateChoice = $true }
    elseif ($NoAutoUpdates) { $AutoUpdateChoice = $false }
    elseif ($FreshInstall) {
        if ($Yes -or [Console]::IsInputRedirected -or -not [Environment]::UserInteractive) {
            $AutoUpdateChoice = $true
        } else {
            $Answer = Read-Host 'Enable automatic update checks? [Y/n]'
            if (-not $Answer -or $Answer -match '^(?i:y|yes)$') { $AutoUpdateChoice = $true }
            elseif ($Answer -match '^(?i:n|no)$') { $AutoUpdateChoice = $false }
            else { throw 'please answer yes or no' }
        }
    }

    Write-InstallReceipt -Method 'portable' -Target $Target -Executable $Destination
    if ($null -ne $AutoUpdateChoice) {
        Set-AutoUpdatePreference -Executable $Destination -Enabled $AutoUpdateChoice
    }
    if (-not $env:CODEX_START_SKIP_PATH_UPDATE) {
        $PathScope = if ($System) { 'Machine' } else { 'User' }
        Add-PathEntry -Directory $InstallDir -Scope $PathScope
    }

    Write-Output "Installed codex-start $ReleaseVersion to $Destination"
} finally {
    if ($StagedDestination) { Remove-Item -LiteralPath $StagedDestination -Force -ErrorAction SilentlyContinue }
    if ($TemporaryDirectory) { Remove-Item -LiteralPath $TemporaryDirectory -Recurse -Force -ErrorAction SilentlyContinue }
}
