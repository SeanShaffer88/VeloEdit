$ErrorActionPreference = 'Stop'
$root = (Resolve-Path "$PSScriptRoot/..").Path
$workflow = Get-Content "$root/.github/workflows/release.yml" -Raw
function Block($start, $end) {
 $text = $workflow.Substring($workflow.IndexOf($start))
 $text = $text.Substring($text.IndexOf('        run: |') + 15)
 $text = $text.Substring(0, $text.IndexOf($end))
 [scriptblock]::Create((($text -split '\r?\n') | ForEach-Object { $_ -replace '^          ', '' }) -join "`n")
}
$resolve = Block '      - name: Resolve the release tag' '      - uses: actions/checkout'
$verify = Block '      - name: Verify existing tag and source versions' '      # ---- Gate first:'
$sha = 'a' * 40
$other = 'b' * 40
$tagRef = 'refs/tags/v1.1.3'
function git {
 $global:LASTEXITCODE = 0
 if (($args -join '|') -ceq 'rev-parse|HEAD') { return $script:head }
 if (($args -join '|') -cne "ls-remote|origin|$tagRef|$tagRef^{}") { throw 'Unexpected git arguments' }
 $global:LASTEXITCODE = $script:remoteExit
 $script:refs
}
function Get-Content {
 param([string]$Path, [switch]$Raw)
 $text = Microsoft.PowerShell.Management\Get-Content -LiteralPath $Path -Raw
 if ($script:mismatch -and $Path -ceq $script:mismatch) { $text = $text.Replace('1.1.3','9.9.9') }
 $text
}
$env:GITHUB_OUTPUT = Join-Path ([IO.Path]::GetTempPath()) ([guid]::NewGuid().ToString() + ".txt")
$env:GITHUB_REF = 'refs/heads/main'
foreach ($tag in @('v1.1.3','v01.1.3','v1.1.3;throw 1','v1.1.3-rc1','V1.1.3',"v1.1.3`n")) {
 $env:RELEASE_INPUT_TAG = $tag
 $passed = $true
 try { & $resolve } catch { $passed = $false; Write-Output $_.Exception.Message }
 if ($passed -ne ($tag -ceq 'v1.1.3')) { throw "Tag validation failed: $tag" }
}
$env:RELEASE_TAG = 'v1.1.3'
$env:RELEASE_VERSION = '1.1.3'
$cases = @(
 @{name='lightweight'; refs=@("$sha`t$tagRef"); pass=$true},
 @{name='annotated'; refs=@("$other`t$tagRef", "$sha`t$tagRef^{}"); pass=$true},
 @{name='missing'; refs=@(); pass=$false},
 @{name='branch only'; refs=@("$sha`trefs/heads/v1.1.3"); pass=$false},
 @{name='wrong commit'; refs=@("$other`t$tagRef"); pass=$false},
 @{name='wrong peeled'; refs=@("$sha`t$tagRef", "$other`t$tagRef^{}"); pass=$false},
 @{name='remote failure'; refs=@(); code=128; pass=$false},
 @{name='malformed'; refs=@('invalid'); pass=$false},
 @{name='bad head'; refs=@("$sha`t$tagRef"); head='invalid'; pass=$false},
 @{name='lock mismatch'; refs=@("$sha`t$tagRef"); mismatch='Cargo.lock'; pass=$false},
 @{name='NSIS mismatch'; refs=@("$sha`t$tagRef"); mismatch='installer/VeloEdit.nsi'; pass=$false},
 @{name='WiX mismatch'; refs=@("$sha`t$tagRef"); mismatch='installer/Package.wxs'; pass=$false},
 @{name='version mismatch'; refs=@("$sha`t$tagRef"); version='9.9.9'; pass=$false}
)
Push-Location $root
try {
 foreach ($case in $cases) {
  $script:mismatch = $case.mismatch
  $script:head = if ($case.head) { $case.head } else { $sha }
  $script:refs = $case.refs
  $script:remoteExit = if ($case.code) { $case.code } else { 0 }
  $env:RELEASE_VERSION = if ($case.version) { $case.version } else { '1.1.3' }
  $passed = $true
  try { & $verify } catch { $passed = $false; Write-Output $_.Exception.Message }
  if ($passed -ne $case.pass) { throw "Unexpected outcome: $($case.name)" }
  if ($passed -and (Microsoft.PowerShell.Management\Get-Content $env:GITHUB_OUTPUT) -cnotcontains "sha=$sha") { throw 'Checkout SHA was not persisted' }
  Write-Output "PASS: $($case.name)"
 }
 if ($workflow -notmatch "--verify-tag --target '\$\{\{ steps.source.outputs.sha \}\}'") { throw 'Missing SHA binding' }
 if ($workflow -match '--clobber|gh release upload') { throw 'Publication must fail closed' }
 if ($workflow.IndexOf('./scripts/Test-MsiArtifact.ps1') -gt $workflow.IndexOf('      - name: Generate SHA256SUMS.txt') -or $workflow -notmatch './scripts/Test-MsiArtifact.ps1') { throw 'MSI audit must precede publication checksums' }
 $ci = Microsoft.PowerShell.Management\Get-Content "$root/.github/workflows/ci.yml" -Raw
 foreach ($text in @($ci,$workflow)) {
  if ($text -notmatch 'Package.wxs -arch x64 -ext WixToolset.UI.wixext') { throw 'Missing explicit MSI architecture' }
  if ($text -notmatch './tests/Test-MsiContract.ps1') { throw 'Missing MSI fixtures' }
 }
 if ($ci -notmatch 'contents: read' -or $ci -match 'contents: write|gh release') { throw 'Candidate CI must be read-only' }
 if ($ci.IndexOf('./scripts/Test-MsiArtifact.ps1') -gt $ci.IndexOf('uses: actions/upload-artifact') -or $ci -notmatch 'if-no-files-found: error') { throw 'Candidate upload must follow audit' }
} finally {
 Pop-Location
 Remove-Item -LiteralPath $env:GITHUB_OUTPUT -ErrorAction SilentlyContinue
}

