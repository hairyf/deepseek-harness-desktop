//! shim 脚本内容生成：`dsh` / `pnpm` 的 cmd、ps1、sh 包装脚本与落盘。
//!
//! 各构建函数是纯函数（便于测试），`write_shims` 负责写入 bin 目录。
//! shim 文本必须全英文：cmd/ps1 按系统代码页解析，中文注释会乱码成命令执行。

use crate::config;
use std::fs;
use std::path::Path;
use tauri::AppHandle;

/// Windows 下 shim 文件名（cmd 为主入口，ps1 供 PowerShell 原生体验）
pub const SHIM_CMD_NAME: &str = "dsh.cmd";
pub const SHIM_PS1_NAME: &str = "dsh.ps1";
pub const PNPM_SHIM_CMD_NAME: &str = "pnpm.cmd";
pub const PNPM_SHIM_PS1_NAME: &str = "pnpm.ps1";

/// Unix 下 shim 文件名
#[cfg(unix)]
pub const SHIM_SH_NAME: &str = "dsh";
#[cfg(unix)]
pub const PNPM_SHIM_SH_NAME: &str = "pnpm";

// ---------------------------------------------------------------------------
// shim 共享片段：node 解析逻辑（dsh / pnpm shim 共用）
//
// 规则：优先 PATH 中版本兼容的本地 node（v22.15+ / v23.8+ / v24+，
// 与 config::is_supported_node_version 一致），否则回退捆绑运行时。
// 变量约定：cmd 用 %APP_DIR%，ps1 用 $appDir，sh 用 $APP_DIR。
// 这些常量作为 format! 的参数传入，其中的 `{`/`}` 是字面量。
// ---------------------------------------------------------------------------

const CMD_NODE_RESOLVE: &str = r#"
rem Prefer a version-compatible local node, fall back to the bundled runtime.
rem Pure batch version check (for /f tokens + numeric compare), no powershell
rem child: avoids console flashes when invoked without a console and skips the
rem per-call powershell startup cost.
where node >nul 2>nul
if errorlevel 1 goto :use_bundled
for /f "tokens=1,2 delims=v." %%a in ('node --version 2^>nul ^| findstr /b "v"') do set "NODE_MAJOR=%%a" & set "NODE_MINOR=%%b"
if not defined NODE_MAJOR goto :use_bundled
if %NODE_MAJOR% GEQ 24 goto :node_ok
if %NODE_MAJOR% EQU 22 if defined NODE_MINOR if %NODE_MINOR% GEQ 15 goto :node_ok
if %NODE_MAJOR% EQU 23 if defined NODE_MINOR if %NODE_MINOR% GEQ 8 goto :node_ok
goto :use_bundled

:node_ok
set "NODE=node"
goto :launch

:use_bundled
if not exist "%APP_DIR%\runtime\node.exe" goto :no_node
set "NODE=%APP_DIR%\runtime\node.exe"
set "PATH=%APP_DIR%\runtime;%PATH%"
"#;

const PS1_NODE_RESOLVE: &str = r#"
# Prefer a version-compatible local node, fall back to the bundled runtime.
$node = $null
$localNode = Get-Command node -ErrorAction SilentlyContinue
if ($localNode) {
    try {
        $version = & node --version 2>$null
        if ($version -match '^v(\d+)\.(\d+)') {
            $major = [int]$matches[1]
            $minor = [int]$matches[2]
            if (($major -eq 22 -and $minor -ge 15) -or ($major -eq 23 -and $minor -ge 8) -or $major -ge 24) {
                $node = 'node'
            }
        }
    } catch { }
}
if (-not $node) {
    $bundled = Join-Path $appDir 'runtime\node.exe'
    if (Test-Path -LiteralPath $bundled) {
        $node = $bundled
        $env:PATH = (Split-Path -Parent $bundled) + ';' + $env:PATH
    }
}
if (-not $node) {
    Write-Error 'Node.js runtime not found. Please run DeepSeek Harness Desktop to install it first.'
    exit 1
}
"#;

