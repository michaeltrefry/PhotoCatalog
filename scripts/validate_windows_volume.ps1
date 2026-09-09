# Native local-volume proof using only newly created fixtures. No VHD, disk,
# volume mount, detach, network-share or existing-directory mutation.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if (-not $IsWindows) { throw 'This validation requires native Windows PowerShell' }
$fixtureRoot = Join-Path ([IO.Path]::GetTempPath()) ('photocatalog-volume-' + [guid]::NewGuid().ToString('N'))
$actual = Join-Path $fixtureRoot 'actual'
$alias = Join-Path $fixtureRoot 'junction'
$source = Join-Path $actual 'snow-雪.jpg'
$envNames = @('PHOTOCATALOG_VOLUME_FIXTURE', 'PHOTOCATALOG_EXPECT_VOLUME_ID',
    'PHOTOCATALOG_EXPECT_VOLUME_RELATIVE', 'PHOTOCATALOG_EXPECT_CANONICAL_SOURCE')
$savedEnv = @{}
foreach ($name in $envNames) { $savedEnv[$name] = [Environment]::GetEnvironmentVariable($name) }
[void][IO.Directory]::CreateDirectory($actual)
try {
    [IO.File]::WriteAllBytes($source, [Text.Encoding]::UTF8.GetBytes('owned native volume fixture'))
    $before = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash
    $modified = [IO.File]::GetLastWriteTimeUtc($source)
    $driveRoot = [IO.Path]::GetPathRoot($source)
    if ($driveRoot -notmatch '^[A-Za-z]:\\$') { throw 'Fixture must be on a local drive-letter volume' }
    $driveLetter = $driveRoot.TrimEnd('\')
    # Independent Windows management provider; do not derive the expected GUID
    # from the Rust observation or call its volume lookup implementation.
    $volumes = @(Get-CimInstance -ClassName Win32_Volume -Filter "DriveLetter = '$driveLetter'")
    if ($volumes.Count -ne 1) { throw 'Expected one native local-volume CIM record' }
    $match = [regex]::Match($volumes[0].DeviceID, '\{([0-9A-Fa-f-]{36})\}')
    if (-not $match.Success) { throw 'Native CIM volume GUID absent' }
    $expectedId = ([guid]$match.Groups[1].Value).ToString('D')
    $expectedRelative = [IO.Path]::GetRelativePath($driveRoot, $source)
    [void](New-Item -ItemType Junction -Path $alias -Target $actual)
    $junction = Get-Item -LiteralPath $alias -Force
    if ($junction.LinkType -ne 'Junction') { throw 'Fixture did not create a native junction' }
    $env:PHOTOCATALOG_EXPECT_VOLUME_ID = $expectedId
    $env:PHOTOCATALOG_EXPECT_VOLUME_RELATIVE = $expectedRelative
    $env:PHOTOCATALOG_EXPECT_CANONICAL_SOURCE = $source
    foreach ($path in @($source, (Join-Path $alias 'snow-雪.jpg'))) {
        $env:PHOTOCATALOG_VOLUME_FIXTURE = $path
        $output = @(& cargo test --locked --test storage_volume inspect_explicit_volume_fixture -- --ignored --exact --nocapture)
        $result = $LASTEXITCODE
        $output | ForEach-Object { Write-Host $_ }
        if ($result -ne 0) { throw "Native volume fixture failed with exit code $result" }
        if (-not ($output -match '^test result: ok\. 1 passed;')) {
            throw 'Expected exactly one executed native fixture test; refusing a zero-test pass'
        }
        if ((Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash -ne $before -or
            [IO.File]::GetLastWriteTimeUtc($source) -ne $modified) {
            throw 'Fixture source changed during observation'
        }
    }
    if ([IO.Directory]::GetFileSystemEntries($actual).Count -ne 1 -or
        [IO.Directory]::GetFileSystemEntries($fixtureRoot).Count -ne 2) {
        throw 'Unexpected fixture directory changes during observation'
    }
    Write-Host "Native Windows direct/junction GUID and exact relative/object mapping passed; CIM GUID $expectedId"
}
finally {
    foreach ($name in $envNames) { [Environment]::SetEnvironmentVariable($name, $savedEnv[$name]) }
    # Delete the link itself, without following it or recursively deleting its target.
    if (Test-Path -LiteralPath $alias) {
        $link = Get-Item -LiteralPath $alias -Force
        if ($link.LinkType -ne 'Junction') { throw 'Refusing cleanup of unexpected non-junction path' }
        [IO.Directory]::Delete($alias)
    }
    [IO.Directory]::Delete($fixtureRoot, $true)
}
