//! Secure-desktop UAC policy toggle.
//!
//! Windows renders UAC elevation prompts on the **secure desktop**
//! (`WinSta0\Winlogon`) by default. WGC — Nova's primary capture backend — cannot
//! see that desktop, so during an elevation prompt a remote operator sees a black
//! screen at the exact moment they need to approve it. The full architectural fix
//! is the DDA secure-desktop backend (see [`crate::capture`]); this module is the
//! *complementary, opt-in* knob.
//!
//! Setting `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System\`
//! `PromptOnSecureDesktop = 0` makes UAC prompts appear on the **normal**
//! interactive desktop instead. WGC then captures them directly — no backend
//! swap, no DDA, no secure-desktop plumbing needed for the common "click Yes on a
//! UAC dialog" remote-admin flow.
//!
//! ## Security note (be honest with the user about this)
//!
//! The secure desktop exists to stop malware from spoofing or auto-clicking UAC
//! prompts. Disabling it is a deliberate, documented trade-off that many remote
//! administrators choose anyway (RDP, AnyDesk, and TeamViewer all recommend or
//! require it for unattended elevation). It is therefore **opt-in** in Nova — the
//! installer leaves it untouched unless the user checks the box — and fully
//! reversible: [`set_prompt_on_secure_desktop(true)`] restores the default, and
//! the installer restores it on uninstall.
//!
//! The installer sets this declaratively (Inno `[Registry]` + `[Tasks]`, reverted
//! on uninstall). These runtime helpers exist for a future tray toggle and for
//! diagnostics — Nova can report and flip the state without a reinstall.

use windows::core::{HRESULT, PCWSTR};
use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_VALUE_TYPE,
};

/// Registry key holding the machine UAC policy values.
const POLICY_KEY: &str = "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Policies\\System";
/// DWORD value: 1 = prompt on the secure desktop (Windows default), 0 = prompt
/// on the normal interactive desktop (WGC-capturable).
const VALUE_NAME: &str = "PromptOnSecureDesktop";

fn wide_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Reads the current policy.
///
/// Returns `Some(true)` when UAC prompts are shown on the secure desktop (the
/// Windows default, including when the value is absent — that is what Windows
/// treats as the default), `Some(false)` when they are shown on the normal
/// desktop, or `None` only if the policy key itself cannot be opened.
pub fn is_prompt_on_secure_desktop() -> Option<bool> {
    unsafe {
        let key = wide_nul(POLICY_KEY);
        let mut hkey = HKEY(std::ptr::null_mut());
        if RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(key.as_ptr()),
            0,
            KEY_READ,
            &mut hkey,
        )
        .0 != 0
        {
            return None;
        }

        let value = wide_nul(VALUE_NAME);
        let mut kind = REG_VALUE_TYPE(0);
        let mut data = 0u32;
        let mut size = std::mem::size_of::<u32>() as u32;
        let status = RegQueryValueExW(
            hkey,
            PCWSTR(value.as_ptr()),
            None,
            Some(&mut kind),
            Some(&mut data as *mut u32 as *mut u8),
            Some(&mut size),
        );
        let _ = RegCloseKey(hkey);

        if status == ERROR_FILE_NOT_FOUND {
            // Value absent ⇒ Windows behaves as if it were 1 (secure desktop on).
            return Some(true);
        }
        if status.0 != 0 {
            return None;
        }
        Some(data != 0)
    }
}

/// Sets the policy. `true` restores the Windows default (secure desktop on);
/// `false` moves UAC prompts to the normal desktop so WGC can capture them.
///
/// Tries a native `RegSetValueExW` first (succeeds when Nova already holds an
/// elevated token — which it does under the `NovaServerBoot` task) and falls back
/// to an elevated `reg.exe add` (single UAC prompt) on `ERROR_ACCESS_DENIED`,
/// mirroring the dual-layer pattern used for the VDD registry writes.
pub fn set_prompt_on_secure_desktop(enabled: bool) -> Result<(), String> {
    let data: u32 = if enabled { 1 } else { 0 };
    match set_native(data) {
        Ok(()) => {
            println!(
                "🔐 PromptOnSecureDesktop set to {data} ({})",
                if enabled {
                    "secure desktop — Windows default"
                } else {
                    "normal desktop — WGC-capturable UAC prompts"
                }
            );
            Ok(())
        }
        Err(e) if e == HRESULT::from_win32(ERROR_ACCESS_DENIED.0) => {
            println!("🔐 PromptOnSecureDesktop write needs elevation — falling back to reg.exe");
            set_via_reg_exe(data)
        }
        Err(e) => Err(format!("failed to write PromptOnSecureDesktop: {e:?}")),
    }
}

fn set_native(data: u32) -> Result<(), HRESULT> {
    unsafe {
        let key = wide_nul(POLICY_KEY);
        let mut hkey = HKEY(std::ptr::null_mut());
        let status = RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(key.as_ptr()),
            0,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut hkey,
            None,
        );
        if status.0 != 0 {
            return Err(HRESULT::from_win32(status.0));
        }

        let value = wide_nul(VALUE_NAME);
        let bytes = data.to_le_bytes();
        let status = RegSetValueExW(hkey, PCWSTR(value.as_ptr()), 0, REG_DWORD, Some(&bytes));
        let _ = RegCloseKey(hkey);

        if status.0 != 0 {
            return Err(HRESULT::from_win32(status.0));
        }
        Ok(())
    }
}

fn set_via_reg_exe(data: u32) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    // Single elevated reg.exe via Start-Process -Verb RunAs (one UAC prompt),
    // matching virtual_display's elevated-write idiom.
    let key_arg = format!("HKLM\\{POLICY_KEY}");
    let data_arg = format!("{data}");
    let ps = format!(
        "$p = Start-Process -FilePath 'reg.exe' -ArgumentList \
         'add','{key}','/v','{val}','/t','REG_DWORD','/d','{data}','/f' \
         -Verb RunAs -Wait -PassThru; exit $p.ExitCode",
        key = key_arg,
        val = VALUE_NAME,
        data = data_arg,
    );

    let status = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|e| format!("failed to launch elevated reg.exe: {e}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "elevated reg.exe exited with code {:?}",
            status.code()
        ))
    }
}