const SH_NODE_RESOLVE: &str = r#"
NODE=""
if command -v node >/dev/null 2>&1; then
  NODE_V=$(node --version 2>/dev/null)
  MAJOR=$(printf '%s' "$NODE_V" | awk -F. '{ gsub(/^v/, "", $1); print $1 }')
  MINOR=$(printf '%s' "$NODE_V" | awk -F. '{ print $2 }')
  if { [ -n "$MAJOR" ] && [ "$MAJOR" -ge 24 ]; } 2>/dev/null || \
     { [ "$MAJOR" -eq 22 ] && [ "$MINOR" -ge 15 ]; } 2>/dev/null || \
     { [ "$MAJOR" -eq 23 ] && [ "$MINOR" -ge 8 ]; } 2>/dev/null; then
    NODE="node"
  fi
fi
if [ -z "$NODE" ]; then
  if [ -x "$APP_DIR/runtime/bin/node" ]; then
    NODE="$APP_DIR/runtime/bin/node"
    export PATH="$APP_DIR/runtime/bin:$PATH"
  fi
fi
if [ -z "$NODE" ]; then
  echo "Node.js runtime not found. Please run DeepSeek Harness Desktop to install it first." >&2
  exit 1
fi
"#;

// ---------------------------------------------------------------------------
// dsh shim 共享片段：用户已安装的 dsh 优先（避免覆盖/遮蔽用户自己的 dsh 与
// $DSH_HOME）。与 pnpm shim 的"用户优先"策略一致：先转发 PATH 中（排除本
// shim 目录）的用户 dsh，转发时不注入本应用的 DSH_HOME，保留用户环境；
// 仅找不到用户 dsh 时才回退到捆绑 dsh。
// 变量约定：cmd 用 %SELF_PREFIX%/%USER_DSH%，ps1 用 $selfDir/$userDsh，
// sh 用 $SELF_DIR。dsh shim 仅 release 构建写入（debug 构建不覆盖共享的
// dsh shim，见 write_shims），debug 下这些常量/函数未使用，允许 dead_code。
#[cfg_attr(debug_assertions, allow(dead_code))]
const CMD_USER_DSH_PRECEDENCE: &str = r#"
rem Prefer a user-installed dsh on PATH (skip our own shim dir), fall back to bundled.
rem This preserves your own dsh binary and its $DSH_HOME config; nothing is overwritten.
set "SELF_PREFIX=%~dp0"
set "SELF_PREFIX=%SELF_PREFIX:~0,-1%"
set "USER_DSH="
for /f "delims=" %%d in ('where dsh 2^>nul') do (
  if not defined USER_DSH (
    if /i not "%%d"=="%SELF_PREFIX%\dsh.cmd" (
      if /i not "%%d"=="%SELF_PREFIX%\dsh.ps1" (
        if /i not "%%d"=="%SELF_PREFIX%\dsh.exe" (
          if /i not "%%d"=="%SELF_PREFIX%\dsh.bat" (
            if /i "%%~xd"==".cmd" set "USER_DSH=%%d"
            if /i "%%~xd"==".exe" set "USER_DSH=%%d"
            if /i "%%~xd"==".bat" set "USER_DSH=%%d"
          )
        )
      )
    )
  )
)
if defined USER_DSH (
  call "%USER_DSH%" %*
  exit /b %ERRORLEVEL%
)
"#;

