#Requires -Version 5.1
# End-to-end fixture tests for install.ps1. No network access is used.
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$RunningOnWindows = $env:OS -eq 'Windows_NT'
$SimulatedWindows = $env:CODEX_START_TEST_WINDOWS -eq '1'
if (-not $RunningOnWindows -and -not $SimulatedWindows) {
    Write-Output 'test-install-ps1: skipped outside Windows'
    exit 0
}

$Root = [System.IO.Path]::GetFullPath((Join-Path (Join-Path $PSScriptRoot '..') '..'))
$Installer = Join-Path $Root 'install.ps1'
$TestRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("codex-start-installer-test-" + [Guid]::NewGuid().ToString('N'))

function Assert-True {
    param([bool] $Condition, [string] $Message)
    if (-not $Condition) { throw "test-install-ps1: $Message" }
}

try {
    $Version = '1.2.3'
    $Tag = "v$Version"
    $ReleaseBase = Join-Path (Join-Path $TestRoot 'releases') 'download'
    $ReleaseDirectory = Join-Path $ReleaseBase $Tag
    $InstallDirectory = Join-Path $TestRoot 'codex start bin'
    $DataDirectory = Join-Path (Join-Path $TestRoot 'data') 'codex-start'
    [System.IO.Directory]::CreateDirectory($ReleaseDirectory) | Out-Null

    $Architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToLowerInvariant()
    switch ($Architecture) {
        'x64' { $ManifestArchitecture = 'x86_64' }
        'arm64' { $ManifestArchitecture = 'aarch64' }
        default {
            Write-Output 'test-install-ps1: skipped on unsupported architecture'
            exit 0
        }
    }

    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $ArtifactName = "codex-start-$Version-fixture.zip"
    $ArtifactPath = Join-Path $ReleaseDirectory $ArtifactName
    $Archive = [System.IO.Compression.ZipFile]::Open($ArtifactPath, [System.IO.Compression.ZipArchiveMode]::Create)
    try {
        $Entry = $Archive.CreateEntry('codex-start-fixture/codex-start.exe')
        $Stream = $Entry.Open()
        try {
            $Bytes = [System.Text.Encoding]::UTF8.GetBytes('fixture executable')
            $Stream.Write($Bytes, 0, $Bytes.Length)
        } finally {
            $Stream.Dispose()
        }
    } finally {
        $Archive.Dispose()
    }
    $ArtifactItem = Get-Item -LiteralPath $ArtifactPath
    $ArtifactSha = (Get-FileHash -LiteralPath $ArtifactPath -Algorithm SHA256).Hash.ToLowerInvariant()

    $Manifest = [ordered]@{
        schema_version = 1
        version = $Version
        tag = $Tag
        artifacts = @(
            [ordered]@{
                kind = 'archive'
                os = 'windows'
                arch = $ManifestArchitecture
                libc = $null
                filename = $ArtifactName
                size = $ArtifactItem.Length
                sha256 = $ArtifactSha
                bundle = "$ArtifactName.bundle"
                sbom = "$ArtifactName.spdx.json"
            }
        )
    }
    $ManifestPath = Join-Path $ReleaseDirectory 'release-manifest.json'
    [System.IO.File]::WriteAllText($ManifestPath, ($Manifest | ConvertTo-Json -Depth 5), (New-Object System.Text.UTF8Encoding($false)))
    $ManifestSha = (Get-FileHash -LiteralPath $ManifestPath -Algorithm SHA256).Hash.ToLowerInvariant()
    [System.IO.File]::WriteAllText(
        (Join-Path $ReleaseDirectory 'SHA256SUMS'),
        "$ArtifactSha  $ArtifactName`n$ManifestSha  release-manifest.json`n",
        (New-Object System.Text.UTF8Encoding($false))
    )
    [System.IO.File]::WriteAllText((Join-Path $ReleaseDirectory 'SHA256SUMS.bundle'), 'fixture checksum bundle')
    [System.IO.File]::WriteAllText((Join-Path $ReleaseDirectory "$ArtifactName.bundle"), 'fixture artifact bundle')
    $LatestPath = Join-Path $TestRoot 'latest.json'
    [System.IO.File]::WriteAllText($LatestPath, "{`"tag_name`":`"$Tag`"}", (New-Object System.Text.UTF8Encoding($false)))

    $ConfigLog = Join-Path $TestRoot 'config.log'
    if ($RunningOnWindows) {
        $ConfigCommand = Join-Path $TestRoot 'config-command.cmd'
        [System.IO.File]::WriteAllText($ConfigCommand, "@echo off`r`necho %5>>`"$ConfigLog`"`r`n", [System.Text.Encoding]::ASCII)
    } else {
        $ConfigCommand = Join-Path $TestRoot 'config-command.sh'
        [System.IO.File]::WriteAllText($ConfigCommand, "#!/bin/sh`nprintf '%s\n' `"`$5`" >>`"$ConfigLog`"`n", [System.Text.Encoding]::ASCII)
        & /bin/chmod 755 $ConfigCommand
    }
    $CosignLog = Join-Path $TestRoot 'cosign.log'
    if ($RunningOnWindows) {
        $CosignCommand = Join-Path $TestRoot 'cosign.cmd'
        [System.IO.File]::WriteAllText($CosignCommand, "@echo off`r`necho %*>>`"$CosignLog`"`r`n", [System.Text.Encoding]::ASCII)
    } else {
        $CosignCommand = Join-Path $TestRoot 'cosign.sh'
        [System.IO.File]::WriteAllText($CosignCommand, "#!/bin/sh`nprintf '%s\n' `"`$*`" >>`"$CosignLog`"`n", [System.Text.Encoding]::ASCII)
        & /bin/chmod 755 $CosignCommand
    }

    $LatestUri = New-Object System.Uri($LatestPath)
    $ReleaseBaseUri = New-Object System.Uri(($ReleaseBase + [System.IO.Path]::DirectorySeparatorChar))
    $env:CODEX_START_LATEST_RELEASE_URL = $LatestUri.AbsoluteUri
    $env:CODEX_START_RELEASE_DOWNLOAD_BASE = $ReleaseBaseUri.AbsoluteUri.TrimEnd('/')
    $env:CODEX_START_INSTALL_DIR = $InstallDirectory
    $env:CODEX_START_INSTALLER_TEST_DATA_DIR = $DataDirectory
    $env:CODEX_START_COSIGN = 'none'
    $env:CODEX_START_CONFIG_COMMAND = $ConfigCommand
    $env:CODEX_START_SKIP_PATH_UPDATE = '1'

    $StrictFailed = $false
    try { & $Installer -Yes -RequireSignature | Out-Null } catch { $StrictFailed = $true }
    Assert-True $StrictFailed '-RequireSignature succeeded without Cosign'
    Assert-True (-not (Test-Path -LiteralPath (Join-Path $InstallDirectory 'codex-start.exe'))) 'failed strict install wrote an executable'

    $env:CODEX_START_COSIGN = $CosignCommand
    $env:CODEX_START_INSTALL_DIR = Join-Path $TestRoot 'signed bin'
    & $Installer -Yes -RequireSignature | Out-Null
    Assert-True (Test-Path -LiteralPath (Join-Path $env:CODEX_START_INSTALL_DIR 'codex-start.exe')) 'strict signed install did not write an executable'
    Assert-True (@([System.IO.File]::ReadAllLines($CosignLog)).Count -eq 2) 'Cosign did not verify both checksum and artifact bundles'
    Assert-True (([System.IO.File]::ReadAllText($CosignLog)) -match '--certificate-oidc-issuer https://token.actions.githubusercontent.com') 'Cosign issuer constraint was not supplied'
    $env:CODEX_START_COSIGN = 'none'
    $StrictUpgradeFailed = $false
    try { & $Installer -Yes | Out-Null } catch { $StrictUpgradeFailed = $true }
    Assert-True $StrictUpgradeFailed 'ordinary upgrade silently disabled the existing strict signature policy'
    $env:CODEX_START_COSIGN = 'none'
    $env:CODEX_START_INSTALL_DIR = $InstallDirectory
    Remove-Item -LiteralPath $ConfigLog -Force -ErrorAction SilentlyContinue

    & $Installer -Yes | Out-Null
    $Destination = Join-Path $InstallDirectory 'codex-start.exe'
    Assert-True (Test-Path -LiteralPath $Destination -PathType Leaf) 'portable executable was not installed'
    Assert-True (([System.IO.File]::ReadAllText($Destination)) -eq 'fixture executable') 'installed executable is not the fixture'
    $ConfigContents = ([System.IO.File]::ReadAllText($ConfigLog)).Trim()
    Assert-True ($ConfigContents -eq 'true') "fresh non-interactive install did not enable update checks (got '$ConfigContents')"
    $ReceiptPath = Join-Path $DataDirectory 'install.json'
    Assert-True (Test-Path -LiteralPath $ReceiptPath -PathType Leaf) 'installation receipt was not written'
    $Receipt = Get-Content -LiteralPath $ReceiptPath -Raw | ConvertFrom-Json
    Assert-True ($Receipt.schema_version -eq 1 -and $Receipt.method -eq 'portable') 'installation receipt metadata is incorrect'
    Assert-True ($Receipt.executable -eq [System.IO.Path]::GetFullPath($Destination)) 'receipt executable is incorrect'

    & $Installer -Yes | Out-Null
    Assert-True (@([System.IO.File]::ReadAllLines($ConfigLog)).Count -eq 1) 'upgrade rewrote the update preference'

    & $Installer -Yes -NoAutoUpdates | Out-Null
    Assert-True (([System.IO.File]::ReadAllLines($ConfigLog))[-1] -eq 'false') '-NoAutoUpdates was not persisted'

    $BeforeSha = (Get-FileHash -LiteralPath $Destination -Algorithm SHA256).Hash
    [System.IO.File]::AppendAllText($ArtifactPath, 'tampered')
    $Failed = $false
    try { & $Installer -Yes | Out-Null } catch { $Failed = $true }
    Assert-True $Failed 'tampered artifact was accepted'
    Assert-True ((Get-FileHash -LiteralPath $Destination -Algorithm SHA256).Hash -eq $BeforeSha) 'failed install changed the existing executable'

    Write-Output 'test-install-ps1: all tests passed'
} finally {
    foreach ($Name in @(
        'CODEX_START_LATEST_RELEASE_URL', 'CODEX_START_RELEASE_DOWNLOAD_BASE',
        'CODEX_START_INSTALL_DIR', 'CODEX_START_INSTALLER_TEST_DATA_DIR', 'CODEX_START_COSIGN',
        'CODEX_START_CONFIG_COMMAND', 'CODEX_START_SKIP_PATH_UPDATE'
    )) {
        Remove-Item "Env:$Name" -ErrorAction SilentlyContinue
    }
    Remove-Item -LiteralPath $TestRoot -Recurse -Force -ErrorAction SilentlyContinue
}
