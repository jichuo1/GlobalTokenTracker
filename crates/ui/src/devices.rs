//! LAN multi-device viewing for the WinUI shell — the same core calls the
//! macOS app makes (`core::peer`). Everything here runs on a background
//! thread (`spawn_background`): it opens the ledger, does the network or
//! ledger work, and hands plain data back to the UI thread. Protocol
//! failures arrive as core's fixed codes; `message` puts them into words.

use globaltokentracker_core::Store;
use globaltokentracker_core::peer::devices::{PeerDevice, Registry};
use globaltokentracker_core::peer::discovery::{self, Found};
use globaltokentracker_core::peer::merged::{OverlapWarning, overlaps};
use globaltokentracker_core::peer::share::{self, ShareStatus, Sharing};
use globaltokentracker_core::peer::sync::{self, SyncReport};
use globaltokentracker_core::peer::trust::{Ambiguous, Viewer};
use globaltokentracker_core::peer::{self, pairing};
use std::path::Path;
use std::time::Duration;

/// This PC + the devices it has paired with (ledger registry).
#[derive(Clone, Debug, Default)]
pub struct DevicesInfo {
    pub local_id: String,
    pub local_name: String,
    pub peers: Vec<PeerDevice>,
    pub warnings: Vec<OverlapWarning>,
}

impl DevicesInfo {
    /// A display name for any device id the shell meets.
    pub fn name_of(&self, id: &str) -> String {
        if id == self.local_id {
            return local_label(&self.local_name);
        }
        self.peers
            .iter()
            .find(|p| p.device_id == id)
            .map_or_else(|| id.chars().take(8).collect(), |p| p.name.clone())
    }
}

/// core names an unnamed local device "This device" — say "本机" instead.
pub fn local_label(name: &str) -> String {
    if name == "This device" {
        crate::t!("本机").to_string()
    } else {
        name.to_string()
    }
}

/// The "Add device" form on the Devices page.
#[derive(Clone, Debug, Default)]
pub struct PairForm {
    pub addr: String,
    pub code: String,
    pub busy: bool,
    /// Last failure (a fixed code, localized at render time).
    pub error: Option<String>,
    /// The last successful pairing (device name), for a confirmation line.
    pub paired: Option<String>,
}

impl PairForm {
    pub fn digits(&self) -> String {
        self.code
            .chars()
            .filter(char::is_ascii_digit)
            .take(6)
            .collect()
    }

    pub fn ready(&self) -> bool {
        !self.busy && !self.addr.trim().is_empty() && self.digits().len() == 6
    }
}

/// The Settings "LAN sharing" section's state.
#[derive(Default)]
pub struct ShareState {
    pub running: Option<Sharing>,
    pub status: Option<ShareStatus>,
    /// Local clock time the shown code stops working (status snapshot).
    pub code_until: Option<std::time::SystemTime>,
    pub viewers: Vec<Viewer>,
    /// Starting / stopping in the background.
    pub busy: bool,
    /// Generation of the status follow-up timer (a restart supersedes it).
    pub tick: u64,
    pub error: Option<String>,
}

impl ShareState {
    pub fn refresh_status(&mut self) {
        self.status = self.running.as_ref().map(Sharing::status);
        self.code_until = self.status.as_ref().and_then(|s| {
            s.code_expires_in
                .map(|secs| std::time::SystemTime::now() + Duration::from_secs(secs))
        });
    }
}

fn store(db: &Path) -> Result<Store, String> {
    Store::open(db).map_err(|e| e.to_string())
}

pub fn load(db: &Path) -> Result<DevicesInfo, String> {
    let store = store(db)?;
    let (local_id, local_name) = peer::local_device(&store).map_err(|e| e.to_string())?;
    let peers = Registry::open(&store)
        .and_then(|r| r.list())
        .map_err(|e| e.to_string())?;
    let warnings = overlaps(&store).map_err(|e| e.to_string())?;
    Ok(DevicesInfo {
        local_id,
        local_name,
        peers,
        warnings,
    })
}

/// Pull every enabled paired device (`if_due`: core's 5-minute throttle).
/// `Ok(None)` = not due.
pub fn sync(db: &Path, if_due: bool) -> Result<Option<SyncReport>, String> {
    let store = store(db)?;
    let options = sync::SyncOptions::default();
    if if_due {
        sync::sync_if_due(&store, sync::SYNC_INTERVAL, &options)
    } else {
        sync::sync_all(&store, &options).map(Some)
    }
    .map_err(|e| e.to_string())
}

pub fn discover(db: &Path) -> Result<Vec<Found>, String> {
    let store = store(db)?;
    let local = peer::local_device(&store).map_err(|e| e.to_string())?.0;
    discovery::discover(&store, Duration::from_secs(3))
        .map(|(found, _)| found.into_iter().filter(|f| f.device_id != local).collect())
        .map_err(|e| e.to_string())
}

