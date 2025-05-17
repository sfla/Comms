# Expects the following environment variables to be set:
# WINDOWS_SDK_BIN_PATH: path to the Windows SDK arch-specific binary directory (that contains the "SignTool" binary)
#   for example: "C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64"
# CERTIFICATE: Windows .pfx file encoded as base64
# CERTIFICATE_PASSWORD: password for the .pfx file

$sign_tool_path = Join-Path "$env:WINDOWS_SDK_BIN_PATH" "signtool.exe"
$bytes = [Convert]::FromBase64String($env:CERTIFICATE)
[IO.File]::WriteAllBytes("$PWD\certificate.pfx", $bytes)
& "$sign_tool_path" sign /tr http://timestamp.digicert.com /td SHA256 /fd SHA256 /f certificate.pfx /p $env:CERTIFICATE_PASSWORD comms.exe
rm .\certificate.pfx
