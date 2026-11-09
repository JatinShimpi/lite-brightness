#![windows_subsystem = "windows"]

use std::ffi::c_void;
use std::os::windows::process::CommandExt;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use windows::core::*;
use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::*;
use windows::Win32::System::Shutdown::LockWorkStation;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::System::Wmi::*;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Input::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const WM_TRAY: u32 = WM_APP + 1;
const WM_DIAL: u32 = WM_APP + 2;
const TIMER_APPLY: usize = 1;
const TIMER_OSD: usize = 2;
const TIMER_TRAY: usize = 3;
const STEP: i32 = 5;
const VCP_POWER: u8 = 0xD6;
const DISPLAYSWITCH: &str = "C:\\Windows\\System32\\DisplaySwitch.exe";

const WM_BIND: u32 = WM_APP + 3;

// ---------- rebindable actions (like a game's controls screen) ----------

// a key plus held modifiers; vk 0 = not bound
#[derive(Clone, Copy, PartialEq, Eq)]
struct Key {
    vk: u16,
    mods: u8,
}

// same bit values as RegisterHotKey's modifiers, so they can be passed straight through
const MOD_ALT: u8 = 1;
const MOD_CTRL: u8 = 2;
const MOD_SHIFT: u8 = 4;
const MOD_WIN: u8 = 8;

const NONE_KEY: Key = Key { vk: 0, mods: 0 };
const fn bare(vk: u16) -> Key {
    Key { vk, mods: 0 }
}

const A_UP: usize = 0;
const A_DOWN: usize = 1;
const A_CYCLE: usize = 2;
const A_LOCK: usize = 3;
const A_OFF: usize = 4;
const A_LOCK_OFF: usize = 5;
const N_ACTIONS: usize = 6;
const SLOTS: usize = 2;

// (settings-file id, label shown in the key bindings window)
const ACTIONS: [(&str, &str); N_ACTIONS] = [
    ("up", "Brightness up"),
    ("down", "Brightness down"),
    ("cycle", "Switch display"),
    ("lock", "Lock PC"),
    ("off", "Turn screens off"),
    ("lockoff", "Lock PC + screens off"),
];

// the dial sends Volume Up / Down when turned and Mute when pressed
const DEFAULT_BINDS: [[Key; SLOTS]; N_ACTIONS] = [
    [bare(0xAF), NONE_KEY],
    [bare(0xAE), NONE_KEY],
    [bare(0xAD), NONE_KEY],
    [NONE_KEY, NONE_KEY],
    [NONE_KEY, NONE_KEY],
    [NONE_KEY, NONE_KEY],
];

struct Settings {
    dial: bool,     // brightness up/down keys are active
    ext_only: bool, // media keys (the dial) only count from the external keyboard
    cycle_all: bool, // "All displays" is one of the stops when switching the dial target
    binds: [[Key; SLOTS]; N_ACTIONS],
}

struct Mon {
    handle: isize, // physical monitor handle (0 for the laptop panel)
    name: String,
    internal: bool,
    min: u32,
    max: u32,
    pct: i32,
    applied: i32,
    ok: bool,
    off: bool,
}

struct State {
    s: Settings,
    mons: Vec<Mon>,
    target: i32, // -1 = all displays
}

static STATE: Mutex<State> = Mutex::new(State {
    s: Settings { dial: true, ext_only: true, cycle_all: true, binds: DEFAULT_BINDS },
    mons: Vec::new(),
    target: 0,
});
static OSD: Mutex<(String, i32)> = Mutex::new((String::new(), -1));
static TX: Mutex<Option<Sender<Job>>> = Mutex::new(None);
static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);
static OSD_HWND: AtomicIsize = AtomicIsize::new(0);
static FLY_HWND: AtomicIsize = AtomicIsize::new(0);
static ICON: AtomicIsize = AtomicIsize::new(0);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static APPLY_PENDING: AtomicBool = AtomicBool::new(false);
static DRAG: AtomicI32 = AtomicI32::new(-1);
static DPI: AtomicI32 = AtomicI32::new(96);
static FLY_HIDDEN_AT: Mutex<Option<Instant>> = Mutex::new(None);
static KEYS_HWND: AtomicIsize = AtomicIsize::new(0);
// slot waiting for a key press in the bindings window: action * SLOTS + slot, or -1
static CAPTURE: AtomicI32 = AtomicI32::new(-1);
// key whose key-down we swallowed, so its key-up is swallowed too
static SWALLOWED: AtomicU32 = AtomicU32::new(0);
static KEYS_NOTE: Mutex<String> = Mutex::new(String::new());
// a key that clashes with something, waiting for a second press to confirm: (capture slot, key)
static PENDING: Mutex<Option<(i32, Key)>> = Mutex::new(None);

// the external keyboard is identified by this text in its device name (VID/PID), see settings.txt
static DEVICE_MATCH: Mutex<String> = Mutex::new(String::new());
static DEVICE_CACHE: Mutex<Vec<(isize, bool)>> = Mutex::new(Vec::new());
static EXT_AT_MS: AtomicU64 = AtomicU64::new(0);
static START: OnceLock<Instant> = OnceLock::new();
const EXT_WINDOW_MS: u64 = 150;

fn now_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

static HOOK: AtomicIsize = AtomicIsize::new(0);
static LAST_MSG: AtomicU32 = AtomicU32::new(0);
static LAST_WHO: AtomicU32 = AtomicU32::new(0);

unsafe extern "system" fn veh(info: *mut windows::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS) -> i32 {
    let rec = &*(*info).ExceptionRecord;
    if rec.ExceptionCode.0 == 0xC000_0005u32 as i32 || rec.ExceptionCode.0 == 0xC000_041Du32 as i32 {
        let base = GetModuleHandleW(None).map(|h| h.0 as usize).unwrap_or(0);
        let mut frames = [std::ptr::null_mut::<c_void>(); 32];
        let n = windows::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace(0, &mut frames, None);
        let mut out = format!(
            "code={:#x} addr={:#x} base={:#x} last_who={} last_msg={:#x}
",
            rec.ExceptionCode.0 as u32, rec.ExceptionAddress as usize, base,
            LAST_WHO.load(Ordering::Relaxed), LAST_MSG.load(Ordering::Relaxed)
        );
        if rec.NumberParameters >= 2 {
            out.push_str(&format!("av info: {:#x} {:#x}
", rec.ExceptionInformation[0], rec.ExceptionInformation[1]));
        }
        for f in frames.iter().take(n as usize) {
            out.push_str(&format!("  {:#x}  (exe+{:#x})
", *f as usize, (*f as usize).wrapping_sub(base)));
        }
        let _ = std::fs::write(std::env::temp_dir().join("lb_crash.txt"), out);
    }
    0 // EXCEPTION_CONTINUE_SEARCH
}

// start/exit breadcrumbs in %TEMP%\lb_start.txt (unix seconds), so a silent exit at logon leaves a trace
fn trace(msg: &str) {
    use std::io::Write;
    let p = std::env::temp_dir().join("lb_start.txt");
    if std::fs::metadata(&p).map(|m| m.len() > 20_000).unwrap_or(false) {
        let _ = std::fs::remove_file(&p);
    }
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "{} pid={} {}", secs, std::process::id(), msg);
    }
}

fn lock() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn px(v: i32) -> i32 {
    v * DPI.load(Ordering::Relaxed) / 96
}

fn main_hwnd() -> HWND {
    HWND(MAIN_HWND.load(Ordering::Relaxed) as *mut c_void)
}

fn fly_hwnd() -> HWND {
    HWND(FLY_HWND.load(Ordering::Relaxed) as *mut c_void)
}

// ---------- settings ----------

fn settings_path() -> std::path::PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    std::path::Path::new(&base).join("lite-brightness").join("settings.txt")
}

// "175+0,0+0" -> two slots (vk + modifier bits)
fn parse_slots(v: &str) -> [Key; SLOTS] {
    let mut out = [NONE_KEY; SLOTS];
    for (i, part) in v.split(',').take(SLOTS).enumerate() {
        let (vk, mods) = part.split_once('+').unwrap_or((part, "0"));
        out[i] = Key { vk: vk.trim().parse().unwrap_or(0), mods: mods.trim().parse().unwrap_or(0) };
    }
    out
}

fn load_settings() {
    let Ok(text) = std::fs::read_to_string(settings_path()) else { return };
    let mut st = lock();
    let mut old_button: Option<u8> = None;
    let mut have_binds = false;
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        match k.trim() {
            "dial" => st.s.dial = v.trim() == "1",
            "button" => old_button = v.trim().parse().ok(),
            "ext_only" => st.s.ext_only = v.trim() == "1",
            "cycle_all" => st.s.cycle_all = v.trim() == "1",
            "device" => *DEVICE_MATCH.lock().unwrap_or_else(|e| e.into_inner()) = v.trim().to_uppercase(),
            k if k.starts_with("bind_") => {
                if let Some(a) = ACTIONS.iter().position(|(id, _)| *id == &k[5..]) {
                    st.s.binds[a] = parse_slots(v);
                    have_binds = true;
                }
            }
            _ => {}
        }
    }
    // older versions had one "dial button action": move the Mute key (dial press) to that action
    if let (false, Some(b)) = (have_binds, old_button) {
        st.s.binds[A_CYCLE][0] = NONE_KEY;
        let to = [Some(A_CYCLE), Some(A_LOCK), Some(A_OFF), Some(A_LOCK_OFF), None].get(b as usize).copied().flatten();
        if let Some(a) = to {
            st.s.binds[a][0] = bare(0xAD);
        }
    }
}

