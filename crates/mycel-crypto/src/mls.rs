//! MLS group (DESIGN.md §5): one group per space, state kept in a byte blob the caller persists.

use anyhow::{Context, Result, anyhow, ensure};
use openmls::prelude::tls_codec::{Deserialize, Serialize};
use openmls::prelude::*;
use openmls_basic_credential::SignatureKeyPair;
use openmls_memory_storage::MemoryStorage;
use openmls_rust_crypto::RustCrypto;
use openmls_traits::OpenMlsProvider;
use openmls_traits::types::SignatureScheme;

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

pub struct Mls {
    provider: Provider,
    signer: SignatureKeyPair,
    cred: CredentialWithKey,
    group: Option<MlsGroup>,
}

impl Mls {
    /// The MLS identity and signature key are the device key, so MLS sender == NodeId.
    /// `state` is a previous `state()` dump; `group_id` loads that group from it.
    pub fn new(
        key: &crate::SecretKey,
        state: Option<&[u8]>,
        group_id: Option<&[u8]>,
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
        let group = match group_id {
            Some(id) => Some(
                MlsGroup::load(&provider.storage, &GroupId::from_slice(id))?
                    .context("MLS group missing from state")?,
            ),
            None => None,
        };
        Ok(Self {
            provider,
            signer,
            cred,
            group,
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
        self.group = Some(g);
        Ok(())
    }

    /// Fresh KeyPackage to send to the admin; its private part stays in our state.
    pub fn key_package(&self) -> Result<Vec<u8>> {
        let kp =
            KeyPackage::builder().build(SUITE, &self.provider, &self.signer, self.cred.clone())?;
        Ok(kp.key_package().tls_serialize_detached()?)
    }

    /// Adds the KeyPackage owner. Returns (new member's device id, commit, welcome); commit already merged.
    pub fn add_member(&mut self, kp: &[u8]) -> Result<([u8; 32], Vec<u8>, Vec<u8>)> {
        let kp = KeyPackageIn::tls_deserialize_exact(kp)?
            .validate(self.provider.crypto(), ProtocolVersion::Mls10)?;
        let id = device(kp.leaf_node().credential())?;
        let g = self.group.as_mut().context("no group")?;
        let (commit, welcome, _) = g.add_members(&self.provider, &self.signer, &[kp])?;
        g.merge_pending_commit(&self.provider)?;
        Ok((id, commit.to_bytes()?, welcome.to_bytes()?))
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
        self.group = Some(g);
        Ok(id)
    }

    pub fn encrypt(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        let g = self.group.as_mut().context("no group")?;
        Ok(g.create_message(&self.provider, &self.signer, data)?
            .to_bytes()?)
    }

    /// Decrypts a message; commits are merged only if sent by `admin`.
    pub fn process(&mut self, msg: &[u8], admin: &[u8; 32]) -> Result<Incoming> {
        let g = self.group.as_mut().context("no group")?;
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
                ensure!(&sender == admin, "commit not from admin");
                g.merge_staged_commit(&self.provider, *c)?;
                Ok(Incoming::Commit)
            }
            _ => Err(anyhow!("unsupported MLS message")),
        }
    }

    /// MLS exporter secret of the current epoch (call frame keys, §9).
    pub fn export(&self, label: &str, context: &[u8], len: usize) -> Result<Vec<u8>> {
        let g = self.group.as_ref().context("no group")?;
        Ok(g.export_secret(self.provider.crypto(), label, context, len)?)
    }

    pub fn epoch(&self) -> u64 {
        self.group.as_ref().map_or(0, |g| g.epoch().as_u64())
    }

    pub fn has_group(&self) -> bool {
        self.group.is_some()
    }

    pub fn members(&self) -> Vec<[u8; 32]> {
        let Some(g) = &self.group else {
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
        let mut a = Mls::new(&ka, None, None).unwrap();
        a.create_group(&[7; 32]).unwrap();
        let mut b = Mls::new(&kb, None, None).unwrap();
        let (id, _commit, welcome) = a.add_member(&b.key_package().unwrap()).unwrap();
        assert_eq!(id, *kb.public().as_bytes());
        assert_eq!(b.join(&welcome).unwrap(), vec![7; 32]);
        assert_eq!(a.members().len(), 2);

        let ct = a.encrypt(b"secret").unwrap();
        assert!(!ct.windows(6).any(|w| w == b"secret"));
        let Incoming::App { sender, data } = b.process(&ct, &admin).unwrap() else {
            panic!("not app")
        };
        assert_eq!((sender, data.as_slice()), (admin, &b"secret"[..]));

        // Restart B from its dumped state.
        let mut b = Mls::new(&kb, Some(&b.state()), Some(&[7; 32])).unwrap();
        let ct = b.encrypt(b"back").unwrap();
        let Incoming::App { data, .. } = a.process(&ct, &admin).unwrap() else {
            panic!("not app")
        };
        assert_eq!(data, b"back");
    }
}
