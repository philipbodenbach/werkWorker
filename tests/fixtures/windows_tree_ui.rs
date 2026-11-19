//! An ordinary host observer changes only its own station/desktop. Clipboard
//! data is synthetic and confined to the fresh, noninteractive private station.
use super::{current_desktop_name, user_object_name, wide, write_json};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    System::{LibraryLoader::GetModuleHandleW, Memory::*, StationsAndDesktops::*, Threading::*},
    UI::WindowsAndMessaging::*,
};
#[link(name = "user32")]
unsafe extern "system" {
    #[link_name = "RegisterClipboardFormatW"]
    fn register_format(name: *const u16) -> u32;
    #[link_name = "OpenClipboard"]
    fn open_clipboard(owner: HWND) -> i32;
    #[link_name = "GetClipboardData"]
    fn get_clipboard(format: u32) -> HANDLE;
    #[link_name = "SetClipboardData"]
    fn set_clipboard(format: u32, memory: HANDLE) -> HANDLE;
    #[link_name = "EmptyClipboard"]
    fn empty_clipboard() -> i32;
    #[link_name = "CountClipboardFormats"]
    fn count_formats() -> i32;
    #[link_name = "CloseClipboard"]
    fn close_clipboard() -> i32;
}
pub const MESSAGE: u32 = WM_APP + 0x317;
struct Clipboard;
impl Clipboard {
    fn open(window: HWND) -> Self {
        assert_ne!(
            unsafe { open_clipboard(window) },
            0,
            "Open private clipboard: {}",
            std::io::Error::last_os_error()
        );
        Self
    }
}
impl Drop for Clipboard {
    fn drop(&mut self) {
        unsafe {
            close_clipboard();
        }
    }
}
struct Station {
    old_station: HWINSTA,
    old_desktop: HDESK,
    station: HWINSTA,
    desktop: HDESK,
}
impl Station {
    fn open(qualified: &str) -> Self {
        let (station, desktop) = qualified.split_once('\\').unwrap();
        let old_station = unsafe { GetProcessWindowStation() };
        assert!(!station.eq_ignore_ascii_case(&user_object_name(old_station)));
        assert!(
            !station.eq_ignore_ascii_case("WinSta0"),
            "Never use the user's interactive clipboard"
        );
        let mut scope = Self {
            old_station,
            old_desktop: unsafe { GetThreadDesktop(GetCurrentThreadId()) },
            station: std::ptr::null_mut(),
            desktop: std::ptr::null_mut(),
        };
        scope.station =
            unsafe { OpenWindowStationW(wide(station).as_ptr(), 0, WINSTA_ALL_ACCESS as u32) };
        assert!(
            !scope.station.is_null(),
            "Open observer private station: {}",
            std::io::Error::last_os_error()
        );
        assert_eq!(user_object_name(scope.station), station);
        let mut flags: USEROBJECTFLAGS = unsafe { std::mem::zeroed() };
        let mut needed = 0;
        assert_ne!(
            unsafe {
                GetUserObjectInformationW(
                    scope.station,
                    UOI_FLAGS,
                    (&mut flags as *mut USEROBJECTFLAGS).cast(),
                    std::mem::size_of::<USEROBJECTFLAGS>() as u32,
                    &mut needed,
                )
            },
            0
        );
        assert_eq!(
            flags.dwFlags & WSF_VISIBLE as u32,
            0,
            "Clipboard test requires noninteractive private station"
        );
        assert_ne!(unsafe { SetProcessWindowStation(scope.station) }, 0);
        scope.desktop = unsafe {
            OpenDesktopW(
                wide(desktop).as_ptr(),
                0,
                0,
                DESKTOP_READOBJECTS | DESKTOP_CREATEWINDOW | DESKTOP_WRITEOBJECTS,
            )
        };
        assert!(
            !scope.desktop.is_null(),
            "Open observer private desktop: {}",
            std::io::Error::last_os_error()
        );
        assert_ne!(unsafe { SetThreadDesktop(scope.desktop) }, 0);
        assert_eq!(current_desktop_name(), qualified);
        scope
    }
    fn close(&mut self) -> bool {
        let mut success = true;
        if !self.station.is_null() {
            success &= unsafe { SetProcessWindowStation(self.old_station) } != 0;
            success &= unsafe { SetThreadDesktop(self.old_desktop) } != 0;
        }
        if !self.desktop.is_null() {
            success &= unsafe { CloseDesktop(self.desktop) } != 0;
            self.desktop = std::ptr::null_mut();
        }
        if !self.station.is_null() {
            success &= unsafe { CloseWindowStation(self.station) } != 0;
            self.station = std::ptr::null_mut();
        }
        success
    }
}
impl Drop for Station {
    fn drop(&mut self) {
        self.close();
    }
}
struct CanaryState {
    nonce: usize,
    calls: AtomicUsize,
}
unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == MESSAGE {
        let state = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *const CanaryState;
        if !state.is_null() && wparam == unsafe { (*state).nonce } {
            unsafe {
                (*state).calls.fetch_add(1, Ordering::SeqCst);
            }
            return wparam as isize;
        }
    }
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}
struct Canary {
    window: HWND,
    class: Vec<u16>,
    instance: HINSTANCE,
    state: Box<CanaryState>,
}
impl Canary {
    fn new() -> Self {
        let class = wide(&format!("WerkTreeObserver-{}", uuid::Uuid::new_v4()));
        let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
        let state = Box::new(CanaryState {
            nonce: (uuid::Uuid::new_v4().as_u128() as usize) & 0x7fff_ffff,
            calls: AtomicUsize::new(0),
        });
        let mut definition: WNDCLASSW = unsafe { std::mem::zeroed() };
        definition.hInstance = instance;
        definition.lpfnWndProc = Some(window_proc);
        definition.lpszClassName = class.as_ptr();
        assert_ne!(unsafe { RegisterClassW(&definition) }, 0);
        let window = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                0,
                0,
                0,
                1,
                1,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                std::ptr::null(),
            )
        };
        assert!(!window.is_null());
        unsafe {
            SetWindowLongPtrW(
                window,
                GWLP_USERDATA,
                (&*state as *const CanaryState) as isize,
            );
        }
        let mut owner = 0;
        assert_ne!(unsafe { GetWindowThreadProcessId(window, &mut owner) }, 0);
        assert_eq!(owner, std::process::id());
        Self {
            window,
            class,
            instance,
            state,
        }
    }
    fn pump(&self) {
        let mut message: MSG = unsafe { std::mem::zeroed() };
        while unsafe { PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
    fn control(&self) {
        let before = self.state.calls.load(Ordering::SeqCst);
        let mut response = 0;
        assert_ne!(
            unsafe {
                SendMessageTimeoutW(
                    self.window,
                    MESSAGE,
                    self.state.nonce,
                    0,
                    SMTO_BLOCK | SMTO_ABORTIFHUNG,
                    2000,
                    &mut response,
                )
            },
            0
        );
        assert_eq!(response, self.state.nonce);
        assert_eq!(self.state.calls.load(Ordering::SeqCst), before + 1);
    }
    fn wait(&self, path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !path.is_file() {
            self.pump();
            assert!(
                Instant::now() < deadline,
                "Observer did not receive {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        self.pump();
    }
    fn close(&mut self) -> bool {
        if self.window.is_null() {
            return true;
        }
        let success = unsafe { DestroyWindow(self.window) } != 0;
        self.window = std::ptr::null_mut();
        success && unsafe { UnregisterClassW(self.class.as_ptr(), self.instance) } != 0
    }
}
impl Drop for Canary {
    fn drop(&mut self) {
        self.close();
    }
}
fn seed(window: HWND, format: u32, bytes: &[u8], require_empty: bool) {
    let _clipboard = Clipboard::open(window);
    if require_empty {
        assert_eq!(
            unsafe { count_formats() },
            0,
            "Refuse to overwrite a nonempty supposedly fresh clipboard"
        );
    }
    assert_ne!(unsafe { empty_clipboard() }, 0);
    let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) };
    assert!(!memory.is_null());
    let pointer = unsafe { GlobalLock(memory) };
    assert!(!pointer.is_null());
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast::<u8>(), bytes.len());
        GlobalUnlock(memory);
    }
    let accepted = unsafe { set_clipboard(format, memory) };
    if accepted.is_null() {
        unsafe {
            GlobalFree(memory);
        }
    }
    assert!(
        !accepted.is_null(),
        "Seed synthetic private clipboard: {}",
        std::io::Error::last_os_error()
    );
}
fn matches_marker(window: HWND, format: u32, bytes: &[u8]) -> bool {
    let _clipboard = Clipboard::open(window);
    let memory = unsafe { get_clipboard(format) };
    if memory.is_null() || unsafe { GlobalSize(memory) } < bytes.len() {
        return false;
    }
    let pointer = unsafe { GlobalLock(memory) };
    if pointer.is_null() {
        return false;
    }
    let equal = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), bytes.len()) } == bytes;
    unsafe {
        GlobalUnlock(memory);
    }
    equal
}
pub fn observer() {
    let directory = std::path::PathBuf::from(std::env::var_os("WW_TREE_UI_DIRECTORY").unwrap());
    let qualified = std::env::var("WW_TREE_UI_DESKTOP").unwrap();
    let marker = format!(
        "synthetic private clipboard {}",
        std::env::var("WW_TREE_UI_NONCE").unwrap()
    );
    let mut station = Station::open(&qualified);
    // Host CreateDesktop is a positive control on this same private station.
    let control = unsafe {
        CreateDesktopW(
            wide(&format!("observer-control-{}", uuid::Uuid::new_v4())).as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            DESKTOP_CREATEWINDOW | DESKTOP_READOBJECTS | DESKTOP_WRITEOBJECTS,
            std::ptr::null(),
        )
    };
    assert!(!control.is_null());
    assert_ne!(unsafe { SetThreadDesktop(station.desktop) }, 0);
    assert_ne!(unsafe { CloseDesktop(control) }, 0);
    let mut canary = Canary::new();
    canary.control();
    let baseline = canary.state.calls.load(Ordering::SeqCst);
    let format = unsafe {
        register_format(wide(&format!("WerkTreeSynthetic-{}", uuid::Uuid::new_v4())).as_ptr())
    };
    assert_ne!(format, 0);
    seed(canary.window, format, marker.as_bytes(), true);
    assert!(matches_marker(canary.window, format, marker.as_bytes()));
    write_json(
        directory.join("ui-observer-ready.json"),
        json!({"pid":std::process::id(),
        "created":super::birth(unsafe { GetCurrentProcess() }),"desktop":qualified,"window":canary.window as usize,
        "nonce":canary.state.nonce,"clipboard_format":format,"baseline_calls":baseline,
        "synthetic_clipboard_read_write_control":true,"private_station_noninteractive":true}),
    );
    canary.wait(&directory.join("ui-observer-check"));
    let after = canary.state.calls.load(Ordering::SeqCst);
    let unchanged = matches_marker(canary.window, format, marker.as_bytes());
    canary.control();
    seed(canary.window, format, marker.as_bytes(), false);
    let second_control = matches_marker(canary.window, format, marker.as_bytes());
    write_json(
        directory.join("ui-observer-result.json"),
        json!({"pid":std::process::id(),"desktop":qualified,
        "baseline_calls":baseline,"calls_after_payload":after,"clipboard_unchanged":unchanged,
        "synthetic_clipboard_read_write_control_after":second_control}),
    );
    assert!(canary.close(), "Observer window cleanup failed");
    assert!(station.close(), "Observer station/desktop cleanup failed");
}
pub fn clipboard_attempt(format: u32, expected_desktop: &str) -> Value {
    assert_eq!(current_desktop_name(), expected_desktop);
    let station = unsafe { GetProcessWindowStation() };
    assert!(!user_object_name(station).eq_ignore_ascii_case("WinSta0"));
    unsafe {
        SetLastError(0);
    }
    let opened = unsafe { open_clipboard(std::ptr::null_mut()) } != 0;
    let open_error = unsafe { GetLastError() };
    let _clipboard = opened.then_some(Clipboard);
    unsafe {
        SetLastError(0);
    }
    let handle = unsafe { get_clipboard(format) };
    let read_error = unsafe { GetLastError() };
    // No dereference on the restricted side, including an unexpected success.
    unsafe {
        SetLastError(0);
    }
    let cleared = unsafe { empty_clipboard() };
    let write_error = unsafe { GetLastError() };
    json!({"opened":opened,"open_error":open_error,"read_handle_null":handle.is_null(),"read_error":read_error,
        "empty_succeeded":cleared != 0,"empty_error":write_error,"write_operation":"EmptyClipboard"})
}
