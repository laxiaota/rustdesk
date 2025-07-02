use std::{
    collections::HashMap,
    iter::FromIterator,
    sync::{Arc, Mutex},
};

use sciter::Value;

use hbb_common::{
    allow_err,
    config::{LocalConfig, PeerConfig},
    log,
};

#[cfg(not(any(feature = "flutter", feature = "cli")))]
use crate::ui_session_interface::Session;
use crate::{common::get_app_name, ipc, ui_interface::*};

mod cm;
#[cfg(feature = "inline")]
pub mod inline;
pub mod remote;

#[allow(dead_code)]
type Status = (i32, bool, i64, String);

lazy_static::lazy_static! {
    // stupid workaround for https://sciter.com/forums/topic/crash-on-latest-tis-mac-sdk-sometimes/
    static ref STUPID_VALUES: Mutex<Vec<Arc<Vec<Value>>>> = Default::default();
}

#[cfg(not(any(feature = "flutter", feature = "cli")))]
lazy_static::lazy_static! {
    pub static ref CUR_SESSION: Arc<Mutex<Option<Session<remote::SciterHandler>>>> = Default::default();
}

struct UIHostHandler;

pub fn start(args: &mut [String]) {
    #[cfg(target_os = "macos")]
    crate::platform::delegate::show_dock();
    #[cfg(all(target_os = "linux", feature = "inline"))]
    {
        let app_dir = std::env::var("APPDIR").unwrap_or("".to_string());
        let mut so_path = "/usr/share/rustdesk/libsciter-gtk.so".to_owned();
        for (prefix, dir) in [
            ("", "/usr"),
            ("", "/app"),
            (&app_dir, "/usr"),
            (&app_dir, "/app"),
        ]
        .iter()
        {
            let path = format!("{prefix}{dir}/share/rustdesk/libsciter-gtk.so");
            if std::path::Path::new(&path).exists() {
                so_path = path;
                break;
            }
        }
        sciter::set_library(&so_path).ok();
    }
    #[cfg(windows)]
    // Check if there is a sciter.dll nearby.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let sciter_dll_path = parent.join("sciter.dll");
            if sciter_dll_path.exists() {
                // Try to set the sciter dll.
                let p = sciter_dll_path.to_string_lossy().to_string();
                log::debug!("Found dll:{}, \n {:?}", p, sciter::set_library(&p));
            }
        }
    }
    // https://github.com/c-smile/sciter-sdk/blob/master/include/sciter-x-types.h
    // https://github.com/rustdesk/rustdesk/issues/132#issuecomment-886069737
    #[cfg(windows)]
    allow_err!(sciter::set_options(sciter::RuntimeOptions::GfxLayer(
        sciter::GFX_LAYER::WARP
    )));
    use sciter::SCRIPT_RUNTIME_FEATURES::*;
    allow_err!(sciter::set_options(sciter::RuntimeOptions::ScriptFeatures(
        ALLOW_FILE_IO as u8 | ALLOW_SOCKET_IO as u8 | ALLOW_EVAL as u8 | ALLOW_SYSINFO as u8
    )));
    let mut frame = sciter::WindowBuilder::main_window().create();
    #[cfg(windows)]
    allow_err!(sciter::set_options(sciter::RuntimeOptions::UxTheming(true)));
    frame.set_title(&crate::get_app_name());
    #[cfg(target_os = "macos")]
    crate::platform::delegate::make_menubar(frame.get_host(), args.is_empty());
    #[cfg(windows)]
    crate::platform::try_set_window_foreground(frame.get_hwnd() as _);
    let page;
    if args.len() > 1 && args[0] == "--play" {
        args[0] = "--connect".to_owned();
        let path: std::path::PathBuf = (&args[1]).into();
        let id = path
            .file_stem()
            .map(|p| p.to_str().unwrap_or(""))
            .unwrap_or("")
            .to_owned();
        args[1] = id;
    }
    if args.is_empty() {
        std::thread::spawn(move || check_zombie());
        crate::common::check_software_update();
        frame.event_handler(UI {});
        frame.sciter_handler(UIHostHandler {});
        page = "index.html";
        // Start pulse audio local server.
        #[cfg(target_os = "linux")]
        std::thread::spawn(crate::ipc::start_pa);
    } else if args[0] == "--install" {
        frame.event_handler(UI {});
        frame.sciter_handler(UIHostHandler {});
        page = "install.html";
    } else if args[0] == "--cm" {
        frame.register_behavior("connection-manager", move || {
            Box::new(cm::SciterConnectionManager::new())
        });
        page = "cm.html";
    } else if (args[0] == "--connect"
        || args[0] == "--file-transfer"
        || args[0] == "--port-forward"
        || args[0] == "--rdp")
        && args.len() > 1
    {
        #[cfg(windows)]
        {
            let hw = frame.get_host().get_hwnd();
            crate::platform::windows::enable_lowlevel_keyboard(hw as _);
        }
        let mut iter = args.iter();
        let Some(cmd) = iter.next() else {
            log::error!("Failed to get cmd arg");
            return;
        };
        let cmd = cmd.to_owned();
        let Some(id) = iter.next() else {
            log::error!("Failed to get id arg");
            return;
        };
        let id = id.to_owned();
        let pass = iter.next().unwrap_or(&"".to_owned()).clone();
        let args: Vec<String> = iter.map(|x| x.clone()).collect();
        frame.set_title(&id);
        frame.register_behavior("native-remote", move || {
            let handler =
                remote::SciterSession::new(cmd.clone(), id.clone(), pass.clone(), args.clone());
            #[cfg(not(any(feature = "flutter", feature = "cli")))]
            {
                *CUR_SESSION.lock().unwrap() = Some(handler.inner());
            }
            Box::new(handler)
        });
        page = "remote.html";
    } else {
        log::error!("Wrong command: {:?}", args);
        return;
    }
    #[cfg(feature = "inline")]
    {
        let html = if page == "index.html" {
            inline::get_index()
        } else if page == "cm.html" {
            inline::get_cm()
        } else if page == "install.html" {
            inline::get_install()
        } else {
            inline::get_remote()
        };
        frame.load_html(html.as_bytes(), Some(page));
    }
    #[cfg(not(feature = "inline"))]
    frame.load_file(&format!(
        "file://{}/src/ui/{}",
        std::env::current_dir()
            .map(|c| c.display().to_string())
            .unwrap_or("".to_owned()),
        page
    ));
    frame.run_app();
}

