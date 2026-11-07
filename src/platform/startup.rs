//! Start with Windows (HKCU Run key) and the Start-menu shortcut, and the few HKCU registry reads
//! and writes they need (`registry`, also used for Windows' light or dark mode).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Name of the value under the Run key. (Task Manager's "Startup apps" shows the exe's
/// FileDescription instead, see `build.rs`.) Debug builds use their own, like their own data
/// folder and single-instance names, so a test copy never replaces the real entry.
#[cfg(not(debug_assertions))]
pub const RUN_VALUE_NAME: &str = "Truffle";
#[cfg(debug_assertions)]
pub const RUN_VALUE_NAME: &str = "Truffle (dev)";
/// File name of the Start-menu shortcut (separate for debug builds, as above).
#[cfg(not(debug_assertions))]
pub const SHORTCUT_FILE_NAME: &str = "Truffle.lnk";
#[cfg(debug_assertions)]
pub const SHORTCUT_FILE_NAME: &str = "Truffle (dev).lnk";
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Where Task Manager records startup entries the user switched off.
pub const STARTUP_APPROVED_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

/// `"<exe>" --minimized`
pub fn run_command(exe: &Path) -> OsString {
    let mut command = OsString::from("\"");
    command.push(exe.as_os_str());
    command.push("\" --minimized");
    command
}

/// Whether a Run command line starts `exe` (quoted or not, any case, either slash).
pub fn command_targets(command: &str, exe: &Path) -> bool {
    let command = command.trim();
    let target = match command.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or(""),
        None => command.split_whitespace().next().unwrap_or(""),
    };
    !target.is_empty() && normalize(target) == normalize(&exe.to_string_lossy())
}