#[cfg_attr(debug_assertions, allow(dead_code))]
const PS1_USER_DSH_PRECEDENCE: &str = r#"
# Prefer a user-installed dsh on PATH (skip our own shim dir), fall back to bundled.
# This preserves your own dsh binary and its $env:DSH_HOME config; nothing is overwritten.
$selfDir = $PSScriptRoot.TrimEnd('\') + '\'
$userDsh = Get-Command dsh -All -ErrorAction SilentlyContinue |
    Where-Object { $_.Source -and -not $_.Source.StartsWith($selfDir, [System.StringComparison]::OrdinalIgnoreCase) } |
    Select-Object -First 1
if ($userDsh) {
    & $userDsh.Source @args
    exit $LASTEXITCODE
}
"#;

#[cfg_attr(windows, allow(dead_code))] // 仅 Unix shim 使用
#[cfg_attr(debug_assertions, allow(dead_code))]
const SH_USER_DSH_PRECEDENCE: &str = r#"
# Prefer a user-installed dsh on PATH (skip our own shim dir), fall back to bundled.
# This preserves your own dsh binary and its $DSH_HOME config; nothing is overwritten.
SELF_DIR=$(cd "$(dirname "$0")" && pwd)
IFS=:
for dir in $PATH; do
  if [ "$dir" = "$SELF_DIR" ]; then
    continue
  fi
  if [ -x "$dir/dsh" ]; then
    exec "$dir/dsh" "$@"
  fi
done
unset IFS
"#;

// ---------------------------------------------------------------------------
// 路径转义（按目标脚本语言的字符串规则）
// ---------------------------------------------------------------------------

/// 批处理中 `%` 会被展开，需写成 `%%`
#[inline]
pub fn escape_path_cmd(path: &Path) -> String {
    path.to_string_lossy().replace('%', "%%")
}

/// 单引号字符串中 `'` 需翻倍
#[inline]
pub fn escape_path_ps1(path: &Path) -> String {
    path.to_string_lossy().replace('\'', "''")
}

/// 单引号字符串中 `'` 需写成 `'\''`
#[inline]
pub fn escape_path_sh(path: &Path) -> String {
    path.to_string_lossy().replace('\'', "'\\''")
}

// ---------------------------------------------------------------------------
// dsh shim
// ---------------------------------------------------------------------------

/// Windows `dsh.cmd` 内容。`app_dir` 为应用数据目录（绝对路径，生成时写死），
/// `dsh_home` 为官方 `$DSH_HOME`（release 为 `~/.dsh`，生成时写死，与桌面端/
/// 官方一致）。
#[cfg_attr(debug_assertions, allow(dead_code))] // 仅 release 构建写入 dsh shim
pub fn build_cmd_shim(app_dir: &Path, dsh_home: &Path) -> String {
    let dsh_bin = app_dir.join("dependencies/dsh/node_modules/@deepseek-ai/dsh/lib/bin.js");

    format!(
        r#"@echo off
rem DeepSeek Harness Desktop - dsh command shim (generated)
rem Do not edit: regenerated by the desktop app on install/startup.
setlocal
set "APP_DIR={app_dir}"
{user_dsh}
set "DSH_BIN={dsh_bin}"
set "DSH_HOME={dsh_home}"
set "DSH_TELEMETRY_DISABLED=1"
{node_resolve}
:launch
if not exist "%DSH_BIN%" goto :no_cli
"%NODE%" "%DSH_BIN%" %*
exit /b %ERRORLEVEL%

:no_cli
echo [dsh] Harness CLI not found. Please run DeepSeek Harness Desktop to install it first. 1>&2
exit /b 1

:no_node
echo [dsh] Node.js runtime not found. Please run DeepSeek Harness Desktop to install it first. 1>&2
exit /b 1
"#,
        app_dir = escape_path_cmd(app_dir),
        dsh_bin = escape_path_cmd(&dsh_bin),
        dsh_home = escape_path_cmd(&dsh_home),
        user_dsh = CMD_USER_DSH_PRECEDENCE,
        node_resolve = CMD_NODE_RESOLVE,
    )
}

/// Windows `dsh.ps1` 内容
#[cfg_attr(debug_assertions, allow(dead_code))] // 仅 release 构建写入 dsh shim
pub fn build_ps1_shim(app_dir: &Path, dsh_home: &Path) -> String {
    let dsh_bin = app_dir.join("dependencies/dsh/node_modules/@deepseek-ai/dsh/lib/bin.js");

    format!(
        r#"# DeepSeek Harness Desktop - dsh command shim (generated)
# Do not edit: regenerated by the desktop app on install/startup.
$ErrorActionPreference = "Stop"
$appDir = '{app_dir}'
$dshBin = '{dsh_bin}'
{user_dsh}
$dshHome = '{dsh_home}'
$env:DSH_TELEMETRY_DISABLED = '1'
{node_resolve}
if (-not (Test-Path -LiteralPath $dshBin)) {{
    Write-Error '[dsh] Harness CLI not found. Please run DeepSeek Harness Desktop to install it first.'
    exit 1
}}
$env:DSH_HOME = $dshHome
& $node $dshBin @args
exit $LASTEXITCODE
"#,
        app_dir = escape_path_ps1(app_dir),
        dsh_bin = escape_path_ps1(&dsh_bin),
        user_dsh = PS1_USER_DSH_PRECEDENCE,
        node_resolve = PS1_NODE_RESOLVE,
        dsh_home = escape_path_ps1(&dsh_home),
    )
}

/// Unix `dsh` shell 脚本内容（POSIX sh）
#[cfg(not(windows))]
#[cfg_attr(debug_assertions, allow(dead_code))] // 仅 release 构建写入 dsh shim
pub fn build_sh_shim(app_dir: &Path, dsh_home: &Path) -> String {
    let dsh_bin = app_dir.join("dependencies/dsh/node_modules/@deepseek-ai/dsh/lib/bin.js");

    format!(
        r#"#!/bin/sh
# DeepSeek Harness Desktop - dsh command shim (generated)
# Do not edit: regenerated by the desktop app on install/startup.
APP_DIR='{app_dir}'
DSH_BIN='{dsh_bin}'
{user_dsh}
export DSH_HOME='{dsh_home}'
export DSH_TELEMETRY_DISABLED=1
{node_resolve}
if [ ! -f "$DSH_BIN" ]; then
  echo "[dsh] Harness CLI not found. Please run DeepSeek Harness Desktop to install it first." >&2
  exit 1
fi
exec "$NODE" "$DSH_BIN" "$@"
"#,
        app_dir = escape_path_sh(app_dir),
        dsh_bin = escape_path_sh(&dsh_bin),
        user_dsh = SH_USER_DSH_PRECEDENCE,
        dsh_home = escape_path_sh(&dsh_home),
        node_resolve = SH_NODE_RESOLVE,
    )
}

// ---------------------------------------------------------------------------
// pnpm shim（额外带"用户 pnpm 优先"逻辑，见各函数注释）
//
// `DSH_PREFER_BUNDLED_PNPM=1`（应用内部插件安装注入，见 service/plugin/install.rs）
// 时改为捆绑版优先：跳过用户 pnpm 直接运行捆绑 pnpm.cjs，仅当捆绑缺失时回退
// 用户 pnpm。默认（未设置）行为不变：用户 pnpm 优先。
// ---------------------------------------------------------------------------

/// Windows `pnpm.cmd` 内容：优先转发用户自己安装的 pnpm（`where pnpm` 遍历、
/// 跳过本 shim 目录、只收 `.cmd/.exe/.bat`），否则用 node 运行捆绑 pnpm.cjs。
/// `DSH_PREFER_BUNDLED_PNPM=1` 时捆绑版优先（见模块头注）。
///
/// 实现要点：
/// - 不用 `findstr` 匹配路径（`\` 会被当正则转义导致过滤失效）；
/// - 块内变量判断用 for 变量（`%%~xp`）而非 `%VAR%`（块解析时机陷阱）。
pub fn build_pnpm_cmd_shim(app_dir: &Path) -> String {
    let pnpm_bin = app_dir.join("dependencies/pnpm/bin/pnpm.cjs");

    format!(
        r#"@echo off
rem DeepSeek Harness Desktop - pnpm command shim (generated)
rem Do not edit: regenerated by the desktop app on install/startup.
setlocal
set "APP_DIR={app_dir}"
set "PNPM_BIN={pnpm_bin}"

rem App-internal installs (DSH_PREFER_BUNDLED_PNPM=1) use the bundled pnpm,
rem falling back to the user's only when the bundled one is missing.
if "%DSH_PREFER_BUNDLED_PNPM%"=="1" (
  if exist "%PNPM_BIN%" goto :after_user
)

rem Prefer a user-installed pnpm (skip our own shim dir), fall back to bundled.
rem Accept only executable extensions (.cmd/.exe/.bat), ignore extensionless shell scripts.
set "SELF_PREFIX=%~dp0"
set "SELF_PREFIX=%SELF_PREFIX:~0,-1%"
set "USER_PNPM="
for /f "delims=" %%p in ('where pnpm 2^>nul') do (
  if not defined USER_PNPM (
    if /i not "%%p"=="%SELF_PREFIX%\pnpm.cmd" (
      if /i not "%%p"=="%SELF_PREFIX%\pnpm.exe" (
        if /i not "%%p"=="%SELF_PREFIX%\pnpm.bat" (
          if /i "%%~xp"==".cmd" set "USER_PNPM=%%p"
          if /i "%%~xp"==".exe" set "USER_PNPM=%%p"
          if /i "%%~xp"==".bat" set "USER_PNPM=%%p"
        )
      )
    )
  )
)
if defined USER_PNPM (
  call "%USER_PNPM%" %*
  exit /b %ERRORLEVEL%
)

:after_user
{node_resolve}
:launch
if not exist "%PNPM_BIN%" goto :no_pnpm
"%NODE%" "%PNPM_BIN%" %*
exit /b %ERRORLEVEL%

:no_pnpm
echo [pnpm] pnpm not found. Please run DeepSeek Harness Desktop to install it first. 1>&2
exit /b 1

:no_node
echo [pnpm] Node.js runtime not found. Please run DeepSeek Harness Desktop to install it first. 1>&2
exit /b 1
"#,
        app_dir = escape_path_cmd(app_dir),
        pnpm_bin = escape_path_cmd(&pnpm_bin),
        node_resolve = CMD_NODE_RESOLVE,
    )
}

/// Windows `pnpm.ps1` 内容：优先转发用户 pnpm（`Get-Command pnpm -All`，
/// 排除本 shim 目录），否则用 node 运行捆绑 pnpm.cjs。
/// `DSH_PREFER_BUNDLED_PNPM=1` 时捆绑版优先（见模块头注）。
pub fn build_pnpm_ps1_shim(app_dir: &Path) -> String {
    let pnpm_bin = app_dir.join("dependencies/pnpm/bin/pnpm.cjs");

    format!(
        r#"# DeepSeek Harness Desktop - pnpm command shim (generated)
# Do not edit: regenerated by the desktop app on install/startup.
$ErrorActionPreference = "Stop"
$appDir = '{app_dir}'
$pnpmBin = '{pnpm_bin}'

{node_resolve}

# App-internal installs (DSH_PREFER_BUNDLED_PNPM=1) use the bundled pnpm,
# falling back to the user's only when the bundled one is missing.
if ($env:DSH_PREFER_BUNDLED_PNPM -eq '1' -and (Test-Path -LiteralPath $pnpmBin)) {{
    & $node $pnpmBin @args
    exit $LASTEXITCODE
}}

# Prefer a user-installed pnpm (skip our own shim dir), fall back to bundled.
$selfDir = $PSScriptRoot.TrimEnd('\') + '\'
$userPnpm = Get-Command pnpm -All -ErrorAction SilentlyContinue |
    Where-Object {{ $_.Source -and -not $_.Source.StartsWith($selfDir, [System.StringComparison]::OrdinalIgnoreCase) }} |
    Select-Object -First 1
if ($userPnpm) {{
    & $userPnpm.Source @args
    exit $LASTEXITCODE
}}

if (-not (Test-Path -LiteralPath $pnpmBin)) {{
    Write-Error '[pnpm] pnpm not found. Please run DeepSeek Harness Desktop to install it first.'
    exit 1
}}
& $node $pnpmBin @args
exit $LASTEXITCODE
"#,
        app_dir = escape_path_ps1(app_dir),
        pnpm_bin = escape_path_ps1(&pnpm_bin),
        node_resolve = PS1_NODE_RESOLVE,
    )
}

/// Unix `pnpm` shell 脚本内容（POSIX sh）：按 PATH 顺序转发第一个非本目录
/// 的用户 pnpm，否则用 node 运行捆绑 pnpm.cjs。`DSH_PREFER_BUNDLED_PNPM=1`
/// 时捆绑版优先（见模块头注）。
#[cfg_attr(all(windows, not(test)), allow(dead_code))]
pub fn build_pnpm_sh_shim(app_dir: &Path) -> String {
    let pnpm_bin = app_dir.join("dependencies/pnpm/bin/pnpm.cjs");

    format!(
        r#"#!/bin/sh
# DeepSeek Harness Desktop - pnpm command shim (generated)
# Do not edit: regenerated by the desktop app on install/startup.
APP_DIR='{app_dir}'
PNPM_BIN='{pnpm_bin}'
{node_resolve}

# App-internal installs (DSH_PREFER_BUNDLED_PNPM=1) use the bundled pnpm,
# falling back to the user's only when the bundled one is missing.
if [ "$DSH_PREFER_BUNDLED_PNPM" = "1" ] && [ -f "$PNPM_BIN" ]; then
  exec "$NODE" "$PNPM_BIN" "$@"
fi

# Prefer a user-installed pnpm (skip our own shim dir), fall back to bundled.
SELF_DIR=$(cd "$(dirname "$0")" && pwd)
IFS=:
for dir in $PATH; do
  if [ "$dir" = "$SELF_DIR" ]; then
    continue
  fi
  if [ -x "$dir/pnpm" ]; then
    exec "$dir/pnpm" "$@"
  fi
done
unset IFS

if [ ! -f "$PNPM_BIN" ]; then
  echo "[pnpm] pnpm not found. Please run DeepSeek Harness Desktop to install it first." >&2
  exit 1
fi
exec "$NODE" "$PNPM_BIN" "$@"
"#,
        app_dir = escape_path_sh(app_dir),
        pnpm_bin = escape_path_sh(&pnpm_bin),
        node_resolve = SH_NODE_RESOLVE,
    )
}

// ---------------------------------------------------------------------------
// 落盘
// ---------------------------------------------------------------------------

/// 生成的 shim 自带的可识别标记（首行注释）。用于区分"本应用生成的 shim"
/// 与"用户自行放置的同名文件"。读文件只读该标记行，避免误删用户自有文件。
const GENERATED_MARKER: &str = "DeepSeek Harness Desktop - ";

/// 目标路径已存在且不是本应用生成的 shim（即用户手动放置的 `dsh`/`pnpm`）。
///
/// 此时绝不覆盖，保留用户文件，避免"安装后清空了之前手动安装的工具"。
fn is_foreign_file(path: &Path) -> bool {
    match std::fs::read_to_string(path) {
        Ok(content) => !content.contains(GENERATED_MARKER),
        Err(_) => true,
    }
}

/// 主 `dsh` shim 路径下是否保留了用户自行安装的同名文件（用于状态展示）。
pub fn user_dsh_preserved(bin_dir: &Path) -> bool {
    let path = {
        #[cfg(windows)]
        {
            bin_dir.join(SHIM_CMD_NAME)
        }
        #[cfg(not(windows))]
        {
            bin_dir.join(SHIM_SH_NAME)
        }
    };
    path.is_file() && is_foreign_file(&path)
}

/// 将 shim 文件写入 bin 目录；目标已存在但非本应用生成的同名文件时跳过（保留）。
///
/// 覆盖式仅针对本应用生成的 shim（自愈时内容与当前安装一致）；用户手动放置的
/// 同名 `dsh`/`pnpm` 一律保留不动，避免覆盖用户自己的安装与配置。
pub fn write_shims(app_handle: &AppHandle, bin_dir: &Path) -> Result<(), String> {
    let app_dir = config::get_base_dir(app_handle);
    fs::create_dir_all(bin_dir).map_err(|e| format!("create bin dir failed: {e}"))?;

    // 写入单个 shim：若目标已存在且非本应用生成，则跳过不覆盖（保留用户文件）。
    macro_rules! write_if_ours {
        ($path:expr, $content:expr) => {{
            let target = bin_dir.join($path);
            if target.exists() && is_foreign_file(&target) {
                log::warn!(
                    "Skipping shim write to {:?}: an existing user file is preserved",
                    target
                );
            } else {
                fs::write(&target, $content)
                    .map_err(|e| format!("write {} failed: {e}", target.display()))?;
            }
            target
        }};
    }

    // dsh shim 会在内容里烘焙 $DSH_HOME（生产为 ~/.dsh、开发为 ~/.dsh.dev）。
    // 开发构建禁止改写用户共享的 dsh shim——改写会让终端 `dsh` 指向开发数据
    // 目录，并覆盖生产的命令行集成；生产版生成的 dsh shim 原样保留。
    #[cfg(not(debug_assertions))]
    {
        let dsh_home = config::get_dsh_data_path(app_handle);
        #[cfg(windows)]
        {
            write_if_ours!(SHIM_CMD_NAME, build_cmd_shim(&app_dir, &dsh_home));
            write_if_ours!(SHIM_PS1_NAME, build_ps1_shim(&app_dir, &dsh_home));
        }
        #[cfg(not(windows))]
        {
            write_if_ours!(SHIM_SH_NAME, build_sh_shim(&app_dir, &dsh_home));
        }
    }
    #[cfg(debug_assertions)]
    log::debug!("debug build: skip dsh shim write (shared user state kept for release)");

    // pnpm shim 不烘焙 $DSH_HOME（仅绑定 bundle 目录与“用户 pnpm 优先”逻辑），
    // 内容与生产完全一致，开发构建也可写入——dsh plugin 子进程经 PATH 解析
    // pnpm 依赖它，写它不污染任何共享数据。
    #[cfg(windows)]
    {
        write_if_ours!(PNPM_SHIM_CMD_NAME, build_pnpm_cmd_shim(&app_dir));
        write_if_ours!(PNPM_SHIM_PS1_NAME, build_pnpm_ps1_shim(&app_dir));
    }
    #[cfg(not(windows))]
    {
        write_if_ours!(PNPM_SHIM_SH_NAME, build_pnpm_sh_shim(&app_dir));
        // 仅对本应用生成/覆盖过的 shim 设置可执行位；保留的用户文件不动
        let chmod_names: &[&str] = if cfg!(debug_assertions) {
            &[PNPM_SHIM_SH_NAME]
        } else {
            &[SHIM_SH_NAME, PNPM_SHIM_SH_NAME]
        };
        for name in chmod_names {
            let path = bin_dir.join(name);
            if path.is_file() && !is_foreign_file(&path) {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                    .map_err(|e| format!("chmod shim failed: {e}"))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ------------------------------------------------------------------
    // escape_path_* 纯函数基线（与 shim 内嵌路径的场景一致）
    // ------------------------------------------------------------------

    #[test]
    fn escape_path_cmd_doubles_percent() {
        assert_eq!(escape_path_cmd(Path::new(r"C:\Users\%test%\x")), r"C:\Users\%%test%%\x");
        assert_eq!(escape_path_cmd(Path::new("/tmp/a b")), "/tmp/a b");
    }

    #[test]
    fn escape_path_ps1_doubles_single_quotes() {
        assert_eq!(escape_path_ps1(Path::new(r"C:\Users\o'brien")), r"C:\Users\o''brien");
        assert_eq!(escape_path_ps1(Path::new("/plain/path")), "/plain/path");
    }

    #[test]
    fn escape_path_sh_escapes_single_quotes() {
        assert_eq!(escape_path_sh(Path::new("/home/o'brien/.dsh")), r"/home/o'\''brien/.dsh");
        assert_eq!(escape_path_sh(Path::new("/plain/.dsh")), "/plain/.dsh");
    }

    fn sample_app_dir() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\Users\test\AppData\Roaming\io.github.hairyf.deepseek-harness-desktop")
        } else {
            PathBuf::from("/home/test/.local/share/io.github.hairyf.deepseek-harness-desktop")
        }
    }

    /// 官方 $DSH_HOME（~/.dsh）
    fn sample_dsh_home() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\Users\test\.dsh")
        } else {
            PathBuf::from("/home/test/.dsh")
        }
    }

    #[test]
    #[cfg(windows)]
    fn cmd_shim_contains_baked_paths() {
        let content = build_cmd_shim(&sample_app_dir(), &sample_dsh_home());
        assert!(content.contains(r"C:\Users\test\AppData\Roaming"));
        assert!(content.contains("dependencies/dsh/node_modules/@deepseek-ai/dsh/lib/bin.js"));
        assert!(content.contains(r"C:\Users\test\.dsh"));
        assert!(!content.contains("data/dsh"));
        assert!(content.contains("%*"));
    }

    #[test]
    fn cmd_shim_escapes_percent() {
        let dir = PathBuf::from(r"C:\Users\100%test\AppData\Roaming\io.github.hairyf.deepseek-harness-desktop");
        let content = build_cmd_shim(&dir, &sample_dsh_home());
        assert!(content.contains("100%%test"));
        assert!(!content.contains(r#"set "APP_DIR=C:\Users\100%test""#));
    }

    #[test]
    fn pnpm_cmd_shim_contains_user_precedence() {
        let content = build_pnpm_cmd_shim(&sample_app_dir());
        assert!(content.contains("pnpm command shim"));
        assert!(content.contains(r#"dependencies/pnpm/bin/pnpm.cjs"#));
        assert!(content.contains("where pnpm"));
        assert!(content.contains("SELF_PREFIX"));
        assert!(content.contains(r#"call "%USER_PNPM%" %*"#));
        assert!(content.contains(":use_bundled"));
        assert!(content.contains("%APP_DIR%\\runtime\\node.exe"));
        // 应用内部安装可经 DSH_PREFER_BUNDLED_PNPM=1 强制捆绑版（须在用户搜索前生效）
        assert!(content.contains("DSH_PREFER_BUNDLED_PNPM"));
        let env_at = content.find("DSH_PREFER_BUNDLED_PNPM").unwrap();
        let user_at = content.find("where pnpm").unwrap();
        assert!(env_at < user_at);
    }

    #[test]
    fn pnpm_ps1_shim_contains_user_precedence() {
        let content = build_pnpm_ps1_shim(&sample_app_dir());
        assert!(content.contains("Get-Command pnpm -All"));
        assert!(content.contains("$PSScriptRoot"));
        assert!(content.contains("$userPnpm.Source"));
        assert!(content.contains("@args"));
        assert!(content.contains("Join-Path $appDir 'runtime\\node.exe'"));
        assert!(content.contains("$env:DSH_PREFER_BUNDLED_PNPM"));
    }

    #[test]
    fn pnpm_sh_shim_contains_user_precedence() {
        let content = build_pnpm_sh_shim(&sample_app_dir());
        assert!(content.starts_with("#!/bin/sh"));
        assert!(content.contains(r#"exec "$dir/pnpm" "$@""#));
        assert!(content.contains("SELF_DIR"));
        assert!(content.contains(r#"exec "$NODE" "$PNPM_BIN" "$@""#));
        assert!(content.contains(r#"$APP_DIR/runtime/bin/node"#));
        assert!(content.contains("DSH_PREFER_BUNDLED_PNPM"));
    }

    #[test]
    #[cfg(windows)]
    fn ps1_shim_escapes_quotes() {
        let dir = PathBuf::from(r"C:\Users\o'brien\AppData\Roaming\io.github.hairyf.deepseek-harness-desktop");
        let content = build_ps1_shim(&dir, &sample_dsh_home());
        assert!(content.contains(r"o''brien"));
        // dsh_home 同样走 ps1 转义
        assert!(content.contains(r"C:\Users\test\.dsh"));
    }

    #[test]
    fn cmd_shim_prefers_user_dsh() {
        let content = build_cmd_shim(&sample_app_dir(), &sample_dsh_home());
        assert!(content.contains("USER_DSH"));
        assert!(content.contains(r#"call "%USER_DSH%" %*"#));
        assert!(content.contains("SELF_PREFIX"));
        // 用户 dsh 优先转发应出现在捆绑启动之前
        let user_at = content.find("USER_DSH").unwrap();
        let bundled_at = content.find(":use_bundled").unwrap();
        assert!(user_at < bundled_at);
    }

    #[test]
    fn ps1_shim_prefers_user_dsh() {
        let content = build_ps1_shim(&sample_app_dir(), &sample_dsh_home());
        assert!(content.contains("Get-Command dsh -All"));
        assert!(content.contains("$userDsh.Source"));
        assert!(content.contains("$PSScriptRoot"));
        // DSH_HOME 绑定只在捆绑启动分支注入（转发用户 dsh 时保留用户环境）
        let user_at = content.find("$userDsh").unwrap();
        let home_at = content.find("$env:DSH_HOME = $dshHome").unwrap();
        assert!(user_at < home_at);
    }

    #[test]
    fn sh_shim_prefers_user_dsh() {
        #[cfg(not(windows))]
        {
            let content = build_sh_shim(&sample_app_dir(), &sample_dsh_home());
            assert!(content.contains(r#"exec "$dir/dsh" "$@""#));
            assert!(content.contains("SELF_DIR"));
            // 用户 dsh 优先 > 注入 DSH_HOME
            let user_at = content.find(r#""$dir/dsh""#).unwrap();
            let home_at = content.find("export DSH_HOME").unwrap();
            assert!(user_at < home_at);
        }
    }

    #[test]
    fn foreign_file_detection() {
        let dir = std::env::temp_dir().join(format!(
            "dsh-shim-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 用户手动放置的 dsh 脚本 -> 视为 foreign，不应被覆盖
        let user_dsh = dir.join(if cfg!(windows) { "dsh.cmd" } else { "dsh" });
        std::fs::write(&user_dsh, "#!/bin/sh\necho my real dsh\n").unwrap();
        assert!(is_foreign_file(&user_dsh), "user file must be treated as foreign");

        // 本应用生成的 shim -> 不是 foreign，可覆盖
        #[cfg(not(windows))]
        let generated = build_sh_shim(&sample_app_dir(), &sample_dsh_home());
        #[cfg(windows)]
        let generated = build_cmd_shim(&sample_app_dir(), &sample_dsh_home());
        std::fs::write(&user_dsh, generated).unwrap();
        assert!(!is_foreign_file(&user_dsh), "generated shim must not be foreign");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
