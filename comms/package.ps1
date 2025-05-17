param(
    [string]$msvc_bin_path = $env:MSVC_BIN_PATH,
    [switch]$reuse_package_dir,
    [switch]$hide_console,
    [string]$cargo_profile = "release-windows",
    [string]$gtk_build_dir = "C:\gtk-build\gtk\x64\release\"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if (-not $msvc_bin_path) {
    echo 'Missing "$msvc_bin_path" parameter (the path to the VS Build Tools arch-specific binary directory that contains "dumpbin.exe"), for example: "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Tools\MSVC\14.44.35207\bin\Hostx64\x64"'
    exit 1
}

$dumpbin = Join-Path "$msvc_bin_path" "dumpbin.exe"
if (-not (Test-Path $dumpbin)) {
    echo "Could not find 'dumpbin.exe' in provided '`$msvc_bin_path' parameter: '$msvc_bin_path'"
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

if ($cargo_profile -eq "dev") {
   $cargo_profile = "debug"
}

$comms_path = Join-Path -Path $repo_dir -ChildPath "target\$cargo_profile\comms.exe"
if (-not (Test-Path -Path $comms_path)) {
    echo "$comms_path does not exist"
    exit 1
}

$package_dir = Join-Path -Path $repo_dir -ChildPath "package"

if (-not (Test-Path -Path $package_dir) -or -not $reuse_package_dir) {
    Remove-Item $package_dir -Recurse -ErrorAction Ignore
    [void](mkdir $package_dir)

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

$comms_output_path = Join-Path "$repo_dir" 'comms.exe'
$comms_with_console_output_path = Join-Path "$repo_dir" 'comms-with-console.exe'
Remove-Item $comms_output_path -ErrorAction Ignore
Remove-Item $comms_with_console_output_path -ErrorAction Ignore
warp-packer --arch windows-x64 --input_dir $package_dir --exec comms-embedded.exe --output $comms_output_path --coalesce_window_with_app_id mitchhentges.comms

if ($hide_console) {
    mv $comms_output_path $comms_with_console_output_path
    $editbin = Join-Path "$msvc_bin_path" "editbin.exe"
    &$editbin /subsystem:windows $package_dir/comms-embedded.exe
    warp-packer --arch windows-x64 --input_dir $package_dir --exec comms-embedded.exe --output $comms_output_path --coalesce_window_with_app_id mitchhentges.comms
    &$editbin /subsystem:windows $comms_output_path
}

if (Get-Command "ResourceHacker.exe" -ErrorAction SilentlyContinue) {
    $logo_path = Join-Path "$repo_dir" 'comms\resources\logo.ico'
    ResourceHacker.exe -open $comms_output_path -save $comms_output_path -action addskip -res "$logo_path" -mask "ICONGROUP,MAINICON,"

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
