// SPDX-License-Identifier: GPL-3.0-or-later
//! Installation on this Windows computer as a background task of the Task Scheduler.

use std::fmt::Write as _;
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

fn powershell() -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    Path::new(&root)
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe")
}

/// Text as a PowerShell single-quoted literal.
fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Runs a PowerShell script elevated (one UAC prompt) and waits for it.
/// The script goes on the command line, so no file of the user can stand in for it.
fn run_elevated(body: &str) -> Result<()> {
    use base64::Engine as _;
    let script = format!("$ErrorActionPreference = 'Stop'\n{body}\n");
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);
    let ps = powershell();
    let outer = format!(
        "$p = Start-Process -FilePath {} -Verb RunAs -Wait -PassThru -WindowStyle Hidden -ArgumentList '-NoProfile -NonInteractive -EncodedCommand {encoded}'; exit $p.ExitCode",
        ps_quote(&ps.display().to_string())
    );
    let status =
        hidden(Command::new(&ps).args(["-NoProfile", "-NonInteractive", "-Command", &outer]))
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

struct Stage(PathBuf);

impl Drop for Stage {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Files for the elevated script: written to the user's temp folder, then copied into
/// a folder only administrators can write and checked against their SHA-256 there.
struct Staged {
    _dir: Stage,
    script: String,
}

fn stage_files(files: &[(&str, &[u8])]) -> Result<Staged> {
    let dir = std::env::temp_dir().join(format!("hqw-{}", crate::plan::random_token(12)));
    std::fs::create_dir_all(&dir)?;
    let dir = Stage(dir);
    let mut list = String::new();
    for (name, data) in files {
        let path = dir.0.join(name);
        std::fs::write(&path, data).with_context(|| format!("cannot write {}", path.display()))?;
        let hash = ring::digest::digest(&ring::digest::SHA256, data);
        let hex = hash.as_ref().iter().fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02X}");
            s
        });
        let _ = writeln!(list, "$files += ,@({}, '{hex}')", ps_quote(name));
    }
    let script = format!(
        r#"$src = {src}
$in = "$dir\stage"
if (Test-Path $in) {{
  if ((Get-Item $in -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) {{ throw 'unexpected link in the installation folder' }}
  Remove-Item $in -Recurse -Force
}}
New-Item -ItemType Directory $in | Out-Null
icacls $in /inheritance:r /grant:r "*S-1-5-18:(OI)(CI)F" "*S-1-5-32-544:(OI)(CI)F" | Out-Null
if ($LASTEXITCODE -ne 0) {{ throw 'icacls failed' }}
$files = @()
{list}foreach ($f in $files) {{
  $t = Join-Path $in $f[0]
  Copy-Item -LiteralPath (Join-Path $src $f[0]) -Destination $t -Force
  if ((Get-FileHash -Algorithm SHA256 -LiteralPath $t).Hash -ne $f[1]) {{ throw "file changed before the copy: $($f[0])" }}
}}
"#,
        src = ps_quote(&dir.0.display().to_string())
    );
    Ok(Staged { _dir: dir, script })
}

/// Folders and rights: the user may read everything and change only the data.
fn prelude(user: &str) -> String {
    format!(
        r#"$dir = '{WINDOWS_DIR}'
$user = {user}
Stop-ScheduledTask -TaskName '{TASK}' -ErrorAction SilentlyContinue
Get-Process hq-watchman -ErrorAction SilentlyContinue | Stop-Process -Force
New-Item -ItemType Directory -Force "$dir\bin", "$dir\data", "$dir\logs" | Out-Null
icacls $dir /inheritance:r /grant:r "*S-1-5-18:(OI)(CI)F" "*S-1-5-32-544:(OI)(CI)F" "*S-1-5-19:(OI)(CI)RX" "${{user}}:(OI)(CI)RX" | Out-Null
if ($LASTEXITCODE -ne 0) {{ throw 'icacls failed' }}
icacls "$dir\data" /grant:r "*S-1-5-19:(OI)(CI)M" "${{user}}:(OI)(CI)M" | Out-Null
if ($LASTEXITCODE -ne 0) {{ throw 'icacls failed' }}
icacls "$dir\logs" /grant:r "*S-1-5-19:(OI)(CI)M" | Out-Null
if ($LASTEXITCODE -ne 0) {{ throw 'icacls failed' }}
"#,
        user = ps_quote(user)
    )
}

