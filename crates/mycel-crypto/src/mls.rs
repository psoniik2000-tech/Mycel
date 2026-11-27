//! MLS groups (DESIGN.md §5): the space group plus 1:1 chat groups, state kept in a byte blob the caller persists.

use anyhow::{Context, Result, anyhow, ensure};
use openmls::prelude::tls_codec::{Deserialize, Serialize};
use openmls::prelude::*;
use openmls_basic_credential::SignatureKeyPair;
use openmls_memory_storage::MemoryStorage;
use openmls_rust_crypto::RustCrypto;
use openmls_traits::OpenMlsProvider;
use openmls_traits::types::SignatureScheme;
use std::collections::HashMap;

const SUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;
/// Old-epoch app messages stay decryptable for a while (late delivery via sync).
const PAST_EPOCHS: usize = 8;

#[derive(Default)]
struct Provider {
    crypto: RustCrypto,
    storage: MemoryStorage,
}

impl OpenMlsProvider for Provider {
    type CryptoProvider = RustCrypto;
    type RandProvider = RustCrypto;
    type StorageProvider = MemoryStorage;
    fn storage(&self) -> &MemoryStorage {
        &self.storage
    }
    fn crypto(&self) -> &RustCrypto {
        &self.crypto
    }
    fn rand(&self) -> &RustCrypto {
        &self.crypto
    }
}

pub enum Incoming {
    /// Application data from a member (device id).
    App { sender: [u8; 32], data: Vec<u8> },
    /// A merged commit; epoch advanced.
    Commit,
}

/// (added device ids, commit, welcome).
pub type Added = (Vec<[u8; 32]>, Vec<u8>, Vec<u8>);

pub struct Mls {
    provider: Provider,
    signer: SignatureKeyPair,
    cred: CredentialWithKey,
    groups: HashMap<Vec<u8>, MlsGroup>,
}

impl Mls {
    /// The MLS identity and signature key are the device key, so MLS sender == NodeId.
    /// `state` is a previous `state()` dump; the groups `group_ids` are loaded from it.
    pub fn new(
        key: &crate::SecretKey,
        state: Option<&[u8]>,
        group_ids: &[Vec<u8>],
    ) -> Result<Self> {
        let provider = Provider::default();
        if let Some(s) = state {
            let kv: Vec<(Vec<u8>, Vec<u8>)> = postcard::from_bytes(s)?;
            *provider.storage.values.write().unwrap() = kv.into_iter().collect();
        }
        let pk = key.public().as_bytes().to_vec();
        let signer = SignatureKeyPair::from_raw(
            SignatureScheme::ED25519,
            key.to_bytes().to_vec(),
            pk.clone(),
        );
        let cred = CredentialWithKey {
            credential: BasicCredential::new(pk.clone()).into(),
            signature_key: pk.into(),
        };
        let mut groups = HashMap::new();
        for id in group_ids {
            let g = MlsGroup::load(&provider.storage, &GroupId::from_slice(id))?
                .context("MLS group missing from state")?;
            groups.insert(id.clone(), g);
        }
        Ok(Self {
            provider,
            signer,
            cred,
            groups,
        })
    }

    // shortcut: whole MLS store dumped after every operation; row-level StorageProvider on SQLCipher if it gets slow.
    pub fn state(&self) -> Vec<u8> {
        let values = self.provider.storage.values.read().unwrap();
        let kv: Vec<(&Vec<u8>, &Vec<u8>)> = values.iter().collect();
        postcard::to_allocvec(&kv).expect("state serializes")
    }

    pub fn create_group(&mut self, group_id: &[u8]) -> Result<()> {
        let g = MlsGroup::builder()
            .with_group_id(GroupId::from_slice(group_id))
            .ciphersuite(SUITE)
            .max_past_epochs(PAST_EPOCHS)
            .use_ratchet_tree_extension(true)
            .build(&self.provider, &self.signer, self.cred.clone())?;
        self.groups.insert(group_id.to_vec(), g);
        Ok(())
    }

    /// Fresh KeyPackage; its private part stays in our state. A last-resort one survives
    /// being used (the one in our profile, for anyone who opens a 1:1 chat with us).
    pub fn key_package(&self, last_resort: bool) -> Result<Vec<u8>> {
        let mut b = KeyPackage::builder();
        if last_resort {
            let caps =
                Capabilities::new(None, None, Some(&[ExtensionType::LastResort]), None, None);
            b = b.mark_as_last_resort().leaf_node_capabilities(caps);
        }
        let kp = b.build(SUITE, &self.provider, &self.signer, self.cred.clone())?;
        Ok(kp.key_package().tls_serialize_detached()?)
    }

    /// Adds the KeyPackages' owners in one commit. Returns (their device ids, commit, welcome);
    /// commit already merged.
    pub fn add_members(&mut self, gid: &[u8], kps: &[Vec<u8>]) -> Result<Added> {
        let mut ids = vec![];
        let mut v = vec![];
        for kp in kps {
            let kp = KeyPackageIn::tls_deserialize_exact(kp)?
                .validate(self.provider.crypto(), ProtocolVersion::Mls10)?;
            ids.push(device(kp.leaf_node().credential())?);
            v.push(kp);
        }
        let g = self.groups.get_mut(gid).context("no group")?;
        let (commit, welcome, _) = g.add_members(&self.provider, &self.signer, &v)?;
        g.merge_pending_commit(&self.provider)?;
        Ok((ids, commit.to_bytes()?, welcome.to_bytes()?))
    }

