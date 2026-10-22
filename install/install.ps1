# Install or upgrade klausur on Windows.
#
#   irm https://github.com/arseeeeN/klausur/releases/latest/download/install.ps1 | iex
#
# Environment: KLAUSUR_REPO to install from another GitHub owner/name.
$ErrorActionPreference = 'Stop'

$repo = if ($env:KLAUSUR_REPO) { $env:KLAUSUR_REPO } else { 'arseeeeN/klausur' }
$asset = 'klausur-x86_64-pc-windows-msvc.exe'
$base = "https://github.com/$repo/releases/latest/download"
$dir = Join-Path $env:LOCALAPPDATA 'Programs\klausur'
$exe = Join-Path $dir 'klausur.exe'
$tmp = Join-Path ([IO.Path]::GetTempPath()) "klausur-install-$PID"
New-Item -ItemType Directory -Force -Path $tmp, $dir | Out-Null

try {
    Write-Host "Downloading $asset ..."
    Invoke-WebRequest -UseBasicParsing "$base/$asset" -OutFile "$tmp\$asset"
    Invoke-WebRequest -UseBasicParsing "$base/SHA256SUMS" -OutFile "$tmp\SHA256SUMS"

    $line = Get-Content "$tmp\SHA256SUMS" | Where-Object { $_ -match " $([regex]::Escape($asset))$" }
    $expected = ($line -split ' ')[0]
    $actual = (Get-FileHash -Algorithm SHA256 "$tmp\$asset").Hash.ToLower()
    if (-not $expected -or $expected -ne $actual) { throw "checksum mismatch for $asset; not installing" }

    Move-Item -Force "$tmp\$asset" $exe
    Write-Host "Installed $(& $exe --version) to $exe"

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $entries = @($userPath -split ';' | Where-Object { $_ })
    if ($entries -notcontains $dir) {
        [Environment]::SetEnvironmentVariable('Path', (($entries + $dir) -join ';'), 'User')
        Write-Host "Added $dir to your PATH (open a new terminal to use it)."
    }
}
finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