fn normalize(path: &str) -> String {
    let path = path.replace('/', "\\");
    let path = path.strip_prefix(r"\\?\").unwrap_or(&path);
    path.to_lowercase()
}

/// Task Manager's StartupApproved data: the first byte is odd when the entry was switched off.
pub fn startup_approved_disabled(data: &[u8]) -> bool {
    data.first().is_some_and(|b| b & 1 == 1)
}

fn current_exe() -> Result<PathBuf> {
    std::env::current_exe().context("couldn't find where Truffle is running from")
}

pub fn set_start_with_windows(enabled: bool) -> Result<()> {
    if enabled {
        let exe = current_exe()?;
        registry::set_string(RUN_KEY, RUN_VALUE_NAME, &run_command(&exe))
            .context("couldn't add Truffle to the apps that start with Windows")?;
        // If it was switched off in Task Manager, that would silently win: clear it.
        if let Err(e) = registry::delete_value(STARTUP_APPROVED_KEY, RUN_VALUE_NAME) {
            log::debug!("could not clear the Task Manager startup flag: {e}");
        }
        log::info!("Truffle will start with Windows");
    } else {
        registry::delete_value(RUN_KEY, RUN_VALUE_NAME)
            .context("couldn't remove Truffle from the apps that start with Windows")?;
        log::info!("Truffle will no longer start with Windows");
    }
    Ok(())
}

pub fn starts_with_windows() -> bool {
    let Ok(exe) = current_exe() else { return false };
    let command = match registry::get_string(RUN_KEY, RUN_VALUE_NAME) {
        Ok(Some(command)) => command,
        Ok(None) => return false,
        Err(e) => {
            log::debug!("could not read the startup entry: {e}");
            return false;
        }
    };
    if !command_targets(&command, &exe) {
        return false;
    }
    match registry::get_binary(STARTUP_APPROVED_KEY, RUN_VALUE_NAME) {
        Ok(Some(data)) => !startup_approved_disabled(&data),
        _ => true,
    }
}

pub fn create_start_menu_shortcut() -> Result<PathBuf> {
    let exe = current_exe()?;
    let dirs = directories::BaseDirs::new().context("couldn't find your user folders")?;
    // `config_dir` is %APPDATA% (Roaming) on Windows.
    let programs = dirs.config_dir().join(r"Microsoft\Windows\Start Menu\Programs");
    std::fs::create_dir_all(&programs)
        .with_context(|| format!("couldn't open the Start menu folder ({})", programs.display()))?;
    let link = programs.join(SHORTCUT_FILE_NAME);
    shortcut::create(&exe, &link).context("couldn't create the Start menu shortcut")?;
    log::info!("created {}", link.display());
    Ok(link)
}

#[cfg(windows)]
pub(super) mod registry {
    use std::ffi::{OsStr, c_void};
    use std::os::windows::ffi::OsStrExt;

    use anyhow::{Result, bail};
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS, WIN32_ERROR};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, REG_SZ, RRF_RT_REG_BINARY, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
        RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
    };

    fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
        s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
    }

    fn os_error(code: WIN32_ERROR) -> std::io::Error {
        std::io::Error::from_raw_os_error(code as i32)
    }

    /// Writes a REG_SZ value under HKCU, creating the key if needed.
    pub fn set_string(key: &str, name: &str, value: &OsStr) -> Result<()> {
        let (key, name, data) = (wide(key), wide(name), wide(value));
        let bytes = u32::try_from(data.len() * 2)?;
        // SAFETY: all pointers are valid, NUL-terminated buffers that outlive the call.
        let code = unsafe {
            RegSetKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), REG_SZ, data.as_ptr().cast::<c_void>(), bytes)
        };
        if code != ERROR_SUCCESS {
            bail!(os_error(code));
        }
        Ok(())
    }

    /// Deletes a value under HKCU; a value (or key) that isn't there is fine.
    pub fn delete_value(key: &str, name: &str) -> Result<()> {
        let (key, name) = (wide(key), wide(name));
        // SAFETY: valid NUL-terminated strings.
        let code = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) };
        if code != ERROR_SUCCESS && code != ERROR_FILE_NOT_FOUND {
            bail!(os_error(code));
        }
        Ok(())
    }

    /// Reads raw value bytes of the given type (`flags`) under `root`; `None` if it doesn't exist.
    fn get_raw(root: HKEY, key: &str, name: &str, flags: u32) -> Result<Option<Vec<u8>>> {
        let (key, name) = (wide(key), wide(name));
        let mut buffer: Vec<u8> = vec![0; 512];
        for _ in 0..4 {
            let mut size = u32::try_from(buffer.len())?;
            // SAFETY: `buffer` has `size` writable bytes; strings are NUL-terminated.
            let code = unsafe {
                RegGetValueW(
                    root,
                    key.as_ptr(),
                    name.as_ptr(),
                    flags,
                    std::ptr::null_mut(),
                    buffer.as_mut_ptr().cast::<c_void>(),
                    &mut size,
                )
            };
            match code {
                ERROR_SUCCESS => {
                    buffer.truncate(size as usize);
                    return Ok(Some(buffer));
                }
                ERROR_FILE_NOT_FOUND => return Ok(None),
                ERROR_MORE_DATA => buffer.resize((size as usize).max(buffer.len() * 2), 0),
                _ => bail!(os_error(code)),
            }
        }
        bail!("the registry value keeps growing")
    }

    fn string_in(root: HKEY, key: &str, name: &str) -> Result<Option<String>> {
        let Some(bytes) = get_raw(root, key, name, RRF_RT_REG_SZ)? else { return Ok(None) };
        let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        Ok(Some(String::from_utf16_lossy(&units[..end])))
    }

    fn dword_in(root: HKEY, key: &str, name: &str) -> Result<Option<u32>> {
        let Some(bytes) = get_raw(root, key, name, RRF_RT_REG_DWORD)? else { return Ok(None) };
        let Some(value) = bytes.first_chunk::<4>() else { bail!("the registry value is too short") };
        Ok(Some(u32::from_le_bytes(*value)))
    }

    pub fn get_string(key: &str, name: &str) -> Result<Option<String>> {
        string_in(HKEY_CURRENT_USER, key, name)
    }

    pub fn get_binary(key: &str, name: &str) -> Result<Option<Vec<u8>>> {
        get_raw(HKEY_CURRENT_USER, key, name, RRF_RT_REG_BINARY)
    }

    pub fn get_dword(key: &str, name: &str) -> Result<Option<u32>> {
        dword_in(HKEY_CURRENT_USER, key, name)
    }

    /// A REG_SZ value under HKLM (read only; everyone may read the keys Truffle reads there).
    pub fn machine_string(key: &str, name: &str) -> Result<Option<String>> {
        string_in(HKEY_LOCAL_MACHINE, key, name)
    }

    pub fn machine_dword(key: &str, name: &str) -> Result<Option<u32>> {
        dword_in(HKEY_LOCAL_MACHINE, key, name)
    }
}

#[cfg(windows)]
mod shortcut {
    use std::path::Path;

    use anyhow::{Context, Result, anyhow, bail};
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance, CoInitializeEx,
        CoUninitialize, IPersistFile,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::core::{HSTRING, IUnknown, Interface};

    /// Write a .lnk to `exe` at `link`. Runs on a short-lived thread with its own COM apartment,
    /// so it never depends on (or disturbs) the calling thread's COM state.
    pub fn create(exe: &Path, link: &Path) -> Result<()> {
        let (exe, link) = (exe.to_path_buf(), link.to_path_buf());
        std::thread::Builder::new()
            .name("truffle-shortcut".to_owned())
            .spawn(move || write(&exe, &link))
            .context("couldn't start a helper thread")?
            .join()
            .map_err(|_| anyhow!("the shortcut helper stopped unexpectedly"))?
    }

    struct ComApartment;

    impl Drop for ComApartment {
        fn drop(&mut self) {
            // SAFETY: paired with the successful CoInitializeEx below, on the same thread.
            unsafe { CoUninitialize() };
        }
    }

