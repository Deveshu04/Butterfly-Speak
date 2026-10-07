//! API key storage (Windows Credential Manager via `keyring`) and Sarvam key
//! validation. A key never touches settings.json or the webview; commands
//! expose only presence + a masked tail.
//!
//! Several credentials, one store. [`KeySlot`] names which one a call is
//! about, so a second endpoint cannot silently overwrite the Sarvam key the
//! way a single unnamed slot would.

use super::codec;
use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use keyring::Entry;
use std::time::Duration;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

const SERVICE: &str = "ButterflySpeak";

/// Which stored credential a call is about.
///
/// The account name is the only thing that differs, and it is what Windows
/// shows as the credential's target: `sarvam-api-key.ButterflySpeak` and
/// `custom-endpoint-key.ButterflySpeak`. `Sarvam`'s account string is
/// unchanged from before this enum existed, so every already-installed
/// machine still finds its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySlot {
    Sarvam,
    /// The one custom OpenAI-compatible endpoint (`crate::endpoint`).
    CustomEndpoint,
    /// The Butterfly Labs sign-in's refresh token (`crate::auth`). Not a key
    /// the user typed and not one this app can re-mint — the credential store
    /// is the only place it exists, which is why signing out deletes it rather
    /// than blanking it.
    CloudRefresh,
}

impl KeySlot {
    fn account(self) -> &'static str {
        match self {
            KeySlot::Sarvam => "sarvam-api-key",
            KeySlot::CustomEndpoint => "custom-endpoint-key",
            KeySlot::CloudRefresh => "cloud-refresh-token",
        }
    }

    /// For log lines only — never the key itself.
    fn label(self) -> &'static str {
        match self {
            KeySlot::Sarvam => "Sarvam",
            KeySlot::CustomEndpoint => "custom endpoint",
            KeySlot::CloudRefresh => "Butterfly Labs sign-in",
        }
    }
}

fn entry(slot: KeySlot) -> keyring::Result<Entry> {
    Entry::new(SERVICE, slot.account())
}

/// Read a stored key at startup. Any credential-store failure degrades to
/// "no key" — the UI then asks for it again.
pub fn load(slot: KeySlot) -> Option<String> {
    match entry(slot).and_then(|e| e.get_password()) {
        Ok(k) if !k.trim().is_empty() => Some(k),
        Ok(_) => None,
        Err(keyring::Error::NoEntry) => None,
        Err(e) => {
            tracing::warn!(
                "couldn't read the {} key from the credential store: {e}",
                slot.label()
            );
            None
        }
    }
}

pub fn store(slot: KeySlot, key: &str) -> anyhow::Result<()> {
    entry(slot)
        .and_then(|e| e.set_password(key))
        .context("saving key to the Windows credential store")
}

pub fn delete(slot: KeySlot) -> anyhow::Result<()> {
    match entry(slot).and_then(|e| e.delete_credential()) {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e).context("removing key from the Windows credential store"),
    }
}

