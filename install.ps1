# =============================================================================
# WebDep one-line installer for Windows (PowerShell 5.1+)
#
#   Menu (install / update / status / uninstall):
#     irm https://raw.githubusercontent.com/ded-lmfao/WebDep/main/install.ps1 | iex
#
#   Direct, no questions:
#     & ([scriptblock]::Create((irm https://raw.githubusercontent.com/ded-lmfao/WebDep/main/install.ps1))) both
#     & ([scriptblock]::Create((irm https://raw.githubusercontent.com/ded-lmfao/WebDep/main/install.ps1))) core
#     & ([scriptblock]::Create((irm https://raw.githubusercontent.com/ded-lmfao/WebDep/main/install.ps1))) node -Join http://<CORE_IP>:4600 -Token <TOKEN>
#
# Options: -Dir <path> (default %LOCALAPPDATA%\WebDep)  -Branch <name>  -Repo <owner/name>
#          -Name <node name>  -Region <label>  -PublicIp <ip>
#          -SourceDir <local checkout>  build from an existing local checkout instead of downloading
#
# Re-running repairs/updates an install; data\ and .env are kept.
# =============================================================================
param(
    [Parameter(Position = 0)][string]$Action = "",
    [string]$Dir = "",
    [string]$Branch = "main",
    [string]$Repo = "ded-lmfao/WebDep",
    [string]$Join = "",
    [string]$Token = "",
    [string]$Name = "",
    [string]$Region = "",
    [string]$PublicIp = "",
    [string]$SourceDir = ""
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"   # Invoke-WebRequest is very slow with the progress bar on 5.1
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

function Say($msg)  { Write-Host "[webdep] $msg" -ForegroundColor Cyan }
function Warn($msg) { Write-Host "[webdep] $msg" -ForegroundColor Yellow }
function Fail($msg) { Write-Host "[webdep] ERROR: $msg" -ForegroundColor Red; throw "webdep install failed: $msg" }
function Ask($question, $default = "") {
    $prompt = if ($default) { "  $question [$default]" } else { "  $question" }
    $answer = Read-Host $prompt
    if ([string]::IsNullOrWhiteSpace($answer)) { return $default }
    return $answer.Trim()
}

$TaskName = "WebDep"
if (-not $Dir) { $Dir = Join-Path $env:LOCALAPPDATA "WebDep" }
$Action = $Action.ToLower().TrimStart('-')
if ($Action -eq "all") { $Action = "both" }

function Get-Status {
    $procs = Get-Process -Name "webdep" -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -and $_.Path.StartsWith($Dir, [StringComparison]::OrdinalIgnoreCase) }
    $task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    if (-not (Test-Path (Join-Path $Dir "webdep.exe"))) { Say "WebDep is not installed in $Dir."; return }
    Say ("Installed in {0}" -f $Dir)
    Say ("Running:   {0}" -f $(if ($procs) { "yes (PID $($procs.Id -join ', '))" } else { "no" }))
    Say ("Autostart: {0}" -f $(if ($task) { "yes (at logon)" } else { "no" }))
}

function Stop-WebDep {
    Get-Process -Name "webdep" -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -and $_.Path.StartsWith($Dir, [StringComparison]::OrdinalIgnoreCase) } |
        Stop-Process -Force -ErrorAction SilentlyContinue
}

# WebDep's private PostgreSQL (data\control-pg) keeps running when webdep.exe is killed, by design,
# so restarts reuse it. A full stop or uninstall shuts it down cleanly.
function Stop-ControlPostgres {
    $pgdata = Join-Path $Dir "data\control-pg\pgdata"
    if (-not (Test-Path (Join-Path $pgdata "postmaster.pid"))) { return }
    $bin = Find-PostgresBin
    if (-not $bin) { Warn "Could not find pg_ctl.exe to stop the database in $pgdata."; return }
    Invoke-Native (Join-Path $bin "pg_ctl.exe") @("-D", $pgdata, "-m", "fast", "stop") | Out-Null
}

function Uninstall-WebDep {
    Stop-WebDep
    Stop-ControlPostgres
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction SilentlyContinue
    Say "WebDep stopped and autostart removed. Your files are still in $Dir (delete that folder to remove everything)."
}

function Test-Command($cmd) { [bool](Get-Command $cmd -ErrorAction SilentlyContinue) }

# Run a native program and return its exit code. Windows PowerShell 5.1 turns a program's normal
# stderr output (cargo/winget progress) into terminating errors under ErrorActionPreference=Stop,
# so relax it for the call and print everything to the console.
function Invoke-Native {
    param([string]$Exe, [string[]]$ArgList = @())
    $old = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try { & $Exe @ArgList 2>&1 | ForEach-Object { Write-Host $_ } } finally { $ErrorActionPreference = $old }
    return $LASTEXITCODE
}

