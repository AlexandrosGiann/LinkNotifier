# Downloads the Observer models into .\models (about 260 MB in total).
# Files that are already there are skipped, so the script can be re-run.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
Set-Location $PSScriptRoot
$M = Join-Path $PSScriptRoot 'models'
New-Item -ItemType Directory -Force $M | Out-Null

function Get-File($url, $out) {
    if (Test-Path $out) { Write-Host "ok        $out"; return }
    New-Item -ItemType Directory -Force (Split-Path $out) | Out-Null
    Write-Host "download  $url"
    Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile "$out.part"
    Move-Item -Force "$out.part" $out
}

# Speech to text: Cactus Whistle, 17 MB
Get-File 'https://huggingface.co/Cactus-Compute/whistle/resolve/main/whistle.cact' "$M\whistle.cact"

# Action items / decisions: Needle 3, 35 MB (Apache-2.0)
Get-File 'https://huggingface.co/Cactus-Compute/needle3/resolve/main/needle3.cact' "$M\needle3.cact"

# Summaries: SmolLM2-135M-Instruct q8, 140 MB
$S = 'https://huggingface.co/onnx-community/SmolLM2-135M-Instruct-ONNX-MHA/resolve/main'
Get-File "$S/tokenizer.json" "$M\smollm2\tokenizer.json"
Get-File "$S/onnx/model_quantized.onnx" "$M\smollm2\onnx\model_quantized.onnx"

# Sentence embeddings: MiniLM, already in its repository (118 MB)
if (-not (Test-Path "$M\minilm\manifest.json")) {
    Write-Host "copy      minilm"
    Copy-Item -Recurse -Force 'vendor\paraphrase-multilingual-MiniLM-L12-v2_PureRust\model' "$M\minilm"
} else { Write-Host "ok        $M\minilm" }

# Screen understanding: MobileCLIP-S0 graph, weights and text bank (45 MB),
# from the JavaScript repository the Rust port reads them from.
if (-not (Test-Path "$M\mobileclip\compiled-fp32-v1\plan.json")) {
    $tmp = Join-Path $env:TEMP 'mobileclip-s0-purejs'
    if (Test-Path $tmp) { Remove-Item -Recurse -Force $tmp }
    git clone --depth 1 --filter=blob:none --sparse https://github.com/AlexandrosGiann/mobileclip-s0-purejs-oldandroid $tmp
    git -C $tmp sparse-checkout set compiled-fp32-v1 text-bank
    New-Item -ItemType Directory -Force "$M\mobileclip" | Out-Null
    Copy-Item -Recurse -Force "$tmp\compiled-fp32-v1" "$M\mobileclip\"
    if (Test-Path "$tmp\text-bank") { Copy-Item -Recurse -Force "$tmp\text-bank" "$M\mobileclip\" }
    Remove-Item -Recurse -Force $tmp
} else { Write-Host "ok        $M\mobileclip" }

& "$PSScriptRoot\target\release\observer.exe" check --models $M