fn user_name() -> String {
    std::env::var("USERNAME").unwrap_or_default()
}

fn install_script(staged: &Staged) -> String {
    format!(
        r#"{prelude}{stage}$made = $false
try {{
  if (Test-Path "$in\hq-watchman.exe") {{
    Remove-Item "$dir\bin\hq-watchman.prev.exe" -Force -ErrorAction SilentlyContinue
    if (Test-Path "$dir\bin\hq-watchman.exe") {{ Copy-Item "$dir\bin\hq-watchman.exe" "$dir\bin\hq-watchman.prev.exe" -Force; $made = $true }}
    Copy-Item "$in\hq-watchman.exe" "$dir\bin\hq-watchman.exe" -Force
  }}
  if (Test-Path "$in\config.toml") {{ Copy-Item "$in\config.toml", "$in\secrets.env" $dir -Force }}
  if (Test-Path "$in\settings.json") {{
    Get-Content -Raw "$in\settings.json" | & "$dir\bin\hq-watchman.exe" settings set --config "$dir\config.toml"
    if ($LASTEXITCODE -ne 0) {{ throw 'settings were not saved' }}
  }}
  $arg = 'run --config "' + $dir + '\config.toml" --env-file "' + $dir + '\secrets.env" --log-file "' + $dir + '\logs\hq-watchman.log"'
  $action = New-ScheduledTaskAction -Execute "$dir\bin\hq-watchman.exe" -Argument $arg -WorkingDirectory "$dir\data"
  $trigger = New-ScheduledTaskTrigger -AtStartup
  $principal = New-ScheduledTaskPrincipal -UserId 'NT AUTHORITY\LOCAL SERVICE' -LogonType ServiceAccount
  $settings = New-ScheduledTaskSettingsSet -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable -MultipleInstances IgnoreNew
  Register-ScheduledTask -TaskName '{TASK}' -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force | Out-Null
  Start-ScheduledTask -TaskName '{TASK}'
}} catch {{
  if ($made) {{
    Copy-Item "$dir\bin\hq-watchman.prev.exe" "$dir\bin\hq-watchman.exe" -Force
    Start-ScheduledTask -TaskName '{TASK}' -ErrorAction SilentlyContinue
  }}
  throw
}} finally {{
  Remove-Item $in -Recurse -Force -ErrorAction SilentlyContinue
}}
"#,
        prelude = prelude(&user_name()),
        stage = staged.script
    )
}