struct UI {}

impl UI {
    fn recent_sessions_updated(&self) -> bool {
        recent_sessions_updated()
    }

    fn get_id(&self) -> String {
        ipc::get_id()
    }

    fn temporary_password(&mut self) -> String {
        temporary_password()
    }

    fn update_temporary_password(&self) {
        update_temporary_password()
    }

    fn permanent_password(&self) -> String {
        permanent_password()
    }

    fn set_permanent_password(&self, password: String) {
        set_permanent_password(password);
    }

    fn get_remote_id(&mut self) -> String {
        LocalConfig::get_remote_id()
    }

    fn set_remote_id(&mut self, id: String) {
        LocalConfig::set_remote_id(&id);
    }

    fn goto_install(&mut self) {
        goto_install();
    }

    fn install_me(&mut self, _options: String, _path: String) {
        install_me(_options, _path, false, false);
    }

    fn update_me(&self, _path: String) {
        update_me(_path);
    }

    fn run_without_install(&self) {
        run_without_install();
    }

    fn show_run_without_install(&self) -> bool {
        show_run_without_install()
    }

    fn get_license(&self) -> String {
        get_license()
    }

    fn get_option(&self, key: String) -> String {
        get_option(key)
    }

    fn get_local_option(&self, key: String) -> String {
        get_local_option(key)
    }

    fn set_local_option(&self, key: String, value: String) {
        set_local_option(key, value);
    }

    fn peer_has_password(&self, id: String) -> bool {
        peer_has_password(id)
    }

    fn forget_password(&self, id: String) {
        forget_password(id)
    }

    fn get_peer_option(&self, id: String, name: String) -> String {
        get_peer_option(id, name)
    }

    fn set_peer_option(&self, id: String, name: String, value: String) {
        set_peer_option(id, name, value)
    }

    fn using_public_server(&self) -> bool {
        crate::using_public_server()
    }

    fn get_options(&self) -> Value {
        let hashmap: HashMap<String, String> =
            serde_json::from_str(&get_options()).unwrap_or_default();
        let mut m = Value::map();
        for (k, v) in hashmap {
            m.set_item(k, v);
        }
        m
    }

    fn test_if_valid_server(&self, host: String, test_with_proxy: bool) -> String {
        test_if_valid_server(host, test_with_proxy)
    }

    fn get_sound_inputs(&self) -> Value {
        Value::from_iter(get_sound_inputs())
    }

