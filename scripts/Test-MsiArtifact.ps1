#requires -Version 7.0
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$MsiPath,
    [Parameter(Mandatory)][string]$ExecutablePath,
    [Parameter(Mandatory)][string]$LicensePath,
    [Parameter(Mandatory)][string]$ExpectedVersion,
    [Parameter(Mandatory)][string]$OutputDirectory
)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/MsiContract.ps1"
function Release-Com($Object) {
    if ($null -ne $Object -and [Runtime.InteropServices.Marshal]::IsComObject($Object)) {
        [void][Runtime.InteropServices.Marshal]::FinalReleaseComObject($Object)
    }
}
function Query($Database, [string]$Sql, [int]$Columns) {
    $view = $null
    $rows = [Collections.Generic.List[object]]::new()
    try {
        $view = $Database.OpenView($Sql)
        [void]$view.Execute()
        while ($null -ne ($record = $view.Fetch())) {
            try {
                if ($rows.Count -ge 256) { throw 'MSI table exceeds row bound' }
                $row = for ($i = 1; $i -le $Columns; $i++) { [string]$record.StringData($i) }
                $rows.Add(@($row))
            } finally { Release-Com $record }
        }
    } finally {
        if ($view) { [void]$view.Close(); Release-Com $view }
    }
    return ,$rows.ToArray()
}
function Safe-Input([string]$Path, [long]$Limit) {
    $item = Get-Item -LiteralPath $Path
    if ($item.PSIsContainer -or $item.Attributes -band [IO.FileAttributes]::ReparsePoint -or $item.Length -gt $Limit) { throw "Invalid or oversized input: $Path" }
    return $item.FullName
}
$MsiPath = Safe-Input $MsiPath 134217728
$ExecutablePath = Safe-Input $ExecutablePath 67108864
$LicensePath = Safe-Input $LicensePath 1048576
$output = [IO.Path]::GetFullPath($OutputDirectory)
if (Test-Path -LiteralPath $output) { throw 'OutputDirectory must be new' }
$parent = Get-Item -LiteralPath (Split-Path $output -Parent)
for ($ancestor = $parent; $null -ne $ancestor; $ancestor = $ancestor.Parent) {
    if ($ancestor.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Output ancestor is a reparse point' }
}
$null = New-Item -ItemType Directory -Path $output
$audit = [ordered]@{ inputs=@{msi=$MsiPath; executable=$ExecutablePath; license=$LicensePath; version=$ExpectedVersion}; hashes=@{}; status='Failed'; checks=@(); observed=$null; error=$null; lifecycle='Unperformed'; tools=@() }
$installer = $database = $summary = $null
try {
    foreach ($entry in @{msi=$MsiPath; executable=$ExecutablePath; license=$LicensePath}.GetEnumerator()) { $audit.hashes[$entry.Key] = (Get-FileHash -LiteralPath $entry.Value -Algorithm SHA256).Hash }
    $installer = New-Object -ComObject WindowsInstaller.Installer
    $database = $installer.OpenDatabase($MsiPath, 0)
    $summary = $database.SummaryInformation(0)
    $observed = @{Template=[string]$summary.Property(7); Properties=[ordered]@{}}
    foreach ($row in (Query $database 'SELECT `Property`,`Value` FROM `Property`' 2)) { $observed.Properties[$row[0]] = $row[1] }
    foreach ($table in @('Component','Directory','File','Registry','Upgrade','InstallExecuteSequence','Media','LaunchCondition','RemoveFile','CreateFolder')) {
        $columns = @{Component=6;Directory=3;File=8;Registry=6;Upgrade=7;InstallExecuteSequence=3;Media=6;LaunchCondition=2;RemoveFile=5;CreateFolder=2}[$table]
        $observed[$table] = Query $database "SELECT * FROM ``$table``" $columns
    }
    $audit.observed = $observed
    $observed.Shortcut = Query $database 'SELECT `Shortcut`,`Directory_`,`Component_`,`Target`,`WkDir` FROM `Shortcut`' 5
    $observed.StreamNames = @((Query $database 'SELECT `Name` FROM `_Streams`' 1) | ForEach-Object { $_[0] })
    if ($observed.StreamNames.Count -gt 32 -or $observed.StreamNames -cnotcontains 'cab1.cab') { throw 'Missing or excessive streams' }
    # Copy only the bounded CAB record into an owned scratch database for Export.
    # Other MSI streams (including the summary stream) never become file paths.
    $sourceView = $sourceRecord = $exportDb = $exportView = $null
    try {
        $sourceView = $database.OpenView('SELECT `Name`,`Data` FROM `_Streams` WHERE `Name` = ''cab1.cab''')
        $sourceView.Execute()
        $sourceRecord = $sourceView.Fetch()
        if ($null -eq $sourceRecord) { throw 'Missing cabinet stream' }
        $cabinetSize = [long]$sourceRecord.DataSize(2)
        if ($cabinetSize -le 0 -or $cabinetSize -gt 67108864) { throw 'Invalid or oversized cabinet stream' }
        $exportDb = $installer.OpenDatabase((Join-Path $output 'cabinet-export.msi'), 3)
        $create = $null
        try {
            $create = $exportDb.OpenView('CREATE TABLE `CabinetPayload` (`Name` CHAR(72) NOT NULL, `Data` OBJECT PRIMARY KEY `Name`)')
            $create.Execute()
        } finally { Release-Com $create }
        $exportView = $exportDb.OpenView('SELECT `Name`,`Data` FROM `CabinetPayload`')
        $exportView.Execute()
        $exportView.Modify(1, $sourceRecord)
        $exportDb.Export('CabinetPayload', $output, 'streams.idt')
    } finally {
        if ($exportView) { $exportView.Close(); Release-Com $exportView }
        Release-Com $exportDb
        Release-Com $sourceRecord
        if ($sourceView) { $sourceView.Close(); Release-Com $sourceView }
    }
    $cab = Join-Path $output 'streams/cab1.cab.ibd'
    if (-not (Test-Path -LiteralPath $cab)) { throw 'Export did not produce embedded cabinet' }
    if ((Get-Item -LiteralPath $cab).Length -ne $cabinetSize) { throw 'Exported cabinet size mismatch' }
    # Read CFFILE names before expansion: only the two File identifiers are allowed.
    $reader = [IO.BinaryReader]::new([IO.File]::OpenRead($cab))
    try {
        if ($reader.ReadUInt32() -ne 0x4643534D) { throw 'Invalid cabinet signature' }
        $reader.BaseStream.Position = 16
        $fileOffset = $reader.ReadUInt32()
        $reader.BaseStream.Position = 28
        $fileCount = $reader.ReadUInt16()
        $flags = $reader.ReadUInt16()
        if ($fileCount -ne 2 -or $flags -ne 0) { throw 'Unexpected cabinet layout' }
        $reader.BaseStream.Position = $fileOffset
        $members = @()
        $expandedSize = 0L
        for ($i=0; $i -lt $fileCount; $i++) {
            $expandedSize += $reader.ReadUInt32()
            if ($expandedSize -gt 68157440) { throw 'Expanded cabinet exceeds bound' }
            $reader.BaseStream.Position += 12
            $name = ''
            do {
                $byte = $reader.ReadByte()
                if ($byte) { $name += [char]$byte }
                if ($name.Length -gt 32) { throw 'Cabinet name exceeds bound' }
            } while ($byte)
            if ($name -cnotin @('VeloEditExe','LicenseTxt') -or $members -ccontains $name) { throw 'Unexpected cabinet member' }
            $members += $name
        }
    } finally { $reader.Dispose() }
    $extract = Join-Path $output 'payload'
    $null = New-Item -ItemType Directory -Path $extract
    $expand = Join-Path $env:SystemRoot 'System32/expand.exe'
    $toolOutput = & $expand $cab '-F:*' $extract 2>&1
    $code = $LASTEXITCODE
    $audit.tools += @{tool=$expand;exitCode=$code;output=@($toolOutput | ForEach-Object { "$_" })}
    if ($code -ne 0) { throw "expand failed: $code" }
    $actual = @(Get-ChildItem -LiteralPath $extract -Force)
    if ($actual.Count -ne 2 -or @($actual | Where-Object { $_.PSIsContainer -or $_.Attributes -band [IO.FileAttributes]::ReparsePoint }).Count) { throw 'Unexpected extracted entries' }
    $observed.Members = @($actual | ForEach-Object { ,@($_.Name,$_.Name) })
    $observed.Payload = @{}
    foreach ($id in @('VeloEditExe','LicenseTxt')) {
        $path = Join-Path $extract $id
        $expected = if ($id -eq 'VeloEditExe') { $ExecutablePath } else { $LicensePath }
        $observed.Payload[$id] = @{Size=(Get-Item -LiteralPath $path).Length;ExpectedSize=(Get-Item -LiteralPath $expected).Length;Hash=(Get-FileHash -LiteralPath $path).Hash;ExpectedHash=(Get-FileHash -LiteralPath $expected).Hash}
    }
    $pe = [IO.BinaryReader]::new([IO.File]::OpenRead((Join-Path $extract 'VeloEditExe')))
    try {
        if ($pe.ReadUInt16() -ne 0x5A4D) { throw 'Missing MZ header' }
        $pe.BaseStream.Position=0x3C
        $offset=$pe.ReadUInt32()
        if ($offset -gt $pe.BaseStream.Length-6) { throw 'Invalid PE offset' }
        $pe.BaseStream.Position=$offset
        if ($pe.ReadUInt32() -ne 0x4550) { throw 'Missing PE signature' }
        $observed.Payload.VeloEditExe.Machine=$pe.ReadUInt16()
    } finally { $pe.Dispose() }
    $audit.observed=$observed
    $audit.checks=Assert-MsiContract $observed $ExpectedVersion
    $audit.status='Passed'
} catch {
    $audit.error=$_.Exception.Message
    if ($_.Exception.Data.Contains('checks')) { $audit.checks=$_.Exception.Data['checks'] }
    if ($_.Exception.Data.Contains('failedCheck')) { $audit['failedCheck']=$_.Exception.Data['failedCheck'] }
    throw
} finally {
    Release-Com $summary
    Release-Com $database
    Release-Com $installer
    $auditPath=Join-Path $output 'audit.json'
    $audit | ConvertTo-Json -Depth 20 | Set-Content -LiteralPath $auditPath
    "$((Get-FileHash -LiteralPath $auditPath).Hash.ToLowerInvariant())  audit.json" | Set-Content -LiteralPath (Join-Path $output 'audit.sha256')
}
