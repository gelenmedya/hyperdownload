//! `hyperdownload --register` / `--unregister`: the browser host registered by hand (rustyfiles does it when it installs the add-on;
//! this is for a copy that was unpacked by hand). For the current Windows account only: the host's manifest beside the exe, and the
//! browsers' keys under HKEY_CURRENT_USER that point at it.

use serde_json::json;

pub const HOST_NAME: &str = "com.gelenmedya.hyperdownload";
/// The extension's id while it is loaded unpacked; the stores' ids join it once it is published.
pub const EXTENSION_IDS: &[&str] = &["pbnficneolckhnkmpagjimjbhjahcepa"];
const KEYS: &[&str] = &[
    r"Software\Google\Chrome\NativeMessagingHosts",
    r"Software\Microsoft\Edge\NativeMessagingHosts",
    r"Software\BraveSoftware\Brave-Browser\NativeMessagingHosts",
    r"Software\Chromium\NativeMessagingHosts",
];

pub fn register() -> Result<std::path::PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().ok_or("no folder")?;
    let origins: Vec<String> = EXTENSION_IDS.iter().map(|id| format!("chrome-extension://{id}/")).collect();
    let manifest = json!({ "name": HOST_NAME, "description": "HyperDownload, the download manager of rustyfiles", "path": exe, "type": "stdio", "allowed_origins": origins });
    let path = dir.join("native-host.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&manifest).unwrap_or_default()).map_err(|e| format!("{}: {e}", path.display()))?;
    #[cfg(windows)]
    for key in KEYS {
        let k = windows_registry::CURRENT_USER.create(format!("{key}\\{HOST_NAME}")).map_err(|e| e.to_string())?;
        k.set_string("", path.to_string_lossy().as_ref()).map_err(|e| e.to_string())?;
    }
    Ok(path)
}

pub fn unregister() {
    #[cfg(windows)]
    for key in KEYS {
        if let Ok(k) = windows_registry::CURRENT_USER.open(key) {
            let _ = k.remove_tree(HOST_NAME);
        }
    }
}
