# Reproduce or update Zuno's audited Agent Client Protocol snapshot.
#
# Modes:
#   -Mode Verify         Offline verification of checked-in files (default).
#   -Mode CheckUpstream  Re-download the pinned upstream inputs and compare them.
#   -Mode Refresh        Rebuild the snapshot after all upstream checks succeed.
#
# Optional Refresh pin overrides:
#   ACP_STABLE_TAG, ACP_CRATE_TAG, ACP_PREVIEW_TAG, ZED_COMMIT
#
# GITHUB_TOKEN may be set to raise GitHub API rate limits. It is never printed.
[CmdletBinding()]
param(
  [ValidateSet("Verify", "CheckUpstream", "Refresh")]
  [string]$Mode = "Verify"
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = Split-Path -Parent $ScriptDir
$SnapshotDir = Join-Path $RepoRoot "docs/upstream/acp"
$ManifestPath = Join-Path $SnapshotDir "manifest.json"

$AcpRepo = "agentclientprotocol/agent-client-protocol"
$AcpRepoUrl = "https://github.com/$AcpRepo"
$AcpDocsUrl = "https://agentclientprotocol.com/protocol/v1/overview"
$ZedRepo = "zed-industries/zed"
$ZedRepoUrl = "https://github.com/$ZedRepo"
$ZedDocsUrl = "https://zed.dev/docs/ai/external-agents"
$ZedSourcePath = "crates/agent_servers/src/acp.rs"

$InitialStableTag = "schema-v1.21.0"
$InitialCrateTag = "v1.7.0"
$InitialPreviewTag = "schema-v2.0.0-alpha.3"
$InitialZedCommit = "ac099b4a809a564f06907125e7a536c33cb60084"

$ExpectedSnapshotFiles = @(
  "LICENSE"
  "README.md"
  "SHA256SUMS"
  "assets/stable/meta.json"
  "assets/stable/meta.unstable.json"
  "assets/stable/schema.json"
  "assets/stable/schema.unstable.json"
  "assets/v2-preview/meta.json"
  "assets/v2-preview/meta.unstable.json"
  "assets/v2-preview/schema.json"
  "assets/v2-preview/schema.unstable.json"
  "manifest.json"
)

$ChecksumPaths = @(
  "LICENSE"
  "assets/stable/meta.json"
  "assets/stable/meta.unstable.json"
  "assets/stable/schema.json"
  "assets/stable/schema.unstable.json"
  "assets/v2-preview/meta.json"
  "assets/v2-preview/meta.unstable.json"
  "assets/v2-preview/schema.json"
  "assets/v2-preview/schema.unstable.json"
)

function Fail([string]$Message) {
  throw $Message
}

function Write-Utf8NoBom([string]$Path, [string]$Content) {
  $Encoding = New-Object System.Text.UTF8Encoding($false)
  [System.IO.File]::WriteAllText($Path, $Content, $Encoding)
}

function Get-Sha256([string]$Path) {
  return (Get-FileHash -Algorithm SHA256 -Path $Path).Hash.ToLowerInvariant()
}

function Assert-Tag([string]$Tag) {
  if ([string]::IsNullOrWhiteSpace($Tag) -or $Tag -notmatch '^[A-Za-z0-9._-]+$') {
    Fail "invalid release tag: $Tag"
  }
}

function Assert-Commit([string]$Commit) {
  if ($Commit -notmatch '^[0-9a-f]{40}$') {
    Fail "commit must contain exactly 40 lowercase hexadecimal characters: $Commit"
  }
}

function Get-GitHubHeaders {
  $Headers = @{
    "Accept" = "application/vnd.github+json"
    "X-GitHub-Api-Version" = "2022-11-28"
    "User-Agent" = "zuno-acp-spec-updater"
  }
  if ($env:GITHUB_TOKEN) {
    $Headers["Authorization"] = "Bearer $($env:GITHUB_TOKEN)"
  }
  return $Headers
}

function Invoke-GitHubJson([string]$Path) {
  return Invoke-RestMethod `
    -Uri "https://api.github.com/$Path" `
    -Headers (Get-GitHubHeaders)
}

function Save-Download([string]$Uri, [string]$Path) {
  $Parsed = [Uri]$Uri
  if ($Parsed.Scheme -ne "https" -or
      $Parsed.Host -notin @("github.com", "raw.githubusercontent.com")) {
    Fail "refusing unexpected download host: $Uri"
  }

  $Headers = @{ "User-Agent" = "zuno-acp-spec-updater" }
  if ($env:GITHUB_TOKEN) {
    $Headers["Authorization"] = "Bearer $($env:GITHUB_TOKEN)"
  }
  Invoke-WebRequest -Uri $Uri -Headers $Headers -OutFile $Path | Out-Null
}

function Resolve-AcpTag([string]$Tag) {
  Assert-Tag $Tag
  $Ref = Invoke-GitHubJson "repos/$AcpRepo/git/ref/tags/$Tag"
  if ($Ref.ref -ne "refs/tags/$Tag" -or
      $Ref.object.type -ne "tag" -or
      [string]$Ref.object.sha -notmatch '^[0-9a-f]{40}$') {
    Fail "$Tag is not an annotated tag with a valid tag object"
  }

  $TagObject = [string]$Ref.object.sha
  $TagMetadata = Invoke-GitHubJson "repos/$AcpRepo/git/tags/$TagObject"
  if ($TagMetadata.tag -ne $Tag -or
      $TagMetadata.sha -ne $TagObject -or
      $TagMetadata.object.type -ne "commit" -or
      [string]$TagMetadata.object.sha -notmatch '^[0-9a-f]{40}$') {
    Fail "$Tag did not peel to one immutable commit"
  }

  return [PSCustomObject]@{
    TagObject = $TagObject
    Commit = [string]$TagMetadata.object.sha
  }
}

function Save-ReleaseAssets(
  [string]$Tag,
  [string]$Channel,
  [string]$StageDir
) {
  $Release = Invoke-GitHubJson "repos/$AcpRepo/releases/tags/$Tag"
  if ($Release.tag_name -ne $Tag -or $Release.draft -ne $false) {
    Fail "release metadata did not match $Tag"
  }

  $Entries = @()
  foreach ($Name in @("meta.json", "meta.unstable.json", "schema.json", "schema.unstable.json")) {
    $AssetMatches = @($Release.assets | Where-Object { $_.name -eq $Name })
    if ($AssetMatches.Count -ne 1) {
      Fail "$Tag must publish exactly one $Name asset"
    }

    $Asset = $AssetMatches[0]
    $Digest = [string]$Asset.digest
    if ($Digest -notmatch '^sha256:([0-9a-f]{64})$') {
      Fail "$Tag/$Name has no valid SHA-256 release digest"
    }
    $Expected = $Digest.Substring(7)
    $ExpectedUrl = "https://github.com/$AcpRepo/releases/download/$Tag/$Name"
    if ([string]$Asset.browser_download_url -ne $ExpectedUrl) {
      Fail "unexpected release URL for $Tag/$Name`: $($Asset.browser_download_url)"
    }

    $DestinationDir = Join-Path $StageDir "assets/$Channel"
    New-Item -ItemType Directory -Force -Path $DestinationDir | Out-Null
    $Destination = Join-Path $DestinationDir $Name
    Save-Download $ExpectedUrl $Destination

    $Actual = Get-Sha256 $Destination
    if ($Actual -ne $Expected) {
      Fail "checksum mismatch for $Tag/$Name`: expected $Expected, got $Actual"
    }
    $Size = (Get-Item $Destination).Length
    if ($Size -ne [long]$Asset.size) {
      Fail "size mismatch for $Tag/$Name`: expected $($Asset.size), got $Size"
    }

    $Entries += [ordered]@{
      name = $Name
      snapshotPath = "assets/$Channel/$Name"
      sha256 = $Actual
      size = $Size
      releaseAssetId = [long]$Asset.id
      url = $ExpectedUrl
    }
  }
  return ,$Entries
}

function New-StagedSnapshot(
  [string]$StableTag,
  [string]$CrateTag,
  [string]$PreviewTag,
  [string]$ZedCommit,
  [string]$WorkDir
) {
  Assert-Tag $StableTag
  Assert-Tag $CrateTag
  Assert-Tag $PreviewTag
  Assert-Commit $ZedCommit

  $StageDir = Join-Path $WorkDir "stage"
  New-Item -ItemType Directory -Force -Path $StageDir | Out-Null

  $Stable = Resolve-AcpTag $StableTag
  $Crate = Resolve-AcpTag $CrateTag
  $Preview = Resolve-AcpTag $PreviewTag

  $CrateRelease = Invoke-GitHubJson "repos/$AcpRepo/releases/tags/$CrateTag"
  if ($CrateRelease.tag_name -ne $CrateTag -or
      $CrateRelease.draft -ne $false -or
      $CrateRelease.prerelease -ne $false) {
    Fail "crate release metadata did not match $CrateTag"
  }

  $StableAssets = Save-ReleaseAssets $StableTag "stable" $StageDir
  $PreviewAssets = Save-ReleaseAssets $PreviewTag "v2-preview" $StageDir

  $LicenseUrl = "https://raw.githubusercontent.com/$AcpRepo/$($Stable.Commit)/LICENSE"
  $LicensePath = Join-Path $StageDir "LICENSE"
  Save-Download $LicenseUrl $LicensePath
  $LicenseText = [System.IO.File]::ReadAllText($LicensePath)
  if (-not $LicenseText.Contains("Apache License") -or
      -not $LicenseText.Contains("Version 2.0, January 2004")) {
    Fail "upstream LICENSE is not Apache License 2.0"
  }
  $LicenseSha = Get-Sha256 $LicensePath

  $AcpMainCommit = [string](Invoke-GitHubJson "repos/$AcpRepo/commits/main").sha
  Assert-Commit $AcpMainCommit

  $ZedCommitMetadata = Invoke-GitHubJson "repos/$ZedRepo/commits/$ZedCommit"
  if ([string]$ZedCommitMetadata.sha -ne $ZedCommit) {
    Fail "Zed commit endpoint did not return $ZedCommit"
  }
  $ZedMainCommit = [string](Invoke-GitHubJson "repos/$ZedRepo/commits/main").sha
  Assert-Commit $ZedMainCommit

  $ZedRawUrl = "https://raw.githubusercontent.com/$ZedRepo/$ZedCommit/$ZedSourcePath"
  $ZedRawPath = Join-Path $WorkDir "zed-acp.rs"
  Save-Download $ZedRawUrl $ZedRawPath
  $ZedText = [System.IO.File]::ReadAllText($ZedRawPath)
  if (-not $ZedText.Contains(
      "const MINIMUM_SUPPORTED_VERSION: ProtocolVersion = ProtocolVersion::V1;")) {
    Fail "Zed no longer declares ProtocolVersion::V1 as its minimum supported ACP version"
  }

  $ZedLines = [System.IO.File]::ReadAllLines($ZedRawPath)
  $ZedProtocolLine = 0
  for ($Index = 0; $Index -lt $ZedLines.Length; $Index++) {
    if ($ZedLines[$Index].Contains(
        "acp::InitializeRequest::new(ProtocolVersion::V1)")) {
      $ZedProtocolLine = $Index + 1
      break
    }
  }
  if ($ZedProtocolLine -eq 0) {
    Fail "Zed no longer initializes the ACP connection with ProtocolVersion::V1"
  }

  $ManifestObject = [ordered]@{
    formatVersion = 1
    fetchedAt = [DateTime]::UtcNow.ToString("yyyy-MM-ddTHH:mm:ssZ")
    sources = [ordered]@{
      agentClientProtocolRepository = $AcpRepoUrl
      protocolV1Documentation = $AcpDocsUrl
      zedExternalAgentsDocumentation = $ZedDocsUrl
    }
    license = [ordered]@{
      spdx = "Apache-2.0"
      snapshotPath = "LICENSE"
      sourceUrl = $LicenseUrl
      sha256 = $LicenseSha
    }
    acp = [ordered]@{
      observedMainCommit = $AcpMainCommit
      crate = [ordered]@{
        tag = $CrateTag
        tagObject = $Crate.TagObject
        commit = $Crate.Commit
        releaseUrl = "$AcpRepoUrl/releases/tag/$CrateTag"
      }
      stableSchema = [ordered]@{
        tag = $StableTag
        tagObject = $Stable.TagObject
        commit = $Stable.Commit
        releaseUrl = "$AcpRepoUrl/releases/tag/$StableTag"
        assets = $StableAssets
      }
      v2PreviewSchema = [ordered]@{
        tag = $PreviewTag
        tagObject = $Preview.TagObject
        commit = $Preview.Commit
        releaseUrl = "$AcpRepoUrl/releases/tag/$PreviewTag"
        assets = $PreviewAssets
      }
    }
    zed = [ordered]@{
      repository = $ZedRepoUrl
      commit = $ZedCommit
      observedMainCommit = $ZedMainCommit
      sourcePath = $ZedSourcePath
      requestedProtocolVersion = "V1"
      sourceLine = $ZedProtocolLine
      sourceUrl = "$ZedRepoUrl/blob/$ZedCommit/$ZedSourcePath#L$ZedProtocolLine"
      licenseBoundary = "Reference metadata only. No Zed GPL source is copied into this snapshot."
    }
  }

  $ManifestJson = $ManifestObject | ConvertTo-Json -Depth 20
  Write-Utf8NoBom (Join-Path $StageDir "manifest.json") ($ManifestJson + "`n")

  $ChecksumLines = foreach ($Relative in $ChecksumPaths) {
    $Path = Join-Path $StageDir ($Relative -replace '/', [IO.Path]::DirectorySeparatorChar)
    "$(Get-Sha256 $Path)  $Relative"
  }
  Write-Utf8NoBom (Join-Path $StageDir "SHA256SUMS") (($ChecksumLines -join "`n") + "`n")
  return $StageDir
}

function Get-RelativeFiles([string]$Root) {
  $RootPath = (Resolve-Path $Root).Path.TrimEnd(
    [IO.Path]::DirectorySeparatorChar,
    [IO.Path]::AltDirectorySeparatorChar
  )
  return @(
    Get-ChildItem -Path $RootPath -Recurse -File | ForEach-Object {
      $_.FullName.Substring($RootPath.Length).TrimStart('\', '/').Replace('\', '/')
    } | Sort-Object
  )
}

function Assert-SameStringSet(
  [string[]]$Expected,
  [string[]]$Actual,
  [string]$Description
) {
  $Difference = Compare-Object ($Expected | Sort-Object) ($Actual | Sort-Object)
  if ($Difference) {
    $Rendered = $Difference | Out-String
    Fail "$Description differs:`n$Rendered"
  }
}

function Test-Snapshot([string]$Root) {
  $RootManifest = Join-Path $Root "manifest.json"
  $RootChecksums = Join-Path $Root "SHA256SUMS"
  if (-not (Test-Path -LiteralPath $RootManifest -PathType Leaf)) {
    Fail "missing $RootManifest"
  }
  if (-not (Test-Path -LiteralPath $RootChecksums -PathType Leaf)) {
    Fail "missing $RootChecksums"
  }

  Assert-SameStringSet $ExpectedSnapshotFiles (Get-RelativeFiles $Root) "snapshot file set"

  $SnapshotManifest = Get-Content -Raw -LiteralPath $RootManifest | ConvertFrom-Json
  if ($SnapshotManifest.formatVersion -ne 1 -or
      $SnapshotManifest.license.spdx -ne "Apache-2.0" -or
      [string]$SnapshotManifest.license.sha256 -notmatch '^[0-9a-f]{64}$' -or
      $SnapshotManifest.zed.requestedProtocolVersion -ne "V1" -or
      -not [string]$SnapshotManifest.zed.licenseBoundary.Contains(
        "No Zed GPL source is copied") -or
      [string]$SnapshotManifest.acp.crate.tagObject -notmatch '^[0-9a-f]{40}$' -or
      [string]$SnapshotManifest.acp.crate.commit -notmatch '^[0-9a-f]{40}$' -or
      @($SnapshotManifest.acp.stableSchema.assets).Count -ne 4 -or
      @($SnapshotManifest.acp.v2PreviewSchema.assets).Count -ne 4) {
    Fail "manifest.json failed structural validation"
  }

  $Checksums = @{}
  $Lines = @(Get-Content -LiteralPath $RootChecksums)
  if ($Lines.Count -ne 9) {
    Fail "SHA256SUMS must contain exactly nine entries"
  }
  foreach ($Line in $Lines) {
    if ($Line -notmatch '^(?<hash>[0-9a-f]{64})  (?<path>LICENSE|assets/(?:stable|v2-preview)/(?:meta|meta\.unstable|schema|schema\.unstable)\.json)$') {
      Fail "malformed SHA256SUMS entry: $Line"
    }
    if ($Checksums.ContainsKey($Matches.path)) {
      Fail "duplicate SHA256SUMS entry: $($Matches.path)"
    }
    $Checksums[$Matches.path] = $Matches.hash
  }
  Assert-SameStringSet $ChecksumPaths @($Checksums.Keys) "checksum path set"

  $ManifestAssets = @(
    @($SnapshotManifest.acp.stableSchema.assets)
    @($SnapshotManifest.acp.v2PreviewSchema.assets)
  )
  foreach ($Relative in $ChecksumPaths) {
    $Path = Join-Path $Root ($Relative -replace '/', [IO.Path]::DirectorySeparatorChar)
    $Actual = Get-Sha256 $Path
    if ($Actual -ne $Checksums[$Relative]) {
      Fail "checksum mismatch for $Relative`: expected $($Checksums[$Relative]), got $Actual"
    }

    if ($Relative -eq "LICENSE") {
      $ManifestSha = [string]$SnapshotManifest.license.sha256
    } else {
      $MatchesForPath = @($ManifestAssets | Where-Object { $_.snapshotPath -eq $Relative })
      if ($MatchesForPath.Count -ne 1) {
        Fail "manifest contains a missing or duplicate asset for $Relative"
      }
      $ManifestSha = [string]$MatchesForPath[0].sha256
    }
    if ($ManifestSha -ne $Actual) {
      Fail "manifest checksum mismatch for $Relative`: expected $ManifestSha, got $Actual"
    }
  }

  $LicenseText = [System.IO.File]::ReadAllText((Join-Path $Root "LICENSE"))
  if (-not $LicenseText.Contains("Apache License") -or
      -not $LicenseText.Contains("Version 2.0, January 2004")) {
    Fail "LICENSE is not Apache License 2.0 text"
  }
}

function Get-CanonicalManifest([string]$Path) {
  $Value = Get-Content -Raw -LiteralPath $Path | ConvertFrom-Json
  $Value.PSObject.Properties.Remove("fetchedAt")
  $Value.acp.PSObject.Properties.Remove("observedMainCommit")
  $Value.zed.PSObject.Properties.Remove("observedMainCommit")
  return ($Value | ConvertTo-Json -Depth 20 -Compress)
}

function Copy-StagedSnapshot([string]$StageDir) {
  New-Item -ItemType Directory -Force -Path (Join-Path $SnapshotDir "assets/stable") | Out-Null
  New-Item -ItemType Directory -Force -Path (Join-Path $SnapshotDir "assets/v2-preview") | Out-Null

  Copy-Item -Force (Join-Path $StageDir "LICENSE") (Join-Path $SnapshotDir "LICENSE")
  Copy-Item -Force (Join-Path $StageDir "SHA256SUMS") (Join-Path $SnapshotDir "SHA256SUMS")
  foreach ($Name in @("meta.json", "meta.unstable.json", "schema.json", "schema.unstable.json")) {
    Copy-Item -Force `
      (Join-Path $StageDir "assets/stable/$Name") `
      (Join-Path $SnapshotDir "assets/stable/$Name")
    Copy-Item -Force `
      (Join-Path $StageDir "assets/v2-preview/$Name") `
      (Join-Path $SnapshotDir "assets/v2-preview/$Name")
  }
  Copy-Item -Force (Join-Path $StageDir "manifest.json") $ManifestPath
}

try {
  switch ($Mode) {
    "Verify" {
      Test-Snapshot $SnapshotDir
      Write-Host "ACP snapshot verification passed."
    }
    "CheckUpstream" {
      if ($env:ACP_STABLE_TAG -or $env:ACP_CRATE_TAG -or
          $env:ACP_PREVIEW_TAG -or $env:ZED_COMMIT) {
        Fail "pin overrides are only accepted with -Mode Refresh"
      }
      Test-Snapshot $SnapshotDir
      $CurrentManifest = Get-Content -Raw -LiteralPath $ManifestPath | ConvertFrom-Json
      $WorkDir = Join-Path ([IO.Path]::GetTempPath()) ("zuno-acp-spec-" + [Guid]::NewGuid())
      New-Item -ItemType Directory -Path $WorkDir | Out-Null
      try {
        $StageDir = New-StagedSnapshot `
          ([string]$CurrentManifest.acp.stableSchema.tag) `
          ([string]$CurrentManifest.acp.crate.tag) `
          ([string]$CurrentManifest.acp.v2PreviewSchema.tag) `
          ([string]$CurrentManifest.zed.commit) `
          $WorkDir
        foreach ($Relative in $ChecksumPaths) {
          $CurrentPath = Join-Path $SnapshotDir ($Relative -replace '/', [IO.Path]::DirectorySeparatorChar)
          $StagedPath = Join-Path $StageDir ($Relative -replace '/', [IO.Path]::DirectorySeparatorChar)
          if ((Get-Sha256 $CurrentPath) -ne (Get-Sha256 $StagedPath)) {
            Fail "checked-in $Relative differs from the pinned upstream content"
          }
        }
        if ((Get-CanonicalManifest $ManifestPath) -ne
            (Get-CanonicalManifest (Join-Path $StageDir "manifest.json"))) {
          Fail "checked-in manifest metadata differs from the pinned upstream state"
        }
      } finally {
        Remove-Item -Recurse -Force $WorkDir -ErrorAction SilentlyContinue
      }
      Write-Host "ACP upstream comparison passed."
    }
    "Refresh" {
      if (Test-Path -LiteralPath $ManifestPath -PathType Leaf) {
        $CurrentManifest = Get-Content -Raw -LiteralPath $ManifestPath | ConvertFrom-Json
        $BaseStable = [string]$CurrentManifest.acp.stableSchema.tag
        $BaseCrate = [string]$CurrentManifest.acp.crate.tag
        $BasePreview = [string]$CurrentManifest.acp.v2PreviewSchema.tag
        $BaseZed = [string]$CurrentManifest.zed.commit
      } else {
        $BaseStable = $InitialStableTag
        $BaseCrate = $InitialCrateTag
        $BasePreview = $InitialPreviewTag
        $BaseZed = $InitialZedCommit
      }

      $StableTag = if ($env:ACP_STABLE_TAG) { $env:ACP_STABLE_TAG } else { $BaseStable }
      $CrateTag = if ($env:ACP_CRATE_TAG) { $env:ACP_CRATE_TAG } else { $BaseCrate }
      $PreviewTag = if ($env:ACP_PREVIEW_TAG) { $env:ACP_PREVIEW_TAG } else { $BasePreview }
      $ZedCommit = if ($env:ZED_COMMIT) { $env:ZED_COMMIT } else { $BaseZed }

      $WorkDir = Join-Path ([IO.Path]::GetTempPath()) ("zuno-acp-spec-" + [Guid]::NewGuid())
      New-Item -ItemType Directory -Path $WorkDir | Out-Null
      try {
        $StageDir = New-StagedSnapshot `
          $StableTag $CrateTag $PreviewTag $ZedCommit $WorkDir
        Copy-StagedSnapshot $StageDir
        Test-Snapshot $SnapshotDir
      } finally {
        Remove-Item -Recurse -Force $WorkDir -ErrorAction SilentlyContinue
      }
      Write-Host "ACP snapshot refreshed and verified."
    }
  }
} catch {
  Write-Error $_
  exit 1
}