    fn set_options(&self, v: Value) {
        let mut m = HashMap::new();
        for (k, v) in v.items() {
            if let Some(k) = k.as_string() {
                if let Some(v) = v.as_string() {
                    if !v.is_empty() {
                        m.insert(k, v);
                    }
                }
            }
        }
        set_options(m);
    }

    fn set_option(&self, key: String, value: String) {
        set_option(key, value);
    }

    fn install_path(&mut self) -> String {
        install_path()
    }

    fn install_options(&self) -> String {
        install_options()
    }

    fn get_socks(&self) -> Value {
        Value::from_iter(get_socks())
    }

    fn set_socks(&self, proxy: String, username: String, password: String) {
        set_socks(proxy, username, password)
    }

    fn is_installed(&self) -> bool {
        is_installed()
    }

    fn is_root(&self) -> bool {
        is_root()
    }

    fn is_release(&self) -> bool {
        #[cfg(not(debug_assertions))]
        return true;
        #[cfg(debug_assertions)]
        return false;
    }

    fn is_share_rdp(&self) -> bool {
        is_share_rdp()
    }

    fn set_share_rdp(&self, _enable: bool) {
        set_share_rdp(_enable);
    }

    fn is_installed_lower_version(&self) -> bool {
        is_installed_lower_version()
    }

    fn closing(&mut self, x: i32, y: i32, w: i32, h: i32) {
        crate::server::input_service::fix_key_down_timeout_at_exit();
        LocalConfig::set_size(x, y, w, h);
    }

    fn get_size(&mut self) -> Value {
        let s = LocalConfig::get_size();
        let mut v = Vec::new();
        v.push(s.0);
        v.push(s.1);
        v.push(s.2);
        v.push(s.3);
        Value::from_iter(v)
    }

    fn get_mouse_time(&self) -> f64 {
        get_mouse_time()
    }

    fn check_mouse_time(&self) {
        check_mouse_time()
    }

    fn get_connect_status(&mut self) -> Value {
        let mut v = Value::array(0);
        let x = get_connect_status();
        v.push(x.status_num);
        v.push(x.key_confirmed);
        v.push(x.id);
        v
    }

    #[inline]
    fn get_peer_value(id: String, p: PeerConfig) -> Value {
        let values = vec![
            id,
            p.info.username.clone(),
            p.info.hostname.clone(),
            p.info.platform.clone(),
            p.options.get("alias").unwrap_or(&"".to_owned()).to_owned(),
        ];
        Value::from_iter(values)
    }

    fn get_peer(&self, id: String) -> Value {
        let c = get_peer(id.clone());
        Self::get_peer_value(id, c)
    }

    fn get_fav(&self) -> Value {
        Value::from_iter(get_fav())
    }

    fn store_fav(&self, fav: Value) {
        let mut tmp = vec![];
        fav.values().for_each(|v| {
            if let Some(v) = v.as_string() {
                if !v.is_empty() {
                    tmp.push(v);
                }
            }
        });
        store_fav(tmp);
    }

    fn get_recent_sessions(&mut self) -> Value {
        // to-do: limit number of recent sessions, and remove old peer file
        let peers: Vec<Value> = PeerConfig::peers(None)
            .drain(..)
            .map(|p| Self::get_peer_value(p.0, p.2))
            .collect();
        Value::from_iter(peers)
    }

    fn get_icon(&mut self) -> String {
        get_icon()
    }

    fn remove_peer(&mut self, id: String) {
        PeerConfig::remove(&id);
    }

    fn remove_discovered(&mut self, id: String) {
        remove_discovered(id);
    }

    fn send_wol(&mut self, id: String) {
        crate::lan::send_wol(id)
    }

    fn new_remote(&mut self, id: String, remote_type: String, force_relay: bool) {
        new_remote(id, remote_type, force_relay)
    }

    fn is_process_trusted(&mut self, _prompt: bool) -> bool {
        is_process_trusted(_prompt)
    }

    fn is_can_screen_recording(&mut self, _prompt: bool) -> bool {
        is_can_screen_recording(_prompt)
    }

    fn is_installed_daemon(&mut self, _prompt: bool) -> bool {
        is_installed_daemon(_prompt)
    }

    fn get_error(&mut self) -> String {
        get_error()
    }

    fn is_login_wayland(&mut self) -> bool {
        is_login_wayland()
    }

    fn current_is_wayland(&mut self) -> bool {
        current_is_wayland()
    }

