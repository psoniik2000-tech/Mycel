//! Presence ("online", "last seen") and the privacy settings, as in Telegram.
//! Both live in the local database; peers only learn our presence, sent per peer as the
//! settings allow.

// shortcut: settings are per device and per space (not synced to our other devices); send them
// as an event to our own devices if that surprises.

use crate::{Chat, Node, State, chat_key, hex, is_msg, now_ms, peer_addr};
use anyhow::Result;
use mycel_proto::{CallAction, Event, EventBody, SyncMsg};
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;

/// Who something is open to.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Who {
    Everyone,
    Contacts,
    Nobody,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Privacy {
    /// Who sees when we are online; whoever does not, we do not see either.
    pub last_seen: Who,
    /// Calls from others are declined without ringing.
    pub calls: Who,
    /// Off: no Read receipts in 1:1 chats, ours or shown from the partner.
    pub read_receipts: bool,
    /// Off: our @username does not come up in the directory search.
    pub findable: bool,
}

impl Default for Privacy {
    fn default() -> Self {
        Self {
            last_seen: Who::Everyone,
            calls: Who::Everyone,
            read_receipts: true,
            findable: true,
        }
    }
}

/// What we may show about someone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Presence {
    pub online: bool,
    /// Unix ms they were last active; `hidden`: only roughly known (we last heard from them).
    pub seen: Option<u64>,
    /// They hide it from us, or we hide ours from them.
    pub hidden: bool,
}

/// The latest presence a device sent us, in our clock.
#[derive(Serialize, Deserialize, Clone, Copy)]
struct Report {
    online: bool,
    /// None: hidden from us.
    at: Option<u64>,
    got: u64,
}

type Reports = Vec<([u8; 32], Report)>;

impl State {
    fn privacy(&self) -> Result<Privacy> {
        let p = self.store.meta("privacy")?;
        Ok(p.map(|b| postcard::from_bytes(&b))
            .transpose()?
            .unwrap_or_default())
    }

    fn reports(&self) -> Result<Reports> {
        let r = self.store.meta("presence")?;
        Ok(r.map(|b| postcard::from_bytes(&b))
            .transpose()?
            .unwrap_or_default())
    }

    /// Devices that do not count as contacts until we write to them: they came by our public
    /// link, or we removed them.
    // shortcut: known only to the device that saw it; our other devices count them as contacts.
    fn strangers(&self) -> Result<Vec<[u8; 32]>> {
        let r = self.store.meta("strangers")?;
        Ok(r.map(|b| postcard::from_bytes(&b))
            .transpose()?
            .unwrap_or_default())
    }

    pub(crate) fn add_stranger(&self, dev: [u8; 32]) -> Result<()> {
        let mut s = self.strangers()?;
        if !s.contains(&dev) {
            s.push(dev);
            self.store
                .set_meta("strangers", &postcard::to_allocvec(&s)?)?;
        }
        Ok(())
    }

    /// A contact: added by a contact link (not a stranger), or someone we wrote to.
    fn is_contact(&self, account: &[u8; 32], me: [u8; 32]) -> Result<bool> {
        let contacts = self.contacts()?;
        let added = self
            .dms()?
            .iter()
            .any(|(g, p)| p == account && contacts.contains(g));
        if added {
            let mut stranger = false;
            for s in self.strangers()? {
                stranger |= self.account_of(&s)? == *account;
            }
            if !stranger {
                return Ok(true);
            }
        }
        let mine = self.mine(me)?;
        Ok(self
            .store
            .events(chat_key(&Some(*account)))?
            .iter()
            .any(|b| {
                postcard::from_bytes::<Event>(b)
                    .is_ok_and(|e| mine.contains(&e.author_device) && is_msg(&e.body))
            }))
    }

    fn allows(&self, who: Who, account: &[u8; 32], me: [u8; 32]) -> Result<bool> {
        Ok(match who {
            Who::Everyone => true,
            Who::Contacts => self.is_contact(account, me)?,
            Who::Nobody => false,
        })
    }

    /// Our presence for device `to`; None: nothing to tell (never active here, or our device).
    pub(crate) fn presence_msg(
        &self,
        to: &[u8; 32],
        me: [u8; 32],
        online: bool,
    ) -> Result<Option<SyncMsg>> {
        if self.mine(me)?.contains(to) {
            return Ok(None);
        }
        if !self.allows(self.privacy()?.last_seen, &self.account_of(to)?, me)? {
            return Ok(Some(SyncMsg::Presence {
                online: false,
                ago: None,
            }));
        }
        let at = self.store.meta("active_at")?;
        let Some(at) = at.and_then(|v| v.try_into().ok().map(u64::from_le_bytes)) else {
            return Ok(None);
        };
        let ago = if online {
            0
        } else {
            (now_ms()?.saturating_sub(at) / 1000).min(u32::MAX as u64) as u32
        };
        Ok(Some(SyncMsg::Presence {
            online,
            ago: Some(ago),
        }))
    }

    pub(crate) fn touch_active(&self) -> Result<()> {
        self.store.set_meta("active_at", &now_ms()?.to_le_bytes())
    }

    pub(crate) fn take_presence(
        &self,
        from: [u8; 32],
        online: bool,
        ago: Option<u32>,
    ) -> Result<()> {
        let got = now_ms()?;
        let at = ago.map(|a| got.saturating_sub(a as u64 * 1000));
        let mut r = self.reports()?;
        r.retain(|(d, _)| *d != from);
        r.push((from, Report { online, at, got }));
        self.store.set_meta("presence", &postcard::to_allocvec(&r)?)
    }
}

