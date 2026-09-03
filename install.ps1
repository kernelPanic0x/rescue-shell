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
try {
    [System.Net.ServicePointManager]::SecurityProtocol = [System.Net.ServicePointManager]::SecurityProtocol -bor `
        [System.Net.SecurityProtocolType]::Tls12 -bor `
        [System.Net.SecurityProtocolType]::Tls13
} catch {
    [System.Net.ServicePointManager]::SecurityProtocol = [System.Net.ServicePointManager]::SecurityProtocol -bor `
        [System.Net.SecurityProtocolType]::Tls12
}

# --- Architecture Detection ---
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

    # Check if already running in an elevated session
    $isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)

    if ($isAdmin) {
        Write-Log "already running with administrator privileges."
        & $BIN @execArgs
    } else {
        try {
            Write-Log "requesting administrator privileges..."

            $escapedBin = $BIN.Replace("'", "''")
            $escapedRelay = $env:WORMHOLE_RELAY_URL.Replace("'", "''")
            $argString = ($execArgs | ForEach-Object { "`"$_`"" }) -join ' '

            # Runs binary, and if it fails/panics, prevents the window from vanishing before you can read the error
            $elevatedCmd = "`$env:WORMHOLE_RELAY_URL = '$escapedRelay'; & '$escapedBin' $argString; if (`$LASTEXITCODE -ne 0) { Write-Host '`nProcess exited with code ' `$LASTEXITCODE -ForegroundColor Red; Read-Host 'Press Enter to exit...' }"

            $bytes = [System.Text.Encoding]::Unicode.GetBytes($elevatedCmd)
            $encodedCmd = [System.Convert]::ToBase64String($bytes)

            Start-Process powershell -Verb RunAs -ArgumentList "-NoProfile -ExecutionPolicy Bypass -EncodedCommand $encodedCmd" -Wait -ErrorAction Stop
        }
        catch {
            # User clicked "No" on UAC prompt
            Write-Log "elevation declined. falling back to normal user privileges..."
            & $BIN @execArgs
        }
    }
}
finally {
    if (Test-Path -LiteralPath $BIN) {
        Remove-Item -LiteralPath $BIN -ErrorAction SilentlyContinue
    }
}
