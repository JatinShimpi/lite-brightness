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

// dial button actions
const BTN_CYCLE: u8 = 0;
const BTN_LOCK: u8 = 1;
const BTN_OFF: u8 = 2;
const BTN_LOCK_OFF: u8 = 3;
const BTN_NONE: u8 = 4;

struct Settings {
    dial: bool,
    button: u8,
    ext_only: bool, // only react to the external keyboard's dial
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
    s: Settings { dial: true, button: BTN_CYCLE, ext_only: true },
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

fn load_settings() {
    let Ok(text) = std::fs::read_to_string(settings_path()) else { return };
    let mut st = lock();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        match k.trim() {
            "dial" => st.s.dial = v.trim() == "1",
            "button" => st.s.button = v.trim().parse().unwrap_or(BTN_CYCLE).min(BTN_NONE),
            "ext_only" => st.s.ext_only = v.trim() == "1",
            "device" => *DEVICE_MATCH.lock().unwrap_or_else(|e| e.into_inner()) = v.trim().to_uppercase(),
            _ => {}
        }
    }
}

fn save_settings() {
    let st = lock();
    let device = DEVICE_MATCH.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let text = format!("dial={}\nbutton={}\next_only={}\ndevice={}\n", st.s.dial as u8, st.s.button, st.s.ext_only as u8, device);
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

unsafe extern "system" fn kb_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    LAST_WHO.store(4, Ordering::Relaxed);
    LAST_MSG.store(0x1000, Ordering::Relaxed);
    if code == HC_ACTION as i32 {
        let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
        // the keyboard driver delivers its media keys as injected, so do not filter those
        {
            let from_ext = {
                let st = lock();
                !st.s.ext_only || {
                    let at = EXT_AT_MS.load(Ordering::Relaxed);
                    at != 0 && now_ms().saturating_sub(at) <= EXT_WINDOW_MS
                }
            };
            let action = {
                let st = lock();
                if !from_ext {
                    0
                } else {
                match VIRTUAL_KEY(k.vkCode as u16) {
                    VK_VOLUME_UP if st.s.dial && st.target != TARGET_VOLUME => 1,
                    VK_VOLUME_DOWN if st.s.dial && st.target != TARGET_VOLUME => 2,
                    VK_VOLUME_MUTE if st.s.button != BTN_NONE => 3,
                    _ => 0,
                }
                }
            };
            if action != 0 {
                if wp.0 as u32 == WM_KEYDOWN || wp.0 as u32 == WM_SYSKEYDOWN {
                    LAST_MSG.store(0x1001, Ordering::Relaxed);
                    let _ = PostMessageW(main_hwnd(), WM_DIAL, WPARAM(action), LPARAM(0));
                }
                return LRESULT(1); // swallow so Windows volume doesn't change
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

unsafe fn handle_dial(_hwnd: HWND, kind: usize) {
    match kind {
        1 | 2 => {
            let delta = if kind == 1 { STEP } else { -STEP };
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
        3 => {
            let action = lock().s.button;
            match action {
                BTN_CYCLE => {
                    let (name, pct, sync) = {
                        let mut st = lock();
                        let usable = |m: &Mon| m.ok && !m.off;
                        // order: external displays, laptop panel, all displays, volume
                        let mut order: Vec<i32> = (0..st.mons.len()).filter(|&i| usable(&st.mons[i]) && !st.mons[i].internal).map(|i| i as i32).collect();
                        order.extend((0..st.mons.len()).filter(|&i| usable(&st.mons[i]) && st.mons[i].internal).map(|i| i as i32));
                        if order.len() > 1 {
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
                            TARGET_ALL => ("All displays".to_string(), sync_all(&mut st).unwrap_or(-1)),
                            i => (st.mons[i as usize].name.clone(), st.mons[i as usize].pct),
                        };
                        (name, pct, next == TARGET_ALL)
                    };
                    show_osd(format!("Controlling: {}", name), pct);
                    if sync {
                        schedule_apply();
                    }
                    let _ = InvalidateRect(fly_hwnd(), None, FALSE);
                }
                BTN_LOCK => {
                    let _ = LockWorkStation();
                }
                BTN_OFF => screens_off(),
                BTN_LOCK_OFF => {
                    let _ = LockWorkStation();
                    screens_off();
                }
                _ => {}
            }
        }
        _ => {}
    }
}

// ---------- tray menu (right-click) ----------

const ID_DIAL: usize = 10;
const ID_EXT_ONLY: usize = 11;
const ID_BTN: usize = 20; // 20..=24
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
    let (dial, button, target, mons) = {
        let st = lock();
        let mons: Vec<(String, bool, bool, i32)> = st.mons.iter().map(|m| (m.name.clone(), m.off, m.ok, m.pct)).collect();
        (st.s.dial, st.s.button, st.target, mons)
    };
    let startup = startup_enabled();

    let menu = CreatePopupMenu().unwrap();
    add(menu, chk(dial), ID_DIAL, "Dial controls brightness");
    add(menu, chk(lock().s.ext_only), ID_EXT_ONLY, "Only the external keyboard's dial");

    let btn = CreatePopupMenu().unwrap();
    for (i, name) in ["Switch display", "Lock PC", "Turn screens off", "Lock PC + turn screens off", "Do nothing"].iter().enumerate() {
        add(btn, chk(button == i as u8), ID_BTN + i, name);
    }
    let t = wide("Dial button action");
    let _ = AppendMenuW(menu, MF_POPUP | MF_STRING, btn.0 as usize, PCWSTR(t.as_ptr()));

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
        c if (ID_BTN..ID_BTN + 5).contains(&c) => {
            lock().s.button = (c - ID_BTN) as u8;
            save_settings();
        }
        ID_TARGET_ALL => {
            let synced = {
                let mut st = lock();
                st.target = TARGET_ALL;
                sync_all(&mut st).is_some()
            };
            if synced {
                schedule_apply();
            }
        }
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
            handle_dial(hwnd, wp.0);
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

        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);

        let icon = make_icon();
        ICON.store(icon.0 as isize, Ordering::Relaxed);
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