function Install-WithWinget($id, $extra = @()) {
    if (-not (Test-Command "winget")) { Fail "winget is not available. Install '$id' manually, then run this again." }
    Say "Installing $id (Windows may ask for permission)..."
    $code = Invoke-Native "winget" (@("install", "-e", "--id", $id, "--silent", "--accept-package-agreements", "--accept-source-agreements") + $extra)
    if ($code -ne 0 -and $code -ne -1978335189) {   # -1978335189 = already installed
        Fail "winget could not install $id (exit $code)."
    }
}

function Find-PostgresBin {
    if ($env:PG_BIN_DIR -and (Test-Path (Join-Path $env:PG_BIN_DIR "pg_ctl.exe"))) { return $env:PG_BIN_DIR }
    foreach ($base in @("C:\Program Files\PostgreSQL", "C:\Program Files (x86)\PostgreSQL")) {
        if (Test-Path $base) {
            $hit = Get-ChildItem $base -Directory | Sort-Object Name -Descending |
                Where-Object { Test-Path (Join-Path $_.FullName "bin\pg_ctl.exe") } | Select-Object -First 1
            if ($hit) { return (Join-Path $hit.FullName "bin") }
        }
    }
    $onPath = Get-Command "pg_ctl.exe" -ErrorAction SilentlyContinue
    if ($onPath) { return (Split-Path $onPath.Source) }
    return $null
}

function Ensure-Postgres {
    if (Find-PostgresBin) { return }
    # WebDep only needs the PostgreSQL programs (it runs its own private database in data\control-pg).
    $pw = -join ((48..57) + (65..90) + (97..122) | Get-Random -Count 24 | ForEach-Object { [char]$_ })
    Install-WithWinget "PostgreSQL.PostgreSQL.17" @("--override", "--mode unattended --unattendedmodeui none --superpassword $pw")
    if (-not (Find-PostgresBin)) { Fail "PostgreSQL was installed but pg_ctl.exe was not found. Set PG_BIN_DIR to its bin folder." }
}

function Refresh-Path {
    $env:Path = [Environment]::GetEnvironmentVariable("Path", "Machine") + ";" + [Environment]::GetEnvironmentVariable("Path", "User")
    $cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
    if (Test-Path $cargoBin) { $env:Path = "$cargoBin;$env:Path" }
}

function Ensure-Rust {
    Refresh-Path
    if (-not (Test-Command "cargo")) {
        Install-WithWinget "Rustlang.Rustup"
        Refresh-Path
        if (-not (Test-Command "cargo")) { Fail "Rust was installed but 'cargo' is not on PATH yet. Open a new PowerShell window and run this again." }
    }
    # The MSVC linker is required to compile on Windows.
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    $hasVc = (Test-Path $vswhere) -and (& $vswhere -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath)
    if (-not $hasVc) {
        Install-WithWinget "Microsoft.VisualStudio.2022.BuildTools" @("--override", "--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended")
    }
}

function Build-FromSource($src) {
    Ensure-Rust
    Say "Compiling WebDep from source in $src (first build takes 10-20 minutes)..."
    Push-Location $src
    try {
        if ((Invoke-Native "cargo" @("build", "--release")) -ne 0) { Fail "Build failed (see messages above)." }
    } finally { Pop-Location }
    return (Join-Path $src "target\release\webdep.exe")
}

