$ErrorActionPreference = 'Stop'
function Get-MsiBaseline([string]$Name) {
    $value = Get-Content -LiteralPath "$PSScriptRoot/../tests/fixtures/msi/$Name.json" -Raw | ConvertFrom-Json -NoEnumerate
    if ($value.Count -and $value[0] -is [string]) { return ,@(,$value) }
    return ,@($value)
}
function Assert-MsiContract($Observed, [string]$Version) {
    $checks = [System.Collections.Generic.List[string]]::new()
    function Require($Condition, $Name) {
        if (-not $Condition) {
            $failure = [InvalidOperationException]::new("MSI contract failed: $Name")
            $failure.Data['checks'] = $checks.ToArray()
            $failure.Data['failedCheck'] = $Name
            throw $failure
        }
        $checks.Add($Name)
    }
    function RowsEqual($Actual, $Expected, $Name) {
        $a = @($Actual | ForEach-Object { ConvertTo-Json -InputObject @($_) -Compress } | Sort-Object)
        $b = @($Expected | ForEach-Object { ConvertTo-Json -InputObject @($_) -Compress } | Sort-Object)
        Require (($a -join "`n") -ceq ($b -join "`n")) $Name
    }
    Require ($Observed.Template -ceq 'x64;1033') 'platform/language'
    Require ($Observed.Properties.ProductVersion -ceq $Version) 'version'
    Require ($Observed.Properties.UpgradeCode -ceq '{A38C6BC4-F086-4855-A7E6-8706BCA8F987}') 'UpgradeCode'
    $product = [guid]::Empty
    Require ([guid]::TryParse($Observed.Properties.ProductCode, [ref]$product) -and $product -ne [guid]::Empty -and $product -ne [guid]'2E287B69-7EE8-4EEA-A769-59D85F082AA4') 'new ProductCode'
    Require (-not $Observed.Properties.Contains('ALLUSERS') -and -not $Observed.Properties.Contains('MSIINSTALLPERUSER')) 'per-user scope'
    $components = Get-MsiBaseline Component
    $components[0][1] = '{F29561D1-37D1-48D8-9608-F52962453DD5}'
    $components[0][3] = '256'
    RowsEqual $Observed.Component $components 'component identities/bitness/key paths'
    foreach ($table in @('Directory','Registry','InstallExecuteSequence','Media','LaunchCondition')) {
        RowsEqual $Observed[$table] (Get-MsiBaseline $table) $table
    }
    $upgrade = Get-MsiBaseline Upgrade
    $upgrade[0][2] = $Version
    $upgrade[1][1] = $Version
    RowsEqual $Observed.Upgrade $upgrade 'upgrade/downgrade bounds'
    Require (@($Observed.StreamNames) -ccontains 'cab1.cab') 'embedded cabinet'
    RowsEqual $Observed.Members @(@('LicenseTxt','LicenseTxt'),@('VeloEditExe','VeloEditExe')) 'exact cabinet members'
    $files = Get-MsiBaseline File
    foreach ($row in $files) {
        $payload = $Observed.Payload[$row[0]]
        Require ($null -ne $payload -and $payload.Size -gt 0 -and $payload.Size -eq $payload.ExpectedSize -and $payload.Hash -ceq $payload.ExpectedHash) "payload bytes: $($row[0])"
        $row[3] = [string]$payload.Size
    }
    RowsEqual $Observed.File $files 'file relationships/sizes/compression'
    Require ($Observed.Payload.VeloEditExe.Machine -eq 0x8664) 'executable PE x64'
    Require ($Observed.Shortcut.Count -eq 1 -and $Observed.Shortcut[0][0] -ceq 'StartMenuShortcut' -and $Observed.Shortcut[0][1] -ceq 'AppShortcutFolder' -and $Observed.Shortcut[0][2] -ceq 'ApplicationShortcut' -and $Observed.Shortcut[0][3] -ceq '[INSTALLFOLDER]VeloEdit.exe' -and $Observed.Shortcut[0][4] -ceq 'INSTALLFOLDER') 'shortcut'
    RowsEqual $Observed.RemoveFile @(@('RemoveDataFolder','DataFolderComponent','','DataFolder','2'),@('RemoveAppShortcutFolder','ApplicationShortcut','','AppShortcutFolder','2')) 'empty folder removal'
    RowsEqual $Observed.CreateFolder @(,@('DataFolder','DataFolderComponent')) 'adjacent data folder'
    return ,$checks.ToArray()
}

