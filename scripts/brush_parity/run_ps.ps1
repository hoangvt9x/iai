# Paint the brush parity cases in Photoshop (via COM) into
# tests/fixtures/brush_pts/ps/. Photoshop opens a window while it works.
#   powershell -ExecutionPolicy Bypass -File scripts\brush_parity\run_ps.ps1 [case-id prefix] [-Cases dodge_burn.json]
param([string]$Only = "", [string]$Cases = "cases.json")
$ErrorActionPreference = "Stop"
$root = Resolve-Path (Join-Path $PSScriptRoot "..\..")
$cases = Join-Path $root ("tests\fixtures\brush_pts\" + $Cases)
$out = Join-Path $root "tests\fixtures\brush_pts\ps"
New-Item -ItemType Directory -Force $out | Out-Null
$jsx = Join-Path $PSScriptRoot "ps_paint.jsx"
$ps = New-Object -ComObject Photoshop.Application
$r = $ps.DoJavaScriptFile($jsx, @($cases.ToString().Replace('\', '/'), $out.ToString().Replace('\', '/'), $Only))
"result: $r"
Get-Content (Join-Path $out "ps_paint.log")
