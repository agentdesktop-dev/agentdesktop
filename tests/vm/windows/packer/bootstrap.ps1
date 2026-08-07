$ErrorActionPreference = "Stop"

Set-LocalUser -Name "agentdesktop" -PasswordNeverExpires $true
Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0
Set-Service -Name sshd -StartupType Automatic

$sshFirewallRule = Get-NetFirewallRule -Name "OpenSSH-Server-In-TCP" -ErrorAction SilentlyContinue
if ($sshFirewallRule) {
    $sshFirewallRule | Set-NetFirewallRule -Enabled True -Profile Any -Action Allow
} else {
    New-NetFirewallRule -Name "OpenSSH-Server-In-TCP" -DisplayName "OpenSSH Server (sshd)" -Enabled True -Profile Any -Direction Inbound -Protocol TCP -Action Allow -LocalPort 22
}

powercfg.exe /hibernate off
bcdedit.exe -set TESTSIGNING ON
if ($LASTEXITCODE -ne 0) {
    throw "failed to enable Windows test-signing mode"
}

Restart-Computer -Force