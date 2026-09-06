# Exercise the HTTPS download branch with real HTTP types and an in-memory handler.
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Net.Http
if (-not ('CodexStartDownloadFixtureHandler' -as [type])) {
    Add-Type -ReferencedAssemblies System.Net.Http -TypeDefinition @'
using System.Net.Http;
using System.Threading;
using System.Threading.Tasks;
public sealed class CodexStartDownloadFixtureHandler : HttpClientHandler {
    protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken token) {
        return Task.FromResult(new HttpResponseMessage() {
            RequestMessage = request,
            Content = new ByteArrayContent(new byte[] { 1, 2, 3 })
        });
    }
}
'@
}
$InstallerPath = Join-Path (Join-Path (Join-Path $PSScriptRoot '..') '..') 'install.ps1'
$Tokens = $null
$Errors = $null
$Ast = [System.Management.Automation.Language.Parser]::ParseFile($InstallerPath, [ref] $Tokens, [ref] $Errors)
if ($Errors.Count) { throw ($Errors | Out-String) }
foreach ($Name in @('Assert-SafeDownloadUri', 'Receive-File')) {
    $Definition = $Ast.Find({
        param($Node)
        $Node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $Node.Name -eq $Name
    }, $false)
    # Replace only the transport; retain the production download and size checks.
    Invoke-Expression ($Definition.Extent.Text.Replace('New-Object System.Net.Http.HttpClientHandler', 'New-Object CodexStartDownloadFixtureHandler'))
}
$Directory = Join-Path ([System.IO.Path]::GetTempPath()) ([Guid]::NewGuid().ToString('N'))
[System.IO.Directory]::CreateDirectory($Directory) | Out-Null
try {
    $Destination = Join-Path $Directory 'download'
    Receive-File -Uri 'https://example.invalid/artifact' -Destination $Destination -MaximumBytes 3
    if (([System.IO.File]::ReadAllBytes($Destination) -join ',') -ne '1,2,3') { throw 'HTTPS response body was not saved' }
    $Rejected = $false
    try { Receive-File -Uri 'https://example.invalid/artifact' -Destination $Destination -MaximumBytes 2 }
    catch { $Rejected = $_.Exception.Message -like '*byte limit*' }
    if (-not $Rejected) { throw 'oversized HTTPS response was not rejected by its length' }
    if (Test-Path -LiteralPath "$Destination.part") { throw 'failed download left a partial file' }
    Write-Output 'test-download-ps1: all tests passed'
} finally {
    Remove-Item -LiteralPath $Directory -Recurse -Force
}
