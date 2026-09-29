#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
#![allow(unsafe_code)] // AttachConsole/SetStdHandle in attach_console()
//! `GlobalTokenTracker` self-extracting installer — per-user, no admin.
//!
//! `installer/package.ps1` drops `payload.zip` (globaltokentracker-ui.exe +
//! globaltokentracker-cli.exe) into `crates/setup/payload/` before building; build.rs
//! embeds it via `include_bytes!`. Plain dev builds embed an empty zip and
//! refuse to install.
//!
//! Modes:
//! - no args / `--dir <p>`          → GUI install
//! - `--uninstall [--dir <p>]`      → GUI uninstall confirmation
//! - `--quiet` (with either)        → console path, no GUI
//! - `--cli`                        → force console path
//!
//! GUI is pure Win32/GDI (gui.rs) — the installer must run on machines that
//! do NOT yet have `WinAppRuntime`.

use anyhow::{Context, Result, bail};
use std::env;
use std::fs;
use std::io::{Cursor, Read};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

mod gui;

static PAYLOAD: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/payload.zip"));

const APP: &str = "GlobalTokenTracker";
const VER: &str = env!("CARGO_PKG_VERSION");
const RUNTIME_URL: &str =
    "https://aka.ms/windowsappsdk/1.8/latest/windowsappruntimeinstall-x64.exe";
const UNINSTALL_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Uninstall\GlobalTokenTracker";
/// Spawned console tools (taskkill/cmd/powershell) must never allocate a
/// console window from our GUI process — it flickers on screen and, under
/// some endpoint-protection policies, console allocation hangs the child.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;

fn local_appdata() -> Result<PathBuf> {
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .context("LOCALAPPDATA not set")
}

fn dest_dir(custom: Option<&str>) -> Result<PathBuf> {
    Ok(match custom {
        Some(d) => PathBuf::from(d),
        None => local_appdata()?.join("Programs").join(APP),
    })
}

fn ps(script: &str) -> Result<std::process::Output> {
    Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("spawn powershell")
}

fn winapp_runtime_present() -> bool {
    ps("Get-AppxPackage -Name 'Microsoft.WindowsAppRuntime*' | Select-Object -First 1 -ExpandProperty Name")
        .is_ok_and(|o| o.status.success() && !o.stdout.is_empty())
}

/// Existing install's recorded (version, location). Lets a newer installer
/// upgrade in place even when the original install used a custom `--dir`.
fn installed_info() -> Option<(String, PathBuf)> {
    let key = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey(UNINSTALL_KEY)
        .ok()?;
    let loc: String = key.get_value("InstallLocation").ok()?;
    let dir = PathBuf::from(loc);
    // Stale entry (dir deleted without uninstalling) is not an install.
    if !dir.join("globaltokentracker-ui.exe").exists() {
        return None;
    }
    let ver: String = key.get_value("DisplayVersion").unwrap_or_default();
    Some((ver, dir))
}

fn ensure_runtime(log: &dyn Fn(String)) -> Result<()> {
    if winapp_runtime_present() {
        log("Windows App Runtime 已就位".into());
        return Ok(());
    }
    log("未检测到 Windows App Runtime，下载微软官方安装程序…".into());
    let tmp = env::temp_dir().join("windowsappruntimeinstall-x64.exe");
    let mut resp = ureq::get(RUNTIME_URL).call().context("download runtime")?;
    let mut f = fs::File::create(&tmp)?;
    std::io::copy(&mut resp.body_mut().as_reader(), &mut f)?;
    log("安装运行时（可能触发 UAC）…".into());
    Command::new(&tmp).arg("--quiet").status().context("run runtime installer")?;
    if !winapp_runtime_present() {
        bail!("Windows App Runtime 安装未完成 —— GUI 将无法启动");
    }
    log("Windows App Runtime 已安装".into());
    Ok(())
}