    fn write(exe: &Path, link: &Path) -> Result<()> {
        // SAFETY: plain COM calls on this thread; every interface is dropped before the
        // apartment guard (declared first, so dropped last).
        unsafe {
            let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
            if hr.is_err() {
                bail!("Windows couldn't start COM ({hr:?})");
            }
            let _apartment = ComApartment;
            let shell_link: IShellLinkW = CoCreateInstance(&ShellLink, None::<&IUnknown>, CLSCTX_INPROC_SERVER)?;
            shell_link.SetPath(&HSTRING::from(exe))?;
            if let Some(dir) = exe.parent() {
                shell_link.SetWorkingDirectory(&HSTRING::from(dir))?;
            }
            shell_link.SetDescription(&HSTRING::from("Truffle: a small, fast Spotify player"))?;
            shell_link.SetIconLocation(&HSTRING::from(exe), 0)?;
            let file: IPersistFile = shell_link.cast()?;
            file.Save(&HSTRING::from(link), true)?;
        }
        Ok(())
    }
}

#[cfg(not(windows))]
pub(super) mod registry {
    use std::ffi::OsStr;

    use anyhow::{Result, bail};

    pub fn set_string(_key: &str, _name: &str, _value: &OsStr) -> Result<()> {
        bail!("only available on Windows")
    }
    pub fn delete_value(_key: &str, _name: &str) -> Result<()> {
        bail!("only available on Windows")
    }
    pub fn get_string(_key: &str, _name: &str) -> Result<Option<String>> {
        Ok(None)
    }
    pub fn get_binary(_key: &str, _name: &str) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
    pub fn get_dword(_key: &str, _name: &str) -> Result<Option<u32>> {
        Ok(None)
    }
    pub fn machine_string(_key: &str, _name: &str) -> Result<Option<String>> {
        Ok(None)
    }
    pub fn machine_dword(_key: &str, _name: &str) -> Result<Option<u32>> {
        Ok(None)
    }
}

#[cfg(not(windows))]
mod shortcut {
    use std::path::Path;

    use anyhow::{Result, bail};

    pub fn create(_exe: &Path, _link: &Path) -> Result<()> {
        bail!("only available on Windows")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_command_quotes_the_exe() {
        let exe = Path::new(r"C:\Program Files\Truffle\truffle.exe");
        assert_eq!(run_command(exe).to_string_lossy(), r#""C:\Program Files\Truffle\truffle.exe" --minimized"#);
    }

    #[test]
    fn run_command_recognises_this_exe_only() {
        let exe = Path::new(r"C:\Program Files\Truffle\truffle.exe");
        assert!(command_targets(&run_command(exe).to_string_lossy(), exe));
        assert!(command_targets(r#""c:/program files/truffle/TRUFFLE.EXE" --minimized"#, exe));
        assert!(command_targets(r"C:\Tools\truffle.exe --minimized", Path::new(r"C:\Tools\truffle.exe")));
        assert!(command_targets(r#""\\?\C:\Tools\truffle.exe""#, Path::new(r"C:\Tools\truffle.exe")));
        assert!(!command_targets(r#""C:\Other\truffle.exe" --minimized"#, exe));
        assert!(!command_targets("", exe));
        assert!(!command_targets(r#""""#, exe));
    }

    /// Read-only: never touches the real Run entry.
    #[cfg(windows)]
    #[test]
    fn registry_reads_existing_and_missing_values() {
        let missing = registry::get_string(RUN_KEY, "truffle-test-value-that-does-not-exist").unwrap();
        assert_eq!(missing, None);
        let missing_key = registry::get_binary(r"Software\truffle-test-key-that-does-not-exist", "x").unwrap();
        assert_eq!(missing_key, None);
        let app_data = registry::get_string(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Shell Folders", "AppData")
            .unwrap()
            .unwrap_or_default();
        assert!(app_data.contains('\\'), "{app_data:?}");
        // Deleting a value that isn't there is not an error.
        registry::delete_value(r"Software\truffle-test-key-that-does-not-exist", "x").unwrap();
        // A number: missing is None, and text read as a number is an error.
        assert_eq!(registry::get_dword(RUN_KEY, "truffle-test-value-that-does-not-exist").unwrap(), None);
        assert!(registry::get_dword(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Shell Folders", "AppData").is_err());
    }

    /// Writes a shortcut into a temp folder (not the Start menu) and removes it again.
    #[cfg(windows)]
    #[test]
    fn shortcut_is_written() {
        let dir = std::env::temp_dir().join(format!("truffle-lnk-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("Truffle test.lnk");
        let exe = std::env::current_exe().unwrap();
        shortcut::create(&exe, &link).unwrap();
        let bytes = std::fs::read(&link).unwrap();
        // Shell link header: size 0x4C, then the ShellLink CLSID.
        assert_eq!(&bytes[..4], &[0x4C, 0, 0, 0]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn startup_approved_flag() {
        assert!(!startup_approved_disabled(&[2, 0, 0, 0]));
        assert!(!startup_approved_disabled(&[6, 0, 0, 0]));
        assert!(startup_approved_disabled(&[3, 0, 0, 0, 1, 2]));
        assert!(startup_approved_disabled(&[7]));
        assert!(!startup_approved_disabled(&[]));
    }
}
