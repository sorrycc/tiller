//! Browser registry and CEF handlers. Everything here runs on the CEF UI
//! thread, which on macOS is the main thread.

use crate::ipc;
use cef::*;
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ffi::{CString, c_char, c_void},
    sync::OnceLock,
};

/// Mirrors `TillerBrowserCallbacks` in tiller_core.h.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Callbacks {
    pub ctx: *mut c_void,
    pub address_changed: Option<unsafe extern "C" fn(*mut c_void, *const c_char)>,
    pub title_changed: Option<unsafe extern "C" fn(*mut c_void, *const c_char)>,
    pub loading_state_changed: Option<unsafe extern "C" fn(*mut c_void, bool, bool, bool)>,
    pub favicon_changed: Option<unsafe extern "C" fn(*mut c_void, *const u8, usize)>,
    pub open_tab: Option<unsafe extern "C" fn(*mut c_void, *const c_char, bool)>,
    pub close_ready: Option<unsafe extern "C" fn(*mut c_void)>,
    pub key_equivalent: Option<unsafe extern "C" fn(*mut c_void, *mut c_void) -> bool>,
    pub loading_progress: Option<unsafe extern "C" fn(*mut c_void, f64)>,
    pub find_result: Option<unsafe extern "C" fn(*mut c_void, i32, i32, bool)>,
    pub auto_resize: Option<unsafe extern "C" fn(*mut c_void, i32, i32)>,
}

struct Entry {
    browser: Browser,
    callbacks: Option<Callbacks>,
    /// Keeps the DevTools observer attached. Added on the first DevTools call.
    devtools: Option<Registration>,
}

/// A DevTools call waiting for its result.
struct PendingCall {
    browser_id: i32,
    token: u64,
}

/// Extension folders for `--load-extension`, comma-separated. Set once before
/// CEF initializes.
pub static EXTENSIONS: OnceLock<String> = OnceLock::new();

thread_local! {
    static BROWSERS: RefCell<HashMap<i32, Entry>> = RefCell::new(HashMap::new());
    static DEVTOOLS_CALLS: RefCell<HashMap<i32, PendingCall>> = RefCell::new(HashMap::new());
    static NEXT_MESSAGE_ID: Cell<i32> = const { Cell::new(1) };
}

pub fn get(id: i32) -> Option<Browser> {
    BROWSERS.with_borrow(|map| map.get(&id).map(|e| e.browser.clone()))
}

fn callbacks_for(browser: Option<&mut Browser>) -> Option<Callbacks> {
    callbacks_for_id(browser?.identifier())
}

fn callbacks_for_id(id: i32) -> Option<Callbacks> {
    BROWSERS.with_borrow(|map| map.get(&id).and_then(|e| e.callbacks))
}

fn to_cstring(s: Option<&CefString>) -> CString {
    let s = s.map(|s| s.to_string()).unwrap_or_default();
    CString::new(s.replace('\0', "")).unwrap_or_default()
}

pub fn create(parent_view: *mut c_void, width: i32, height: i32, url: &str, callbacks: Callbacks) -> i32 {
    let window_info = WindowInfo {
        parent_view,
        bounds: Rect { x: 0, y: 0, width, height },
        runtime_style: RuntimeStyle::ALLOY,
        ..Default::default()
    };
    let mut client = TillerClient::new();
    let Some(browser) = browser_host_create_browser_sync(
        Some(&window_info),
        Some(&mut client),
        Some(&CefString::from(url)),
        Some(&BrowserSettings::default()),
        None,
        None,
    ) else {
        return -1;
    };
    let id = browser.identifier();
    BROWSERS.with_borrow_mut(|map| map.insert(id, Entry { browser, callbacks: Some(callbacks), devtools: None }));
    id
}

/// Stops callbacks into the Swift side, whose context may be freed soon.
pub fn detach(id: i32) {
    BROWSERS.with_borrow_mut(|map| {
        if let Some(entry) = map.get_mut(&id) {
            entry.callbacks = None;
        }
    });
}

/// Starts closing one browser. The page's beforeunload runs first, then
/// `close_ready` fires and the Swift side removes the tab's view.
pub fn close(id: i32) {
    if let Some(host) = get(id).and_then(|b| b.host()) {
        host.close_browser(0);
    }
}

/// Starts closing every browser. Each one runs its beforeunload handlers, then
/// asks its window to close. Quits right away when no browser exists.
pub fn close_all() {
    // Collect first: closing can call back into handlers that borrow the map.
    let browsers: Vec<Browser> = BROWSERS.with_borrow(|map| map.values().map(|e| e.browser.clone()).collect());
    if browsers.is_empty() {
        quit_message_loop();
        return;
    }
    for browser in browsers {
        if let Some(host) = browser.host() {
            host.close_browser(0);
        }
    }
}