function Get-WebDepBinary {
    if ($SourceDir) {
        $src = (Resolve-Path $SourceDir).Path
        return (Build-FromSource $src)
    }
    $tmp = Join-Path $env:TEMP ("webdep-install-" + [Guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Force -Path $tmp | Out-Null

    if ($Branch -eq "main") {
        $asset = "webdep-windows-x86_64.zip"
        $base = "https://github.com/$Repo/releases/latest/download"
        try {
            Say "Downloading the latest prebuilt release..."
            Invoke-WebRequest -UseBasicParsing "$base/checksums.txt" -OutFile "$tmp\checksums.txt"
            Invoke-WebRequest -UseBasicParsing "$base/$asset" -OutFile "$tmp\$asset"
            $line = Get-Content "$tmp\checksums.txt" | Where-Object { $_ -match [Regex]::Escape($asset) + '$' } | Select-Object -First 1
            if (-not $line) { throw "no checksum listed for $asset" }
            $expected = ($line -split '\s+')[0].ToLower()
            $actual = (Get-FileHash "$tmp\$asset" -Algorithm SHA256).Hash.ToLower()
            if ($expected -ne $actual) { throw "SHA-256 mismatch (expected $expected, got $actual)" }
            Say "SHA-256 verified."
            Expand-Archive "$tmp\$asset" -DestinationPath "$tmp\bin" -Force
            return (Join-Path "$tmp\bin" "webdep.exe")
        } catch {
            Warn "No Windows release download available ($($_.Exception.Message)). Building from source instead."
        }
    }

    Say "Downloading source code ($Repo@$Branch)..."
    Invoke-WebRequest -UseBasicParsing "https://github.com/$Repo/archive/refs/heads/$Branch.zip" -OutFile "$tmp\src.zip"
    Expand-Archive "$tmp\src.zip" -DestinationPath "$tmp\src" -Force
    $srcRoot = (Get-ChildItem "$tmp\src" -Directory | Select-Object -First 1).FullName
    return (Build-FromSource $srcRoot)
}

function Write-Launcher($flags) {
    # start-webdep.ps1 owns "stop old copy, start hidden, log to data\logs". Installer and autostart both call it.
    $d = $Dir -replace "'", "''"
    $argPart = if ($flags) { "-ArgumentList '$flags' " } else { "" }   # Start-Process rejects an empty -ArgumentList
    $launcher = @"
`$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath '$d'
Get-Process -Name webdep -ErrorAction SilentlyContinue | Where-Object { `$_.Path -eq '$d\webdep.exe' } | Stop-Process -Force
New-Item -ItemType Directory -Force -Path 'data\logs' | Out-Null
Start-Process -FilePath '$d\webdep.exe' $argPart-WorkingDirectory '$d' -WindowStyle Hidden -RedirectStandardOutput 'data\logs\webdep.out.log' -RedirectStandardError 'data\logs\webdep.err.log'
"@
    Set-Content -Path (Join-Path $Dir "start-webdep.ps1") -Value $launcher -Encoding UTF8
    $pgCtl = Find-PostgresBin
    $pgStop = if ($pgCtl) {
        "if (Test-Path '$d\data\control-pg\pgdata\postmaster.pid') { & '$($pgCtl -replace "'", "''")\pg_ctl.exe' -D '$d\data\control-pg\pgdata' -m fast stop }"
    } else { "" }
    $stop = @"
Get-Process -Name webdep -ErrorAction SilentlyContinue | Where-Object { `$_.Path -eq '$d\webdep.exe' } | Stop-Process -Force
$pgStop
"@
    Set-Content -Path (Join-Path $Dir "stop-webdep.ps1") -Encoding UTF8 -Value $stop
}

function Start-Launcher {
    # Start-Process (not "& powershell.exe"): the server must not inherit this console's output
    # handles, or the installer would wait on them for as long as WebDep runs.
    $ps = "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe"
    $proc = Start-Process -FilePath $ps -WindowStyle Hidden -PassThru -ArgumentList @(
        "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "`"$(Join-Path $Dir 'start-webdep.ps1')`"")
    $proc.WaitForExit()   # waits for the launcher only (-Wait would also wait for the server it starts)
}

function Register-Autostart {
    try {
        $ps = "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe"
        $act = New-ScheduledTaskAction -Execute $ps -Argument "-NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File `"$Dir\start-webdep.ps1`""
        $trg = New-ScheduledTaskTrigger -AtLogOn -User "$env:USERDOMAIN\$env:USERNAME"
        $set = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero)
        Register-ScheduledTask -TaskName $TaskName -Action $act -Trigger $trg -Settings $set -Force | Out-Null
        Say "Autostart enabled: WebDep starts when you log in."
    } catch {
        Warn "Could not enable autostart ($($_.Exception.Message)). Start it yourself with: $Dir\start-webdep.ps1"
    }
}

function Install-WebDep($role) {
    if ($role -eq "node" -and (-not $Join -or -not $Token)) {
        Fail "Node install needs -Join <dashboard URL> and -Token <token>. Get both from the dashboard: Nodes -> + Add Node."
    }
    Say "Installing WebDep ($role) into $Dir"
    New-Item -ItemType Directory -Force -Path $Dir | Out-Null

    if ($role -ne "node") { Ensure-Postgres }

    $bin = Get-WebDepBinary
    Stop-WebDep
    Copy-Item $bin (Join-Path $Dir "webdep.exe") -Force
    if ((Invoke-Native (Join-Path $Dir "webdep.exe") @("--version")) -ne 0) { Fail "The installed webdep.exe does not start." }

    $flags = switch ($role) { "core" { "--core" } "node" { "--node" } default { "" } }
    if ($role -eq "node") {
        $enroll = @("--node", "--join", $Join, "--token", $Token, "--enroll-only")
        if ($Name) { $enroll += @("--name", $Name) }
        if ($Region) { $enroll += @("--region", $Region) }
        if ($PublicIp) { $enroll += @("--public-ip", $PublicIp) }
        Push-Location $Dir
        try {
            if ((Invoke-Native (Join-Path $Dir "webdep.exe") $enroll) -ne 0) { Fail "Enrollment failed. Check the dashboard URL and that the token is new and unused." }
        } finally { Pop-Location }
    }

    Write-Launcher $flags
    Start-Launcher
    Register-Autostart

    Write-Host ""
    Say "WebDep is running. Installed in $Dir"
    if ($role -ne "node") {
        Start-Sleep -Seconds 4
        $port = "4600"
        $envFile = Join-Path $Dir ".env"
        if (Test-Path $envFile) {
            $m = Select-String -Path $envFile -Pattern '^PORT=(\d+)' | Select-Object -First 1
            if ($m) { $port = $m.Matches[0].Groups[1].Value }
        }
        Say "Dashboard:      http://127.0.0.1:$port"
        Say "First login:    see ADMIN_USERNAME / ADMIN_PASSWORD in $envFile"
        Start-Process "http://127.0.0.1:$port"
    }
    Say "Logs:           $Dir\data\logs"
    Say "Stop / start:   $Dir\stop-webdep.ps1  /  $Dir\start-webdep.ps1"
}

# ---------------------------------------------------------------------------- menu
if (-not $Action) {
    Write-Host ""
    Write-Host "  ===============================================" -ForegroundColor Cyan
    Write-Host "            WebDep - Easy Installer (Windows)"     -ForegroundColor Cyan
    Write-Host "  ===============================================" -ForegroundColor Cyan
    Write-Host "    1) Install Both  - dashboard + host websites here (best for 1 PC)"
    Write-Host "    2) Install Core  - dashboard only"
    Write-Host "    3) Install Node  - join an existing dashboard"
    Write-Host "    4) Update        - get the latest version"
    Write-Host "    5) Status        - show whether WebDep is running"
    Write-Host "    6) Uninstall     - stop WebDep and remove autostart"
    Write-Host "    0) Exit"
    Write-Host ""
    $choice = Ask "Choose an option" "1"
    $Action = switch ($choice) {
        "1" { "both" } "2" { "core" } "3" { "node" } "4" { "update" }
        "5" { "status" } "6" { "uninstall" } "0" { "exit" }
        default { Fail "Invalid choice '$choice'." }
    }
    if ($Action -in @("both", "core", "node")) {
        $Dir = Ask "Install folder" $Dir
        if ($Action -eq "node") {
            Write-Host "  (Get the URL and token from your dashboard: Nodes -> + Add Node)"
            if (-not $Join)  { $Join  = Ask "Dashboard URL (e.g. http://1.2.3.4:4600)" }
            if (-not $Token) { $Token = Ask "Join token" }
            if (-not $Name)  { $Name  = Ask "Name for this computer (optional)" $env:COMPUTERNAME }
            if (-not $Region) { $Region = Ask "Region label (optional, e.g. eu-central)" }
        }
    } elseif ($Action -eq "uninstall") {
        if ((Ask "Stop WebDep and remove autostart? Your files stay. Type yes to confirm" "no") -ne "yes") { Say "Cancelled."; $Action = "exit" }
    }
}

switch ($Action) {
    { $_ -in @("both", "core", "node") } { Install-WebDep $Action }
    "update" {
        $launcher = Join-Path $Dir "start-webdep.ps1"
        if (-not (Test-Path $launcher)) { Fail "No WebDep install found in $Dir." }
        $line = Get-Content $launcher | Where-Object { $_ -match "-ArgumentList '([^']*)'" } | Select-Object -First 1
        $flags = if ($line -match "-ArgumentList '([^']*)'") { $Matches[1] } else { "" }
        $bin = Get-WebDepBinary
        Stop-WebDep
        Copy-Item $bin (Join-Path $Dir "webdep.exe") -Force
        Start-Launcher
        Say "Updated and restarted ($((& (Join-Path $Dir 'webdep.exe') --version)))."
    }
    "status" { Get-Status }
    "uninstall" { Uninstall-WebDep }
    "exit" { }
    default { Fail "Unknown option '$Action'. Use: both | core | node | update | status | uninstall" }
}
