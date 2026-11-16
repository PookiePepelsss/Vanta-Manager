// Keeps many Roblox clients from eating the PC: CPU caps for clients you aren't
// looking at, an even share of the CPU between clients, clients spread over the
// cores, and a throttle (or a restart) for a client that has grown too big.
//
// Every two seconds the settings are read and each client this app launched is
// given what the settings ask for, and only what changed is applied:
//
//   CPU cap      a Windows job object per client with a hard CPU rate. Nothing is
//                injected into Roblox and it is never suspended, it just gets
//                less time on the CPU. 100 means no cap.
//   Efficiency   the process's power throttling flag (EcoQoS), which moves it to
//                the efficient cores on hybrid CPUs and lowers its clock.
//   Cores        the process affinity mask.
//
// Turning a setting off puts the client back as it was, and so does closing the
// app. If the app is killed outright these stay on the clients until they close.
use super::{emit_log, memory, process};
use crate::state::AppState;
use once_cell::sync::Lazy;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};

const TICK: Duration = Duration::from_secs(2);
/// A client has to stay over the restart size for this many ticks in a row, so a
/// short spike while a game loads can't restart it.
const RESTART_TICKS: u32 = 3;
/// One restart per account in this long. A game that is over the size the moment
/// it loads would otherwise be restarted forever.
const RESTART_COOLDOWN: Duration = Duration::from_secs(10 * 60);
/// A throttled client is let go once it is this far under the size (percent), so
/// one sitting right at the size doesn't flip back and forth.
const RELEASE_AT_PERCENT: u64 = 90;
/// Lowest share a fair cap gives a client.
const FAIR_FLOOR: u32 = 5;

const BG_CPU_CHOICES: [u32; 5] = [0, 50, 25, 10, 5];
const BG_TRIM_CHOICES: [u32; 5] = [0, 5, 15, 30, 60];
const THROTTLE_CPU_CHOICES: [u32; 4] = [25, 10, 5, 2];

/// What the settings ask for.
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    pub adaptive: bool,
    /// Cap for clients that aren't in front, in percent of the whole CPU. 0 = none.
    pub bg_cpu: u32,
    pub bg_eco: bool,
    /// Minutes between memory trims of clients that aren't in front. 0 = never.
    pub bg_trim_min: u32,
    pub fair: bool,
    pub spread: bool,
    pub throttle: bool,
    pub throttle_mb: u64,
    pub throttle_cpu: u32,
    pub restart: bool,
    pub restart_mb: u64,
}

fn pick(v: Option<i64>, allowed: &[u32], default: u32) -> u32 {
    v.and_then(|n| u32::try_from(n).ok())
        .filter(|n| allowed.contains(n))
        .unwrap_or(default)
}

fn mb_setting(key: &str, default: u64) -> u64 {
    crate::settings::get_i64(key)
        .and_then(|n| u64::try_from(n).ok())
        .filter(|n| (512..=1_048_576).contains(n))
        .unwrap_or(default)
}

impl Policy {
    pub fn from_settings() -> Self {
        let get = |k| crate::settings::get_i64(k);
        Policy {
            adaptive: crate::settings::get_bool("adaptiveLimits", false),
            bg_cpu: pick(get("bgCpuLimit"), &BG_CPU_CHOICES, 25),
            bg_eco: crate::settings::get_bool("bgEfficiency", true),
            bg_trim_min: pick(get("bgTrimMin"), &BG_TRIM_CHOICES, 0),
            fair: crate::settings::get_bool("fairCpuCap", false),
            spread: crate::settings::get_bool("spreadCores", false),
            throttle: crate::settings::get_bool("throttleHighMem", false),
            throttle_mb: mb_setting("throttleMemMb", 4096),
            throttle_cpu: pick(get("throttleCpu"), &THROTTLE_CPU_CHOICES, 5),
            restart: crate::settings::get_bool("restartHighMem", false),
            restart_mb: mb_setting("restartMemMb", 6144),
        }
    }

