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
        *cm::HIDE_CM.lock().unwrap() = crate::ipc::get_config("hide_cm")
            .ok()
            .flatten()
            .unwrap_or_default()
            == "true";
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
    let hide_cm = *cm::HIDE_CM.lock().unwrap();
    if !args.is_empty() && args[0] == "--cm" && hide_cm {
        // run_app calls expand(show) + run_loop, we use collapse(hide) + run_loop instead to create a hidden window
        frame.collapse(true);
        frame.run_loop();
        return;
    }
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

    fn set_permanent_password(&self, password: String) {
        let _ = set_permanent_password_with_result(password);
    }

    fn is_local_permanent_password_set(&self) -> bool {
        is_local_permanent_password_set()
    }

    fn is_permanent_password_set(&self) -> bool {
        is_permanent_password_set()
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

    fn is_incoming_only(&self) -> bool {
        hbb_common::config::is_incoming_only()
    }

    pub fn is_outgoing_only(&self) -> bool {
        hbb_common::config::is_outgoing_only()
    }

    pub fn is_custom_client(&self) -> bool {
        crate::common::is_custom_client()
    }

    pub fn is_disable_settings(&self) -> bool {
        hbb_common::config::is_disable_settings()
    }

    pub fn is_disable_account(&self) -> bool {
        hbb_common::config::is_disable_account()
    }

    pub fn is_disable_installation(&self) -> bool {
        hbb_common::config::is_disable_installation()
    }

    pub fn is_disable_ab(&self) -> bool {
        hbb_common::config::is_disable_ab()
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

    fn get_supported_privacy_mode_impls(&self) -> String {
        serde_json::to_string(&crate::privacy_mode::get_supported_privacy_mode_impl())
            .unwrap_or_default()
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

    fn is_option_fixed(&self, key: String) -> bool {
        crate::ui_interface::is_option_fixed(&key)
    }

    fn get_builtin_option(&self, key: String) -> String {
        crate::ui_interface::get_builtin_option(&key)
    }

    fn is_remote_modify_enabled_by_control_permissions(&self) -> String {
        match crate::ui_interface::is_remote_modify_enabled_by_control_permissions() {
            Some(true) => "true",
            Some(false) => "false",
            None => "",
        }
        .to_string()
    }
}

impl sciter::EventHandler for UI {
    sciter::dispatch_script_call! {
        fn t(String);
        fn get_api_server();
        fn is_xfce();
        fn using_public_server();
        fn is_custom_client();
        fn is_outgoing_only();
        fn is_incoming_only();
        fn is_disable_settings();
        fn is_disable_account();
        fn is_disable_installation();
        fn is_disable_ab();
        fn get_id();
        fn temporary_password();
        fn update_temporary_password();
        fn set_permanent_password(String);
        fn is_local_permanent_password_set();
        fn is_permanent_password_set();
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
        fn get_supported_privacy_mode_impls();
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
        fn is_option_fixed(String);
        fn get_builtin_option(String);
        fn is_remote_modify_enabled_by_control_permissions();
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
