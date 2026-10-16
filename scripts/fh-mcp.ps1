# Call FileHunter's MCP tools from a shell, without registering an MCP server in the client.
# Useful for an agent session that is already running, or for testing.
#
#   powershell -NoProfile -File scripts\fh-mcp.ps1 tools
#   powershell -NoProfile -File scripts\fh-mcp.ps1 search '{"query":"invoice ext:pdf","limit":20}'
#   powershell -NoProfile -File scripts\fh-mcp.ps1 search_by_meaning '{"query":"games"}'
#
# FileHunter must be running with Menu -> Settings -> "Access for AI agents (MCP)" turned on.
param(
    [Parameter(Mandatory = $true)][string]$Tool,
    [string]$Arguments = '{}',
    [string]$Exe = "$env:LOCALAPPDATA\FileHunter\filehunter.exe"
)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
if (-not (Test-Path $Exe)) { throw "FileHunter not found: $Exe (pass -Exe <path>)" }

$init = '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fh-mcp.ps1","version":"1"}}}'
$ready = '{"jsonrpc":"2.0","method":"notifications/initialized"}'
if ($Tool -eq 'tools') {
    $call = '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
} else {
    $null = $Arguments | ConvertFrom-Json   # проверка, что аргументы — корректный JSON
    $call = '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"' + $Tool + '","arguments":' + $Arguments + '}}'
}

$psi = New-Object System.Diagnostics.ProcessStartInfo $Exe, '--mcp'
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.UseShellExecute = $false
$psi.StandardOutputEncoding = [Text.Encoding]::UTF8
$p = [Diagnostics.Process]::Start($psi)
$w = New-Object IO.StreamWriter($p.StandardInput.BaseStream, (New-Object Text.UTF8Encoding $false))
$w.Write("$init`n$ready`n$call`n")
$w.Close()
$out = $p.StandardOutput.ReadToEnd()
$p.WaitForExit(660000) | Out-Null

$reply = $out -split "`n" | Where-Object { $_ } | ForEach-Object { $_ | ConvertFrom-Json } | Where-Object { $_.id -eq 2 } | Select-Object -First 1
if (-not $reply) { throw "No answer from FileHunter" }
if ($Tool -eq 'tools') {
    $reply.result.tools | ForEach-Object { "{0}`n    {1}`n    args: {2}`n" -f $_.name, $_.description, (($_.inputSchema.properties.PSObject.Properties | ForEach-Object { $_.Name }) -join ', ') }
} else {
    $text = $reply.result.content[0].text
    if ($reply.result.isError) { Write-Error $text } else { $text }
}