fn stop_running() {
    // An older installer/uninstaller window left open inside dest would lock
    // globaltokentracker-setup.exe and fail the payload copy. Skip self by
    // image name — a renamed download must not taskkill its own process.
    let self_name = env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
    for name in [
        "globaltokentracker-ui.exe",
        "globaltokentracker-cli.exe",
        "globaltokentracker-setup.exe",
    ] {
        if self_name.as_deref() == Some(name) {
            continue;
        }
        let _ = Command::new("taskkill")
            .args(["/F", "/IM", name])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn extract_payload(dest: &Path, log: &dyn Fn(String)) -> Result<u64> {
    let mut zip = zip::ZipArchive::new(Cursor::new(PAYLOAD)).context("read embedded payload")?;
    if zip.is_empty() {
        bail!("此安装程序不含载荷（开发桩）。请运行 installer\\package.ps1 生成正式安装包。");
    }
    let mut total = 0u64;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        if entry.is_dir() {
            continue;
        }
        let Some(name) = entry.enclosed_name() else {
            continue; // skip path-traversal entries
        };
        let out = dest.join(&name);
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut buf = Vec::with_capacity(usize::try_from(entry.size()).unwrap_or(0));
        entry.read_to_end(&mut buf)?;
        fs::write(&out, &buf)?;
        total += buf.len() as u64;
        log(format!("    + {}", name.display()));
    }
    Ok(total)
}

fn make_shortcuts(dest: &Path) -> Result<()> {
    let start = env::var_os("APPDATA")
        .map(PathBuf::from)
        .context("APPDATA not set")?
        .join(r"Microsoft\Windows\Start Menu\Programs")
        .join(APP);
    fs::create_dir_all(&start)?;
    let ui = dest.join("globaltokentracker-ui.exe");
    let uninstall = dest.join("globaltokentracker-setup.exe");
    let script = format!(
        "$ws=New-Object -ComObject WScript.Shell;\
         $s=$ws.CreateShortcut('{}');$s.TargetPath='{}';$s.IconLocation='{}';$s.WorkingDirectory='{}';$s.Save();\
         $u=$ws.CreateShortcut('{}');$u.TargetPath='{}';$u.Arguments='--uninstall';$u.Save()",
        start.join(format!("{APP}.lnk")).display(),
        ui.display(),
        ui.display(),
        dest.display(),
        start.join(format!("Uninstall {APP}.lnk")).display(),
        uninstall.display(),
    );
    let out = ps(&script)?;
    if !out.status.success() {
        bail!("创建快捷方式失败: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}

fn register_uninstall(dest: &Path, size: u64) -> Result<()> {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(UNINSTALL_KEY)?;
    let setup = dest.join("globaltokentracker-setup.exe");
    key.set_value("DisplayName", &APP)?;
    key.set_value("DisplayVersion", &VER)?;
    key.set_value("Publisher", &APP)?;
    key.set_value("InstallLocation", &dest.display().to_string())?;
    key.set_value("DisplayIcon", &dest.join("globaltokentracker-ui.exe").display().to_string())?;
    key.set_value("EstimatedSize", &u32::try_from(size / 1024).unwrap_or(u32::MAX))?;
    key.set_value("NoModify", &1u32)?;
    key.set_value("NoRepair", &1u32)?;
    // Always pin the resolved dir — a custom --dir install must not
    // uninstall into the default location.
    let cmd = format!(
        "\"{}\" --uninstall --dir \"{}\"",
        setup.display(),
        dest.display()
    );
    key.set_value("UninstallString", &cmd)?;
    key.set_value("QuietUninstallString", &format!("{cmd} --quiet"))?;
    Ok(())
}

/// Append dest to the *user* PATH so `globaltokentracker-cli` works in
/// terminals. Best-effort: EDR/policy may guard HKCU\Environment for unsigned
/// binaries — a denied write must not fail the whole install.
fn extend_user_path(dest: &Path, log: &dyn Fn(String)) {
    let inner = || -> Result<bool> {
        let env_key = open_user_env()?;
        let wrote = add_dir_to_path(&env_key, &dest.display().to_string(), &backup_path_value)?;
        if wrote {
            broadcast_env_change();
        }
        Ok(wrote)
    };
    match inner() {
        Ok(true) => log("已加入用户 PATH（新开的终端生效）".into()),
        Ok(false) => log("用户 PATH 已包含安装目录".into()),
        Err(e) => log(format!(
            "PATH 追加被拒（{e}）——不影响使用，CLI 可用完整路径"
        )),
    }
}

fn remove_user_path(dest: &Path, log: &dyn Fn(String)) {
    let inner = || -> Result<bool> {
        let env_key = open_user_env()?;
        let wrote =
            remove_dir_from_path(&env_key, &dest.display().to_string(), &backup_path_value)?;
        if wrote {
            broadcast_env_change();
        }
        Ok(wrote)
    };
    match inner() {
        Ok(true) => log("已从用户 PATH 移除安装目录".into()),
        Ok(false) => {}
        Err(e) => log(format!("PATH 清理跳过（{e}）——未改动用户 PATH")),
    }
}

fn open_user_env() -> Result<winreg::RegKey> {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    Ok(hkcu.open_subkey_with_flags(
        "Environment",
        winreg::enums::KEY_READ | winreg::enums::KEY_WRITE,
    )?)
}

/// Expand `%VAR%` references with the Windows rules (`ExpandEnvironmentStringsW`,
/// the same expansion a `REG_EXPAND_SZ` PATH gets at logon): unknown variables
/// stay literal. Any API failure returns the input unchanged, i.e. comparison
/// degrades to the old literal behaviour rather than erroring.
fn expand_env(s: &str) -> String {
    use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
    use windows::core::PCWSTR;
    if !s.contains('%') {
        return s.to_string();
    }
    let src: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    let mut buf = vec![0u16; 512];
    loop {
        // SAFETY: `src` is NUL-terminated and outlives the call; `buf` is a
        // valid writable slice whose length the API honours.
        let need =
            unsafe { ExpandEnvironmentStringsW(PCWSTR(src.as_ptr()), Some(&mut buf)) } as usize;
        if need == 0 {
            return s.to_string();
        }
        if need <= buf.len() {
            // `need` counts the terminating NUL.
            return String::from_utf16_lossy(&buf[..need - 1]);
        }
        buf.resize(need, 0);
    }
}

/// Same directory? Both sides are `%VAR%`-expanded first (a PATH entry written
/// as `%LOCALAPPDATA%\Programs\X` is the same directory as its absolute
/// spelling), then compared ASCII-case-insensitively without surrounding
/// whitespace / trailing `\`. Purely textual otherwise: no `..`, `/`, or
/// 8.3-short-name resolution.
fn path_eq(a: &str, b: &str) -> bool {
    let key = |p: &str| expand_env(p).trim().trim_end_matches('\\').to_string();
    key(a).eq_ignore_ascii_case(&key(b))
}

fn path_with(cur: &str, dir: &str) -> Option<String> {
    if cur.split(';').any(|p| path_eq(p, dir)) {
        return None;
    }
    Some(if cur.is_empty() {
        dir.to_string()
    } else if cur.ends_with(';') {
        format!("{cur}{dir}")
    } else {
        format!("{cur};{dir}")
    })
}

fn path_without(cur: &str, dir: &str) -> Option<String> {
    let segments: Vec<&str> = cur.split(';').collect();
    if !segments.iter().any(|p| path_eq(p, dir)) {
        return None;
    }
    let kept: Vec<&str> = segments.into_iter().filter(|p| !path_eq(p, dir)).collect();
    Some(kept.join(";"))
}

fn read_path_value(key: &winreg::RegKey) -> Result<Option<(String, winreg::enums::RegType)>> {
    use winreg::enums::RegType;
    let raw = match key.get_raw_value("Path") {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("读取用户 PATH"),
    };
    if !matches!(raw.vtype, RegType::REG_SZ | RegType::REG_EXPAND_SZ) {
        bail!("用户 PATH 类型异常（{:?}）", raw.vtype);
    }
    let units: Vec<u16> = raw
        .bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    let text = String::from_utf16(&units).context("用户 PATH 不是有效 UTF-16")?;
    Ok(Some((text.trim_end_matches('\0').to_string(), raw.vtype)))
}

fn write_path_value(
    key: &winreg::RegKey,
    value: &str,
    vtype: winreg::enums::RegType,
) -> Result<()> {
    let bytes: Vec<u8> = value
        .encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect();
    key.set_raw_value("Path", &winreg::RegValue { bytes, vtype })
        .context("写入用户 PATH")
}

type Backup<'a> = &'a dyn Fn(&str, &winreg::enums::RegType) -> Result<()>;

/// `backup` runs with the previous raw value right before a write; its
/// failure aborts the modification.
fn add_dir_to_path(key: &winreg::RegKey, dir: &str, backup: Backup) -> Result<bool> {
    match read_path_value(key)? {
        None => {
            write_path_value(key, dir, winreg::enums::RegType::REG_EXPAND_SZ)?;
            Ok(true)
        }
        Some((cur, vtype)) => {
            let Some(new) = path_with(&cur, dir) else {
                return Ok(false);
            };
            backup(&cur, &vtype).context("备份原 PATH 失败")?;
            write_path_value(key, &new, vtype)?;
            Ok(true)
        }
    }
}

fn remove_dir_from_path(key: &winreg::RegKey, dir: &str, backup: Backup) -> Result<bool> {
    let Some((cur, vtype)) = read_path_value(key)? else {
        return Ok(false);
    };
    let Some(new) = path_without(&cur, dir) else {
        return Ok(false);
    };
    if new.trim().is_empty()
        && cur
            .split(';')
            .any(|p| !p.trim().is_empty() && !path_eq(p, dir))
    {
        bail!("清理结果为空，拒绝写入");
    }
    backup(&cur, &vtype).context("备份原 PATH 失败")?;
    write_path_value(key, &new, vtype)?;
    Ok(true)
}

fn backup_path_value(raw: &str, vtype: &winreg::enums::RegType) -> Result<()> {
    let dir = env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .context("USERPROFILE not set")?
        .join(".globaltokentracker");
    fs::create_dir_all(&dir)?;
    let tname = if *vtype == winreg::enums::RegType::REG_EXPAND_SZ {
        "REG_EXPAND_SZ"
    } else {
        "REG_SZ"
    };
    fs::write(dir.join("path.bak"), format!("type={tname}\n{raw}\n"))?;
    Ok(())
}

fn broadcast_env_change() {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
    };
    use windows::core::w;
    unsafe {
        let _ = SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(w!("Environment").as_ptr() as isize),
            SMTO_ABORTIFHUNG,
            5000,
            None,
        );
    }
}

/// Shared install body — `step(pct, label)` drives a progress bar,
/// `log(line)` records detail lines. Used by both the console path and
/// the GUI worker thread.
pub fn install_steps(
    dest: &Path,
    want_shortcut: bool,
    want_path: bool,
    step: &mut dyn FnMut(u32, &str),
    log: &dyn Fn(String),
) -> Result<()> {
    step(5, "检查 Windows App Runtime…");
    ensure_runtime(log)?;
    step(15, "结束正在运行的实例…");
    stop_running();
    fs::create_dir_all(dest)?;
    step(25, "释放程序文件…");
    let size = extract_payload(dest, log)?;
    // Persist a copy of this installer as the uninstaller.
    let self_exe = env::current_exe()?;
    let setup_copy = dest.join("globaltokentracker-setup.exe");
    if self_exe.canonicalize()? != setup_copy.canonicalize().unwrap_or(setup_copy.clone()) {
        fs::copy(&self_exe, &setup_copy)?;
    }
    step(70, "写入卸载注册信息…");
    register_uninstall(dest, size)?;
    if want_shortcut {
        step(80, "创建开始菜单快捷方式…");
        make_shortcuts(dest)?;
    }
    if want_path {
        step(90, "加入用户 PATH…");
        extend_user_path(dest, log);
    }
    step(100, "安装完成");
    log(format!("程序目录 : {}", dest.display()));
    Ok(())
}

/// Start-menu folder holding the app and uninstall shortcuts.
fn start_menu_dir() -> Result<PathBuf> {
    Ok(env::var_os("APPDATA")
        .map(PathBuf::from)
        .context("APPDATA not set")?
        .join(r"Microsoft\Windows\Start Menu\Programs")
        .join(APP))
}

/// `<profile>\.globaltokentracker` — the ledger, settings and backups the app
/// keeps outside the program directory. An uninstall leaves it alone unless
/// asked to purge it.
fn user_data_dir_in(profile: &Path) -> PathBuf {
    profile.join(".globaltokentracker")
}

fn user_data_dir() -> Option<PathBuf> {
    env::var_os("USERPROFILE").map(|p| user_data_dir_in(Path::new(&p)))
}

/// Bytes under `dir` (files only; symlinks are not followed). Bounded, so a
/// pathological tree can't stall the window that asks for the number.
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut seen = 0usize;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            seen += 1;
            if seen > 50_000 {
                return total;
            }
            let Ok(md) = fs::symlink_metadata(e.path()) else {
                continue;
            };
            if md.is_dir() {
                stack.push(e.path());
            } else if md.is_file() {
                total += md.len();
            }
        }
    }
    total
}