fn save_settings() {
    let st = lock();
    let device = DEVICE_MATCH.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut text = format!("dial={}\next_only={}\ncycle_all={}\ndevice={}\n", st.s.dial as u8, st.s.ext_only as u8, st.s.cycle_all as u8, device);
    for (a, (id, _)) in ACTIONS.iter().enumerate() {
        let slots: Vec<String> = st.s.binds[a].iter().map(|k| format!("{}+{}", k.vk, k.mods)).collect();
        text.push_str(&format!("bind_{}={}\n", id, slots.join(",")));
    }
    drop(st);
    let p = settings_path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, text);
}

// ---------- run at startup ----------

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_NAME: &str = "LiteBrightness";

// a logon Scheduled Task named like the Run value may also start the app (more reliable at logon)
fn schtasks(args: &[&str]) -> bool {
    use std::process::Stdio;
    std::process::Command::new("schtasks")
        .args(args)
        .creation_flags(0x0800_0000)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

unsafe fn startup_enabled() -> bool {
    let mut key = HKEY::default();
    let k = wide(RUN_KEY);
    let mut found = false;
    if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(k.as_ptr()), 0, KEY_QUERY_VALUE, &mut key).is_ok() {
        let n = wide(RUN_NAME);
        found = RegQueryValueExW(key, PCWSTR(n.as_ptr()), None, None, None, None).is_ok();
        let _ = RegCloseKey(key);
    }
    found || schtasks(&["/Query", "/TN", RUN_NAME])
}

unsafe fn set_startup(on: bool) {
    let mut key = HKEY::default();
    let k = wide(RUN_KEY);
    if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(k.as_ptr()), 0, KEY_SET_VALUE, &mut key).is_err() {
        return;
    }
    let n = wide(RUN_NAME);
    if on {
        if let Ok(exe) = std::env::current_exe() {
            let v = wide(&format!("\"{}\"", exe.display()));
            let bytes = std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 2);
            let _ = RegSetValueExW(key, PCWSTR(n.as_ptr()), 0, REG_SZ, Some(bytes));
        }
    } else {
        let _ = RegDeleteValueW(key, PCWSTR(n.as_ptr()));
        schtasks(&["/Delete", "/TN", RUN_NAME, "/F"]);
    }
    let _ = RegCloseKey(key);
}

// ---------- laptop panel brightness (WMI) ----------

unsafe fn connect_wmi() -> Option<IWbemServices> {
    let loc: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER).ok()?;
    let svc = loc
        .ConnectServer(&BSTR::from("ROOT\\WMI"), &BSTR::new(), &BSTR::new(), &BSTR::new(), 0, &BSTR::new(), None)
        .ok()?;
    CoSetProxyBlanket(
        &svc,
        10, // RPC_C_AUTHN_WINNT
        0,  // RPC_C_AUTHZ_NONE
        None,
        RPC_C_AUTHN_LEVEL_CALL,
        RPC_C_IMP_LEVEL_IMPERSONATE,
        None,
        EOAC_NONE,
    )
    .ok()?;
    Some(svc)
}

unsafe fn wmi_first(svc: &IWbemServices, query: &str) -> Option<IWbemClassObject> {
    let en = svc
        .ExecQuery(
            &BSTR::from("WQL"),
            &BSTR::from(query),
            WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
            None,
        )
        .ok()?;
    let mut objs: [Option<IWbemClassObject>; 1] = [None];
    let mut ret = 0u32;
    let _ = en.Next(WBEM_INFINITE, &mut objs, &mut ret);
    objs[0].take()
}

unsafe fn wmi_get_brightness(svc: &IWbemServices) -> Option<i32> {
    let obj = wmi_first(svc, "SELECT CurrentBrightness FROM WmiMonitorBrightness WHERE Active=TRUE")?;
    let mut v = VARIANT::default();
    obj.Get(w!("CurrentBrightness"), 0, &mut v, None, None).ok()?;
    u32::try_from(&v).ok().map(|x| x as i32)
}

fn wmi_log(step: &str, err: &windows::core::Error) {
    let _ = std::fs::write(std::env::temp_dir().join("lb_wmi.txt"), format!("step={} err={:?}
", step, err));
}

unsafe fn wmi_set_brightness(svc: &IWbemServices, pct: u32) -> Option<()> {
    let mut class: Option<IWbemClassObject> = None;
    if let Err(e) = svc.GetObject(&BSTR::from("WmiMonitorBrightnessMethods"), WBEM_FLAG_RETURN_WBEM_COMPLETE, None, Some(&mut class), None) {
        wmi_log("GetObject", &e);
        return None;
    }
    let class = class?;
    let mut in_sig: Option<IWbemClassObject> = None;
    if let Err(e) = class.GetMethod(w!("WmiSetBrightness"), 0, &mut in_sig, std::ptr::null_mut()) {
        wmi_log("GetMethod", &e);
        return None;
    }
    let inst = match in_sig?.SpawnInstance(0) {
        Ok(i) => i,
        Err(e) => {
            wmi_log("SpawnInstance", &e);
            return None;
        }
    };
    if let Err(e) = inst.Put(w!("Timeout"), 0, &VARIANT::from(0i32), 0) {
        wmi_log("Put Timeout", &e);
        return None;
    }
    // Brightness is a uint8 property: hand WMI a real byte (VT_UI1) rather than an int
    if let Err(e) = inst.Put(w!("Brightness"), 0, &VARIANT::from(pct as u8), 0) {
        wmi_log("Put Brightness", &e);
        return None;
    }

    let Some(obj) = wmi_first(svc, "SELECT * FROM WmiMonitorBrightnessMethods WHERE Active=TRUE") else {
        wmi_log("query returned no instance", &windows::core::Error::from(E_FAIL));
        return None;
    };
    let mut v = VARIANT::default();
    if let Err(e) = obj.Get(w!("__PATH"), 0, &mut v, None, None) {
        wmi_log("Get __PATH", &e);
        return None;
    }
    let Ok(path) = BSTR::try_from(&v) else {
        wmi_log("path not a string", &windows::core::Error::from(E_FAIL));
        return None;
    };
    if let Err(e) = svc.ExecMethod(&path, &BSTR::from("WmiSetBrightness"), WBEM_FLAG_RETURN_WBEM_COMPLETE, None, &inst, None, None) {
        wmi_log("ExecMethod", &e);
        return None;
    }
    Some(())
}

// ---------- background worker (keeps slow hardware calls off the UI thread) ----------

enum Job {
    Brightness { internal: bool, handle: isize, value: u32 },
}

fn send_job(j: Job) {
    if let Some(tx) = TX.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        let _ = tx.send(j);
    }
}

fn worker(rx: Receiver<Job>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let mut svc: Option<IWbemServices> = None;
        while let Ok(first) = rx.recv() {
            let mut jobs = vec![first];
            while let Ok(j) = rx.try_recv() {
                jobs.push(j);
            }
            // latest value per display wins
            let mut done: Vec<(bool, isize)> = Vec::new();
            for j in jobs.into_iter().rev() {
                let Job::Brightness { internal, handle, value } = j;
                let key = (internal, handle);
                if done.contains(&key) {
                    continue;
                }
                done.push(key);
                if internal {
                    if svc.is_none() {
                        svc = connect_wmi();
                    }
                    // one retry on a fresh connection if the call fails
                    for _ in 0..2 {
                        if let Some(s) = &svc {
                            let r = wmi_set_brightness(s, value);
                            if r.is_some() {
                                break;
                            }
                            svc = None;
                        }
                        svc = connect_wmi();
                    }
                } else {
                    let _ = SetMonitorBrightness(HANDLE(handle as *mut c_void), value);
                }
            }
        }
    }
}

fn schedule_apply() {
    if !APPLY_PENDING.swap(true, Ordering::Relaxed) {
        unsafe {
            SetTimer(main_hwnd(), TIMER_APPLY, 50, None);
        }
    }
}

unsafe fn apply_brightness(hwnd: HWND) {
    let _ = KillTimer(hwnd, TIMER_APPLY);
    APPLY_PENDING.store(false, Ordering::Relaxed);
    let mut st = lock();
    for m in st.mons.iter_mut() {
        if m.ok && !m.off && m.pct != m.applied {
            let value = m.min + (m.pct as u32) * (m.max - m.min) / 100;
            send_job(Job::Brightness { internal: m.internal, handle: m.handle, value });
            m.applied = m.pct;
        }
    }
}

// ---------- display discovery ----------

struct Topology {
    internal_present: bool,
    internal_active: bool,
    external_names: Vec<String>, // active external displays, in path order
}

