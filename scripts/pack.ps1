# 打包（Windows）：编译、把程序与清单放进 dist/stage、用 hhh-im-pack 打成 .hhhim 并写版本索引。
# 用法：powershell -File scripts/pack.ps1 [-SignKey <私钥文件>] [-Publish]
#   -SignKey：用官方私钥给版本索引签名，写出 hhhim-release.json.sig（Slack 是官方插件，发版本必须签）。
#             私钥在私有的密钥仓库：<hhh-keys>/official/hhh-official-signing.key，不放进这个仓库、不贴进对话。
#   -Publish：打好后发到清单 source 写的仓库；令牌放环境变量 HHH_IM_PUBLISH_TOKEN（或 GITHUB_TOKEN / GITLAB_TOKEN），不写进任何文件。
# 目前只出 windows-x64 的包。
# hhh-im-pack 没装时：cargo install --git https://github.com/MagicLizi/hhh-im-sdk --tag v0.4.0 hhh-im-protocol --features transport,package
param([string]$SignKey, [switch]$Publish)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
cargo build --release
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$stage = Join-Path $root 'dist/stage'
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item hhh-im-provider.json, logo.svg $stage
Copy-Item target/release/hhh-im-slack.exe $stage
$pack = @($stage, '--target', 'windows-x64', '--out', (Join-Path $root 'dist'), '--notes', 'notes.md')
if ($SignKey) { $pack += @('--sign-key', $SignKey) }
if ($Publish) {
    if (-not $SignKey) { Write-Error 'Slack 是官方插件，发布必须带 -SignKey（官方私钥）。'; exit 1 }
    $pack += '--publish'
}
hhh-im-pack @pack
exit $LASTEXITCODE