/// "31.6 MB" style size for the uninstall summary.
#[allow(clippy::cast_precision_loss)] // a size label; sub-byte precision is irrelevant
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// What an uninstall of `dest` would touch — read-only, for the confirmation
/// screen. Items that are not present are simply not listed.
pub struct UninstallPlan {
    pub program_bytes: u64,
    pub shortcuts: bool,
    pub on_path: bool,
    pub registered: bool,
    /// The user-data directory, when it exists.
    pub data_dir: Option<PathBuf>,
    pub data_bytes: u64,
}

pub fn uninstall_plan(dest: &Path) -> UninstallPlan {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    let on_path = hkcu
        .open_subkey("Environment")
        .ok()
        .and_then(|k| read_path_value(&k).ok().flatten())
        .is_some_and(|(cur, _)| path_with(&cur, &dest.display().to_string()).is_none());
    let data_dir = user_data_dir().filter(|d| d.is_dir());
    let data_bytes = data_dir.as_deref().map_or(0, dir_size);
    UninstallPlan {
        program_bytes: dir_size(dest),
        shortcuts: start_menu_dir().is_ok_and(|d| d.exists()),
        on_path,
        registered: hkcu.open_subkey(UNINSTALL_KEY).is_ok(),
        data_dir,
        data_bytes,
    }
}