    fn idle(&self) -> bool {
        !(self.adaptive || self.fair || self.spread || self.throttle || self.restart)
    }
}

/// One running client, as the policy sees it.
#[derive(Debug, Clone, Copy)]
pub struct Client {
    pub pid: u32,
    pub front: bool,
    pub ws_mb: u64,
    /// Still loading its first assets: never throttled or restarted yet.
    pub loading: bool,
}

/// What one client should have right now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Want {
    /// Percent of the whole CPU, 100 = no cap.
    pub cap: u32,
    pub eco: bool,
    /// Cores it may run on, None = all of them.
    pub mask: Option<u64>,
}

impl Want {
    pub const NONE: Want = Want { cap: 100, eco: false, mask: None };
}

/// The cores for client `i` of `n`: its own block of cores when there are enough,
/// else one core each, taken in turn. One client alone gets every core.
fn core_mask(i: usize, n: usize, cores: u32) -> Option<u64> {
    let c = cores.clamp(1, 64) as usize;
    if n <= 1 || c <= 1 {
        return None;
    }
    let (start, end) = if n <= c {
        (i * c / n, ((i + 1) * c / n).max(i * c / n + 1))
    } else {
        (i % c, i % c + 1)
    };
    let len = end - start;
    Some(if len >= 64 { u64::MAX } else { ((1u64 << len) - 1) << start })
}

/// The pure part: what each client should have. `throttled` carries which
/// clients are being throttled between ticks (it is what gives the release point
/// its margin).
pub fn decide(p: &Policy, clients: &[Client], throttled: &mut HashSet<u32>, cores: u32) -> Vec<(u32, Want)> {
    let mut order: Vec<&Client> = clients.iter().collect();
    order.sort_by_key(|c| c.pid);
    let n = order.len();
    throttled.retain(|pid| order.iter().any(|c| c.pid == *pid));
    let mut out = Vec::with_capacity(n);
    for (i, c) in order.iter().enumerate() {
        let mut cap = 100u32;
        if p.adaptive && !c.front && p.bg_cpu > 0 {
            cap = cap.min(p.bg_cpu);
        }
        if p.fair && n > 1 {
            cap = cap.min((100 / n as u32).max(FAIR_FLOOR));
        }
        if p.throttle && !c.loading {
            if c.ws_mb > p.throttle_mb {
                throttled.insert(c.pid);
            } else if c.ws_mb * 100 < p.throttle_mb * RELEASE_AT_PERCENT {
                throttled.remove(&c.pid);
            }
        } else {
            throttled.remove(&c.pid);
        }
        if throttled.contains(&c.pid) {
            cap = cap.min(p.throttle_cpu);
        }
        out.push((
            c.pid,
            Want {
                cap,
                eco: p.adaptive && p.bg_eco && !c.front,
                mask: if p.spread { core_mask(i, n, cores) } else { None },
            },
        ));
    }
    out
}

// ---- Windows ----