/// Puts the given files in place and (re)starts the task. A failed program swap is undone by the script.
fn apply(exe_bytes: Option<&[u8]>, plan: Option<&Plan>) -> Result<()> {
    let mut files: Vec<(&str, zeroize::Zeroizing<Vec<u8>>)> = Vec::new();
    if let Some(b) = exe_bytes {
        files.push(("hq-watchman.exe", b.to_vec().into()));
    }
    if let Some(p) = plan {
        files.push(("config.toml", p.config_toml().into_bytes().into()));
        files.push(("secrets.env", p.secrets_env().as_bytes().to_vec().into()));
        files.push(("settings.json", serde_json::to_vec(&p.settings())?.into()));
    }
    let refs: Vec<(&str, &[u8])> = files.iter().map(|(n, d)| (*n, d.as_slice())).collect();
    let staged = stage_files(&refs)?;
    run_elevated(&install_script(&staged))
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

const STOP: &str = "Stop-ScheduledTask -TaskName 'HQ Watchman' -ErrorAction SilentlyContinue\nGet-Process hq-watchman -ErrorAction SilentlyContinue | Stop-Process -Force";

pub fn restart() -> Result<()> {
    run_elevated(&format!("{STOP}\nStart-ScheduledTask -TaskName '{TASK}'"))
}

const DB_FILES: &str = "'hq-watchman.db', 'hq-watchman.db-wal', 'hq-watchman.db-shm'";

/// Replaces the database with a checked backup; the old one is kept and comes back if the bot does not start.
pub async fn restore(db: Vec<u8>, log: &Log) -> Result<()> {
    crate::deploy::check_db(&db).await?;
    let keep = format!(
        "before-restore-{}",
        jiff::Timestamp::now().strftime("%Y%m%d-%H%M%S")
    );
    let k = keep.clone();
    tokio::task::spawn_blocking(move || {
        let staged = stage_files(&[("restore.db", &db)])?;
        run_elevated(&format!(
            r#"{prelude}{stage}try {{
  $d = "$dir\data"
  $keep = "$d\{k}"
  New-Item -ItemType Directory $keep | Out-Null
  foreach ($n in @({DB_FILES})) {{ if (Test-Path "$d\$n") {{ Move-Item "$d\$n" $keep -Force }} }}
  Copy-Item "$in\restore.db" "$d\hq-watchman.db" -Force
}} catch {{
  if (Test-Path "$keep\hq-watchman.db") {{
    foreach ($n in @({DB_FILES})) {{ Remove-Item "$d\$n" -Force -ErrorAction SilentlyContinue }}
    foreach ($n in @({DB_FILES})) {{ if (Test-Path "$keep\$n") {{ Move-Item "$keep\$n" $d -Force }} }}
    Remove-Item $keep -Force -ErrorAction SilentlyContinue
  }}
  throw
}} finally {{
  Remove-Item $in -Recurse -Force -ErrorAction SilentlyContinue
  Start-ScheduledTask -TaskName '{TASK}'
}}
"#,
            prelude = prelude(&user_name()),
            stage = staged.script
        ))
    })
    .await??;
    log(format!(
        "  {} {keep}",
        tr!(
            "прежняя база сохранена в папке data:",
            "the previous database is kept in the data folder:"
        )
    ));
    if let Err(e) = wait_healthy(log).await {
        log(format!(
            "! {}",
            tr!(
                "Бот не запустился, возвращаю прежнюю базу (Windows ещё раз спросит разрешение)",
                "The bot did not start, putting the previous database back (Windows asks for permission again)"
            )
        ));
        tokio::task::spawn_blocking(move || {
            run_elevated(&format!(
                r#"{STOP}
$d = '{WINDOWS_DIR}\data'
$keep = "$d\{keep}"
if (-not (Test-Path $keep)) {{ throw 'the saved database is missing' }}
if (Test-Path "$keep\hq-watchman.db") {{
  foreach ($n in @({DB_FILES})) {{ Remove-Item "$d\$n" -Force -ErrorAction SilentlyContinue }}
  foreach ($n in @({DB_FILES})) {{ if (Test-Path "$keep\$n") {{ Move-Item "$keep\$n" $d -Force }} }}
  Remove-Item $keep -Force -ErrorAction SilentlyContinue
}}
Start-ScheduledTask -TaskName '{TASK}'
"#
            ))
        })
        .await??;
        return Err(e);
    }
    Ok(())
}

pub fn uninstall(keep_data: bool) -> Result<()> {
    let rm = if keep_data {
        format!(
            r"Remove-Item '{WINDOWS_DIR}\bin', '{WINDOWS_DIR}\secrets.env' -Recurse -Force -ErrorAction SilentlyContinue"
        )
    } else {
        format!("Remove-Item '{WINDOWS_DIR}' -Recurse -Force -ErrorAction SilentlyContinue")
    };
    run_elevated(&format!(
        "{STOP}\nUnregister-ScheduledTask -TaskName '{TASK}' -Confirm:$false -ErrorAction SilentlyContinue\n{rm}"
    ))
}