/// Delete the user-data directory. Refuses anything but exactly
/// `<profile>\.globaltokentracker`, so a bad argument can never turn "delete my
/// data" into "delete my profile".
fn remove_user_data(dir: &Path, profile: &Path) -> Result<()> {
    if dir != user_data_dir_in(profile) {
        bail!("拒绝删除非用户数据目录：{}", dir.display());
    }
    match fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("删除用户数据目录 {}", dir.display())),
    }
}

/// `purge_data`: also delete the user-data directory (ledger, settings,
/// backups). Off for `--quiet` and by default in the GUI. A failed purge does
/// not fail the uninstall — the program is already gone by then; the caller
/// can see whether the directory is still there.
pub fn uninstall_steps(
    dest: &Path,
    purge_data: bool,
    step: &mut dyn FnMut(u32, &str),
    log: &dyn Fn(String),
) -> Result<()> {
    step(10, "结束正在运行的实例…");
    stop_running();
    step(35, "移除快捷方式…");
    let _ = fs::remove_dir_all(start_menu_dir()?);
    step(55, "移除卸载注册项…");
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    let _ = hkcu.delete_subkey_all(UNINSTALL_KEY);
    step(75, "清理用户 PATH…");
    remove_user_path(dest, log);
    if purge_data {
        step(90, "删除用户数据…");
        let profile = env::var_os("USERPROFILE").map(PathBuf::from);
        match (user_data_dir(), profile) {
            (Some(dir), Some(profile)) => match remove_user_data(&dir, &profile) {
                Ok(()) => log(format!("已删除用户数据 {}", dir.display())),
                Err(e) => log(format!("用户数据未能删除：{e:#}")),
            },
            _ => log("USERPROFILE 未设置，跳过用户数据删除".into()),
        }
    } else {
        log("用户数据保留在 %USERPROFILE%\\.globaltokentracker".into());
    }
    step(100, "已卸载");
    Ok(())
}