    fn get_software_update_url(&self) -> String {
        crate::SOFTWARE_UPDATE_URL.lock().unwrap().clone()
    }

    fn get_new_version(&self) -> String {
        get_new_version()
    }

    fn get_version(&self) -> String {
        get_version()
    }

    fn get_fingerprint(&self) -> String {
        get_fingerprint()
    }

    fn get_app_name(&self) -> String {
        get_app_name()
    }

    fn get_software_ext(&self) -> String {
        #[cfg(windows)]
        let p = "exe";
        #[cfg(target_os = "macos")]
        let p = "dmg";
        #[cfg(target_os = "linux")]
        let p = "deb";
        p.to_owned()
    }

    fn get_software_store_path(&self) -> String {
        let mut p = std::env::temp_dir();
        let name = crate::SOFTWARE_UPDATE_URL
            .lock()
            .unwrap()
            .split("/")
            .last()
            .map(|x| x.to_owned())
            .unwrap_or(crate::get_app_name());
        p.push(name);
        format!("{}.{}", p.to_string_lossy(), self.get_software_ext())
    }

    fn create_shortcut(&self, _id: String) {
        #[cfg(windows)]
        create_shortcut(_id)
    }

    fn discover(&self) {
        std::thread::spawn(move || {
            allow_err!(crate::lan::discover());
        });
    }

    fn get_lan_peers(&self) -> String {
        // let peers = get_lan_peers()
        //     .into_iter()
        //     .map(|mut peer| {
        //         (
        //             peer.remove("id").unwrap_or_default(),
        //             peer.remove("username").unwrap_or_default(),
        //             peer.remove("hostname").unwrap_or_default(),
        //             peer.remove("platform").unwrap_or_default(),
        //         )
        //     })
        //     .collect::<Vec<(String, String, String, String)>>();
        serde_json::to_string(&get_lan_peers()).unwrap_or_default()
    }

    fn get_uuid(&self) -> String {
        get_uuid()
    }

    fn open_url(&self, url: String) {
        #[cfg(windows)]
        let p = "explorer";
        #[cfg(target_os = "macos")]
        let p = "open";
        #[cfg(target_os = "linux")]
        let p = if std::path::Path::new("/usr/bin/firefox").exists() {
            "firefox"
        } else {
            "xdg-open"
        };
        allow_err!(std::process::Command::new(p).arg(url).spawn());
    }

    fn change_id(&self, id: String) {
        reset_async_job_status();
        let old_id = self.get_id();
        change_id_shared(id, old_id);
    }

    fn http_request(&self, url: String, method: String, body: Option<String>, header: String) {
        http_request(url, method, body, header)
    }

    fn post_request(&self, url: String, body: String, header: String) {
        post_request(url, body, header)
    }

    fn is_ok_change_id(&self) -> bool {
        hbb_common::machine_uid::get().is_ok()
    }

    fn get_async_job_status(&self) -> String {
        get_async_job_status()
    }

    fn get_http_status(&self, url: String) -> Option<String> {
        get_async_http_status(url)
    }

    fn t(&self, name: String) -> String {
        crate::client::translate(name)
    }

    fn is_xfce(&self) -> bool {
        crate::platform::is_xfce()
    }

    fn get_api_server(&self) -> String {
        get_api_server()
    }

    fn has_hwcodec(&self) -> bool {
        has_hwcodec()
    }

    fn has_vram(&self) -> bool {
        has_vram()
    }

    fn get_langs(&self) -> String {
        get_langs()
    }

    fn video_save_directory(&self, root: bool) -> String {
        video_save_directory(root)
    }

    fn handle_relay_id(&self, id: String) -> String {
        handle_relay_id(&id).to_owned()
    }

    fn get_login_device_info(&self) -> String {
        get_login_device_info_json()
    }

    fn support_remove_wallpaper(&self) -> bool {
        support_remove_wallpaper()
    }

    fn has_valid_2fa(&self) -> bool {
        has_valid_2fa()
    }

    fn generate2fa(&self) -> String {
        generate2fa()
    }

    pub fn verify2fa(&self, code: String) -> bool {
        verify2fa(code)
    }
        
    fn verify_login(&self, raw: String, id: String) -> bool {
       crate::verify_login(&raw, &id)
    }

