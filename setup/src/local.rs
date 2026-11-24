// SPDX-License-Identifier: GPL-3.0-or-later
//! Installation on this Windows computer as a background task of the Task Scheduler.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use hq_watchman::tr;

use crate::deploy::Log;
use crate::plan::{Plan, WINDOWS_DIR};
use crate::release::{self, Bundle};

const TASK: &str = "HQ Watchman";

fn exe() -> PathBuf {
    Path::new(WINDOWS_DIR).join("bin").join("hq-watchman.exe")
}

fn config() -> PathBuf {
    Path::new(WINDOWS_DIR).join("config.toml")
}

pub fn log_file() -> PathBuf {
    Path::new(WINDOWS_DIR).join("logs").join("hq-watchman.log")
}

pub fn installed() -> bool {
    exe().exists() && config().exists()
}

fn hidden(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd
}

/// Runs a PowerShell script elevated (one UAC prompt) and waits for it.
fn run_elevated(script: &Path) -> Result<()> {
    let inner = format!(
        "-NoProfile -ExecutionPolicy Bypass -File \"{}\"",
        script.display()
    );
    let outer = format!(
        "$p = Start-Process powershell -Verb RunAs -Wait -PassThru -WindowStyle Hidden -ArgumentList '{}'; exit $p.ExitCode",
        inner.replace('\'', "''")
    );
    let status = hidden(Command::new("powershell").args(["-NoProfile", "-Command", &outer]))
        .status()
        .context("cannot start PowerShell")?;
    if !status.success() {
        bail!(
            "{}",
            tr!(
                "действие с правами администратора не выполнено (отказ в окне UAC или ошибка)",
                "the administrator step failed (UAC declined or an error)"
            )
        );
    }
    Ok(())
}

