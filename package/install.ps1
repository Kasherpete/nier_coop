# nier_coop installer: copies dinput8.dll into the NieR:Automata folder and writes nier_coop.cfg.
# -Uninstall removes it and restores a previous dinput8.dll. -Game skips the folder search.
param([switch]$Uninstall, [string]$Game)
$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
$port = 27015

function Find-Game {
    if ($Game) { return $Game }
    $steam = (Get-ItemProperty 'HKCU:\Software\Valve\Steam' -ErrorAction SilentlyContinue).SteamPath
    $libs = @()
    if ($steam) {
        $libs += $steam
        $vdf = Join-Path $steam 'steamapps\libraryfolders.vdf'
        if (Test-Path $vdf) {
            $libs += Select-String -Path $vdf -Pattern '"path"\s+"([^"]+)"' | ForEach-Object { $_.Matches[0].Groups[1].Value -replace '\\\\', '\' }
        }
    }
    foreach ($l in $libs) {
        $dir = Join-Path $l 'steamapps\common\NieRAutomata'
        if (Test-Path (Join-Path $dir 'NieRAutomata.exe')) { return $dir }
    }
    while ($true) {
        $dir = (Read-Host 'NieR:Automata folder not found. Paste the folder that contains NieRAutomata.exe').Trim('"', ' ')
        if (Test-Path (Join-Path $dir 'NieRAutomata.exe')) { return $dir }
        Write-Host 'NieRAutomata.exe is not in that folder.' -ForegroundColor Red
    }
}

Write-Host '=== NieR:Automata co-op (nier_coop) installer ===' -ForegroundColor Cyan
$game = Find-Game
Write-Host "Game folder: $game"
if (Get-Process NieRAutomata -ErrorAction SilentlyContinue) { throw 'Close NieR:Automata first, then run this again.' }

$dll = Join-Path $game 'dinput8.dll'
if ($Uninstall) {
    Remove-Item $dll -ErrorAction SilentlyContinue
    if (Test-Path "$dll.bak") { Move-Item "$dll.bak" $dll; Write-Host 'Restored your previous dinput8.dll.' }
    Write-Host 'nier_coop removed.' -ForegroundColor Green
    return
}

# Another mod may already use dinput8.dll: keep it as a backup (Uninstall.bat restores it).
$ours = Join-Path $here 'dinput8.dll'
if ((Test-Path $dll) -and -not (Test-Path "$dll.bak") -and (Get-FileHash $dll).Hash -ne (Get-FileHash $ours).Hash -and -not (Select-String -Path $dll -Pattern 'nier_coop' -SimpleMatch -Quiet)) {
    Move-Item $dll "$dll.bak"
    Write-Host 'Another dinput8.dll was there (another mod). Saved it as dinput8.dll.bak; that mod is off while co-op is installed.' -ForegroundColor Yellow
}
Copy-Item $ours $dll -Force

Write-Host ''
Write-Host 'One of you is the HOST (their game controls the enemies), the other is not.'
$host_ = (Read-Host 'Are you the host? (y/n)') -match '^[yY]'

$myIp = try { (Invoke-RestMethod -Uri 'https://api.ipify.org' -TimeoutSec 5).ToString().Trim() } catch { $null }
Write-Host ''
if ($myIp) { Write-Host "Your public IP: $myIp   <- send this to your friend" -ForegroundColor Green }
$ts = Get-NetIPAddress -AddressFamily IPv4 -InterfaceAlias 'Tailscale*' -ErrorAction SilentlyContinue | Select-Object -First 1
if ($ts) { Write-Host "Your Tailscale IP: $($ts.IPAddress)   <- using Tailscale? send THIS one instead" -ForegroundColor Green }
else { Write-Host '(Using Tailscale, ZeroTier or Radmin VPN? Send the IP that app shows instead.)' }
$localIps = @(Get-NetIPAddress -AddressFamily IPv4 -ErrorAction SilentlyContinue | ForEach-Object IPAddress)
while ($true) {
    $peer = (Read-Host "Your friend's IP (or type solo to test alone: a copy of you follows you)").Trim()
    $ip = $null
    if ($peer -eq 'solo') { break }
    if ($peer -eq $myIp -or $localIps -contains $peer) {
        # Routers do not loop packets to your own public IP back to you, so nothing would arrive.
        Write-Host 'That is YOUR IP. Type your friend''s IP, or solo to test alone.' -ForegroundColor Red
        continue
    }
    if ([System.Net.IPAddress]::TryParse($peer, [ref]$ip)) { break }
    Write-Host 'That is not an IP address (like 203.0.113.5).' -ForegroundColor Red
}
# Solo (mirror test): this game receives its own packets; the copy stands 2 m aside.
$bind = if ($peer -eq 'solo') { "127.0.0.1:$port" } else { "0.0.0.0:$port" }
$peerAddr = if ($peer -eq 'solo') { "127.0.0.1:$port" } else { "${peer}:$port" }

$cfg = Join-Path $game 'nier_coop.cfg'
if (Test-Path $cfg) { Copy-Item $cfg "$cfg.bak" -Force }
$lines = @(
    '# Written by Install.bat. Run it again to change the IP or host setting.',
    "bind=$bind",
    "peer=$peerAddr",
    'spawn_2b=true',
    # Inputs alone do not start walk/run on the spawned 2B: replay the other player's animations too.
    'anim_replay=true',
    'world_sync=true',
    # The host's enemies decide every enemy action; delete this line if enemies crash the game.
    'enemy_anim=true',
    # The host's newest save is copied into the same slot here (own file kept as SlotData_N.dat.nier_coop.bak).
    'save_sync=true'
)
if ($host_) { $lines += 'host=true' }
Set-Content -Path $cfg -Value $lines -Encoding ascii

Write-Host ''
Write-Host 'Installed.' -ForegroundColor Green
Write-Host '- Start the game from Steam as usual. A black log window opens with it: do NOT close it (that closes the game).'
Write-Host '- If Windows Firewall asks about NieRAutomata, click Allow.'
Write-Host '- Both load into the same area. The log window says "connected to ..." once you are connected.'
Write-Host "- After playing, send each other nier_coop.log from: $game"