    /// Removes a member's leaves; returns the commit (already merged), `None` if not a member.
    pub fn remove_member(&mut self, gid: &[u8], dev: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        let g = self.groups.get_mut(gid).context("no group")?;
        let leaves: Vec<_> = g
            .members()
            .filter(|m| device(&m.credential).ok().as_ref() == Some(dev))
            .map(|m| m.index)
            .collect();
        if leaves.is_empty() {
            return Ok(None);
        }
        let (commit, _, _) = g.remove_members(&self.provider, &self.signer, &leaves)?;
        g.merge_pending_commit(&self.provider)?;
        Ok(Some(commit.to_bytes()?))
    }

    /// Joins from a Welcome; returns the group id.
    pub fn join(&mut self, welcome: &[u8]) -> Result<Vec<u8>> {
        let MlsMessageBodyIn::Welcome(w) = MlsMessageIn::tls_deserialize_exact(welcome)?.extract()
        else {
            return Err(anyhow!("not a welcome"));
        };
        let cfg = MlsGroupJoinConfig::builder()
            .max_past_epochs(PAST_EPOCHS)
            .use_ratchet_tree_extension(true)
            .build();
        let g = StagedWelcome::new_from_welcome(&self.provider, &cfg, w, None)?
            .into_group(&self.provider)?;
        let id = g.group_id().as_slice().to_vec();
        self.groups.insert(id.clone(), g);
        Ok(id)
    }

    pub fn encrypt(&mut self, gid: &[u8], data: &[u8]) -> Result<Vec<u8>> {
        let g = self.groups.get_mut(gid).context("no group")?;
        Ok(g.create_message(&self.provider, &self.signer, data)?
            .to_bytes()?)
    }

    /// Decrypts a message; commits are merged only if sent by `admin` (any member when `None`).
    pub fn process(
        &mut self,
        gid: &[u8],
        msg: &[u8],
        admin: Option<&[u8; 32]>,
    ) -> Result<Incoming> {
        let g = self.groups.get_mut(gid).context("no group")?;
        let m = MlsMessageIn::tls_deserialize_exact(msg)?.try_into_protocol_message()?;
        let p = g.process_message(&self.provider, m)?;
        let sender = device(p.credential())?;
        match p.into_content() {
            ProcessedMessageContent::ApplicationMessage(a) => Ok(Incoming::App {
                sender,
                data: a.into_bytes(),
            }),
            ProcessedMessageContent::StagedCommitMessage(c) => {
                // shortcut: single admin (space creator), so no epoch conflicts; add smaller-hash-wins (§5) with multiple admins.
                ensure!(admin.is_none_or(|a| *a == sender), "commit not from admin");
                g.merge_staged_commit(&self.provider, *c)?;
                Ok(Incoming::Commit)
            }
            _ => Err(anyhow!("unsupported MLS message")),
        }
    }

    /// MLS exporter secret of the current epoch (call frame keys, §9).
    pub fn export(&self, gid: &[u8], label: &str, context: &[u8], len: usize) -> Result<Vec<u8>> {
        let g = self.groups.get(gid).context("no group")?;
        Ok(g.export_secret(self.provider.crypto(), label, context, len)?)
    }

    pub fn epoch(&self, gid: &[u8]) -> u64 {
        self.groups.get(gid).map_or(0, |g| g.epoch().as_u64())
    }

    pub fn members(&self, gid: &[u8]) -> Vec<[u8; 32]> {
        let Some(g) = self.groups.get(gid) else {
            return vec![];
        };
        g.members()
            .filter_map(|m| device(&m.credential).ok())
            .collect()
    }
}

fn device(c: &Credential) -> Result<[u8; 32]> {
    c.serialized_content()
        .try_into()
        .ok()
        .context("bad credential")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SecretKey;

    #[test]
    fn add_encrypt_restart() {
        let (ka, kb) = (SecretKey::generate(), SecretKey::generate());
        let admin = *ka.public().as_bytes();
        let g = &[7; 32];
        let mut a = Mls::new(&ka, None, &[]).unwrap();
        a.create_group(g).unwrap();
        let mut b = Mls::new(&kb, None, &[]).unwrap();
        let (id, _commit, welcome) = a.add_members(g, &[b.key_package(false).unwrap()]).unwrap();
        assert_eq!(id, vec![*kb.public().as_bytes()]);
        assert_eq!(b.join(&welcome).unwrap(), g.to_vec());
        assert_eq!(a.members(g).len(), 2);

        let ct = a.encrypt(g, b"secret").unwrap();
        assert!(!ct.windows(6).any(|w| w == b"secret"));
        let Incoming::App { sender, data } = b.process(g, &ct, Some(&admin)).unwrap() else {
            panic!("not app")
        };
        assert_eq!((sender, data.as_slice()), (admin, &b"secret"[..]));

        // Restart B from its dumped state.
        let mut b = Mls::new(&kb, Some(&b.state()), &[g.to_vec()]).unwrap();
        let ct = b.encrypt(g, b"back").unwrap();
        let Incoming::App { data, .. } = a.process(g, &ct, Some(&admin)).unwrap() else {
            panic!("not app")
        };
        assert_eq!(data, b"back");

        // A last-resort KeyPackage (profile) lets several groups add B.
        let kp = b.key_package(true).unwrap();
        for gid in [[8; 32], [9; 32]] {
            a.create_group(&gid).unwrap();
            let (_, _, w) = a.add_members(&gid, std::slice::from_ref(&kp)).unwrap();
            assert_eq!(b.join(&w).unwrap(), gid.to_vec());
        }
        // Removal: B is gone from A's view.
        assert!(
            a.remove_member(&[9; 32], &kb.public().as_bytes().clone())
                .unwrap()
                .is_some()
        );
        assert_eq!(a.members(&[9; 32]).len(), 1);
    }
}