    fn generate_2fa_img_src(&self, data: String) -> String {
        let v = qrcode_generator::to_png_to_vec(data, qrcode_generator::QrCodeEcc::Low, 128)
            .unwrap_or_default();
        let s = hbb_common::sodiumoxide::base64::encode(
            v,
            hbb_common::sodiumoxide::base64::Variant::Original,
        );
        format!("data:image/png;base64,{s}")
    }

    pub fn check_hwcodec(&self) {
        check_hwcodec()
    }
}

impl sciter::EventHandler for UI {
    sciter::dispatch_script_call! {
        fn t(String);
        fn get_api_server();
        fn is_xfce();
        fn using_public_server();
        fn get_id();
        fn temporary_password();
        fn update_temporary_password();
        fn permanent_password();
        fn set_permanent_password(String);
        fn get_remote_id();
        fn set_remote_id(String);
        fn closing(i32, i32, i32, i32);
        fn get_size();
        fn new_remote(String, String, bool);
        fn send_wol(String);
        fn remove_peer(String);
        fn remove_discovered(String);
        fn get_connect_status();
        fn get_mouse_time();
        fn check_mouse_time();
        fn get_recent_sessions();
        fn get_peer(String);
        fn get_fav();
        fn store_fav(Value);
        fn recent_sessions_updated();
        fn get_icon();
        fn install_me(String, String);
        fn is_installed();
        fn is_root();
        fn is_release();
        fn set_socks(String, String, String);
        fn get_socks();
        fn is_share_rdp();
        fn set_share_rdp(bool);
        fn is_installed_lower_version();
        fn install_path();
        fn install_options();
        fn goto_install();
        fn is_process_trusted(bool);
        fn is_can_screen_recording(bool);
        fn is_installed_daemon(bool);
        fn get_error();
        fn is_login_wayland();
        fn current_is_wayland();
        fn get_options();
        fn get_option(String);
        fn get_local_option(String);
        fn set_local_option(String, String);
        fn get_peer_option(String, String);
        fn peer_has_password(String);
        fn forget_password(String);
        fn set_peer_option(String, String, String);
        fn get_license();
        fn test_if_valid_server(String, bool);
        fn get_sound_inputs();
        fn set_options(Value);
        fn set_option(String, String);
        fn get_software_update_url();
        fn get_new_version();
        fn get_version();
        fn get_fingerprint();
        fn update_me(String);
        fn show_run_without_install();
        fn run_without_install();
        fn get_app_name();
        fn get_software_store_path();
        fn get_software_ext();
        fn open_url(String);
        fn change_id(String);
        fn get_async_job_status();
        fn post_request(String, String, String);
        fn is_ok_change_id();
        fn create_shortcut(String);
        fn discover();
        fn get_lan_peers();
        fn get_uuid();
        fn has_hwcodec();
        fn has_vram();
        fn get_langs();
        fn video_save_directory(bool);
        fn handle_relay_id(String);
        fn get_login_device_info();
        fn support_remove_wallpaper();
        fn has_valid_2fa();
        fn generate2fa();
        fn generate_2fa_img_src(String);
        fn verify2fa(String);
        fn check_hwcodec();
        fn verify_login(String, String);
    }
}

impl sciter::host::HostHandler for UIHostHandler {
    fn on_graphics_critical_failure(&mut self) {
        log::error!("Critical rendering error: e.g. DirectX gfx driver error. Most probably bad gfx drivers.");
    }
}

#[cfg(not(target_os = "linux"))]
fn get_sound_inputs() -> Vec<String> {
    let mut out = Vec::new();
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    if let Ok(devices) = host.devices() {
        for device in devices {
            if device.default_input_config().is_err() {
                continue;
            }
            if let Ok(name) = device.name() {
                out.push(name);
            }
        }
    }
    out
}

#[cfg(target_os = "linux")]
fn get_sound_inputs() -> Vec<String> {
    crate::platform::linux::get_pa_sources()
        .drain(..)
        .map(|x| x.1)
        .collect()
}

// sacrifice some memory
pub fn value_crash_workaround(values: &[Value]) -> Arc<Vec<Value>> {
    let persist = Arc::new(values.to_vec());
    STUPID_VALUES.lock().unwrap().push(persist.clone());
    persist
}

pub fn get_icon() -> String {
    // 128x128
    #[cfg(target_os = "macos")]
    // 128x128 on 160x160 canvas, then shrink to 128, mac looks better with padding
    {
        "data:image/png;base64,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".into()
    }
    #[cfg(not(target_os = "macos"))] // 128x128 no padding
    {
        "data:image/png;base64,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".into()
    }
}