/// Sends one DevTools protocol command to a tab and replies to the control
/// socket request `token` with `{"result": ...}` or `{"error": ...}`.
pub fn devtools_call(id: i32, method: &str, params: Value, token: u64) {
    let Some(host) = get(id).and_then(|b| b.host()) else {
        return ipc::reply_error(token, format!("no tab with id {id}"));
    };
    let attached = BROWSERS.with_borrow_mut(|map| {
        let Some(entry) = map.get_mut(&id) else { return false };
        if entry.devtools.is_none() {
            let mut observer = TillerDevToolsObserver::new();
            entry.devtools = host.add_dev_tools_message_observer(Some(&mut observer));
        }
        entry.devtools.is_some()
    });
    if !attached {
        return ipc::reply_error(token, "could not attach to the tab's DevTools agent");
    }

    let message_id = NEXT_MESSAGE_ID.get();
    NEXT_MESSAGE_ID.set(message_id.wrapping_add(1).max(1));
    DEVTOOLS_CALLS.with_borrow_mut(|calls| calls.insert(message_id, PendingCall { browser_id: id, token }));
    let message = json!({ "id": message_id, "method": method, "params": params }).to_string();
    if host.send_dev_tools_message(Some(message.as_bytes())) == 0 {
        DEVTOOLS_CALLS.with_borrow_mut(|calls| calls.remove(&message_id));
        ipc::reply_error(token, "DevTools message was rejected");
    }
}

/// Fails every DevTools call still waiting on a browser that is going away.
fn fail_devtools_calls(browser_id: i32) {
    let tokens: Vec<u64> = DEVTOOLS_CALLS.with_borrow_mut(|calls| {
        let ids: Vec<i32> = calls.iter().filter(|(_, c)| c.browser_id == browser_id).map(|(id, _)| *id).collect();
        ids.iter().filter_map(|id| calls.remove(id)).map(|c| c.token).collect()
    });
    for token in tokens {
        ipc::reply_error(token, "the tab closed");
    }
}

wrap_dev_tools_message_observer! {
    struct TillerDevToolsObserver;

    impl DevToolsMessageObserver {
        /// Answers the matching call. Events and replies to anyone else's calls
        /// are left for CEF's default handling.
        fn on_dev_tools_message(&self, _browser: Option<&mut Browser>, message: Option<&[u8]>) -> i32 {
            let Some(message) = message.and_then(|m| serde_json::from_slice::<Value>(m).ok()) else { return 0 };
            let Some(id) = message["id"].as_i64() else { return 0 };
            let Some(call) = DEVTOOLS_CALLS.with_borrow_mut(|calls| calls.remove(&(id as i32))) else { return 0 };
            let reply = match message.get("error") {
                Some(error) => json!({ "error": error["message"].as_str().unwrap_or("DevTools error") }),
                None => json!({ "result": message.get("result").cloned().unwrap_or_else(|| json!({})) }),
            };
            ipc::reply(call.token, reply);
            1
        }
    }
}

wrap_client! {
    struct TillerClient;

    impl Client {
        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(TillerDisplayHandler::new())
        }

        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(TillerLifeSpanHandler::new())
        }

        fn keyboard_handler(&self) -> Option<KeyboardHandler> {
            Some(TillerKeyboardHandler::new())
        }

        fn load_handler(&self) -> Option<LoadHandler> {
            Some(TillerLoadHandler::new())
        }

        fn find_handler(&self) -> Option<FindHandler> {
            Some(TillerFindHandler::new())
        }
    }
}

wrap_find_handler! {
    struct TillerFindHandler;

    impl FindHandler {
        fn on_find_result(
            &self,
            browser: Option<&mut Browser>,
            _identifier: i32,
            count: i32,
            _selection_rect: Option<&Rect>,
            active_match_ordinal: i32,
            final_update: i32,
        ) {
            if let Some(cb) = callbacks_for(browser) && let Some(f) = cb.find_result {
                unsafe { f(cb.ctx, count, active_match_ordinal, final_update != 0) };
            }
        }
    }
}

