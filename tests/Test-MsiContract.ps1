$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/../scripts/MsiContract.ps1"
function Fixture {
    $o = @{Template='x64;1033';Properties=[ordered]@{ProductVersion='1.1.3';UpgradeCode='{A38C6BC4-F086-4855-A7E6-8706BCA8F987}';ProductCode='{59BE42EA-5BA1-4A43-BC85-10D81E3FCB79}'}}
    foreach ($table in @('Component','Directory','File','Registry','Upgrade','InstallExecuteSequence','Media','LaunchCondition')) { $o[$table]=Get-MsiBaseline $table }
    $o.Component[0][1]='{F29561D1-37D1-48D8-9608-F52962453DD5}'
    $o.Component[0][3]='256'
    $o.Upgrade[0][2]='1.1.3'; $o.Upgrade[1][1]='1.1.3'
    $o.StreamNames=@('cab1.cab')
    $o.Members=@(@('VeloEditExe','VeloEditExe'),@('LicenseTxt','LicenseTxt'))
    $o.Payload=@{VeloEditExe=@{Size=1872896;ExpectedSize=1872896;Hash='ABC';ExpectedHash='ABC';Machine=0x8664};LicenseTxt=@{Size=1090;ExpectedSize=1090;Hash='DEF';ExpectedHash='DEF'}}
    $o.Shortcut=@(,@('StartMenuShortcut','AppShortcutFolder','ApplicationShortcut','[INSTALLFOLDER]VeloEdit.exe','INSTALLFOLDER'))
    $o.RemoveFile=@(@('RemoveDataFolder','DataFolderComponent','','DataFolder','2'),@('RemoveAppShortcutFolder','ApplicationShortcut','','AppShortcutFolder','2'))
    $o.CreateFolder=@(,@('DataFolder','DataFolderComponent'))
    return $o
}
$cases=[ordered]@{
    pass={param($o)}
    platform={param($o) $o.Template='Intel;1033'}
    guid={param($o) $o.Component[0][1]='{954CB1DE-1C44-5B81-A925-A12F946A4412}'}
    bitness={param($o) $o.Component[0][3]='0'}
    neutral={param($o) $o.Component[1][1]='{59BE42EA-5BA1-4A43-BC85-10D81E3FCB79}'}
    registry={param($o) $o.Registry[0][1]='2'}
    path={param($o) $o.Directory[1][1]='ProgramFilesFolder'}
    version={param($o) $o.Properties.ProductVersion='1.1.2'}
    downgrade={param($o) $o.Upgrade[1][4]='0'}
    launch={param($o) $o.LaunchCondition[0][0]='1'}
    removal={param($o) ($o.InstallExecuteSequence | Where-Object { $_[0] -eq 'RemoveExistingProducts' })[2]='1501'}
    cabinet={param($o) $o.StreamNames=@()}
    member={param($o) $o.Members[0][0]='unexpected'}
    hash={param($o) $o.Payload.VeloEditExe.Hash='BAD'}
    license={param($o) $o.Payload.LicenseTxt.Size=1}
    machine={param($o) $o.Payload.VeloEditExe.Machine=0x14c}
    scope={param($o) $o.Properties.ALLUSERS='1'}
    shortcut={param($o) $o.Shortcut[0][3]='elsewhere'}
}
foreach ($case in $cases.GetEnumerator()) {
    $o=Fixture
    & $case.Value $o
    $passed=$false
    try { $null=Assert-MsiContract $o '1.1.3'; $passed=$true } catch { Write-Output $_.Exception.Message }
    if ($passed -ne ($case.Key -eq 'pass')) { throw "Unexpected fixture outcome: $($case.Key)" }
    Write-Output "PASS: $($case.Key)"
}
