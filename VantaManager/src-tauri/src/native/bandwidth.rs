// An upload limit for every Roblox client together, as a Windows QoS policy.
//
// Windows can throttle a program's outgoing traffic by its exe name. Setting
// that up needs admin rights, so it is done by an elevated PowerShell that
// Windows asks the user to allow each time. The policy goes into the "active
// store", which Windows clears on restart, so nothing is left behind: there is
// no permanent change, and a restart is the same as turning it off.
//
// Reading a policy back needs admin as well, so the app remembers what it set
// and when Windows was started. If Windows has restarted since, the limit is
// gone, and the app says so instead of showing a stale "on".
use super::{now_ms, process};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::process::Command;

const POLICY: &str = "VantaManager";
const EXE: &str = "RobloxPlayerBeta.exe";
const APPLIED_KEY: &str = "bandwidthApplied";
pub const MIN_KBPS: u64 = 64;
pub const MAX_KBPS: u64 = 10_000_000;
/// Windows waits for the user at the UAC prompt, so this is generous.
const ELEVATION_TIMEOUT: Duration = Duration::from_secs(120);

pub fn clamp_kbps(v: f64) -> u64 {
    if !v.is_finite() {
        return MIN_KBPS;
    }
    (v.round().max(0.0) as u64).clamp(MIN_KBPS, MAX_KBPS)
}

/// The script that sets the limit. The only thing put into it is a number.
pub fn apply_script(kbps: u64) -> String {
    let bits = kbps.saturating_mul(1000);
    format!(
        "$ErrorActionPreference = 'Stop'; try {{ \
         Remove-NetQosPolicy -Name '{POLICY}' -PolicyStore ActiveStore -Confirm:$false -ErrorAction SilentlyContinue; \
         New-NetQosPolicy -Name '{POLICY}' -AppPathNameMatchCondition '{EXE}' -ThrottleRateActionBitsPerSecond {bits} -PolicyStore ActiveStore | Out-Null; \
         exit 0 }} catch {{ exit 3 }}"
    )
}

pub fn remove_script() -> String {
    format!(
        "$ErrorActionPreference = 'Stop'; try {{ \
         Remove-NetQosPolicy -Name '{POLICY}' -PolicyStore ActiveStore -Confirm:$false -ErrorAction SilentlyContinue; \
         exit 0 }} catch {{ exit 3 }}"
    )
}

/// PowerShell's -EncodedCommand: the script as UTF-16 little endian, in base64.
/// It travels as one word, so there is nothing to quote or escape.
fn encode(script: &str) -> String {
    use base64::Engine;
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The command that asks Windows for admin rights and runs `script` with them.
fn elevated_command(script: &str) -> String {
    format!(
        "$p = Start-Process -FilePath powershell.exe -Verb RunAs -Wait -PassThru -WindowStyle Hidden \
         -ArgumentList '-NoProfile','-NonInteractive','-EncodedCommand','{}'; exit $p.ExitCode",
        encode(script)
    )
}

/// Windows started this many milliseconds since the epoch (to within a moment).
#[cfg(windows)]
fn boot_ms() -> i64 {
    use windows::Win32::System::SystemInformation::GetTickCount64;
    now_ms() - unsafe { GetTickCount64() } as i64
}
#[cfg(not(windows))]
fn boot_ms() -> i64 {
    0
}

fn remember(kbps: Option<u64>) {
    let _ = crate::settings::update(|s| match kbps {
        Some(k) => {
            s.insert(APPLIED_KEY.into(), json!({ "kbps": k, "boot": boot_ms() }));
        }
        None => {
            s.remove(APPLIED_KEY);
        }
    });
}

/// { active, kbps }: whether the limit this app set is still in force.
pub fn status() -> Value {
    let snap = crate::settings::snapshot();
    let saved = snap.get(APPLIED_KEY);
    let kbps = saved.and_then(|v| v.get("kbps")).and_then(Value::as_u64);
    let boot = saved.and_then(|v| v.get("boot")).and_then(Value::as_i64);
    // The two readings of the start time differ by a few milliseconds at most.
    let same_boot = boot.is_some_and(|b| (b - boot_ms()).abs() < 15_000);
    match kbps {
        Some(k) if same_boot => json!({ "active": true, "kbps": k }),
        Some(k) => json!({ "active": false, "kbps": k, "lostOnRestart": true }),
        None => json!({ "active": false }),
    }
}

async fn run_elevated(script: &str) -> Result<(), String> {
    if !cfg!(windows) {
        return Err("Windows only".into());
    }
    let mut cmd = Command::new(process::system_tool("WindowsPowerShell\\v1.0\\powershell.exe"));
    cmd.args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", &elevated_command(script)]);
    process::no_window(&mut cmd);
    cmd.kill_on_drop(true);
    match tokio::time::timeout(ELEVATION_TIMEOUT, cmd.output()).await {
        Err(_) => Err("Windows didn't answer the admin prompt in time".into()),
        Ok(Err(e)) => Err(format!("could not start PowerShell: {e}")),
        Ok(Ok(out)) if out.status.success() => Ok(()),
        Ok(Ok(out)) => {
            let text = String::from_utf8_lossy(&out.stderr).to_lowercase();
            if text.contains("cancel") {
                Err("Cancelled: the admin prompt was declined".into())
            } else if out.status.code() == Some(3) {
                Err("Windows refused the limit (is the QoS feature available on this PC?)".into())
            } else {
                Err("Could not get admin rights".into())
            }
        }
    }
}

/// Sets the limit (`on`) or takes it away, then reports the state.
pub async fn apply(on: bool, kbps: f64) -> Value {
    let kbps = clamp_kbps(kbps);
    let script = if on { apply_script(kbps) } else { remove_script() };
    match run_elevated(&script).await {
        Ok(()) => {
            remember(on.then_some(kbps));
            let mut s = status();
            s["ok"] = json!(true);
            s["kbps"] = json!(kbps);
            s
        }
        Err(e) => json!({ "ok": false, "error": e }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limit_is_kept_in_range() {
        assert_eq!(clamp_kbps(1000.0), 1000);
        assert_eq!(clamp_kbps(1.0), MIN_KBPS);
        assert_eq!(clamp_kbps(-5.0), MIN_KBPS);
        assert_eq!(clamp_kbps(f64::NAN), MIN_KBPS);
        assert_eq!(clamp_kbps(1e15), MAX_KBPS);
        assert_eq!(clamp_kbps(1000.4), 1000);
    }

    #[test]
    fn the_script_holds_only_a_number_from_the_user() {
        let s = apply_script(1500);
        assert!(s.contains("-ThrottleRateActionBitsPerSecond 1500000 "));
        assert!(s.contains("-PolicyStore ActiveStore"));
        assert!(s.contains("RobloxPlayerBeta.exe"));
        assert!(remove_script().contains("Remove-NetQosPolicy"));
        assert!(!remove_script().contains("New-NetQosPolicy"));
    }

    #[test]
    fn the_encoded_command_is_utf16_base64() {
        use base64::Engine;
        let e = encode("exit 0");
        let raw = base64::engine::general_purpose::STANDARD.decode(e).unwrap();
        assert_eq!(raw.len(), "exit 0".len() * 2);
        assert_eq!(&raw[..4], &[b'e', 0, b'x', 0]);
        let cmd = elevated_command("exit 0");
        assert!(cmd.contains("-Verb RunAs") && cmd.contains("-EncodedCommand"));
        assert!(!cmd.contains("exit 0'"), "the script itself is not in the command line");
    }
}