wrap_display_handler! {
    struct TillerDisplayHandler;

    impl DisplayHandler {
        fn on_address_change(&self, browser: Option<&mut Browser>, frame: Option<&mut Frame>, url: Option<&CefString>) {
            if frame.is_none_or(|f| f.is_main() == 0) {
                return;
            }
            if let Some(cb) = callbacks_for(browser) && let Some(f) = cb.address_changed {
                let url = to_cstring(url);
                unsafe { f(cb.ctx, url.as_ptr()) };
            }
        }

        fn on_favicon_urlchange(&self, browser: Option<&mut Browser>, icon_urls: Option<&mut CefStringList>) {
            let Some(browser) = browser else { return };
            let first = icon_urls.and_then(first_string);
            let Some(url) = first else {
                send_favicon(browser.identifier(), &[]);
                return;
            };
            if let Some(host) = browser.host() {
                let mut callback = TillerFaviconCallback::new(browser.identifier(), page_origin(browser));
                host.download_image(Some(&CefString::from(url.as_str())), 1, 64, 0, Some(&mut callback));
            }
        }

        fn on_title_change(&self, browser: Option<&mut Browser>, title: Option<&CefString>) {
            if let Some(cb) = callbacks_for(browser) && let Some(f) = cb.title_changed {
                let title = to_cstring(title);
                unsafe { f(cb.ctx, title.as_ptr()) };
            }
        }

        fn on_loading_progress_change(&self, browser: Option<&mut Browser>, progress: f64) {
            if let Some(cb) = callbacks_for(browser) && let Some(f) = cb.loading_progress {
                unsafe { f(cb.ctx, progress) };
            }
        }

        /// The page's size in points, for browsers with auto-resize on.
        fn on_auto_resize(&self, browser: Option<&mut Browser>, new_size: Option<&Size>) -> i32 {
            let Some(size) = new_size else { return 0 };
            match callbacks_for(browser) {
                Some(Callbacks { ctx, auto_resize: Some(f), .. }) => {
                    unsafe { f(ctx, size.width, size.height) };
                    1
                }
                _ => 0,
            }
        }
    }
}

/// First entry of a list CEF lends to a callback. The crate's `Clone` and
/// `IntoIterator` for a borrowed list lose its contents, so read it directly.
fn first_string(list: &mut CefStringList) -> Option<String> {
    let raw: *mut sys::_cef_string_list_t = list.into();
    if raw.is_null() || unsafe { sys::cef_string_list_size(raw) } == 0 {
        return None;
    }
    let mut value: sys::cef_string_t = unsafe { std::mem::zeroed() };
    if unsafe { sys::cef_string_list_value(raw, 0, &mut value) } == 0 {
        return None;
    }
    let s = CefString::from(std::ptr::from_ref(&value)).to_string();
    unsafe { sys::cef_string_utf16_clear(&mut value) };
    Some(s)
}

/// Scheme, host and port of the page the browser shows, such as
/// "https://example.com".
fn page_origin(browser: &Browser) -> String {
    let url = browser.main_frame().map(|f| CefString::from(&f.url()).to_string()).unwrap_or_default();
    let start = url.find("://").map_or(0, |i| i + 3);
    let end = url[start..].find('/').map_or(url.len(), |i| start + i);
    url[..end].to_string()
}

fn send_favicon(id: i32, png: &[u8]) {
    if let Some(cb) = callbacks_for_id(id) && let Some(f) = cb.favicon_changed {
        unsafe { f(cb.ctx, png.as_ptr(), png.len()) };
    }
}

wrap_download_image_callback! {
    struct TillerFaviconCallback {
        browser_id: i32,
        // The site the icon belongs to. An icon that arrives after the tab
        // went to another site is dropped, so it can't be shown or saved
        // as that site's.
        origin: String,
    }

    impl DownloadImageCallback {
        fn on_download_image_finished(&self, _image_url: Option<&CefString>, _http_status_code: i32, image: Option<&mut Image>) {
            if get(self.browser_id).is_none_or(|b| page_origin(&b) != self.origin) {
                return;
            }
            // CEF returns nothing unless both size out-parameters are given.
            let (mut width, mut height) = (0, 0);
            let png = image.and_then(|image| image.as_png(2.0, 1, Some(&mut width), Some(&mut height)));
            match png {
                Some(png) if png.size() > 0 => {
                    let bytes = unsafe { std::slice::from_raw_parts(png.raw_data().cast::<u8>(), png.size()) };
                    send_favicon(self.browser_id, bytes);
                }
                _ => send_favicon(self.browser_id, &[]),
            }
        }
    }
}

wrap_keyboard_handler! {
    struct TillerKeyboardHandler;

    impl KeyboardHandler {
        /// Gives the menu bar first pick of Command and Control shortcuts, so
        /// Cmd+W, Cmd+R, Cmd+[ and the rest work while a page has focus. The
        /// Swift side leaves Edit menu keys alone so pages still get Cmd+Z etc.
        fn on_pre_key_event(
            &self,
            browser: Option<&mut Browser>,
            event: Option<&KeyEvent>,
            os_event: *mut u8,
            _is_keyboard_shortcut: Option<&mut i32>,
        ) -> i32 {
            let Some(event) = event else { return 0 };
            let modifiers = (sys::cef_event_flags_t::EVENTFLAG_COMMAND_DOWN.0 | sys::cef_event_flags_t::EVENTFLAG_CONTROL_DOWN.0) as u32;
            if event.type_ != KeyEventType::RAWKEYDOWN || event.modifiers & modifiers == 0 || os_event.is_null() {
                return 0;
            }
            match callbacks_for(browser) {
                Some(Callbacks { ctx, key_equivalent: Some(f), .. }) => unsafe { f(ctx, os_event.cast()) }.into(),
                _ => 0,
            }
        }
    }
}

