# Install a released Zuno binary on Windows.
#
# The archive is never expanded before its SHA-256 has been compared against the
# `SHA256SUMS` published with the SAME release. A one-line installer downloads
# remote content and puts it on the user's PATH, so a digest mismatch is a hard
# failure and never a warning.
#
# Environment:
#   ZUNO_VERSION      release to install, with or without a leading `v`.
#                     Defaults to the latest published release.
#   ZUNO_INSTALL_DIR  destination directory.
#                     Defaults to `$env:LOCALAPPDATA\Programs\zuno`.

$ErrorActionPreference = "Stop"

$Repo = "sunerpy/zuno"
$Bin = "zuno"
$ChecksumFile = "SHA256SUMS"

function Die($Message) {
  Write-Error $Message
  exit 1
}

switch ($env:PROCESSOR_ARCHITECTURE) {
  "AMD64" { $Arch = "x86_64" }
  "ARM64" { $Arch = "aarch64" }
  default { Die "unsupported architecture: $env:PROCESSOR_ARCHITECTURE (AMD64 and ARM64 are published)" }
}

if ($env:ZUNO_VERSION) {
  $Version = $env:ZUNO_VERSION -replace '^v', ''
} else {
  Write-Host "Resolving the latest Zuno release..."
  $Release = Invoke-RestMethod `
    -Uri "https://api.github.com/repos/$Repo/releases/latest" `
    -Headers @{ "User-Agent" = "zuno-install" }
  $Version = $Release.tag_name -replace '^v', ''
}
if (-not $Version) { Die "could not resolve a release version for $Repo" }

$Target = "$Arch-pc-windows-msvc"
$Asset = "$Bin-$Version-$Target.zip"
$BaseUrl = "https://github.com/$Repo/releases/download/v$Version"
$InstallDir = if ($env:ZUNO_INSTALL_DIR) {
  $env:ZUNO_INSTALL_DIR
} else {
  Join-Path $env:LOCALAPPDATA "Programs\$Bin"
}
$TempDir = New-Item -ItemType Directory -Path (Join-Path $env:TEMP ([System.Guid]::NewGuid()))

try {
  Write-Host "Installing Zuno v$Version for $Target..."
  $Archive = Join-Path $TempDir $Asset
  $Checksums = Join-Path $TempDir $ChecksumFile
  Invoke-WebRequest -Uri "$BaseUrl/$Asset" -OutFile $Archive
  Invoke-WebRequest -Uri "$BaseUrl/$ChecksumFile" -OutFile $Checksums

  # Anchor on the exact asset name so a checksum file listing several archives
  # cannot end up verifying a different one.
  $EscapedAsset = [Regex]::Escape($Asset)
  $Line = Get-Content $Checksums |
    Where-Object { $_ -match "\s\*?$EscapedAsset$" } |
    Select-Object -First 1
  if (-not $Line) { Die "$Asset is not listed in $ChecksumFile" }

  $Expected = ($Line -split '\s+')[0].ToLowerInvariant()
  $Actual = (Get-FileHash -Algorithm SHA256 -Path $Archive).Hash.ToLowerInvariant()
  if ($Actual -ne $Expected) {
    Die "checksum mismatch for ${Asset}: expected $Expected, got $Actual"
  }
  Write-Host "Verified $Asset against $ChecksumFile."

  Expand-Archive -Path $Archive -DestinationPath $TempDir -Force
  $Unpacked = Join-Path $TempDir "$Bin.exe"
  if (-not (Test-Path $Unpacked)) { Die "$Asset does not contain $Bin.exe" }

  New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
  Move-Item -Force -Path $Unpacked -Destination (Join-Path $InstallDir "$Bin.exe")
  Write-Host "Installed $Bin to $InstallDir\$Bin.exe"

  $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
  if ($UserPath -notlike "*$InstallDir*") {
    Write-Host "Add $InstallDir to PATH:"
    Write-Host "  setx Path `"$InstallDir;`$env:Path`""
  }
} finally {
  Remove-Item -Recurse -Force $TempDir -ErrorAction SilentlyContinue
}