#[cfg(windows)]
mod sys {
    use std::collections::HashMap;
    use std::ffi::c_void;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectCpuRateControlInformation,
        SetInformationJobObject, JOBOBJECT_CPU_RATE_CONTROL_INFORMATION,
        JOB_OBJECT_CPU_RATE_CONTROL_ENABLE, JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
    };
    use windows::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
    use windows::Win32::System::Threading::{
        GetProcessAffinityMask, OpenProcess, ProcessPowerThrottling, SetProcessAffinityMask,
        SetProcessInformation, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION, PROCESS_SET_QUOTA,
        PROCESS_TERMINATE,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

    pub fn foreground_pid() -> Option<u32> {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.0.is_null() {
                return None;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            (pid != 0).then_some(pid)
        }
    }

    pub fn core_count() -> u32 {
        unsafe {
            let mut info = SYSTEM_INFO::default();
            GetSystemInfo(&mut info);
            info.dwNumberOfProcessors.clamp(1, 64)
        }
    }

    /// Caps the process at `percent` of the whole CPU (100 lifts the cap). One job
    /// object per process, kept open in `jobs` so the cap can change later.
    pub fn set_cap(jobs: &mut HashMap<u32, isize>, pid: u32, percent: u32) -> Result<(), String> {
        unsafe {
            let job = match jobs.get(&pid) {
                Some(h) => HANDLE(*h as *mut c_void),
                None => {
                    if percent >= 100 {
                        return Ok(());
                    }
                    let job = CreateJobObjectW(None, PCWSTR::null())
                        .map_err(|e| format!("could not create a job object: {}", e.message()))?;
                    let process = match OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid) {
                        Ok(h) => h,
                        Err(e) => {
                            let _ = CloseHandle(job);
                            return Err(format!("could not open the process: {}", e.message()));
                        }
                    };
                    let assigned = AssignProcessToJobObject(job, process);
                    let _ = CloseHandle(process);
                    if let Err(e) = assigned {
                        let _ = CloseHandle(job);
                        return Err(format!("could not limit the process: {}", e.message()));
                    }
                    jobs.insert(pid, job.0 as isize);
                    job
                }
            };
            let mut info: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION = std::mem::zeroed();
            info.ControlFlags = JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
            info.Anonymous.CpuRate = percent.clamp(1, 100) * 100; // hundredths of a percent
            SetInformationJobObject(
                job,
                JobObjectCpuRateControlInformation,
                (&info as *const JOBOBJECT_CPU_RATE_CONTROL_INFORMATION).cast::<c_void>(),
                std::mem::size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
            )
            .map_err(|e| format!("Windows refused the CPU cap: {}", e.message()))
        }
    }

    pub fn forget(jobs: &mut HashMap<u32, isize>, pid: u32) {
        if let Some(h) = jobs.remove(&pid) {
            unsafe {
                let _ = CloseHandle(HANDLE(h as *mut c_void));
            }
        }
    }

    pub fn set_eco(pid: u32, on: bool) -> Result<(), String> {
        unsafe {
            let h = OpenProcess(PROCESS_SET_INFORMATION, false, pid)
                .map_err(|e| format!("could not open the process: {}", e.message()))?;
            // ControlMask 0 hands the choice back to Windows.
            let state = PROCESS_POWER_THROTTLING_STATE {
                Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
                ControlMask: if on { PROCESS_POWER_THROTTLING_EXECUTION_SPEED } else { 0 },
                StateMask: if on { PROCESS_POWER_THROTTLING_EXECUTION_SPEED } else { 0 },
            };
            let r = SetProcessInformation(
                h,
                ProcessPowerThrottling,
                (&state as *const PROCESS_POWER_THROTTLING_STATE).cast::<c_void>(),
                std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            );
            let _ = CloseHandle(h);
            r.map_err(|e| format!("Windows refused efficiency mode: {}", e.message()))
        }
    }

    pub fn set_affinity(pid: u32, mask: Option<u64>) -> Result<(), String> {
        unsafe {
            let h = OpenProcess(PROCESS_SET_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
                .map_err(|e| format!("could not open the process: {}", e.message()))?;
            let (mut process, mut system) = (0usize, 0usize);
            let r = GetProcessAffinityMask(h, &mut process, &mut system)
                .map_err(|e| format!("could not read the cores: {}", e.message()))
                .and_then(|_| {
                    let mut target = match mask {
                        Some(m) => (m as usize) & system,
                        None => system,
                    };
                    if target == 0 {
                        target = system;
                    }
                    if target == process {
                        return Ok(());
                    }
                    SetProcessAffinityMask(h, target)
                        .map_err(|e| format!("Windows refused the core choice: {}", e.message()))
                });
            let _ = CloseHandle(h);
            r
        }
    }
}