wrap_load_handler! {
    struct TillerLoadHandler;

    impl LoadHandler {
        fn on_loading_state_change(&self, browser: Option<&mut Browser>, is_loading: i32, can_go_back: i32, can_go_forward: i32) {
            if let Some(cb) = callbacks_for(browser) && let Some(f) = cb.loading_state_changed {
                unsafe { f(cb.ctx, is_loading != 0, can_go_back != 0, can_go_forward != 0) };
            }
        }
    }
}

wrap_life_span_handler! {
    struct TillerLifeSpanHandler;

    impl LifeSpanHandler {
        /// Keep script popups native so OAuth can post its result to the opener.
        /// Ordinary new-window links still open as Tiller tabs.
        fn on_before_popup(
            &self,
            browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: i32,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            target_disposition: WindowOpenDisposition,
            _user_gesture: i32,
            _popup_features: Option<&PopupFeatures>,
            window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut i32>,
        ) -> i32 {
            if target_disposition == WindowOpenDisposition::NEW_POPUP {
                if let Some(info) = window_info {
                    info.parent_view = std::ptr::null_mut();
                    info.runtime_style = RuntimeStyle::ALLOY;
                }
                // Let CEF create the popup, preserving opener and request context.
                return 0;
            }
            if let Some(cb) = callbacks_for(browser) && let Some(f) = cb.open_tab {
                let url = to_cstring(target_url);
                let background = target_disposition == WindowOpenDisposition::NEW_BACKGROUND_TAB;
                unsafe { f(cb.ctx, url.as_ptr(), background) };
            }
            1
        }

        /// A tab closing must not close the window, so instead of letting CEF
        /// send performClose: to it, tell Swift to remove the tab's view. Tearing
        /// down that view finishes the close and leads to `on_before_close`.
        fn do_close(&self, browser: Option<&mut Browser>) -> i32 {
            let Some(browser) = browser else { return 0 };
            let is_tab = BROWSERS.with_borrow(|map| map.contains_key(&browser.identifier()));
            if is_tab {
                if let Some(cb) = callbacks_for(Some(browser)) && let Some(f) = cb.close_ready {
                    unsafe { f(cb.ctx) };
                }
                return 1;
            }
            // Native popups have no Swift tab; CEF owns their windows.
            0
        }

        fn on_before_close(&self, browser: Option<&mut Browser>) {
            let Some(browser) = browser else { return };
            let id = browser.identifier();
            if !BROWSERS.with_borrow(|map| map.contains_key(&id)) {
                return; // A native popup must not affect the tab registry or app lifetime.
            }
            fail_devtools_calls(id);
            let empty = BROWSERS.with_borrow_mut(|map| {
                map.remove(&id);
                map.is_empty()
            });
            // One window for now, so the last tab closing quits the app.
            if empty {
                quit_message_loop();
            }
        }
    }
}

wrap_app! {
    pub struct TillerApp;

    impl App {
        fn on_before_command_line_processing(&self, process_type: Option<&CefString>, command_line: Option<&mut CommandLine>) {
            let is_browser = process_type.is_none_or(|t| t.to_string().is_empty());
            if let (true, Some(cmd)) = (is_browser, command_line) {
                // Keeps Chromium from asking for the login keychain password to
                // encrypt cookies. Cookies are stored with a fixed key instead.
                cmd.append_switch(Some(&CefString::from("use-mock-keychain")));
                // A window covered by other windows would otherwise count as
                // hidden, and Chromium drops input to hidden pages, so the
                // agent's clicks and keys would vanish while the user works
                // in another app. The cost is that covered windows keep drawing.
                cmd.append_switch(Some(&CefString::from("disable-backgrounding-occluded-windows")));
                // The profile's enabled extensions, loaded unpacked like
                // Chrome's Load unpacked. Chromium only reads this at startup.
                // An extension that fails to load would otherwise get an error
                // dialog, which hangs startup without Chrome's UI; the error
                // goes to chrome_debug.log instead.
                if let Some(paths) = EXTENSIONS.get().filter(|p| !p.is_empty()) {
                    cmd.append_switch_with_value(
                        Some(&CefString::from("load-extension")),
                        Some(&CefString::from(paths.as_str())),
                    );
                    cmd.append_switch(Some(&CefString::from("noerrdialogs")));
                }
            }
        }
    }
}