/// After an upgrade moved the install dir, retire the old one: drop its
/// PATH entry and defer-delete the folder (old exes were already stopped by
/// `stop_running`). Refuses to wipe when `new` nests inside `old` — a rmdir
/// of the parent would eat the fresh install.
pub fn cleanup_prior_install(old: &Path, new: &Path, log: &dyn Fn(String)) {
    let same = old
        .canonicalize()
        .ok()
        .zip(new.canonicalize().ok())
        .is_some_and(|(a, b)| a == b)
        || old.display().to_string().trim_end_matches('\\').eq_ignore_ascii_case(
            new.display().to_string().trim_end_matches('\\'),
        );
    if same || new.starts_with(old) || !old.join("globaltokentracker-ui.exe").exists() {
        return;
    }
    log(format!("清理旧安装目录 {}", old.display()));
    remove_user_path(old, log);
    if let Err(e) = schedule_dir_delete(old) {
        log(format!("旧目录延迟删除失败：{e}"));
    }
}

/// Schedule deletion of `dest` after this process exits. Must be called at
/// the LAST possible moment — while running, our own exe inside `dest` is
/// locked and the rmdir would leave it behind. GUI mode calls this when the
/// window actually closes; console mode right before exit.
/// `raw_arg` keeps `/C ...` unquoted so cmd parses `&`/`>` as metachars
/// (`.arg()` would backslash-escape the inner quotes and break `/C`).
pub fn schedule_dir_delete(dest: &Path) -> Result<()> {
    Command::new("cmd")
        .raw_arg(format!(
            "/C ping 127.0.0.1 -n 2 >nul & rmdir /S /Q \"{}\"",
            dest.display()
        ))
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("schedule self-delete")?;
    Ok(())
}

/// A GUI-subsystem exe launched from a terminal has no console — attach to
/// the parent's and reopen std handles so `--quiet`/`--help` still print.
#[cfg(windows)]
fn attach_console() {
    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_MODE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE, SetStdHandle,
    };
    use windows::core::w;
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            return;
        }
        let share = FILE_SHARE_MODE(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0);
        let rw = GENERIC_READ.0 | GENERIC_WRITE.0;
        if let Ok(out) = CreateFileW(
            w!("CONOUT$"),
            rw,
            share,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        ) {
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, out);
            let _ = SetStdHandle(STD_ERROR_HANDLE, out);
        }
        if let Ok(inp) = CreateFileW(
            w!("CONIN$"),
            rw,
            share,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        ) {
            let _ = SetStdHandle(STD_INPUT_HANDLE, inp);
        }
    }
}

fn usage() {
    println!("{APP} {VER} 安装程序");
    println!();
    println!("用法:");
    println!("  globaltokentracker-setup [--dir <路径>]      图形界面安装");
    println!("  globaltokentracker-setup --uninstall         图形界面卸载");
    println!("  globaltokentracker-setup --quiet [--uninstall] [--dir <路径>]");
    println!("                                             静默命令行安装/卸载");
    println!("  globaltokentracker-setup --cli               强制命令行模式");
    println!("  globaltokentracker-setup --quiet --launch    静默安装后启动程序");
}