#[cfg(not(windows))]
mod sys {
    use std::collections::HashMap;
    pub fn foreground_pid() -> Option<u32> {
        None
    }
    pub fn core_count() -> u32 {
        1
    }
    pub fn set_cap(_: &mut HashMap<u32, isize>, _: u32, _: u32) -> Result<(), String> {
        Err("Windows only".into())
    }
    pub fn forget(_: &mut HashMap<u32, isize>, _: u32) {}
    pub fn set_eco(_: u32, _: bool) -> Result<(), String> {
        Err("Windows only".into())
    }
    pub fn set_affinity(_: u32, _: Option<u64>) -> Result<(), String> {
        Err("Windows only".into())
    }
}

// ---- the loop ----

#[derive(Default)]
struct Gov {
    /// What each client has now (only clients that differ from the default).
    applied: HashMap<u32, Want>,
    jobs: HashMap<u32, isize>,
    throttled: HashSet<u32>,
    /// Ticks in a row over the restart size, by account.
    over: HashMap<String, u32>,
    last_restart: HashMap<String, Instant>,
    last_trim: HashMap<u32, Instant>,
    /// Clients Windows already refused something for, so it is logged once.
    refused: HashSet<u32>,
}

static GOV: Lazy<Mutex<Gov>> = Lazy::new(|| Mutex::new(Gov::default()));

fn gov() -> std::sync::MutexGuard<'static, Gov> {
    GOV.lock().unwrap_or_else(|e| e.into_inner())
}

/// Puts one client from `from` to `to`, step by step; a step Windows refuses is
/// logged once and doesn't hold up the rest.
fn apply(g: &mut Gov, app: &AppHandle, account: &str, pid: u32, from: Want, to: Want) {
    let fail = |what: &str, e: String, g: &mut Gov| {
        if g.refused.insert(pid) {
            emit_log(app, "warn", "system", &format!("{what} not applied: {e}"), Some(json!({ "accountId": account, "pid": pid })));
        }
    };
    if to.cap != from.cap {
        if let Err(e) = sys::set_cap(&mut g.jobs, pid, to.cap) {
            fail("CPU limit", e, g);
        }
    }
    if to.eco != from.eco {
        if let Err(e) = sys::set_eco(pid, to.eco) {
            fail("Efficiency mode", e, g);
        }
    }
    if to.mask != from.mask {
        if let Err(e) = sys::set_affinity(pid, to.mask) {
            fail("Core spreading", e, g);
        }
    }
}

