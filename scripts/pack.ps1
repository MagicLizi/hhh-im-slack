# 打包（Windows）：编译、把程序与清单放进 dist/stage、用 hhh-im-pack 打成 .hhhim 并写版本索引。
# 用法：powershell -File scripts/pack.ps1 [-Publish]
#   -Publish：打好后发到清单 source 写的仓库；令牌放环境变量 HHH_IM_PUBLISH_TOKEN（或 GITHUB_TOKEN / GITLAB_TOKEN），不写进任何文件。
# hhh-im-pack 没装时：cargo install --git https://github.com/MagicLizi/hhh-im-sdk --tag v0.4.0 hhh-im-protocol --features transport,package
param([switch]$Publish)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
cargo build --release
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$stage = Join-Path $root 'dist/stage'
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item hhh-im-provider.json, logo.svg $stage
Copy-Item target/release/demo-im.exe $stage
$pack = @($stage, '--target', 'windows-x64', '--out', (Join-Path $root 'dist'), '--notes', 'notes.md')
if ($Publish) { $pack += '--publish' }
hhh-im-pack @pack
exit $LASTEXITCODE
