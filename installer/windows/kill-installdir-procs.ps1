[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$InstallDir
)

# Enforce directory boundary: a bare StartsWith($InstallDir) would also match
# ...\Programs\TidaLunar-beta\ or ...\Programs\TidaLunar.old\, killing foreign
# sibling installs. The trailing backslash forces a real path-component break.
$root = $InstallDir.TrimEnd('\') + '\'

Get-Process -ErrorAction SilentlyContinue |
    Where-Object {
        $_.Path -and
        $_.Path.StartsWith($root, [System.StringComparison]::OrdinalIgnoreCase)
    } |
    Stop-Process -Force -ErrorAction SilentlyContinue