/// One pass. Returns the accounts to restart for being over the size.
fn step(app: &AppHandle, state: &AppState) -> Vec<(String, u64)> {
    let policy = Policy::from_settings();
    let running: Vec<(String, u32)> = state
        .account_pids
        .lock()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    let mut g = gov();

    // Closed clients: let go of their jobs and bookkeeping.
    let live: HashSet<u32> = running.iter().map(|(_, p)| *p).collect();
    let gone: Vec<u32> = g.jobs.keys().chain(g.applied.keys()).copied().filter(|p| !live.contains(p)).collect();
    for pid in gone {
        sys::forget(&mut g.jobs, pid);
        g.applied.remove(&pid);
        g.refused.remove(&pid);
    }
    g.last_trim.retain(|p, _| live.contains(p));
    let ids: HashSet<&str> = running.iter().map(|(id, _)| id.as_str()).collect();
    g.over.retain(|id, _| ids.contains(id.as_str()));

    if running.is_empty() || (policy.idle() && g.applied.is_empty()) {
        return Vec::new();
    }

    let front = sys::foreground_pid();
    let clients: Vec<Client> = running
        .iter()
        .map(|(_, pid)| Client {
            pid: *pid,
            front: front == Some(*pid),
            ws_mb: memory::working_set_mb(*pid).unwrap_or(0),
            loading: memory::is_loading(*pid),
        })
        .collect();

    let mut throttled = std::mem::take(&mut g.throttled);
    let wants = decide(&policy, &clients, &mut throttled, sys::core_count());
    g.throttled = throttled;

    let id_of = |pid: u32| running.iter().find(|(_, p)| *p == pid).map(|(id, _)| id.clone()).unwrap_or_default();
    for (pid, want) in wants {
        let from = g.applied.get(&pid).copied().unwrap_or(Want::NONE);
        if from != want {
            apply(&mut g, app, &id_of(pid), pid, from, want);
            if want == Want::NONE {
                g.applied.remove(&pid);
            } else {
                g.applied.insert(pid, want);
            }
        }
    }

    // Trimming the clients that aren't in front, on their own timer.
    if policy.adaptive && policy.bg_trim_min > 0 {
        let every = Duration::from_secs(u64::from(policy.bg_trim_min) * 60);
        for c in clients.iter().filter(|c| !c.front && !c.loading) {
            let due = g.last_trim.get(&c.pid).is_none_or(|t| t.elapsed() >= every);
            if due {
                // The first time only starts the clock: nothing is trimmed the
                // moment the setting is turned on.
                if g.last_trim.contains_key(&c.pid) {
                    process::trim_process_memory(c.pid);
                }
                g.last_trim.insert(c.pid, Instant::now());
            }
        }
    }

    // Clients that stayed over the restart size.
    let mut restart = Vec::new();
    if policy.restart {
        for (id, pid) in &running {
            let Some(c) = clients.iter().find(|c| c.pid == *pid) else { continue };
            if c.loading || c.ws_mb <= policy.restart_mb {
                g.over.remove(id);
                continue;
            }
            let n = g.over.entry(id.clone()).or_insert(0);
            *n += 1;
            if *n >= RESTART_TICKS && g.last_restart.get(id).is_none_or(|t| t.elapsed() >= RESTART_COOLDOWN) {
                g.over.remove(id);
                g.last_restart.insert(id.clone(), Instant::now());
                restart.push((id.clone(), c.ws_mb));
            }
        }
    } else {
        g.over.clear();
    }
    restart
}

/// Closes the client and starts the account again the way a relaunch does.
async fn restart_account(app: AppHandle, id: String, ws_mb: u64) {
    let state = app.state::<AppState>();
    let info = crate::storage::account_info(&state, &id).unwrap_or_default();
    let name = info.display(&id).to_string();
    if info.cookie.is_empty() {
        return;
    }
    emit_log(
        &app,
        "warn",
        "system",
        &format!("{name} is using {ws_mb} MB, restarting it"),
        Some(json!({ "accountId": id, "workingSetMb": ws_mb })),
    );
    process::kill_account_roblox(&app, &state, &id).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    super::launch::relaunch_with_retry(&app, &state, &id, &name, &info.cookie, &info.game_target).await;
}

pub fn start(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(TICK).await;
            let state = app.state::<AppState>();
            for (id, ws) in step(&app, &state) {
                tauri::async_runtime::spawn(restart_account(app.clone(), id, ws));
            }
        }
    });
}