/// Pair, mutually: the other device also gets access to this PC (the caller
/// starts sharing when it isn't). `Err` carries a fixed code (or an engine
/// error text).
pub fn pair(db: &Path, addr: &str, code: &str) -> Result<pairing::Paired, String> {
    let store = store(db)?;
    let name = peer::local_device(&store)
        .map(|(_, n)| local_label(&n))
        .unwrap_or_else(|_| "Windows".into());
    match pairing::pair_mutual(&store, addr.trim(), code, &name, true) {
        Ok(Ok(paired)) => Ok(paired),
        Ok(Err(code)) => Err(code.to_string()),
        Err(e) => Err(e.to_string()),
    }
}

pub fn forget(db: &Path, device_id: &str) -> Result<(), String> {
    let store = store(db)?;
    let registry = Registry::open(&store).map_err(|e| e.to_string())?;
    match registry.forget(device_id) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err("unknown_peer".into()),
        Err(e) if e.is::<Ambiguous>() => Err("ambiguous_peer".into()),
        Err(e) => Err(e.to_string()),
    }
}

pub fn set_enabled(db: &Path, device_id: &str, on: bool) -> Result<(), String> {
    let store = store(db)?;
    Registry::open(&store)
        .and_then(|r| r.set_enabled(device_id, on))
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Start sharing this PC on the LAN (TLS on 0.0.0.0:4319, mDNS).
pub fn start_sharing(db: &Path) -> Result<Sharing, String> {
    Sharing::start(db, share::LAN_BIND, None, true).map_err(|e| format!("{e:#}"))
}

pub fn viewers(db: &Path) -> Result<Vec<Viewer>, String> {
    share::viewers(db).map_err(|e| e.to_string())
}

pub fn revoke(db: &Path, viewer_id: &str) -> Result<(), String> {
    match share::revoke(db, viewer_id) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err("unknown_viewer".into()),
        Err(e) => Err(e.to_string()),
    }
}

/// core's fixed codes (docs/PEER_API.md) → localized words. Anything else
/// (an I/O or ledger error text) is shown as is.
pub fn message(code: &str) -> String {
    message_key(code).map_or_else(|| code.to_string(), |k| crate::i18n::tr(k).to_string())
}

/// The zh key (`tr` table) for a fixed code.
fn message_key(code: &str) -> Option<&'static str> {
    Some(match code {
        "invalid_code" => "配对码为 6 位数字。",
        "pairing_failed" => "配对码不匹配。请在另一台设备上生成新的配对码后重试。",
        "pairing_code_reused" => "此配对码已失败过一次。请在另一台设备上生成新的配对码。",
        "pairing_code_expired" => "配对码已过期。请在另一台设备上生成新的配对码。",
        "pairing_unavailable" => "另一台设备当前没有有效的配对码。",
        "pairing_locked" => "尝试次数过多。请稍等几秒再试。",
        "pairing_busy" => "另一台设备正在与其他设备配对，请稍后再试。",
        "pairing_self" => "这是本机。",
        "pairing_aborted" | "pairing_refused" => "配对中断，请重试。",
        "tls_required" => "该设备未通过安全连接共享。",
        "connect_failed" => "无法连接该设备。请检查地址，并确认它正在共享。",
        "tls_failed" => "无法与该设备建立安全连接。",
        "peer_fingerprint_mismatch" => "此地址上响应的是另一台设备，其身份与已配对的设备不符。",
        "auth_invalid" | "auth_required" => "该设备已不再接受本机，请重新配对。",
        "not_paired" => "未配对。",
        "plaintext_not_allowed" => "网络中只允许安全连接。",
        "sync_timeout" => "该设备响应超时。",
        "sync_failed" | "malformed_response" => "同步失败。",
        "unknown_peer" => "该设备未配对。",
        "ambiguous_peer" => "有多台设备匹配。",
        "unknown_viewer" => "找不到该查看方。",
        "external_interface_disabled" => "该设备已关闭外部接口。",
        "ledger_busy" | "ledger_unavailable" => "该设备的账本正忙，请稍后再试。",
        "unsupported_protocol_version" => "该设备运行的版本不兼容。",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_worded_code_has_an_english_entry() {
        let mut worded = 0;
        for code in peer::ERROR_CODES {
            if let Some(zh) = message_key(code) {
                worded += 1;
                let en = crate::i18n::en(zh);
                assert_ne!(en, zh, "{code}: missing English");
                assert!(en.is_ascii(), "{code}: {en}");
            }
        }
        assert!(worded >= 20);
    }

    #[test]
    fn the_pair_form_wants_an_address_and_six_digits() {
        let mut f = PairForm {
            addr: "192.168.1.20".into(),
            code: "12a345".into(),
            ..PairForm::default()
        };
        assert!(!f.ready());
        f.code = "123 456".into();
        assert_eq!(f.digits(), "123456");
        assert!(f.ready());
        f.busy = true;
        assert!(!f.ready());
    }
}