fn main() -> Result<()> {
    let mut uninstall_flag = false;
    let mut quiet = false;
    let mut force_cli = false;
    let mut launch = false;
    let mut help = false;
    let mut dir: Option<String> = None;
    let mut it = env::args().skip(1);
    let mut parse_err: Option<String> = None;
    while let Some(a) = it.next() {
        match a.as_str() {
            "--uninstall" | "-u" => uninstall_flag = true,
            "--quiet" | "-q" => quiet = true,
            "--cli" => force_cli = true,
            "--launch" => launch = true,
            "--dir" => match it.next() {
                Some(d) if !d.starts_with("--") => dir = Some(d),
                _ => {
                    parse_err = Some("--dir 需要紧跟一个路径".into());
                    break;
                }
            },
            "--help" | "-h" => help = true,
            other => {
                parse_err = Some(format!("未知参数 {other}"));
                break;
            }
        }
    }
    let gui_mode = !quiet && !force_cli && !help;
    if !gui_mode {
        attach_console();
    }
    if help {
        usage();
        return Ok(());
    }
    if let Some(e) = parse_err {
        eprintln!("{e}");
        usage();
        std::process::exit(2);
    }
    let prior = if uninstall_flag { None } else { installed_info() };
    let dest = match dir.as_deref() {
        Some(d) => PathBuf::from(d),
        // The uninstaller copy lives at <dest>\globaltokentracker-setup.exe —
        // uninstalling from it must target our own dir, not the default path.
        None if uninstall_flag => env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or(dest_dir(None)?),
        // Upgrade in place: a custom --dir install keeps its location.
        None => match &prior {
            Some((_, d)) => d.clone(),
            None => dest_dir(None)?,
        },
    };

    if gui_mode {
        return gui::run(
            if uninstall_flag { gui::Mode::Uninstall } else { gui::Mode::Install },
            &dest,
            prior.as_ref().map(|(v, _)| v.as_str()),
            prior.as_ref().map(|(_, d)| d.clone()),
        );
    }

    let log = |s: String| println!("    {s}");
    let mut step = |pct: u32, msg: &str| println!("==> [{pct:3}%] {msg}");
    if uninstall_flag {
        println!("{APP} 卸载 —— 移除 {}", dest.display());
        uninstall_steps(&dest, false, &mut step, &log)?;
        schedule_dir_delete(&dest)?; // fires after this process exits
        println!("已移除程序、快捷方式与卸载项；用户数据保留在 %USERPROFILE%\\.globaltokentracker");
    } else {
        match &prior {
            Some((v, _)) if v == VER => println!("{APP} {VER} 已安装 —— 重装修复 {}", dest.display()),
            Some((v, _)) => println!("{APP} v{v} → v{VER} —— 更新 {}", dest.display()),
            None => println!("{APP} {VER} 安装程序 —— 安装到 {}", dest.display()),
        }
        install_steps(&dest, true, true, &mut step, &log)?;
        if let Some((_, old)) = &prior {
            cleanup_prior_install(old, &dest, &log);
        }
        if launch && quiet {
            let ui = dest.join("globaltokentracker-ui.exe");
            match Command::new(&ui)
                .creation_flags(DETACHED_PROCESS)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(_) => log("已启动 globaltokentracker-ui".into()),
                Err(e) => log(format!("启动 globaltokentracker-ui 失败：{e}")),
            }
        }
        println!();
        println!("{APP} {VER} 安装完成。");
        println!("  开始菜单 : %APPDATA%\\Microsoft\\Windows\\Start Menu\\Programs\\{APP}");
        println!("  数据目录 : %USERPROFILE%\\.globaltokentracker（首次运行创建）");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, RegType};

    const DIR: &str = r"C:\Apps\GTT";

    #[allow(clippy::unnecessary_wraps)]
    fn no_backup(_: &str, _: &RegType) -> Result<()> {
        Ok(())
    }

    #[test]
    fn human_size_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(33_120_256), "31.6 MB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    fn scratch_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = env::temp_dir().join(format!("gtt-setup-{tag}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn dir_size_sums_nested_files() {
        let d = scratch_dir("size");
        fs::create_dir_all(d.join("a/b")).unwrap();
        fs::write(d.join("x.bin"), [0u8; 100]).unwrap();
        fs::write(d.join("a/y.bin"), [0u8; 50]).unwrap();
        fs::write(d.join("a/b/z.bin"), [0u8; 7]).unwrap();
        assert_eq!(dir_size(&d), 157);
        assert_eq!(dir_size(&d.join("missing")), 0);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn purge_only_touches_the_data_dir() {
        let profile = scratch_dir("purge");
        let data = user_data_dir_in(&profile);
        fs::create_dir_all(data.join("backups")).unwrap();
        fs::write(data.join("ledger.db"), b"x").unwrap();
        let bystander = profile.join("Documents");
        fs::create_dir_all(&bystander).unwrap();
        fs::write(bystander.join("keep.txt"), b"x").unwrap();

        // Anything but `<profile>\.globaltokentracker` is refused untouched.
        assert!(remove_user_data(&profile, &profile).is_err());
        assert!(remove_user_data(&bystander, &profile).is_err());
        assert!(bystander.join("keep.txt").exists());
        assert!(data.join("ledger.db").exists());

        remove_user_data(&data, &profile).unwrap();
        assert!(!data.exists());
        assert!(bystander.join("keep.txt").exists());
        // Already gone → still Ok (idempotent).
        remove_user_data(&data, &profile).unwrap();
        fs::remove_dir_all(&profile).unwrap();
    }

    #[test]
    fn with_cases() {
        assert_eq!(path_with("", DIR).as_deref(), Some(DIR));
        assert_eq!(path_with("a;b", DIR).as_deref(), Some(r"a;b;C:\Apps\GTT"));
        assert_eq!(path_with("a;b;", DIR).as_deref(), Some(r"a;b;C:\Apps\GTT"));
        assert_eq!(path_with(r"a;c:\apps\gtt\;b", DIR), None);
        assert_eq!(path_with(r"a; C:\APPS\GTT ", DIR), None);
    }

    #[test]
    fn without_cases() {
        assert_eq!(
            path_without(r"C:\x;%USERPROFILE%\bin;c:\apps\gtt\;;C:\y", DIR).as_deref(),
            Some(r"C:\x;%USERPROFILE%\bin;;C:\y")
        );
        assert_eq!(
            path_without(r"a;C:\Apps\GTT;b", DIR).as_deref(),
            Some("a;b")
        );
        assert_eq!(path_without("a;b", DIR), None);
        assert_eq!(path_without(DIR, DIR).as_deref(), Some(""));
    }

    /// The default install dir spelled with `%LOCALAPPDATA%` and absolutely.
    /// Pure strings: nothing here reads or writes the registry.
    fn env_spellings() -> (String, String) {
        let local = env::var("LOCALAPPDATA").expect("LOCALAPPDATA must be set");
        (
            r"%LOCALAPPDATA%\Programs\GlobalTokenTracker".to_string(),
            format!(r"{local}\Programs\GlobalTokenTracker"),
        )
    }

    #[test]
    fn expand_env_follows_windows_rules() {
        let local = env::var("LOCALAPPDATA").expect("LOCALAPPDATA must be set");
        assert_eq!(expand_env(r"%LOCALAPPDATA%\x"), format!(r"{local}\x"));
        // Variable names are case-insensitive.
        assert_eq!(expand_env(r"%localappdata%\x"), format!(r"{local}\x"));
        // Unknown variables stay literal, like the API.
        assert_eq!(expand_env(r"%GTT_NO_SUCH_VAR%\x"), r"%GTT_NO_SUCH_VAR%\x");
        assert_eq!(expand_env(r"C:\plain"), r"C:\plain");
        assert_eq!(expand_env(""), "");
        // Longer than the initial 512-unit buffer → exercises the grow-and-retry path.
        let tail = "x".repeat(600);
        assert_eq!(
            expand_env(&format!(r"%LOCALAPPDATA%\{tail}")),
            format!(r"{local}\{tail}")
        );
    }

    #[test]
    fn eq_expands_env_before_comparing() {
        let (var, abs) = env_spellings();
        assert!(path_eq(&var, &abs));
        assert!(path_eq(&abs, &var));
        // Existing normalisation still applies on top of expansion.
        assert!(path_eq(&format!(" {var}\\ "), &abs));
        assert!(path_eq(
            &var.to_ascii_lowercase(),
            &abs.to_ascii_uppercase()
        ));
        // A different directory is still different.
        assert!(!path_eq(r"%LOCALAPPDATA%\Programs\Other", &abs));
        // An unresolvable variable only equals its own literal spelling.
        assert!(path_eq(r"%GTT_NO_SUCH_VAR%\x", r"%gtt_no_such_var%\X\"));
        assert!(!path_eq(r"%GTT_NO_SUCH_VAR%\x", &abs));
    }

    #[test]
    fn with_recognises_env_spelling() {
        let (var, abs) = env_spellings();
        // Already present as %LOCALAPPDATA%\… → adding the absolute path is a no-op.
        assert_eq!(path_with(&format!(r"C:\x;{var};C:\y"), &abs), None);
        assert_eq!(path_with(&var, &abs), None);
        // …and the other way round.
        assert_eq!(path_with(&format!(r"C:\x;{abs};C:\y"), &var), None);
        // Unrelated %VAR% entries don't count as a match.
        assert_eq!(
            path_with(r"%USERPROFILE%\bin", &abs).as_deref(),
            Some(format!(r"%USERPROFILE%\bin;{abs}").as_str())
        );
    }

    #[test]
    fn without_removes_both_spellings() {
        let (var, abs) = env_spellings();
        let cur = format!(r"C:\x;{var};%USERPROFILE%\bin;{abs}\;C:\y");
        let expect = r"C:\x;%USERPROFILE%\bin;C:\y";
        // Uninstall passes the absolute dir; the variable spelling goes too.
        assert_eq!(path_without(&cur, &abs).as_deref(), Some(expect));
        assert_eq!(path_without(&cur, &var).as_deref(), Some(expect));
        // Only the variable spelling present.
        assert_eq!(
            path_without(&format!("a;{var};b"), &abs).as_deref(),
            Some("a;b")
        );
        // Nothing but the two spellings → empty (the "refuse to write an empty
        // PATH" guard in remove_dir_from_path judges by path_eq as well).
        assert_eq!(
            path_without(&format!("{var};{abs}"), &abs).as_deref(),
            Some("")
        );
    }

    struct Scratch {
        path: String,
    }

    impl Scratch {
        fn new() -> (Self, RegKey) {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = format!(
                r"Software\GlobalTokenTracker-test-{}-{nanos}",
                std::process::id()
            );
            let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
                .create_subkey(&path)
                .unwrap();
            (Self { path }, key)
        }

        fn open(&self, flags: u32) -> RegKey {
            RegKey::predef(HKEY_CURRENT_USER)
                .open_subkey_with_flags(&self.path, flags)
                .unwrap()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = RegKey::predef(HKEY_CURRENT_USER).delete_subkey_all(&self.path);
        }
    }

    #[test]
    fn registry_roundtrip_preserves_expand_sz() {
        let (_g, key) = Scratch::new();
        let orig = r"C:\x;%USERPROFILE%\bin";
        write_path_value(&key, orig, RegType::REG_EXPAND_SZ).unwrap();
        assert!(add_dir_to_path(&key, DIR, &no_backup).unwrap());
        let (cur, t) = read_path_value(&key).unwrap().unwrap();
        assert_eq!(t, RegType::REG_EXPAND_SZ);
        assert!(cur.contains(r"%USERPROFILE%\bin"));
        assert!(cur.ends_with(DIR));
        assert!(!add_dir_to_path(&key, DIR, &no_backup).unwrap());
        assert!(remove_dir_from_path(&key, DIR, &no_backup).unwrap());
        assert_eq!(
            read_path_value(&key).unwrap(),
            Some((orig.to_string(), RegType::REG_EXPAND_SZ))
        );
    }

    #[test]
    fn registry_absent_value() {
        let (_g, key) = Scratch::new();
        assert!(!remove_dir_from_path(&key, DIR, &no_backup).unwrap());
        assert!(add_dir_to_path(&key, DIR, &no_backup).unwrap());
        assert_eq!(
            read_path_value(&key).unwrap(),
            Some((DIR.to_string(), RegType::REG_EXPAND_SZ))
        );
    }

    #[test]
    fn write_only_handle_errors_without_change() {
        let (g, key) = Scratch::new();
        let orig = format!(r"C:\x;{DIR};%USERPROFILE%\bin");
        write_path_value(&key, &orig, RegType::REG_EXPAND_SZ).unwrap();
        let wo = g.open(KEY_WRITE);
        assert!(remove_dir_from_path(&wo, DIR, &no_backup).is_err());
        assert!(add_dir_to_path(&wo, r"C:\other", &no_backup).is_err());
        let ro = g.open(KEY_READ);
        assert_eq!(
            read_path_value(&ro).unwrap(),
            Some((orig, RegType::REG_EXPAND_SZ))
        );
    }

    #[test]
    fn backup_failure_blocks_write() {
        let (_g, key) = Scratch::new();
        write_path_value(&key, "a", RegType::REG_SZ).unwrap();
        let fail = |_: &str, _: &RegType| -> Result<()> { bail!("nope") };
        assert!(add_dir_to_path(&key, DIR, &fail).is_err());
        assert_eq!(
            read_path_value(&key).unwrap(),
            Some(("a".to_string(), RegType::REG_SZ))
        );
    }
}