/// Validate a key by doing a real realtime-STT handshake: connect, wait for
/// `session.begin`, send `end`. Zero audio, zero tokens billed — and it proves
/// the exact entitlement dictation needs, not just "some API responds".
pub async fn validate(key: &str) -> Result<(), String> {
    let cfg = super::SessionCfg {
        language_code: "auto".into(),
        stream_type: "balanced".into(),
        mode: "transcribe".into(),
        endpointing: super::Endpointing::Manual,
        prompt: None,
        // A key is only ever validated against the host it is a key *for*:
        // the relay takes no Sarvam key and would answer 401 to one.
        lane: super::Lane::Byok,
    };
    let mut request = codec::ws_url(&cfg)
        .into_client_request()
        .map_err(|e| format!("Couldn't build the request: {e}"))?;
    request.headers_mut().insert(
        super::AUTH_HEADER,
        key.parse()
            .map_err(|_| "That doesn't look like a valid API key".to_string())?,
    );

    let connect = tokio_tungstenite::connect_async(request);
    let (mut ws, _resp) = match tokio::time::timeout(Duration::from_secs(8), connect).await {
        Ok(Ok(ok)) => ok,
        Ok(Err(tokio_tungstenite::tungstenite::Error::Http(resp))) => {
            let status = resp.status().as_u16();
            return Err(match status {
                401 | 403 => "Sarvam rejected this key — double-check it on dashboard.sarvam.ai"
                    .to_string(),
                429 => "Sarvam rate limit hit — try again in a moment".to_string(),
                _ => format!("Sarvam returned HTTP {status} — try again"),
            });
        }
        Ok(Err(e)) => {
            tracing::warn!("key validation connect failed: {e}");
            return Err("Couldn't reach Sarvam — check your internet connection".to_string());
        }
        Err(_) => return Err("Couldn't reach Sarvam — the connection timed out".to_string()),
    };

    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        let frame = match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(frame))) => frame,
            Ok(Some(Err(e))) => {
                tracing::warn!("key validation stream error: {e}");
                return Err("Sarvam closed the connection — check your key".to_string());
            }
            Ok(None) => return Err("Sarvam closed the connection — check your key".to_string()),
            Err(_) => return Err("Sarvam didn't respond — try again".to_string()),
        };
        match frame {
            Message::Text(text) => match codec::parse_server(&text) {
                Some(codec::ServerMsg::SessionBegin) => {
                    let _ = ws.send(Message::Text(codec::ClientMsg::End.to_json())).await;
                    let _ = ws.close(None).await;
                    return Ok(());
                }
                Some(codec::ServerMsg::Error { message, .. }) => {
                    return Err(if message.is_empty() {
                        "Sarvam rejected the session — check your key".to_string()
                    } else {
                        format!("Sarvam error: {message}")
                    });
                }
                _ => continue,
            },
            Message::Close(frame) => {
                tracing::warn!("key validation closed: {frame:?}");
                return Err("Sarvam rejected this key — double-check it on dashboard.sarvam.ai"
                    .to_string());
            }
            _ => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every slot. Nothing checks this list is complete: the exhaustive match
    /// below makes a new variant fail to compile until it gets an arm there,
    /// which is the prompt to add it here too, but it compiles and passes
    /// with the arm alone. A variant missing from this list is also missing
    /// from `the_uninstall_hook_deletes_every_saved_credential`, so the
    /// uninstaller would leave that credential behind. Add it here by hand.
    const ALL: &[KeySlot] = &[
        KeySlot::Sarvam,
        KeySlot::CustomEndpoint,
        KeySlot::CloudRefresh,
    ];

    /// One slot per credential. If any two ever collide, storing one silently
    /// destroys the other — and the only symptom is dictation, or a sign-in,
    /// evaporating after a restart.
    #[test]
    fn every_slot_names_a_different_credential() {
        // A `match` with no wildcard arm, so a new variant stops this
        // compiling until someone looks here (and, see `ALL`, adds it there).
        for slot in ALL {
            match slot {
                KeySlot::Sarvam | KeySlot::CustomEndpoint | KeySlot::CloudRefresh => {}
            }
        }
        let accounts: std::collections::BTreeSet<_> =
            ALL.iter().map(|slot| slot.account()).collect();
        assert_eq!(accounts.len(), ALL.len(), "two slots share an account name");
    }

    /// The refresh token is the one stored credential that is a *token*, not a
    /// key the user typed, so its account name is pinned here: change it and
    /// every already-signed-in machine silently signs itself out.
    #[test]
    fn the_cloud_refresh_slot_is_the_one_sign_in_looks_under() {
        assert_eq!(KeySlot::CloudRefresh.account(), "cloud-refresh-token");
    }

    /// The Sarvam account string is a compatibility constant: every machine
    /// that already has a key stored finds it under this exact name.
    #[test]
    fn the_sarvam_account_name_is_the_one_already_on_disk() {
        assert_eq!(KeySlot::Sarvam.account(), "sarvam-api-key");
        assert_eq!(SERVICE, "ButterflySpeak");
    }

    /// The uninstaller's "Delete app data" removes every credential this app
    /// saves, by exact name (src-tauri/windows/hooks.nsh). `ALL` must list
    /// every slot by hand (see its comment); a slot in `ALL` fails here until
    /// the hook names it too.
    #[test]
    fn the_uninstall_hook_deletes_every_saved_credential() {
        const HOOK: &str = include_str!("../../windows/hooks.nsh");
        for slot in ALL {
            let line = format!("!insertmacro BS_DELETE_CREDENTIAL \"{}\"", slot.account());
            assert!(HOOK.contains(&line), "hooks.nsh does not delete {}", slot.account());
        }
        assert!(HOOK.contains(&format!("!define BS_CRED_SERVICE \"{SERVICE}\"")));
    }
}
