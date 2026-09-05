$ErrorActionPreference = "Stop"

function Assert-Equal($Expected, $Actual, $Message) {
  if ($Expected -cne $Actual) {
    throw "${Message}: expected '$Expected', got '$Actual'"
  }
}

$InstallerPath = Join-Path $PSScriptRoot "install.ps1"
$Tokens = $null
$Errors = $null
$Ast = [System.Management.Automation.Language.Parser]::ParseFile(
  $InstallerPath,
  [ref]$Tokens,
  [ref]$Errors
)
if ($Errors.Count -ne 0) {
  throw "install.ps1 has parse errors: $($Errors -join '; ')"
}

$FunctionNames = @(
  "Normalize-PathEntry",
  "Get-PathEntries",
  "Test-PathContains",
  "Add-PathEntry"
)
$Definitions = $Ast.FindAll({
  param($Node)
  $Node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and
    $FunctionNames -contains $Node.Name
}, $true)

Assert-Equal $FunctionNames.Count $Definitions.Count "installer helper count"
foreach ($Definition in $Definitions) {
  . ([ScriptBlock]::Create($Definition.Extent.Text))
}

$InstallDir = 'C:\Users\tester\AppData\Local\Programs\zuno'
$Existing = '%JAVA_HOME%\bin;C:\Windows\System32'
Assert-Equal `
  "$InstallDir;$Existing" `
  (Add-PathEntry $Existing $InstallDir) `
  "a missing install directory is prepended without expanding existing entries"
Assert-Equal `
  $Existing `
  (Add-PathEntry $Existing 'C:\Windows\System32') `
  "an existing entry is not duplicated"
Assert-Equal `
  $Existing `
  (Add-PathEntry $Existing 'c:\windows\system32\') `
  "comparison ignores case and a trailing separator"
Assert-Equal `
  $InstallDir `
  (Add-PathEntry $null $InstallDir) `
  "an empty PATH becomes the install directory"
Assert-Equal `
  "$InstallDir;C:\Tools\zuno-helper" `
  (Add-PathEntry 'C:\Tools\zuno-helper' $InstallDir) `
  "a substring match does not hide a missing entry"

$Installer = Get-Content -Raw $InstallerPath
if ($Installer -match '(?i)\bsetx(?:\.exe)?\b') {
  throw "install.ps1 must not invoke setx"
}
foreach ($Required in @(
  '[Environment]::GetEnvironmentVariable(',
  '[Environment]::SetEnvironmentVariable(',
  '[EnvironmentVariableTarget]::User',
  '$env:Path = Add-PathEntry $env:Path $InstallDir'
)) {
  if (-not $Installer.Contains($Required)) {
    throw "install.ps1 is missing the PATH contract '$Required'"
  }
}

Write-Host "PowerShell installer tests passed."