/// Takes every cap, efficiency flag and core choice off, for when the app closes.
pub fn release_all() {
    let mut g = gov();
    let pids: Vec<u32> = g.applied.keys().copied().collect();
    for pid in pids {
        let from = g.applied.remove(&pid).unwrap_or(Want::NONE);
        if from.cap != 100 {
            let _ = sys::set_cap(&mut g.jobs, pid, 100);
        }
        if from.eco {
            let _ = sys::set_eco(pid, false);
        }
        if from.mask.is_some() {
            let _ = sys::set_affinity(pid, None);
        }
    }
    let all: Vec<u32> = g.jobs.keys().copied().collect();
    for pid in all {
        sys::forget(&mut g.jobs, pid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        Policy {
            adaptive: false,
            bg_cpu: 25,
            bg_eco: true,
            bg_trim_min: 0,
            fair: false,
            spread: false,
            throttle: false,
            throttle_mb: 4096,
            throttle_cpu: 5,
            restart: false,
            restart_mb: 6144,
        }
    }

    fn client(pid: u32, front: bool, ws_mb: u64) -> Client {
        Client { pid, front, ws_mb, loading: false }
    }

    fn want_of(w: &[(u32, Want)], pid: u32) -> Want {
        w.iter().find(|(p, _)| *p == pid).unwrap().1
    }

    #[test]
    fn nothing_is_asked_when_every_setting_is_off() {
        let mut t = HashSet::new();
        let w = decide(&policy(), &[client(1, false, 9000), client(2, true, 100)], &mut t, 8);
        assert!(w.iter().all(|(_, w)| *w == Want::NONE));
    }

    #[test]
    fn only_clients_that_arent_in_front_are_limited() {
        let p = Policy { adaptive: true, bg_cpu: 10, ..policy() };
        let mut t = HashSet::new();
        let w = decide(&p, &[client(1, true, 100), client(2, false, 100)], &mut t, 8);
        assert_eq!(want_of(&w, 1), Want::NONE);
        assert_eq!(want_of(&w, 2), Want { cap: 10, eco: true, mask: None });
        // "Off" for the CPU limit still leaves efficiency mode.
        let p = Policy { adaptive: true, bg_cpu: 0, bg_eco: false, ..policy() };
        assert_eq!(want_of(&decide(&p, &[client(2, false, 1)], &mut t, 8), 2), Want::NONE);
    }

    #[test]
    fn the_fair_cap_shrinks_as_more_clients_run() {
        let p = Policy { fair: true, ..policy() };
        let mut t = HashSet::new();
        let mut cap = |n: u32| {
            let cs: Vec<Client> = (1..=n).map(|i| client(i, false, 1)).collect();
            want_of(&decide(&p, &cs, &mut t, 8), 1).cap
        };
        assert_eq!((cap(1), cap(2), cap(4), cap(10)), (100, 50, 25, 10));
        assert_eq!(cap(50), FAIR_FLOOR, "never below the floor");
    }

    #[test]
    fn the_tightest_cap_wins() {
        let p = Policy { adaptive: true, bg_cpu: 25, fair: true, ..policy() };
        let mut t = HashSet::new();
        let cs: Vec<Client> = (1..=8).map(|i| client(i, false, 1)).collect();
        assert_eq!(want_of(&decide(&p, &cs, &mut t, 8), 1).cap, 12, "100 / 8 beats 25");
    }

    #[test]
    fn a_big_client_is_throttled_and_let_go_below_the_margin() {
        let p = Policy { throttle: true, ..policy() };
        let mut t = HashSet::new();
        assert_eq!(want_of(&decide(&p, &[client(1, true, 4000)], &mut t, 8), 1).cap, 100);
        assert_eq!(want_of(&decide(&p, &[client(1, true, 4200)], &mut t, 8), 1).cap, 5);
        // Just under the size is still throttled; it takes 90% of it to let go.
        assert_eq!(want_of(&decide(&p, &[client(1, true, 4000)], &mut t, 8), 1).cap, 5);
        assert_eq!(want_of(&decide(&p, &[client(1, true, 3700)], &mut t, 8), 1).cap, 5, "90% of 4096 is 3686");
        assert_eq!(want_of(&decide(&p, &[client(1, true, 3600)], &mut t, 8), 1).cap, 100);
        // A client still loading is left alone, and a closed one is forgotten.
        let loading = Client { loading: true, ..client(2, true, 9000) };
        assert_eq!(want_of(&decide(&p, &[loading], &mut t, 8), 2).cap, 100);
        decide(&p, &[client(1, true, 9000)], &mut t, 8);
        decide(&p, &[], &mut t, 8);
        assert!(t.is_empty());
    }

    #[test]
    fn cores_are_shared_out_without_overlap() {
        // 2 clients on 8 cores: a half each.
        assert_eq!(core_mask(0, 2, 8), Some(0b0000_1111));
        assert_eq!(core_mask(1, 2, 8), Some(0b1111_0000));
        // 3 on 8: 2, 3 and 3 cores, every core used once.
        let masks: Vec<u64> = (0..3).map(|i| core_mask(i, 3, 8).unwrap()).collect();
        assert_eq!(masks.iter().fold(0, |a, m| a | m), 0xFF);
        assert_eq!(masks.iter().map(|m| m.count_ones()).sum::<u32>(), 8);
        // More clients than cores: one core each, in turn.
        assert_eq!(core_mask(0, 5, 4), Some(0b0001));
        assert_eq!(core_mask(4, 5, 4), Some(0b0001));
        assert_eq!(core_mask(3, 5, 4), Some(0b1000));
        // A lone client keeps every core, and 64 cores don't overflow the shift.
        assert_eq!(core_mask(0, 1, 8), None);
        assert_eq!(core_mask(0, 1, 64), None);
        assert_eq!(core_mask(0, 2, 64), Some(0x0000_0000_FFFF_FFFF));
        assert_eq!(core_mask(1, 2, 64), Some(0xFFFF_FFFF_0000_0000));
    }

    #[test]
    fn settings_outside_the_choices_fall_back() {
        assert_eq!(pick(Some(10), &BG_CPU_CHOICES, 25), 10);
        assert_eq!(pick(Some(33), &BG_CPU_CHOICES, 25), 25);
        assert_eq!(pick(Some(-1), &BG_CPU_CHOICES, 25), 25);
        assert_eq!(pick(None, &THROTTLE_CPU_CHOICES, 5), 5);
    }

    /// A real busy process, capped for a few seconds: it should get far less than
    /// it would uncapped. Run with `cargo test -- --ignored governor`.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn a_real_process_really_is_capped() {
        use std::process::{Command, Stdio};
        let mut child = Command::new("powershell")
            .args(["-NoProfile", "-Command", "while($true){}"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("powershell");
        let pid = child.id();
        std::thread::sleep(Duration::from_millis(800));

        let cores = sys::core_count();
        // About a quarter of one core, whatever the machine has.
        let percent = (25 / cores).max(1);
        let mut jobs = HashMap::new();
        sys::set_cap(&mut jobs, pid, percent).expect("cap");
        sys::set_eco(pid, true).expect("efficiency mode");
        sys::set_affinity(pid, Some(1)).expect("one core");

        let cpu_ms = |pid: u32| -> u64 { process_cpu_ms(pid) };
        let before = cpu_ms(pid);
        let t0 = Instant::now();
        std::thread::sleep(Duration::from_secs(5));
        let used = cpu_ms(pid) - before;
        let wall = t0.elapsed().as_millis() as u64;
        eprintln!("cap {percent}% of {cores} cores: used {used} ms of CPU in {wall} ms");

        sys::set_cap(&mut jobs, pid, 100).expect("lift");
        sys::set_eco(pid, false).expect("lift efficiency mode");
        sys::set_affinity(pid, None).expect("all cores");
        let before = cpu_ms(pid);
        let t1 = Instant::now();
        std::thread::sleep(Duration::from_secs(2));
        let free = cpu_ms(pid) - before;
        let wall2 = t1.elapsed().as_millis() as u64;
        eprintln!("lifted: used {free} ms of CPU in {wall2} ms");
        sys::forget(&mut jobs, pid);
        let _ = child.kill();

        assert!(used * 100 < wall * 50, "capped process used {used} ms in {wall} ms");
        assert!(free * 100 > wall2 * 70, "lifted process only used {free} ms in {wall2} ms");
    }

    #[cfg(windows)]
    fn process_cpu_ms(pid: u32) -> u64 {
        use windows::Win32::Foundation::{CloseHandle, FILETIME};
        use windows::Win32::System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).expect("open");
            let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
            GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u).expect("times");
            let _ = CloseHandle(h);
            let t = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
            (t(k) + t(u)) / 10_000
        }
    }
}
