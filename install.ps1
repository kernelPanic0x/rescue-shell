# Requires -Version 3.0
$ErrorActionPreference = 'Stop'

# --- Output Helpers ---
function Write-Log {
    param([string]$Message)
    [Console]::Error.WriteLine("[*] $Message")
}

function Write-Die {
    param([string]$Message)
    [Console]::Error.WriteLine("[!] $Message")
    throw $Message
}

# --- Force Modern TLS Support ---
# Windows PowerShell 5.1 defaults to TLS 1.0 which GitHub rejects.
try {
    [System.Net.ServicePointManager]::SecurityProtocol = [System.Net.ServicePointManager]::SecurityProtocol -bor `
        [System.Net.SecurityProtocolType]::Tls12 -bor `
        [System.Net.SecurityProtocolType]::Tls13
} catch {
    [System.Net.ServicePointManager]::SecurityProtocol = [System.Net.ServicePointManager]::SecurityProtocol -bor `
        [System.Net.SecurityProtocolType]::Tls12
}

# --- Architecture Detection ---
# Check PROCESSOR_ARCHITEW6432 first to identify the true host architecture
# when running under 32-bit or x64 emulation on Windows on ARM.
$nativeArch = $env:PROCESSOR_ARCHITEW6432
if (-not $nativeArch) {
    $nativeArch = $env:PROCESSOR_ARCHITECTURE
}

$TUPLE = switch -Regex ($nativeArch) {
    '^(ARM64|AARCH64)$'    { 'aarch64-pc-windows-msvc' }
    '^(AMD64|x86_64|X64)$' { 'x86_64-pc-windows-msvc' }
    default {
        Write-Die "unsupported Windows architecture: $nativeArch (only x86_64 and arm64 are supported)"
    }
}

Write-Log "target: $TUPLE"

# --- Temp Directory Discovery ---
$TARGET_DIR = $env:TEMP
if (-not $TARGET_DIR -or -not (Test-Path -LiteralPath $TARGET_DIR)) {
    $TARGET_DIR = [System.IO.Path]::GetTempPath()
}
Write-Log "target dir: $TARGET_DIR"

$BIN = [System.IO.Path]::Combine($TARGET_DIR, "rescue-shell-$([System.Guid]::NewGuid().ToString('N')).exe")
$URL = "https://github.com/kernelPanic0x/rescue-shell/releases/download/latest/rescue-shell-${TUPLE}.exe"

# --- Download Helper ---
function Download-Binary {
    param(
        [string]$DownloadUrl,
        [string]$DestinationPath
    )
    
    $webClient = New-Object System.Net.WebClient
    try {
        $webClient.Headers.Add('User-Agent', 'rescue-shell-installer')
        $webClient.DownloadFile($DownloadUrl, $DestinationPath)
    }
    catch {
        Write-Die "download failed (no build for ${TUPLE}?): $($_.Exception.Message)"
    }
    finally {
        $webClient.Dispose()
    }
}

# --- Main Execution ---
try {
    Write-Log "downloading $URL"
    Download-Binary -DownloadUrl $URL -DestinationPath $BIN

    if (-not $env:WORMHOLE_RELAY_URL) {
        $env:WORMHOLE_RELAY_URL = 'tcp://nbg.ell.dns64.de:4001'
    }

    if ($args -and $args.Count -gt 0) {
        [string[]]$execArgs = $args
    } else {
        [string[]]$execArgs = @('serve')
    }

    & $BIN @execArgs
}
finally {
    if (Test-Path -LiteralPath $BIN) {
        Remove-Item -LiteralPath $BIN -ErrorAction SilentlyContinue
    }
}