impl Node {
    pub fn privacy(&self) -> Privacy {
        self.st.lock().unwrap().privacy().unwrap_or_default()
    }

    /// Saves the settings and tells the peers at once. A changed `findable` needs a new
    /// `dir_register`.
    pub fn set_privacy(&self, p: Privacy) -> Result<()> {
        self.st
            .lock()
            .unwrap()
            .store
            .set_meta("privacy", &postcard::to_allocvec(&p)?)?;
        self.send_presence();
        Ok(())
    }

    /// The app is on screen (`true`) or went away: peers learn it now and with every sync.
    pub fn set_active(&self, active: bool) {
        if self.active.swap(active, Ordering::Relaxed) == active {
            return;
        }
        if let Err(e) = self.st.lock().unwrap().touch_active() {
            eprintln!("mycel: presence failed: {e}");
        }
        self.send_presence();
    }

    /// One try per peer, like typing: the periodic sync repeats it.
    fn send_presence(&self) {
        let (me, online) = (self.device_id(), self.active.load(Ordering::Relaxed));
        let out = (|| {
            let st = self.st.lock().unwrap();
            let mut out = vec![];
            for p in st.store.peers()? {
                if let Some(addr) = peer_addr(&p)
                    && addr.id != self.net.id()
                    && st.knows(addr.id.as_bytes())?
                    && let Some(m) = st.presence_msg(addr.id.as_bytes(), me, online)?
                {
                    out.push((addr, m));
                }
            }
            anyhow::Ok(out)
        })();
        match out {
            Ok(out) => {
                for (addr, msg) in out {
                    let net = self.net.clone();
                    tokio::spawn(async move { net.send(addr, &msg).await });
                }
            }
            Err(e) => eprintln!("mycel: presence failed: {e}"),
        }
    }

    /// Online / last seen of a member or contact (an account), as far as both sides' settings
    /// allow. An older app sends no presence: online = connected.
    pub fn presence(&self, account: &[u8; 32]) -> Presence {
        let me = self.device_id();
        let (reports, visible) = {
            let st = self.st.lock().unwrap();
            let mut r = st.reports().unwrap_or_default();
            r.retain(|(d, _)| st.account_of(d).is_ok_and(|a| a == *account));
            let who = st.privacy().unwrap_or_default().last_seen;
            (r, st.allows(who, account, me).unwrap_or(false))
        };
        let (link, heard) = self.link(account);
        let heard = reports
            .iter()
            .flat_map(|(d, r)| [Some(r.got), self.link(d).1])
            .chain([heard])
            .flatten()
            .max();
        if !visible || (!reports.is_empty() && reports.iter().all(|(_, r)| r.at.is_none())) {
            return Presence {
                online: false,
                seen: heard,
                hidden: true,
            };
        }
        if reports.is_empty() {
            return Presence {
                online: link.is_some(),
                seen: heard,
                hidden: false,
            };
        }
        // "Online" is repeated every sync; an old one is from an app killed while on screen, and a
        // phone woken by a push would flash it until its fresh report arrives.
        let fresh = now_ms()
            .unwrap_or(0)
            .saturating_sub(2 * crate::RESYNC.as_millis() as u64);
        Presence {
            online: reports.iter().any(|(d, r)| {
                r.online && r.at.is_some() && r.got >= fresh && self.link(d).0.is_some()
            }),
            seen: reports.iter().filter_map(|(_, r)| r.at).max(),
            hidden: false,
        }
    }

    /// A contact removed on this device is a stranger to the "contacts" settings again.
    pub fn forget_contact(&self, account: &[u8; 32]) -> Result<()> {
        self.st.lock().unwrap().add_stranger(*account)
    }

    /// Declines an incoming call the settings do not allow; true = do not ring.
    pub(crate) fn screen_call(&self, ev: &Event) -> bool {
        let EventBody::Call {
            call_id,
            action: CallAction::Offer { .. } | CallAction::Invite { .. },
        } = &ev.body
        else {
            return false;
        };
        let (from, me) = (self.account_of(&ev.author_device), self.device_id());
        if from == self.account() {
            return false;
        }
        let ok = {
            let st = self.st.lock().unwrap();
            st.privacy().and_then(|p| st.allows(p.calls, &from, me))
        };
        if ok.unwrap_or(false) {
            return false;
        }
        if let Err(e) = self.call_reject(*call_id) {
            eprintln!("mycel: call reject failed: {e}");
        }
        true
    }

    fn read_mark_key(chat: &Chat) -> String {
        format!("read_mark:{}", hex(chat_key(chat)))
    }

    /// Whether Read receipts are off for this chat.
    pub(crate) fn hides_reads(&self, chat: &Chat) -> bool {
        chat.is_some() && !self.privacy().read_receipts
    }

    /// How far we read while receipts were off (lamport), kept here instead of a receipt.
    // shortcut: not synced to our other devices.
    pub(crate) fn read_mark(&self, chat: &Chat) -> Result<Option<u64>> {
        let st = self.st.lock().unwrap();
        let v = st.store.meta(&Self::read_mark_key(chat))?;
        Ok(v.and_then(|v| v.try_into().ok().map(u64::from_le_bytes)))
    }

    pub(crate) fn set_read_mark(&self, chat: &Chat, lamport: u64) -> Result<()> {
        let st = self.st.lock().unwrap();
        st.store
            .set_meta(&Self::read_mark_key(chat), &lamport.to_le_bytes())
    }
}
