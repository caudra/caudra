#Requires -Version 5.1
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$installer = Join-Path $PSScriptRoot '../install.ps1'
$source = [IO.File]::ReadAllText($installer)
$entry = 'Invoke-CaudraInstaller $args'
if (-not $source.TrimEnd().EndsWith($entry)) { throw 'installer entry point changed' }
. ([scriptblock]::Create($source.Substring(0, $source.LastIndexOf($entry))))

Add-Type -TypeDefinition @'
public class InstallerFixtureStream : System.IO.MemoryStream {
    public long BytesRead;
    public InstallerFixtureStream(byte[] bytes) : base(bytes) { }
    public override int Read(byte[] buffer, int offset, int count) {
        int result = base.Read(buffer, offset, count);
        BytesRead += result;
        return result;
    }
}
'@

function Open-ReleaseResponse([string]$Uri, [hashtable]$Headers, [int]$TimeoutMilliseconds) {
    $api = ([uri]$Uri).Host -eq 'api.github.com'
    $limit = if ($api) { 30000 } else { 300000 }
    if ($TimeoutMilliseconds -le 0 -or $TimeoutMilliseconds -gt $limit) { throw 'unbounded request timeout' }
    if ($api -and $env:GITHUB_TOKEN -and $Headers.Authorization -cne "Bearer $env:GITHUB_TOKEN") {
        throw 'missing API token'
    }
    if (-not $api -and $Headers.ContainsKey('Authorization')) { throw 'asset request leaked API token' }
    $result = & $env:TEST_PYTHON (Join-Path $PSScriptRoot 'test-install-http.py') --response $Uri
    if ($LASTEXITCODE -ne 0) { throw 'fixture response failed' }
    $fixture = $result | ConvertFrom-Json
    if ($fixture.status -eq 0) { throw 'fixture timeout' }
    $response = [pscustomobject]@{
        Uri = $Uri
        StatusCode = $fixture.status
        ContentLength = $fixture.length
        Headers = @{ Location = $fixture.location }
        Stream = [InstallerFixtureStream]::new([Convert]::FromBase64String($fixture.content))
        Opened = $false
        Authenticated = $Headers.ContainsKey('Authorization')
    }
    $response | Add-Member ScriptMethod GetResponseStream { $this.Opened = $true; return $this.Stream }
    $response | Add-Member ScriptMethod Dispose {
        $record = @{ uri = $this.Uri; bytes_read = $this.Stream.BytesRead; opened = $this.Opened; authenticated = $this.Authenticated }
        [IO.File]::AppendAllText($env:TEST_REQUEST_LOG, ($record | ConvertTo-Json -Compress) + "`n")
        $this.Stream.Dispose()
    }
    return $response
}

function Expand-Archive($Path, $DestinationPath, [switch]$Force) {
    [IO.File]::WriteAllText((Join-Path (Split-Path $env:TEST_SCENARIO -Parent) 'extracted'), '')
    Microsoft.PowerShell.Archive\Expand-Archive -Path $Path -DestinationPath $DestinationPath -Force:$Force
}

function Move-Item($LiteralPath, $Destination) {
    if ($env:TEST_PUBLISH_FAILURE -eq '1' -and
        $Destination -eq (Join-Path $env:CAUDRA_INSTALL_DIR 'caudra.exe') -and
        (Split-Path $LiteralPath -Leaf) -eq 'new') { throw 'injected binary publication failure' }
    Microsoft.PowerShell.Management\Move-Item -LiteralPath $LiteralPath -Destination $Destination
}

function Add-ToUserPath([string]$Dir) { }

$arguments = ConvertFrom-Json -InputObject ('{"arguments":' + [IO.File]::ReadAllText($env:TEST_ARGUMENTS) + '}')
Invoke-CaudraInstaller $arguments.arguments
