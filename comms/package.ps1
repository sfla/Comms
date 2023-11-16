# Expects the following environment variables to be set:
# DUMPBIN_PATH: path to the VS Build Tools "dumpbin.exe" binary
# EDITBIN_PATH: path to the VS Build Tools "editbin.exe" binary - only needed if -hide_console is specified
param(
    [switch]$reuse_package_dir,
    [switch]$hide_console,
    [string]$cargo_profile = "release-windows",
    [string]$gtk_build_dir = "C:\gtk-build\gtk\x64\release\"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$editbin = [Environment]::GetEnvironmentVariable('EDITBIN_PATH')
if ($hide_console -and -not $editbin) {
    echo "'-hide_console' was provided, but the required environment variable 'EDITBIN_PATH' is not set"
    exit 1
}

$repo_dir = (Get-Item $MyInvocation.MyCommand.Path).Directory.Parent.FullName
Push-Location $repo_dir
cargo build --bin comms --profile $cargo_profile
if ($LASTEXITCODE -ne 0) {
    Pop-Location
    exit 1
}
Pop-Location

$comms_path = Join-Path -Path $repo_dir -ChildPath "target\$cargo_profile\comms.exe"
if (-not (Test-Path -Path $comms_path)) {
    echo "$comms_path does not exist"
    exit 1
}

$package_dir = Join-Path -Path $repo_dir -ChildPath "package"

if (-not (Test-Path -Path $package_dir) -or -not $reuse_package_dir) {
    Remove-Item $package_dir -Recurse -ErrorAction Ignore
    [void](mkdir $package_dir)

    $dumpbin = [Environment]::GetEnvironmentVariable('DUMPBIN_PATH')
    $global:handled_dependencies = @()
    function CopyWithDependencies {
        param (
            $resource_path
        )

        echo "Copying $resource_path"
        cp $resource_path $package_dir

        $dumpbin_output = & $dumpbin /dependents $resource_path
        foreach ($line in $dumpbin_output) {
            if ((-not $line.EndsWith(".dll")) -or (-not $line.StartsWith("    "))) {
                continue
            }

            $dll_name = $line.Trim()
            if ($global:handled_dependencies.Contains($dll_name)) {
                continue
            }

            $global:handled_dependencies += $dll_name
            $ErrorActionPreference = "SilentlyContinue"
            $dll_path = where.exe $dll_name 2>$null
            $ErrorActionPreference = "Stop"

            if (($dll_path -eq $null) -or (-not $dll_path.Contains($gtk_build_dir))) {
                continue
            }

            CopyWithDependencies $dll_path
        }
    }

    cp (where.exe vcruntime140.dll) $package_dir
    cp (where.exe vcruntime140_1.dll) $package_dir
    CopyWithDependencies $comms_path
    CopyWithDependencies (where.exe gdbus.exe)
    [void](mkdir $package_dir/share/glib-2.0)
    cp -r "$gtk_build_dir/share/glib-2.0/schemas" $package_dir/share/glib-2.0
    mv $package_dir/comms.exe $package_dir/comms-embedded.exe
} else {
    cp $comms_path $package_dir/comms-embedded.exe
}

Remove-Item comms.exe -ErrorAction Ignore
Remove-Item comms-with-console.exe -ErrorAction Ignore
warp-packer --arch windows-x64 --input_dir $package_dir --exec comms-embedded.exe --output comms.exe --coalesce_window_with_app_id mitchhentges.comms

if ($hide_console) {
    mv comms.exe comms-with-console.exe
    &$editbin /subsystem:windows $package_dir/comms-embedded.exe
    warp-packer --arch windows-x64 --input_dir $package_dir --exec comms-embedded.exe --output comms.exe --coalesce_window_with_app_id mitchhentges.comms
    &$editbin /subsystem:windows comms.exe
}

if (Get-Command "ResourceHacker.exe" -ErrorAction SilentlyContinue) {
    ResourceHacker.exe -open comms.exe -save comms.exe -action addskip -res comms\resources\logo.ico -mask "ICONGROUP,MAINICON,"

    if (Test-Path -Path comms-with-console.exe) {
        ResourceHacker.exe -open comms-with-console.exe -save comms-with-console.exe -action addskip -res comms\resources\logo.ico -mask "ICONGROUP,MAINICON,"
    }

    # ResourceHacker forks and dumps output after, which dirties the terminal. This "sleep and echo" ensures that
    # a clean prompt is available afterwards.
    sleep 1
    echo ""
} else {
    echo "Not setting packed executable's icon as 'ResourceHacker.exe' isn't in the $PATH"
}