fn stage_dir() -> Result<PathBuf> {
    let d = std::env::temp_dir().join(format!("hqw-{}", crate::plan::random_token(12)));
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

struct Stage(PathBuf);

impl Drop for Stage {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn user_name() -> String {
    std::env::var("USERNAME").unwrap_or_default()
}

fn install_script(user: &str) -> String {
    format!(
        r#"$ErrorActionPreference = 'Stop'
$dir = '{WINDOWS_DIR}'
$stage = $PSScriptRoot
Stop-ScheduledTask -TaskName '{TASK}' -ErrorAction SilentlyContinue
Get-Process hq-watchman -ErrorAction SilentlyContinue | Stop-Process -Force
New-Item -ItemType Directory -Force "$dir\bin", "$dir\data", "$dir\logs" | Out-Null
if (Test-Path "$stage\hq-watchman.exe") {{
  if (Test-Path "$dir\bin\hq-watchman.exe") {{ Copy-Item "$dir\bin\hq-watchman.exe" "$dir\bin\hq-watchman.prev.exe" -Force }}
  Copy-Item "$stage\hq-watchman.exe" "$dir\bin\hq-watchman.exe" -Force
}}
if (Test-Path "$stage\config.toml") {{ Copy-Item "$stage\config.toml", "$stage\secrets.env" $dir -Force }}
icacls $dir /inheritance:r /grant:r "*S-1-5-18:(OI)(CI)F" "*S-1-5-32-544:(OI)(CI)F" "*S-1-5-19:(OI)(CI)RX" "{user}:(OI)(CI)M" | Out-Null
icacls "$dir\data" /grant "*S-1-5-19:(OI)(CI)M" | Out-Null
icacls "$dir\logs" /grant "*S-1-5-19:(OI)(CI)M" | Out-Null
if (Test-Path "$stage\settings.json") {{
  Get-Content -Raw "$stage\settings.json" | & "$dir\bin\hq-watchman.exe" settings set --config "$dir\config.toml"
  if ($LASTEXITCODE -ne 0) {{ exit 3 }}
}}
$arg = 'run --config "' + $dir + '\config.toml" --env-file "' + $dir + '\secrets.env" --log-file "' + $dir + '\logs\hq-watchman.log"'
$action = New-ScheduledTaskAction -Execute "$dir\bin\hq-watchman.exe" -Argument $arg -WorkingDirectory "$dir\data"
$trigger = New-ScheduledTaskTrigger -AtStartup
$principal = New-ScheduledTaskPrincipal -UserId 'NT AUTHORITY\LOCAL SERVICE' -LogonType ServiceAccount
$settings = New-ScheduledTaskSettingsSet -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable -MultipleInstances IgnoreNew
Register-ScheduledTask -TaskName '{TASK}' -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force | Out-Null
Start-ScheduledTask -TaskName '{TASK}'
"#
    )
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    std::fs::write(path, data).with_context(|| format!("cannot write {}", path.display()))
}

/// Puts the given files in place and (re)starts the task.
fn apply(exe_bytes: Option<&[u8]>, plan: Option<&Plan>) -> Result<()> {
    let stage = Stage(stage_dir()?);
    if let Some(b) = exe_bytes {
        write_private(&stage.0.join("hq-watchman.exe"), b)?;
    }
    if let Some(p) = plan {
        write_private(&stage.0.join("config.toml"), p.config_toml().as_bytes())?;
        write_private(&stage.0.join("secrets.env"), p.secrets_env().as_bytes())?;
        write_private(
            &stage.0.join("settings.json"),
            &serde_json::to_vec(&p.settings())?,
        )?;
    }
    let script = stage.0.join("install.ps1");
    write_private(&script, install_script(&user_name()).as_bytes())?;
    run_elevated(&script)
}

pub async fn install(plan: &Plan, bundle: &Bundle, log: &Log) -> Result<()> {
    let name = release::windows_asset(&bundle.version);
    log(format!("› {} {name}", tr!("Загрузка", "Downloading")));
    let (data, _) = bundle.fetch(&name).await?;
    log(format!(
        "  {}",
        tr!(
            "подпись и контрольная сумма верны",
            "signature and checksum are valid"
        )
    ));
    log(format!(
        "› {}",
        tr!(
            "Установка (Windows спросит разрешение администратора)",
            "Installing (Windows asks for administrator permission)"
        )
    ));
    let plan = plan.clone();
    tokio::task::spawn_blocking(move || apply(Some(&data), Some(&plan))).await??;
    wait_healthy(log).await
}

pub async fn update(bundle: &Bundle, log: &Log) -> Result<()> {
    let (data, _) = bundle
        .fetch(&release::windows_asset(&bundle.version))
        .await?;
    tokio::task::spawn_blocking(move || apply(Some(&data), None)).await??;
    if let Err(e) = wait_healthy(log).await {
        log(format!(
            "! {}",
            tr!(
                "Откат на прежнюю версию",
                "Rolling back to the previous version"
            )
        ));
        let prev = Path::new(WINDOWS_DIR)
            .join("bin")
            .join("hq-watchman.prev.exe");
        let data = std::fs::read(prev)?;
        tokio::task::spawn_blocking(move || apply(Some(&data), None)).await??;
        return Err(e);
    }
    Ok(())
}

pub async fn wait_healthy(log: &Log) -> Result<()> {
    log(format!(
        "› {}",
        tr!("Проверка работы бота", "Checking that the bot runs")
    ));
    for _ in 0..24 {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        if hq_watchman::app::healthcheck("127.0.0.1:9091")
            .await
            .is_ok()
        {
            log(format!("  {}", tr!("бот работает", "the bot is running")));
            return Ok(());
        }
    }
    bail!(
        "{}\n{}",
        tr!(
            "бот не запустился, последние строки журнала:",
            "the bot did not start, last log lines:"
        ),
        logs(30)
    )
}

pub fn logs(lines: usize) -> String {
    let text = std::fs::read_to_string(log_file()).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all.get(all.len().saturating_sub(lines)..)
        .unwrap_or_default()
        .join("\n")
}

fn bot(args: &[&str], stdin: Option<&[u8]>) -> Result<String> {
    use std::io::Write as _;
    let mut cmd = Command::new(exe());
    cmd.args(args)
        .arg("--config")
        .arg(config())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = hidden(&mut cmd)
        .spawn()
        .context("cannot start hq-watchman.exe")?;
    if let (Some(data), Some(mut si)) = (stdin, child.stdin.take()) {
        si.write_all(data)?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn settings_get() -> Result<serde_json::Value> {
    Ok(serde_json::from_str(
        bot(&["settings", "get"], None)?.trim(),
    )?)
}

pub fn settings_set(s: &hq_watchman::settings::Settings) -> Result<()> {
    s.validate()?;
    bot(&["settings", "set"], Some(&serde_json::to_vec(s)?))?;
    Ok(())
}

pub fn backup() -> Result<Vec<u8>> {
    let v: serde_json::Value = serde_json::from_str(bot(&["backup"], None)?.trim())?;
    let file = v["file"].as_str().unwrap_or_default().to_owned();
    Ok(std::fs::read(file)?)
}

pub fn version() -> String {
    let out = hidden(Command::new(exe()).arg("--version")).output();
    out.ok()
        .map(|o| String::from_utf8_lossy(&o.stderr).trim().to_owned())
        .unwrap_or_default()
}

fn simple_elevated(body: &str) -> Result<()> {
    let stage = Stage(stage_dir()?);
    let script = stage.0.join("task.ps1");
    write_private(
        &script,
        format!("$ErrorActionPreference = 'Stop'\n{body}\n").as_bytes(),
    )?;
    run_elevated(&script)
}

pub fn restart() -> Result<()> {
    simple_elevated(&format!(
        "Stop-ScheduledTask -TaskName '{TASK}' -ErrorAction SilentlyContinue\nGet-Process hq-watchman -ErrorAction SilentlyContinue | Stop-Process -Force\nStart-ScheduledTask -TaskName '{TASK}'"
    ))
}

pub fn restore(db: &[u8]) -> Result<()> {
    if !db.starts_with(b"SQLite format 3") {
        bail!(
            "{}",
            tr!(
                "это не файл базы SQLite",
                "this is not an SQLite database file"
            )
        );
    }
    let stage = Stage(stage_dir()?);
    write_private(&stage.0.join("restore.db"), db)?;
    let script = stage.0.join("task.ps1");
    write_private(
        &script,
        format!(
            "$ErrorActionPreference = 'Stop'\nStop-ScheduledTask -TaskName '{TASK}' -ErrorAction SilentlyContinue\nGet-Process hq-watchman -ErrorAction SilentlyContinue | Stop-Process -Force\nRemove-Item '{WINDOWS_DIR}\\data\\hq-watchman.db-wal', '{WINDOWS_DIR}\\data\\hq-watchman.db-shm' -ErrorAction SilentlyContinue\nCopy-Item \"$PSScriptRoot\\restore.db\" '{WINDOWS_DIR}\\data\\hq-watchman.db' -Force\nStart-ScheduledTask -TaskName '{TASK}'\n"
        )
        .as_bytes(),
    )?;
    run_elevated(&script)
}

pub fn uninstall(keep_data: bool) -> Result<()> {
    let rm = if keep_data {
        format!("Remove-Item '{WINDOWS_DIR}\\bin', '{WINDOWS_DIR}\\secrets.env' -Recurse -Force -ErrorAction SilentlyContinue")
    } else {
        format!("Remove-Item '{WINDOWS_DIR}' -Recurse -Force -ErrorAction SilentlyContinue")
    };
    simple_elevated(&format!(
        "Stop-ScheduledTask -TaskName '{TASK}' -ErrorAction SilentlyContinue\nGet-Process hq-watchman -ErrorAction SilentlyContinue | Stop-Process -Force\nUnregister-ScheduledTask -TaskName '{TASK}' -Confirm:$false -ErrorAction SilentlyContinue\n{rm}"
    ))
}