unsafe fn query_paths(flags: QUERY_DISPLAY_CONFIG_FLAGS) -> Vec<DISPLAYCONFIG_PATH_INFO> {
    let (mut np, mut nm) = (0u32, 0u32);
    if GetDisplayConfigBufferSizes(flags, &mut np, &mut nm).is_err() {
        return vec![];
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
    if QueryDisplayConfig(flags, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None).is_err() {
        return vec![];
    }
    paths.truncate(np as usize);
    paths
}

fn is_internal_tech(t: i32) -> bool {
    t == i32::MIN /* INTERNAL */ || t == 6 /* LVDS */ || t == 11 /* DP embedded */
}

unsafe fn topology() -> Topology {
    let mut t = Topology { internal_present: false, internal_active: false, external_names: vec![] };
    for p in query_paths(QDC_ALL_PATHS) {
        if is_internal_tech(p.targetInfo.outputTechnology.0) && p.targetInfo.targetAvailable.as_bool() {
            t.internal_present = true;
            if p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0 {
                t.internal_active = true;
            }
        }
    }
    for p in query_paths(QDC_ONLY_ACTIVE_PATHS) {
        if is_internal_tech(p.targetInfo.outputTechnology.0) {
            continue;
        }
        let mut name = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
        name.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
        name.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
        name.header.adapterId = p.targetInfo.adapterId;
        name.header.id = p.targetInfo.id;
        let mut s = String::new();
        if DisplayConfigGetDeviceInfo(&mut name.header) == 0 {
            let buf = name.monitorFriendlyDeviceName;
            let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
            s = String::from_utf16_lossy(&buf[..len]);
        }
        t.external_names.push(s);
    }
    t
}

unsafe extern "system" fn enum_cb(hm: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
    let v = &mut *(lp.0 as *mut Vec<HMONITOR>);
    v.push(hm);
    TRUE
}

unsafe fn release_handles(power_on: bool, restore_internal: bool) {
    let mut st = lock();
    for m in st.mons.drain(..) {
        if m.internal {
            if restore_internal && m.off {
                spawn_displayswitch("/extend");
            }
            continue;
        }
        let h = HANDLE(m.handle as *mut c_void);
        if m.off && power_on {
            let _ = SetVCPFeature(h, VCP_POWER, 1);
        }
        let _ = DestroyPhysicalMonitor(h);
    }
}

unsafe fn refresh_monitors() {
    release_handles(true, false);
    let topo = topology();

    let mut hms: Vec<HMONITOR> = Vec::new();
    let _ = EnumDisplayMonitors(HDC::default(), None, Some(enum_cb), LPARAM(&mut hms as *mut _ as isize));

    let mut mons: Vec<Mon> = Vec::new();
    let mut ext_idx = 0usize;
    let mut skipped_internal = false;
    for hm in hms {
        let mut n = 0u32;
        let have = GetNumberOfPhysicalMonitorsFromHMONITOR(hm, &mut n).is_ok() && n > 0;
        if !have {
            if topo.internal_active && !skipped_internal {
                skipped_internal = true; // that's the laptop panel
            }
            continue;
        }
        let mut arr = vec![PHYSICAL_MONITOR::default(); n as usize];
        if GetPhysicalMonitorsFromHMONITOR(hm, &mut arr).is_err() {
            continue;
        }
        for pm in arr {
            let (mut mn, mut cur, mut mx) = (0u32, 0u32, 0u32);
            let ok = GetMonitorBrightness(pm.hPhysicalMonitor, &mut mn, &mut cur, &mut mx) != 0 && mx > mn;
            if !ok && topo.internal_active && !skipped_internal {
                skipped_internal = true; // laptop panel that still reports a handle
                let _ = DestroyPhysicalMonitor(pm.hPhysicalMonitor);
                continue;
            }
            let name = match topo.external_names.get(ext_idx) {
                Some(s) if !s.is_empty() => s.clone(),
                _ => format!("Display {}", ext_idx + 1),
            };
            ext_idx += 1;
            let pct = if ok { ((cur - mn) * 100 / (mx - mn)) as i32 } else { 0 };
            mons.push(Mon {
                handle: pm.hPhysicalMonitor.0 as isize,
                name,
                internal: false,
                min: mn,
                max: mx,
                pct,
                applied: pct,
                ok,
                off: false,
            });
        }
    }

    if topo.internal_present {
        let pct = if topo.internal_active {
            connect_wmi().and_then(|s| wmi_get_brightness(&s)).unwrap_or(50)
        } else {
            50
        };
        mons.insert(
            0,
            Mon {
                handle: 0,
                name: "Laptop screen".into(),
                internal: true,
                min: 0,
                max: 100,
                pct,
                applied: pct,
                ok: true,
                off: !topo.internal_active,
            },
        );
    }

    let mut st = lock();
    st.target = mons.iter().position(|m| !m.internal && m.ok).or_else(|| mons.iter().position(|m| m.ok)).map(|i| i as i32).unwrap_or(0);
    st.mons = mons;
}

fn targets(st: &State) -> Vec<usize> {
    let usable = |i: usize| st.mons[i].ok && !st.mons[i].off;
    if st.target == TARGET_VOLUME {
        vec![]
    } else if st.target < 0 {
        (0..st.mons.len()).filter(|&i| usable(i)).collect()
    } else {
        let i = st.target as usize;
        if i < st.mons.len() && usable(i) { vec![i] } else { vec![] }
    }
}

unsafe fn spawn_displayswitch(arg: &str) {
    let _ = std::process::Command::new(DISPLAYSWITCH).arg(arg).creation_flags(0x0800_0000).spawn();
}

unsafe fn toggle_display_power(i: usize) {
    let mut st = lock();
    if i >= st.mons.len() {
        return;
    }
    let turning_off = !st.mons[i].off;
    if turning_off && st.mons.iter().filter(|m| !m.off).count() <= 1 {
        drop(st);
        show_osd("Can't turn off the last display".into(), -1);
        return;
    }
    if st.mons[i].internal {
        // Windows can't switch a laptop panel off over DDC: use "second screen only" / "extend"
        spawn_displayswitch(if turning_off { "/external" } else { "/extend" });
    } else {
        let h = HANDLE(st.mons[i].handle as *mut c_void);
        if SetVCPFeature(h, VCP_POWER, if turning_off { 4 } else { 1 }) == 0 {
            drop(st);
            show_osd("This display doesn't support power control".into(), -1);
            return;
        }
    }
    st.mons[i].off = turning_off;
    if turning_off && st.target == i as i32 {
        st.target = (0..st.mons.len()).find(|&j| !st.mons[j].off && st.mons[j].ok).map(|j| j as i32).unwrap_or(-1);
    }
}

// ---------- on-screen popup ----------

unsafe fn show_osd(text: String, pct: i32) {
    *OSD.lock().unwrap_or_else(|e| e.into_inner()) = (text, pct);
    let osd = HWND(OSD_HWND.load(Ordering::Relaxed) as *mut c_void);
    let cx = GetSystemMetrics(SM_CXSCREEN);
    let cy = GetSystemMetrics(SM_CYSCREEN);
    let (w, h) = (px(200), px(44));
    let _ = SetWindowPos(osd, HWND_TOPMOST, (cx - w) / 2, cy - px(150), w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    let _ = InvalidateRect(osd, None, TRUE);
    SetTimer(main_hwnd(), TIMER_OSD, 1300, None);
}

unsafe fn draw_text(hdc: HDC, s: &str, mut rc: RECT, size: i32, weight: i32, color: u32, fmt: DRAW_TEXT_FORMAT, face: PCWSTR) {
    if s.is_empty() {
        return; // DrawTextW reads through a dangling pointer for empty slices
    }
    let font = CreateFontW(
        -px(size), 0, 0, 0, weight, 0, 0, 0, DEFAULT_CHARSET.0 as u32, OUT_DEFAULT_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32, CLEARTYPE_QUALITY.0 as u32, 0, face,
    );
    let old = SelectObject(hdc, font);
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, COLORREF(color));
    let mut t: Vec<u16> = s.encode_utf16().collect();
    DrawTextW(hdc, &mut t, &mut rc, fmt | DT_SINGLELINE | DT_VCENTER);
    SelectObject(hdc, old);
    let _ = DeleteObject(font);
}

unsafe fn fill(hdc: HDC, rc: &RECT, color: u32) {
    let b = CreateSolidBrush(COLORREF(color));
    FillRect(hdc, rc, b);
    let _ = DeleteObject(b);
}

unsafe extern "system" fn osd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    LAST_WHO.store(3, Ordering::Relaxed);
    LAST_MSG.store(msg, Ordering::Relaxed);
    if msg == WM_NCPAINT {
        return LRESULT(0); // frameless popup: nothing to paint (default handler crashes here)
    }
    if msg == WM_PAINT {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let (text, pct) = OSD.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        fill(hdc, &rc, 0x0028_2828);
        let tr = RECT { left: px(12), top: px(4), right: rc.right - px(12), bottom: px(26) };
        draw_text(hdc, &text, tr, 14, 600, 0x00FF_FFFF, DT_LEFT | DT_END_ELLIPSIS, w!("Segoe UI"));
        if pct >= 0 {
            let track = RECT { left: px(12), top: px(31), right: rc.right - px(12), bottom: px(36) };
            fill(hdc, &track, 0x0050_5050);
            let w = (track.right - track.left) * pct.clamp(0, 100) / 100;
            fill(hdc, &RECT { right: track.left + w, ..track }, 0x00FF_C24C);
        }
        let _ = EndPaint(hwnd, &ps);
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

// ---------- flyout (sliders for every display) ----------

const FLY_W: i32 = 340;
const ROW_H: i32 = 78;
const PAD_TOP: i32 = 14;
const FOOT_H: i32 = 46;

enum Hit {
    Slider(usize),
    Power(usize),
    Gear,
    Nothing,
}

fn fly_height(n: usize) -> i32 {
    px(PAD_TOP) + px(ROW_H) * n as i32 + px(FOOT_H)
}

fn track_x() -> (i32, i32) {
    (px(20), px(FLY_W) - px(84))
}

fn hit_test(x: i32, y: i32, n: usize) -> Hit {
    let fh = fly_height(n);
    if y >= fh - px(FOOT_H) {
        return if x >= px(FLY_W) - px(56) { Hit::Gear } else { Hit::Nothing };
    }
    let rel = y - px(PAD_TOP);
    if rel < 0 {
        return Hit::Nothing;
    }
    let i = (rel / px(ROW_H)) as usize;
    if i >= n {
        return Hit::Nothing;
    }
    let ry = rel % px(ROW_H);
    if ry < px(32) && x >= px(FLY_W) - px(60) {
        return Hit::Power(i);
    }
    if ry >= px(32) && x >= px(10) && x <= px(FLY_W) - px(70) {
        return Hit::Slider(i);
    }
    Hit::Nothing
}

fn pct_from_x(x: i32) -> i32 {
    let (x0, x1) = track_x();
    ((x - x0) * 100 / (x1 - x0)).clamp(0, 100)
}

unsafe fn set_pct(i: usize, pct: i32) {
    {
        let mut st = lock();
        if i >= st.mons.len() || !st.mons[i].ok || st.mons[i].off {
            return;
        }
        st.mons[i].pct = pct;
    }
    schedule_apply();
    let _ = InvalidateRect(fly_hwnd(), None, FALSE);
}

unsafe fn show_flyout() {
    if let Some(t) = *FLY_HIDDEN_AT.lock().unwrap_or_else(|e| e.into_inner()) {
        if t.elapsed() < Duration::from_millis(300) {
            return; // the click that closed it also hit the tray icon
        }
    }
    let n = lock().mons.len();
    let (w, h) = (px(FLY_W), fly_height(n.max(1)));
    let mut wa = RECT::default();
    let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut wa as *mut _ as *mut c_void), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
    let hw = fly_hwnd();
    let _ = SetWindowPos(hw, HWND_TOPMOST, wa.right - w - px(12), wa.bottom - h - px(12), w, h, SWP_SHOWWINDOW);
    let _ = SetForegroundWindow(hw);
    let _ = InvalidateRect(hw, None, TRUE);
}

unsafe fn paint_flyout(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let wnd_dc = BeginPaint(hwnd, &mut ps);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);

    // draw into an off-screen bitmap to avoid flicker
    let hdc = CreateCompatibleDC(wnd_dc);
    let bmp = CreateCompatibleBitmap(wnd_dc, rc.right, rc.bottom);
    let old_bmp = SelectObject(hdc, bmp);
    fill(hdc, &rc, 0x0020_2020);

    let rows: Vec<(String, bool, bool, i32)> = lock().mons.iter().map(|m| (m.name.clone(), m.off, m.ok, m.pct)).collect();
    let (x0, x1) = track_x();
    let face = w!("Segoe UI");
    let sym = w!("Segoe UI Symbol");

    if rows.is_empty() {
        let r = RECT { left: px(20), top: px(PAD_TOP), right: rc.right - px(20), bottom: px(PAD_TOP) + px(ROW_H) };
        draw_text(hdc, "No displays found", r, 16, 400, 0x00C0_C0C0, DT_LEFT, face);
    }

    for (i, (name, off, ok, pct)) in rows.iter().enumerate() {
        let y0 = px(PAD_TOP) + px(ROW_H) * i as i32;
        let dim = *off || !*ok;
        let text_col = if dim { 0x0080_8080 } else { 0x00FF_FFFF };

        let label = if !*ok { format!("{}  (no brightness control)", name) } else { name.clone() };
        let lr = RECT { left: px(20), top: y0, right: rc.right - px(70), bottom: y0 + px(28) };
        draw_text(hdc, &label, lr, 16, 400, text_col, DT_LEFT | DT_END_ELLIPSIS, face);

        // power button
        let pr = RECT { left: rc.right - px(54), top: y0, right: rc.right - px(20), bottom: y0 + px(28) };
        let pcol = if *off { 0x0080_8080 } else { 0x00FF_C24C };
        draw_text(hdc, "\u{23FB}", pr, 18, 400, pcol, DT_CENTER, sym);

        // slider
        let cy = y0 + px(46);
        fill(hdc, &RECT { left: x0, top: cy - px(2), right: x1, bottom: cy + px(2) }, 0x0090_9090);
        let tx = x0 + (x1 - x0) * pct / 100;
        let accent = if dim { 0x0070_7070 } else { 0x00FF_C24C };
        fill(hdc, &RECT { left: x0, top: cy - px(2), right: tx, bottom: cy + px(2) }, accent);
        let brush = CreateSolidBrush(COLORREF(accent));
        let ob = SelectObject(hdc, brush);
        let op = SelectObject(hdc, GetStockObject(NULL_PEN));
        let r = px(9);
        let _ = Ellipse(hdc, tx - r, cy - r, tx + r, cy + r);
        SelectObject(hdc, ob);
        SelectObject(hdc, op);
        let _ = DeleteObject(brush);

        let vr = RECT { left: x1 + px(14), top: cy - px(16), right: rc.right - px(14), bottom: cy + px(16) };
        draw_text(hdc, &pct.to_string(), vr, 20, 400, text_col, DT_RIGHT, face);
    }

    // footer
    let fr = RECT { left: 0, top: rc.bottom - px(FOOT_H), right: rc.right, bottom: rc.bottom };
    fill(hdc, &fr, 0x002B_2B2B);
    let tr = RECT { left: px(20), top: fr.top, right: rc.right - px(70), bottom: fr.bottom };
    draw_text(hdc, "Adjust Brightness", tr, 14, 400, 0x00E0_E0E0, DT_LEFT, face);
    let gr = RECT { left: rc.right - px(56), top: fr.top, right: rc.right - px(20), bottom: fr.bottom };
    draw_text(hdc, "\u{2699}", gr, 18, 400, 0x00E0_E0E0, DT_CENTER, sym);

    let _ = BitBlt(wnd_dc, 0, 0, rc.right, rc.bottom, hdc, 0, 0, SRCCOPY);
    SelectObject(hdc, old_bmp);
    let _ = DeleteObject(bmp);
    let _ = DeleteDC(hdc);
    let _ = EndPaint(hwnd, &ps);
}

