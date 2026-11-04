#requires -Version 7.0
param([string]$MsiPath)
$ErrorActionPreference = 'Stop'
$tokens = $errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile("$PSScriptRoot/../scripts/Test-MsiArtifact.ps1", [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw $errors[0] }
foreach ($name in @('Release-Com','Query')) {
    $function = $ast.Find({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name }, $false)
    . ([scriptblock]::Create($function.Extent.Text))
}
function Check-Rows($Database, [string]$Sql, [int]$Columns, $Expected) {
    $actual = Query $Database $Sql $Columns
    if ($actual -isnot [object[]] -or $actual.Count -ne $Expected.Count) { throw 'Query row count/type mismatch' }
    for ($row = 0; $row -lt $actual.Count; $row++) {
        if ($actual[$row] -isnot [object[]] -or $actual[$row].Count -ne $Columns) { throw 'Query column count/type mismatch' }
    }
    if ((ConvertTo-Json -InputObject $actual -Compress) -cne (ConvertTo-Json -InputObject $Expected -Compress)) { throw 'Query values mismatch' }
}
foreach ($rows in @(@(), (,@('ProductVersion','1.1.2')), @(@('ProductVersion','1.1.2'),@('Empty','')))) {
    $view = [pscustomobject]@{Rows=$rows;Index=0}
    $view | Add-Member ScriptMethod Execute { $null }
    $view | Add-Member ScriptMethod Close { $null }
    $view | Add-Member ScriptMethod Fetch {
        if ($this.Index -ge $this.Rows.Count) { return $null }
        $record = [pscustomobject]@{Values=$this.Rows[$this.Index++]}
        $record | Add-Member ScriptMethod StringData { param($column) $this.Values[$column-1] }
        return $record
    }
    $database = [pscustomobject]@{View=$view}
    $database | Add-Member ScriptMethod OpenView { param($sql) $this.View }
    Check-Rows $database 'fixture' 2 $rows
    Write-Output "PASS: null COM method output with $($rows.Count) rows"
}
if ($MsiPath) {
    $installer = $database = $null
    try {
        $installer = New-Object -ComObject WindowsInstaller.Installer
        $database = $installer.OpenDatabase((Resolve-Path -LiteralPath $MsiPath).Path, 0)
        $properties = Query $database 'SELECT `Property`,`Value` FROM `Property`' 2
        if ($properties.Count -lt 2) { throw 'Native fixture must contain multiple properties' }
        foreach ($row in $properties) {
            if ($row -isnot [object[]] -or $row.Count -ne 2) { throw 'Native property row shape mismatch' }
        }
        $version = @($properties | Where-Object { $_[0] -ceq 'ProductVersion' })
        if ($version.Count -ne 1) { throw 'Native fixture must contain one ProductVersion' }
        Check-Rows $database 'SELECT `Property`,`Value` FROM `Property` WHERE `Property` = ''ProductVersion''' 2 $version
        Check-Rows $database 'SELECT `Value` FROM `Property` WHERE `Property` = ''ProductVersion''' 1 @(,@($version[0][1]))
        Check-Rows $database 'SELECT `Property`,`Value` FROM `Property` WHERE `Property` = ''VeloEditMissingShapeFixture''' 2 @()
        Write-Output 'PASS: native COM multiple/single/empty rows and single column'
    } finally {
        Release-Com $database
        Release-Com $installer
    }
}