unsafe extern "system" fn fly_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    LAST_WHO.store(2, Ordering::Relaxed);
    LAST_MSG.store(msg, Ordering::Relaxed);
    if msg == WM_NCPAINT {
        return LRESULT(0);
    }
    let (x, y) = ((lp.0 & 0xFFFF) as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as i16 as i32);
    match msg {
        WM_PAINT => {
            paint_flyout(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_ACTIVATE => {
            if (wp.0 & 0xFFFF) as u32 == WA_INACTIVE {
                let _ = ShowWindow(hwnd, SW_HIDE);
                *FLY_HIDDEN_AT.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let n = lock().mons.len();
            match hit_test(x, y, n) {
                Hit::Slider(i) => {
                    DRAG.store(i as i32, Ordering::Relaxed);
                    SetCapture(hwnd);
                    set_pct(i, pct_from_x(x));
                }
                Hit::Power(i) => {
                    toggle_display_power(i);
                    let _ = InvalidateRect(hwnd, None, FALSE);
                }
                Hit::Gear => {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    show_menu(main_hwnd());
                }
                Hit::Nothing => {}
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let d = DRAG.load(Ordering::Relaxed);
            if d >= 0 {
                set_pct(d as usize, pct_from_x(x));
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if DRAG.swap(-1, Ordering::Relaxed) >= 0 {
                let _ = ReleaseCapture();
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            // wheel coordinates are screen-relative
            let mut pt = POINT { x, y };
            let _ = ScreenToClient(hwnd, &mut pt);
            let n = lock().mons.len();
            if let Hit::Slider(i) | Hit::Power(i) = hit_test(pt.x, pt.y, n) {
                let delta = ((wp.0 >> 16) & 0xFFFF) as i16 as i32;
                let cur = lock().mons[i].pct;
                set_pct(i, (cur + if delta > 0 { STEP } else { -STEP }).clamp(0, 100));
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ---------- keyboard hook (dial = volume keys, button = mute) ----------

unsafe fn is_external_device(h: HANDLE) -> bool {
    let key = h.0 as isize;
    if let Some(&(_, r)) = DEVICE_CACHE.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(k, _)| *k == key) {
        return r;
    }
    let mut n = 0u32;
    GetRawInputDeviceInfoW(h, RIDI_DEVICENAME, None, &mut n);
    let mut name = vec![0u16; n as usize + 1];
    GetRawInputDeviceInfoW(h, RIDI_DEVICENAME, Some(name.as_mut_ptr() as *mut c_void), &mut n);
    let name = String::from_utf16_lossy(&name).to_uppercase();
    let m = DEVICE_MATCH.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let r = !m.is_empty() && name.contains(&m);
    DEVICE_CACHE.lock().unwrap_or_else(|e| e.into_inner()).push((key, r));
    r
}

// unassigned key, sent after a swallowed Alt/Win combo so the Alt menu / Start menu doesn't open
const VK_MASK: u16 = 0xE8;

fn is_modifier(vk: u16) -> bool {
    matches!(vk, 0x10 | 0x11 | 0x12 | 0x5B | 0x5C | 0xA0..=0xA5)
}

// browser / volume / media / launch keys: these come from the consumer-control page (the dial)
fn is_media(vk: u16) -> bool {
    (0xA6..=0xB7).contains(&vk)
}

unsafe fn held_mods() -> u8 {
    let down = |vk: VIRTUAL_KEY| (GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000) != 0;
    let mut m = 0;
    if down(VK_CONTROL) {
        m |= MOD_CTRL;
    }
    if down(VK_MENU) {
        m |= MOD_ALT;
    }
    if down(VK_SHIFT) {
        m |= MOD_SHIFT;
    }
    if down(VK_LWIN) || down(VK_RWIN) {
        m |= MOD_WIN;
    }
    m
}

unsafe fn send_mask_key() {
    let ev = |flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(VK_MASK), wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    };
    let inputs = [ev(KEYBD_EVENT_FLAGS(0)), ev(KEYEVENTF_KEYUP)];
    SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
}

unsafe extern "system" fn kb_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    LAST_WHO.store(4, Ordering::Relaxed);
    LAST_MSG.store(0x1000, Ordering::Relaxed);
    if code == HC_ACTION as i32 {
        // the keyboard driver delivers its media keys as injected, so do not filter those
        let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
        let vk = k.vkCode as u16;
        let msg = wp.0 as u32;
        let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        if vk != VK_MASK && !is_modifier(vk) {
            // the bindings window is waiting for a key: grab it from any keyboard
            if CAPTURE.load(Ordering::Relaxed) >= 0 {
                if down {
                    let mods = held_mods();
                    if mods & (MOD_ALT | MOD_WIN) != 0 {
                        send_mask_key();
                    }
                    let keys = HWND(KEYS_HWND.load(Ordering::Relaxed) as *mut c_void);
                    let _ = PostMessageW(keys, WM_BIND, WPARAM(vk as usize), LPARAM(mods as isize));
                    SWALLOWED.store(vk as u32, Ordering::Relaxed);
                }
                return LRESULT(1);
            }
            if !down {
                if SWALLOWED.load(Ordering::Relaxed) == vk as u32 {
                    SWALLOWED.store(0, Ordering::Relaxed);
                    return LRESULT(1);
                }
            } else {
                let mods = held_mods();
                let action = {
                    let st = lock();
                    let from_ext = !st.s.ext_only || !is_media(vk) || {
                        let at = EXT_AT_MS.load(Ordering::Relaxed);
                        at != 0 && now_ms().saturating_sub(at) <= EXT_WINDOW_MS
                    };
                    let key = Key { vk, mods };
                    st.s.binds
                        .iter()
                        .position(|slots| slots.contains(&key))
                        .filter(|_| from_ext)
                        // brightness keys are off, or the dial is set to plain Windows volume
                        .filter(|&a| !((a == A_UP || a == A_DOWN) && (!st.s.dial || st.target == TARGET_VOLUME)))
                };
                if let Some(a) = action {
                    LAST_MSG.store(0x1001, Ordering::Relaxed);
                    if mods & (MOD_ALT | MOD_WIN) != 0 {
                        send_mask_key();
                    }
                    SWALLOWED.store(vk as u32, Ordering::Relaxed);
                    let _ = PostMessageW(main_hwnd(), WM_DIAL, WPARAM(a), LPARAM(0));
                    return LRESULT(1); // swallow so the key's normal job (e.g. volume) doesn't happen
                }
            }
        }
    }
    LAST_MSG.store(0x1002, Ordering::Relaxed);
    CallNextHookEx(HHOOK(HOOK.load(Ordering::Relaxed) as *mut c_void), code, wp, lp)
}

unsafe fn screens_off() {
    SendMessageW(main_hwnd(), WM_SYSCOMMAND, WPARAM(SC_MONITORPOWER as usize), LPARAM(2));
}

// dial targets: >= 0 a single display, TARGET_ALL, TARGET_VOLUME (dial passes through to Windows volume)
const TARGET_ALL: i32 = -1;
const TARGET_VOLUME: i32 = -2;

fn avg_all(st: &State) -> Option<i32> {
    let t = targets(st);
    if t.is_empty() {
        return None;
    }
    Some((t.iter().map(|&i| st.mons[i].pct).sum::<i32>() as f32 / t.len() as f32).round() as i32)
}

fn sync_all(st: &mut State) -> Option<i32> {
    let t = targets(st);
    if t.is_empty() {
        return None;
    }
    let avg = (t.iter().map(|&i| st.mons[i].pct).sum::<i32>() as f32 / t.len() as f32).round() as i32;
    for &i in &t {
        st.mons[i].pct = avg;
    }
    Some(avg)
}

unsafe fn run_action(a: usize) {
    match a {
        A_UP | A_DOWN => {
            let delta = if a == A_UP { STEP } else { -STEP };
            let (text, pct) = {
                let mut st = lock();
                let t = targets(&st);
                if t.is_empty() {
                    drop(st);
                    show_osd("This display has no brightness control".into(), -1);
                    return;
                }
                let name = if st.target == TARGET_ALL { "All displays".to_string() } else { st.mons[t[0]].name.clone() };
                let new = if st.target == TARGET_ALL {
                    // keep every display at the same level
                    let avg = sync_all(&mut st).unwrap_or(0);
                    Some((avg + delta).clamp(0, 100))
                } else {
                    None
                };
                for &i in &t {
                    let m = &mut st.mons[i];
                    m.pct = new.unwrap_or((m.pct + delta).clamp(0, 100));
                }
                (format!("{}   {}%", name, st.mons[t[0]].pct), st.mons[t[0]].pct)
            };
            show_osd(text, pct);
            schedule_apply();
            let _ = InvalidateRect(fly_hwnd(), None, FALSE);
        }
        A_CYCLE => {
                    let (name, pct) = {
                        let mut st = lock();
                        let usable = |m: &Mon| m.ok && !m.off;
                        // order: external displays, laptop panel, all displays, volume
                        let mut order: Vec<i32> = (0..st.mons.len()).filter(|&i| usable(&st.mons[i]) && !st.mons[i].internal).map(|i| i as i32).collect();
                        order.extend((0..st.mons.len()).filter(|&i| usable(&st.mons[i]) && st.mons[i].internal).map(|i| i as i32));
                        if order.len() > 1 && st.s.cycle_all {
                            order.push(TARGET_ALL);
                        }
                        order.push(TARGET_VOLUME);
                        let next = match order.iter().position(|&t| t == st.target) {
                            Some(p) => order[(p + 1) % order.len()],
                            None => order[0],
                        };
                        st.target = next;
                        let (name, pct) = match next {
                            TARGET_VOLUME => ("Volume".to_string(), -1),
                            // just passing through: displays only sync once the dial is actually turned
                            TARGET_ALL => ("All displays".to_string(), avg_all(&st).unwrap_or(-1)),
                            i => (st.mons[i as usize].name.clone(), st.mons[i as usize].pct),
                        };
                        (name, pct)
                    };
                    show_osd(format!("Controlling: {}", name), pct);
                    let _ = InvalidateRect(fly_hwnd(), None, FALSE);
        }
        A_LOCK => {
            let _ = LockWorkStation();
        }
        A_OFF => screens_off(),
        A_LOCK_OFF => {
            let _ = LockWorkStation();
            screens_off();
        }
        _ => {}
    }
}

// ---------- key bindings window ----------

// layout in 96-dpi pixels
const KW: i32 = 486;
const K_ROW0: i32 = 48;
const K_ROW: i32 = 40;
const K_COL: [i32; SLOTS] = [196, 336];
const K_BOX_W: i32 = 130;
const K_NOTE_Y: i32 = K_ROW0 + K_ROW * N_ACTIONS as i32 + 4;
const K_FOOT_Y: i32 = K_NOTE_Y + 52;
const KH: i32 = K_FOOT_Y + 56;

fn scaled(l: i32, t: i32, r: i32, b: i32) -> RECT {
    RECT { left: px(l), top: px(t), right: px(r), bottom: px(b) }
}

fn k_slot_rect(a: usize, s: usize) -> RECT {
    let y = K_ROW0 + K_ROW * a as i32;
    scaled(K_COL[s], y + 5, K_COL[s] + K_BOX_W, y + 35)
}

fn k_reset_rect() -> RECT {
    scaled(20, K_FOOT_Y + 12, 190, K_FOOT_Y + 44)
}

fn k_done_rect() -> RECT {
    scaled(KW - 130, K_FOOT_Y + 12, KW - 20, K_FOOT_Y + 44)
}

fn inside(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

enum KHit {
    Slot(usize, usize),
    Reset,
    Done,
    Nothing,
}

fn k_hit(x: i32, y: i32) -> KHit {
    for a in 0..N_ACTIONS {
        for s in 0..SLOTS {
            if inside(&k_slot_rect(a, s), x, y) {
                return KHit::Slot(a, s);
            }
        }
    }
    if inside(&k_reset_rect(), x, y) {
        KHit::Reset
    } else if inside(&k_done_rect(), x, y) {
        KHit::Done
    } else {
        KHit::Nothing
    }
}

unsafe fn vk_name(vk: u16) -> String {
    let fixed = match vk {
        0xAD => "Mute (dial press)",
        0xAE => "Volume Down (dial)",
        0xAF => "Volume Up (dial)",
        0xB0 => "Next Track",
        0xB1 => "Previous Track",
        0xB2 => "Stop",
        0xB3 => "Play/Pause",
        0xA6 => "Browser Back",
        0xA7 => "Browser Forward",
        0xA8 => "Browser Refresh",
        0xA9 => "Browser Stop",
        0xAA => "Browser Search",
        0xAB => "Favorites",
        0xAC => "Browser Home",
        0xB4 => "Mail",
        0xB5 => "Media Select",
        0xB6 => "Launch App 1",
        0xB7 => "Launch App 2",
        0x5D => "Menu",
        0x2C => "Print Screen",
        0x13 => "Pause",
        _ => "",
    };
    if !fixed.is_empty() {
        return fixed.into();
    }
    let sc = MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC);
    // navigation keys need the "extended" bit or Windows names them after the numpad keys
    let ext = matches!(vk, 0x21..=0x28 | 0x2D | 0x2E | 0x6F | 0x90);
    let mut buf = [0u16; 64];
    let n = GetKeyNameTextW(((sc as i32) << 16) | if ext { 1 << 24 } else { 0 }, &mut buf);
    if n > 0 {
        String::from_utf16_lossy(&buf[..n as usize])
    } else {
        format!("Key {:#04X}", vk)
    }
}

unsafe fn key_name(k: Key) -> String {
    let mut s = String::new();
    for (bit, name) in [(MOD_CTRL, "Ctrl+"), (MOD_ALT, "Alt+"), (MOD_SHIFT, "Shift+"), (MOD_WIN, "Win+")] {
        if k.mods & bit != 0 {
            s.push_str(name);
        }
    }
    s + &vk_name(k.vk)
}

fn set_note(s: String) {
    *KEYS_NOTE.lock().unwrap_or_else(|e| e.into_inner()) = s;
}

unsafe fn show_keys() {
    let hw = HWND(KEYS_HWND.load(Ordering::Relaxed) as *mut c_void);
    CAPTURE.store(-1, Ordering::Relaxed);
    set_note("Click a box, then press the key you want. Right-click a box to clear it.".into());
    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU;
    let mut r = RECT { left: 0, top: 0, right: px(KW), bottom: px(KH) };
    let _ = AdjustWindowRectEx(&mut r, style, FALSE, WINDOW_EX_STYLE(0));
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    let mut wa = RECT::default();
    let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut wa as *mut _ as *mut c_void), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
    let x = wa.left + (wa.right - wa.left - w) / 2;
    let y = wa.top + (wa.bottom - wa.top - h) / 2;
    let _ = SetWindowPos(hw, HWND_TOP, x, y, w, h, SWP_SHOWWINDOW);
    let _ = SetForegroundWindow(hw);
    let _ = InvalidateRect(hw, None, TRUE);
}

unsafe fn paint_keys(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let wnd_dc = BeginPaint(hwnd, &mut ps);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let hdc = CreateCompatibleDC(wnd_dc);
    let bmp = CreateCompatibleBitmap(wnd_dc, rc.right, rc.bottom);
    let old_bmp = SelectObject(hdc, bmp);
    fill(hdc, &rc, 0x0020_2020);

    let face = w!("Segoe UI");
    let (binds, dial) = {
        let st = lock();
        (st.s.binds, st.s.dial)
    };
    let cap = CAPTURE.load(Ordering::Relaxed);
    let pending = *PENDING.lock().unwrap_or_else(|e| e.into_inner());

    draw_text(hdc, "Action", scaled(20, 14, 190, 40), 13, 600, 0x00A0_A0A0, DT_LEFT, face);
    for (s, label) in ["Key 1", "Key 2"].iter().enumerate() {
        draw_text(hdc, label, scaled(K_COL[s], 14, K_COL[s] + K_BOX_W, 40), 13, 600, 0x00A0_A0A0, DT_CENTER, face);
    }

    for a in 0..N_ACTIONS {
        let y = K_ROW0 + K_ROW * a as i32;
        let off = !dial && (a == A_UP || a == A_DOWN);
        let col = if off { 0x0080_8080 } else { 0x00FF_FFFF };
        let label = if off { format!("{} (off)", ACTIONS[a].1) } else { ACTIONS[a].1.to_string() };
        draw_text(hdc, &label, scaled(20, y, K_COL[0] - 8, y + K_ROW), 15, 400, col, DT_LEFT | DT_END_ELLIPSIS, face);
        for s in 0..SLOTS {
            let r = k_slot_rect(a, s);
            let selected = cap == (a * SLOTS + s) as i32;
            if selected {
                fill(hdc, &r, 0x00FF_C24C);
                fill(hdc, &RECT { left: r.left + px(2), top: r.top + px(2), right: r.right - px(2), bottom: r.bottom - px(2) }, 0x0038_3838);
            } else {
                fill(hdc, &r, 0x0030_3030);
            }
            let k = binds[a][s];
            let (text, tcol) = if selected {
                match pending {
                    Some((c, pk)) if c == cap => (format!("{} ?", key_name(pk)), 0x00FF_C24C),
                    _ => ("Press a key...".to_string(), 0x00FF_C24C),
                }
            } else if k.vk == 0 {
                ("-".to_string(), 0x0070_7070)
            } else {
                (key_name(k), 0x00FF_FFFF)
            };
            let tr = RECT { left: r.left + px(6), right: r.right - px(6), ..r };
            draw_text(hdc, &text, tr, 13, 400, tcol, DT_CENTER | DT_END_ELLIPSIS, face);
        }
    }

    // a note may carry a second line (a warning's follow-up), which replaces the dial hint
    let note = KEYS_NOTE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let (line1, line2) = note.split_once('\n').unwrap_or((&note, ""));
    draw_text(hdc, line1, scaled(20, K_NOTE_Y, KW - 20, K_NOTE_Y + 24), 13, 400, 0x00FF_C24C, DT_LEFT | DT_END_ELLIPSIS, face);
    let (hint, hcol) = if line2.is_empty() {
        ("Dial: turning sends Volume Up / Down, pressing sends Mute.", 0x0090_9090)
    } else {
        (line2, 0x00FF_FFFF)
    };
    draw_text(hdc, hint, scaled(20, K_NOTE_Y + 24, KW - 20, K_NOTE_Y + 46), 13, 400, hcol, DT_LEFT | DT_END_ELLIPSIS, face);

    fill(hdc, &scaled(0, K_FOOT_Y, KW, KH), 0x002B_2B2B);
    for (r, label) in [(k_reset_rect(), "Reset to defaults"), (k_done_rect(), "Done")] {
        fill(hdc, &r, 0x003C_3C3C);
        draw_text(hdc, label, r, 14, 400, 0x00FF_FFFF, DT_CENTER, face);
    }

    let _ = BitBlt(wnd_dc, 0, 0, rc.right, rc.bottom, hdc, 0, 0, SRCCOPY);
    SelectObject(hdc, old_bmp);
    let _ = DeleteObject(bmp);
    let _ = DeleteDC(hdc);
    let _ = EndPaint(hwnd, &ps);
}

// well-known shortcuts that other apps don't register with Windows, so the hotkey probe can't see them
const KNOWN: &[(u8, u16, &str)] = &[
    (MOD_CTRL, b'C' as u16, "Copy"),
    (MOD_CTRL, b'V' as u16, "Paste"),
    (MOD_CTRL, b'X' as u16, "Cut"),
    (MOD_CTRL, b'Z' as u16, "Undo"),
    (MOD_CTRL, b'Y' as u16, "Redo"),
    (MOD_CTRL, b'A' as u16, "Select all"),
    (MOD_CTRL, b'S' as u16, "Save"),
    (MOD_CTRL, b'F' as u16, "Find"),
    (MOD_CTRL, b'P' as u16, "Print"),
    (MOD_CTRL, b'N' as u16, "New"),
    (MOD_CTRL, b'O' as u16, "Open"),
    (MOD_CTRL, b'W' as u16, "Close tab"),
    (MOD_CTRL, b'T' as u16, "New tab"),
    (MOD_CTRL, b'R' as u16, "Refresh"),
    (MOD_CTRL, 0x09, "Next tab"),
    (MOD_CTRL | MOD_SHIFT, 0x09, "Previous tab"),
    (MOD_CTRL | MOD_SHIFT, b'T' as u16, "Reopen tab"),
    (MOD_CTRL | MOD_SHIFT, 0x1B, "Task Manager"),
    (MOD_CTRL, 0x1B, "Start menu"),
    (MOD_ALT, 0x09, "Switch apps"),
    (MOD_ALT | MOD_SHIFT, 0x09, "Switch apps"),
    (MOD_ALT, 0x73, "Close window"),
    (MOD_ALT, 0x1B, "Switch apps"),
    (MOD_ALT, 0x20, "Window menu"),
    (MOD_ALT, 0x0D, "Properties / fullscreen"),
    (MOD_ALT, 0x25, "Back"),
    (MOD_ALT, 0x27, "Forward"),
    (MOD_WIN, b'L' as u16, "Lock (Windows always wins)"),
    (MOD_WIN, b'D' as u16, "Show desktop"),
    (MOD_WIN, b'E' as u16, "File Explorer"),
    (MOD_WIN, b'R' as u16, "Run"),
    (MOD_WIN, b'I' as u16, "Settings"),
    (MOD_WIN, b'V' as u16, "Clipboard history"),
    (MOD_WIN, b'X' as u16, "Quick Link menu"),
    (MOD_WIN, 0x09, "Task View"),
    (MOD_WIN | MOD_SHIFT, b'S' as u16, "Snipping Tool"),
    (MOD_WIN, 0x25, "Snap window"),
    (MOD_WIN, 0x26, "Maximize window"),
    (MOD_WIN, 0x27, "Snap window"),
    (MOD_WIN, 0x28, "Minimize window"),
    (MOD_WIN, 0xBE, "Emoji panel"),
];

// why this key might be a bad choice, or None if it looks free
unsafe fn conflict(k: Key) -> Option<String> {
    if let Some((_, _, what)) = KNOWN.iter().find(|(m, vk, _)| *m == k.mods && *vk == k.vk) {
        return Some(format!("Windows/app shortcut ({})", what));
    }
    let f13_f24 = (0x7C..=0x87).contains(&k.vk);
    if k.mods == 0 && !is_media(k.vk) && !f13_f24 {
        return Some(match k.vk {
            0x2C => "Print Screen takes screenshots".into(),
            0x70..=0x7B => "Apps use this key".into(),
            _ => "You'd lose this key for typing".into(),
        });
    }
    if k.mods == MOD_SHIFT && !is_media(k.vk) && !f13_f24 {
        return Some("You'd lose this key for typing".into());
    }
    // another app (or Windows) holds it if we can't register it ourselves
    const PROBE_ID: i32 = 0xB0B0;
    let mods = HOT_KEY_MODIFIERS(k.mods as u32) | MOD_NOREPEAT;
    if RegisterHotKey(main_hwnd(), PROBE_ID, mods, k.vk as u32).is_ok() {
        let _ = UnregisterHotKey(main_hwnd(), PROBE_ID);
        None
    } else {
        Some("Another app or Windows already uses this".into())
    }
}

// a key arrived for the slot in CAPTURE
unsafe fn assign_key(vk: u16, mods: u8) {
    let c = CAPTURE.load(Ordering::Relaxed);
    if c < 0 {
        return;
    }
    let (a, s) = (c as usize / SLOTS, c as usize % SLOTS);
    let name = ACTIONS[a].1;
    let pending = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take();
    if mods == 0 && vk == VK_ESCAPE.0 {
        CAPTURE.store(-1, Ordering::Relaxed);
        set_note("Cancelled.".into());
        return;
    }
    if mods == 0 && (vk == VK_DELETE.0 || vk == VK_BACK.0) {
        CAPTURE.store(-1, Ordering::Relaxed);
        lock().s.binds[a][s] = NONE_KEY;
        save_settings();
        set_note(format!("Cleared a key from {}.", name));
        return;
    }
    let key = Key { vk, mods };
    let already_ours = lock().s.binds.iter().any(|slots| slots.contains(&key));
    let confirmed = pending == Some((c, key));
    if !confirmed && !already_ours {
        if let Some(why) = conflict(key) {
            // stay in capture mode: the same key again means "use it anyway"
            *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some((c, key));
            set_note(format!("{}: {}.\nPress it again to use it anyway, or Esc to cancel.", key_name(key), why));
            return;
        }
    }
    CAPTURE.store(-1, Ordering::Relaxed);
    let mut moved_from: Option<&str> = None;
    {
        let mut st = lock();
        for a2 in 0..N_ACTIONS {
            for s2 in 0..SLOTS {
                if (a2, s2) != (a, s) && st.s.binds[a2][s2] == key {
                    st.s.binds[a2][s2] = NONE_KEY;
                    if a2 != a {
                        moved_from = Some(ACTIONS[a2].1);
                    }
                }
            }
        }
        st.s.binds[a][s] = key;
    }
    save_settings();
    let kn = key_name(key);
    set_note(match moved_from {
        Some(from) => format!("{} now does {} (removed from {}).", kn, name, from),
        None => format!("{} now does {}.", kn, name),
    });
}

unsafe extern "system" fn keys_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    LAST_WHO.store(5, Ordering::Relaxed);
    LAST_MSG.store(msg, Ordering::Relaxed);
    let (x, y) = ((lp.0 & 0xFFFF) as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as i16 as i32);
    if matches!(msg, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_CLOSE) || (msg == WM_ACTIVATE && (wp.0 & 0xFFFF) as u32 == WA_INACTIVE) {
        *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = None; // any click or leaving drops an unconfirmed key
    }
    match msg {
        WM_PAINT => {
            paint_keys(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_LBUTTONDOWN => {
            match k_hit(x, y) {
                KHit::Slot(a, s) => {
                    CAPTURE.store((a * SLOTS + s) as i32, Ordering::Relaxed);
                    set_note(format!("Press a key for {}. Esc cancels, Delete clears.", ACTIONS[a].1));
                }
                KHit::Reset => {
                    CAPTURE.store(-1, Ordering::Relaxed);
                    lock().s.binds = DEFAULT_BINDS;
                    save_settings();
                    set_note("Default keys restored.".into());
                }
                KHit::Done => {
                    CAPTURE.store(-1, Ordering::Relaxed);
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
                KHit::Nothing => {
                    CAPTURE.store(-1, Ordering::Relaxed);
                }
            }
            let _ = InvalidateRect(hwnd, None, FALSE);
            LRESULT(0)
        }
        WM_RBUTTONDOWN => {
            if let KHit::Slot(a, s) = k_hit(x, y) {
                CAPTURE.store(-1, Ordering::Relaxed);
                lock().s.binds[a][s] = NONE_KEY;
                save_settings();
                set_note(format!("Cleared a key from {}.", ACTIONS[a].1));
                let _ = InvalidateRect(hwnd, None, FALSE);
            }
            LRESULT(0)
        }
        WM_BIND => {
            assign_key(wp.0 as u16, lp.0 as u8);
            let _ = InvalidateRect(hwnd, None, FALSE);
            LRESULT(0)
        }
        WM_ACTIVATE => {
            // never keep grabbing keys once the window is in the background
            if (wp.0 & 0xFFFF) as u32 == WA_INACTIVE && CAPTURE.swap(-1, Ordering::Relaxed) >= 0 {
                set_note("Cancelled.".into());
                let _ = InvalidateRect(hwnd, None, FALSE);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            CAPTURE.store(-1, Ordering::Relaxed);
            let _ = ShowWindow(hwnd, SW_HIDE);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ---------- tray menu (right-click) ----------

const ID_DIAL: usize = 10;
const ID_EXT_ONLY: usize = 11;
const ID_KEYS: usize = 12;
const ID_CYCLE_ALL: usize = 13;
const ID_TARGET: usize = 100; // 100+i
const ID_TARGET_ALL: usize = 199;
const ID_TARGET_VOL: usize = 198;
const ID_OFF: usize = 200; // 200+i
const ID_STARTUP: usize = 300;
const ID_REFRESH: usize = 301;
const ID_EXIT: usize = 302;

unsafe fn add(menu: HMENU, flags: MENU_ITEM_FLAGS, id: usize, text: &str) {
    let t = wide(text);
    let _ = AppendMenuW(menu, MF_STRING | flags, id, PCWSTR(t.as_ptr()));
}

fn chk(b: bool) -> MENU_ITEM_FLAGS {
    if b { MF_CHECKED } else { MF_UNCHECKED }
}

unsafe fn show_menu(hwnd: HWND) {
    let (dial, target, mons) = {
        let st = lock();
        let mons: Vec<(String, bool, bool, i32)> = st.mons.iter().map(|m| (m.name.clone(), m.off, m.ok, m.pct)).collect();
        (st.s.dial, st.target, mons)
    };
    let startup = startup_enabled();

    let menu = CreatePopupMenu().unwrap();
    add(menu, chk(dial), ID_DIAL, "Dial controls brightness");
    add(menu, chk(lock().s.ext_only), ID_EXT_ONLY, "Only the external keyboard's dial");
    add(menu, chk(lock().s.cycle_all), ID_CYCLE_ALL, "Include 'All displays' when switching");
    add(menu, MF_UNCHECKED, ID_KEYS, "Key bindings...");

    let tgt = CreatePopupMenu().unwrap();
    for (i, (label, off, ok, pct)) in mons.iter().enumerate() {
        let text = if *ok { format!("{}  ({}%)", label, pct) } else { format!("{}  (no brightness control)", label) };
        let mut f = chk(target == i as i32);
        if *off || !*ok {
            f |= MF_GRAYED;
        }
        add(tgt, f, ID_TARGET + i, &text);
    }
    add(tgt, chk(target == TARGET_ALL), ID_TARGET_ALL, "All displays (synced)");
    add(tgt, chk(target == TARGET_VOLUME), ID_TARGET_VOL, "Volume (normal Windows volume)");
    let t = wide("Dial brightness target");
    let _ = AppendMenuW(menu, MF_POPUP | MF_STRING, tgt.0 as usize, PCWSTR(t.as_ptr()));

    let off = CreatePopupMenu().unwrap();
    for (i, (label, is_off, _, _)) in mons.iter().enumerate() {
        add(off, chk(*is_off), ID_OFF + i, label);
    }
    let t = wide("Turn off display");
    let _ = AppendMenuW(menu, MF_POPUP | MF_STRING, off.0 as usize, PCWSTR(t.as_ptr()));

    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    add(menu, chk(startup), ID_STARTUP, "Run at startup");
    add(menu, MF_UNCHECKED, ID_REFRESH, "Refresh displays");
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    add(menu, MF_UNCHECKED, ID_EXIT, "Exit");

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_LEFTALIGN, pt.x, pt.y, 0, hwnd, None);
    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);

    on_command(hwnd, cmd.0 as usize);
}

unsafe fn on_command(hwnd: HWND, cmd: usize) {
    match cmd {
        0 => {}
        ID_DIAL => {
            {
                let mut st = lock();
                st.s.dial = !st.s.dial;
            }
            save_settings();
        }
        ID_EXT_ONLY => {
            {
                let mut st = lock();
                st.s.ext_only = !st.s.ext_only;
            }
            save_settings();
        }
        ID_KEYS => show_keys(),
        ID_CYCLE_ALL => {
            {
                let mut st = lock();
                st.s.cycle_all = !st.s.cycle_all;
                if !st.s.cycle_all && st.target == TARGET_ALL {
                    st.target = 0;
                }
            }
            save_settings();
        }
        // displays only sync once the dial is actually turned
        ID_TARGET_ALL => lock().target = TARGET_ALL,
        ID_TARGET_VOL => lock().target = TARGET_VOLUME,
        c if (ID_TARGET..ID_TARGET + 64).contains(&c) => lock().target = (c - ID_TARGET) as i32,
        c if (ID_OFF..ID_OFF + 64).contains(&c) => toggle_display_power(c - ID_OFF),
        ID_STARTUP => set_startup(!startup_enabled()),
        ID_REFRESH => refresh_monitors(),
        ID_EXIT => {
            let _ = DestroyWindow(hwnd);
        }
        _ => {}
    }
}

// ---------- tray icon / window ----------

unsafe fn make_icon() -> HICON {
    const N: i32 = 32;
    let mut pix = vec![0u8; (N * N * 4) as usize];
    for y in 0..N {
        for x in 0..N {
            let dx = x as f32 - 15.5;
            let dy = y as f32 - 15.5;
            let d = (dx * dx + dy * dy).sqrt();
            let ang = dy.atan2(dx) / std::f32::consts::TAU * 8.0;
            let ray = (ang - ang.round()).abs() < 0.12;
            if d <= 7.0 || (ray && (10.0..=14.5).contains(&d)) {
                let i = ((y * N + x) * 4) as usize;
                pix[i] = 0x00; // B
                pix[i + 1] = 0xC4; // G
                pix[i + 2] = 0xFF; // R
                pix[i + 3] = 0xFF; // A
            }
        }
    }
    let mask = vec![0u8; (N * N / 8) as usize];
    let color = CreateBitmap(N, N, 1, 32, Some(pix.as_ptr() as *const c_void));
    let mbm = CreateBitmap(N, N, 1, 1, Some(mask.as_ptr() as *const c_void));
    let info = ICONINFO { fIcon: TRUE, xHotspot: 0, yHotspot: 0, hbmMask: mbm, hbmColor: color };
    let icon = CreateIconIndirect(&info).unwrap_or_default();
    let _ = DeleteObject(color);
    let _ = DeleteObject(mbm);
    icon
}

unsafe fn tray_data(hwnd: HWND, icon: HICON) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: icon,
        ..Default::default()
    };
    let tip = wide("Lite Brightness");
    nid.szTip[..tip.len()].copy_from_slice(&tip);
    nid
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    LAST_WHO.store(1, Ordering::Relaxed);
    LAST_MSG.store(msg, Ordering::Relaxed);
    if msg == WM_NCPAINT {
        return LRESULT(0);
    }
    match msg {
        WM_TRAY => {
            let ev = (lp.0 as u32) & 0xFFFF;
            if ev == WM_LBUTTONUP {
                show_flyout();
            } else if ev == WM_RBUTTONUP {
                show_menu(hwnd);
            }
            LRESULT(0)
        }
        WM_DIAL => {
            run_action(wp.0);
            LRESULT(0)
        }
        WM_INPUT => {
            // raw input arrives about 1 ms before the matching keyboard-hook event, which lets the hook
            // know whether a key came from the external keyboard
            let hri = HRAWINPUT(lp.0 as *mut c_void);
            let mut size = 0u32;
            let hdr = std::mem::size_of::<RAWINPUTHEADER>() as u32;
            GetRawInputData(hri, RID_INPUT, None, &mut size, hdr);
            let mut buf = vec![0u8; size as usize];
            GetRawInputData(hri, RID_INPUT, Some(buf.as_mut_ptr() as *mut c_void), &mut size, hdr);
            let ri = &*(buf.as_ptr() as *const RAWINPUT);
            if ri.header.dwType == RIM_TYPEHID.0 && is_external_device(ri.header.hDevice) {
                EXT_AT_MS.store(now_ms(), Ordering::Relaxed);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_TIMER => {
            match wp.0 {
                TIMER_APPLY => apply_brightness(hwnd),
                TIMER_TRAY => {
                    // at logon Explorer's tray may not exist yet: keep trying until the icon is in
                    let nid = tray_data(hwnd, HICON(ICON.load(Ordering::Relaxed) as *mut c_void));
                    if Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() || Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
                        let _ = KillTimer(hwnd, TIMER_TRAY);
                    }
                }
                TIMER_OSD => {
                    let _ = KillTimer(hwnd, TIMER_OSD);
                    let osd = HWND(OSD_HWND.load(Ordering::Relaxed) as *mut c_void);
                    let _ = ShowWindow(osd, SW_HIDE);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_DISPLAYCHANGE => {
            let ext_off = lock().mons.iter().any(|m| m.off && !m.internal);
            if !ext_off {
                refresh_monitors();
                let _ = InvalidateRect(fly_hwnd(), None, TRUE);
            }
            LRESULT(0)
        }
        WM_ENDSESSION => {
            trace("windows is ending the session");
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_DESTROY => {
            trace("main window destroyed");
            let nid = tray_data(hwnd, HICON(ICON.load(Ordering::Relaxed) as *mut c_void));
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            release_handles(true, true);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ if msg == TASKBAR_CREATED.load(Ordering::Relaxed) && msg != 0 => {
            let nid = tray_data(hwnd, HICON(ICON.load(Ordering::Relaxed) as *mut c_void));
            let _ = Shell_NotifyIconW(NIM_ADD, &nid);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn main() {
    unsafe {
        // single instance
        trace("start");
        SetLastError(WIN32_ERROR(0)); // a stale error code would look like "already running"
        let _guard = CreateMutexW(None, true, w!("Local\\LiteBrightnessSingleton"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            trace("exit: another instance is already running");
            return;
        }

        std::panic::set_hook(Box::new(|info| {
            let _ = std::fs::write(std::env::temp_dir().join("lb_crash.txt"), format!("panic: {}\n", info));
        }));
        windows::Win32::System::Diagnostics::Debug::AddVectoredExceptionHandler(1, Some(veh));
        let _ = now_ms(); // start the clock
        let _ = SetProcessDPIAware();
        DPI.store(GetDpiForSystem() as i32, Ordering::Relaxed);
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        let (tx, rx) = channel();
        *TX.lock().unwrap() = Some(tx);
        std::thread::spawn(move || worker(rx));

        load_settings();
        refresh_monitors();

        let hinst = GetModuleHandleW(None).unwrap();

        let cls = w!("LiteBrightnessMain");
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinst.into(), lpszClassName: cls, ..Default::default() };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(WS_EX_TOOLWINDOW, cls, w!("Lite Brightness"), WS_POPUP, 0, 0, 0, 0, None, None, hinst, None).unwrap();
        MAIN_HWND.store(hwnd.0 as isize, Ordering::Relaxed);

        let ocls = w!("LiteBrightnessOsd");
        let owc = WNDCLASSW { lpfnWndProc: Some(osd_proc), hInstance: hinst.into(), lpszClassName: ocls, ..Default::default() };
        RegisterClassW(&owc);
        let osd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TRANSPARENT,
            ocls, w!(""), WS_POPUP, 0, 0, px(200), px(44), None, None, hinst, None,
        )
        .unwrap();
        let _ = SetLayeredWindowAttributes(osd, COLORREF(0), 235, LWA_ALPHA);
        OSD_HWND.store(osd.0 as isize, Ordering::Relaxed);

        let fcls = w!("LiteBrightnessFlyout");
        let fwc = WNDCLASSW {
            lpfnWndProc: Some(fly_proc),
            hInstance: hinst.into(),
            lpszClassName: fcls,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&fwc);
        let fly = CreateWindowExW(WS_EX_TOPMOST | WS_EX_TOOLWINDOW, fcls, w!(""), WS_POPUP, 0, 0, px(FLY_W), px(200), None, None, hinst, None).unwrap();
        FLY_HWND.store(fly.0 as isize, Ordering::Relaxed);

        let kcls = w!("LiteBrightnessKeys");
        let kwc = WNDCLASSW {
            lpfnWndProc: Some(keys_proc),
            hInstance: hinst.into(),
            lpszClassName: kcls,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&kwc);
        let keys = CreateWindowExW(
            WINDOW_EX_STYLE(0), kcls, w!("Lite Brightness - Key bindings"), WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            0, 0, px(KW), px(KH), None, None, hinst, None,
        )
        .unwrap();
        KEYS_HWND.store(keys.0 as isize, Ordering::Relaxed);

        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);

        let icon = make_icon();
        ICON.store(icon.0 as isize, Ordering::Relaxed);
        SendMessageW(keys, WM_SETICON, WPARAM(ICON_SMALL as usize), LPARAM(icon.0 as isize));
        SendMessageW(keys, WM_SETICON, WPARAM(ICON_BIG as usize), LPARAM(icon.0 as isize));
        if !Shell_NotifyIconW(NIM_ADD, &tray_data(hwnd, icon)).as_bool() {
            SetTimer(hwnd, TIMER_TRAY, 2000, None);
        }

        if DEVICE_MATCH.lock().unwrap().is_empty() {
            *DEVICE_MATCH.lock().unwrap() = "VID_0E6A".into();
        }
        let rid = [RAWINPUTDEVICE { usUsagePage: 0x0C, usUsage: 1, dwFlags: RIDEV_INPUTSINK, hwndTarget: hwnd }];
        let _ = RegisterRawInputDevices(&rid, std::mem::size_of::<RAWINPUTDEVICE>() as u32);

        if let Ok(h) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(kb_hook), hinst, 0) {
            HOOK.store(h.0 as isize, Ordering::Relaxed);
        }

        trace("running");
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        trace("exit: message loop ended");
    }
}
