use std::{
    cell::RefCell,
    collections::HashMap,
    fmt::{Display, Formatter, Write},
    fs,
    fs::File,
    io::Write as IoWrite,
    path::Path,
    sync::Arc,
};

use hex::FromHex;
use sequoia_openpgp::{
    crypto::SessionKey,
    parse::{
        stream::{
            DecryptionHelper, DecryptorBuilder, DetachedVerifierBuilder, MessageLayer,
            MessageStructure, VerificationHelper,
        },
        Parse,
    },
    serialize::{
        stream::{Armorer, Encryptor, LiteralWriter, Message, Signer},
        Serialize,
    },
    types::{RevocationStatus, SymmetricAlgorithm},
    Cert, KeyHandle,
};

pub use crate::error::{Error, Result};
use crate::{
    crypto::VerificationError::InfrastructureError,
    pass::OwnerTrustLevel,
    signature::{KeyRingStatus, Recipient, SignatureStatus},
};

/// The different pgp implementations we support
#[non_exhaustive]
#[derive(PartialEq, Eq, Debug)]
pub enum CryptoImpl {
    /// Implemented with the help of the gpgme crate
    GpgMe,
    /// Implemented with the help of the sequoia crate
    Sequoia,
}

impl std::convert::TryFrom<&str> for CryptoImpl {
    type Error = Error;

    fn try_from(value: &str) -> Result<Self> {
        match value {
            "gpg" => Ok(Self::GpgMe),
            "sequoia" => Ok(Self::Sequoia),
            _ => Err(Error::Generic(
                "unknown pgp implementation value, valid values are 'gpg' and 'sequoia'",
            )),
        }
    }
}

impl Display for CryptoImpl {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        match self {
            Self::GpgMe => write!(f, "gpg"),
            Self::Sequoia => write!(f, "sequoia"),
        }?;
        Ok(())
    }
}

/// The different types of errors that can occur when doing a signature verification
#[non_exhaustive]
#[derive(Debug)]
pub enum VerificationError {
    /// Error message from the pgp library.
    InfrastructureError(String),
    /// The data was signed, but not from one of the supplied recipients.
    SignatureFromWrongRecipient,
    /// The signature was invalid,
    BadSignature,
    /// No signature found.
    MissingSignatures,
    /// More than one signature, this shouldn't happen and can indicate that someone have tried
    /// to trick the process by appending an additional signature.
    TooManySignatures,
}

impl From<std::io::Error> for VerificationError {
    fn from(err: std::io::Error) -> Self {
        InfrastructureError(format!("{err:?}"))
    }
}

impl From<crate::error::Error> for VerificationError {
    fn from(err: crate::error::Error) -> Self {
        InfrastructureError(format!("{err:?}"))
    }
}

impl From<anyhow::Error> for VerificationError {
    fn from(err: anyhow::Error) -> Self {
        InfrastructureError(format!("{err:?}"))
    }
}

/// The strategy for finding the gpg key to sign with can either be to look at the git
/// config, or ask gpg.
#[non_exhaustive]
pub enum FindSigningFingerprintStrategy {
    /// Will look at the git configuration to find the users fingerprint
    GIT,
    /// Will ask gpg to find the users fingerprint
    GPG,
}

/// TODO
pub enum FindKeyError {
    /// Decrypting the key failed with the given password. Either no password was given
    /// where one was needed, a password was given where none was needed, or a password
    /// was given but failed to decrypt the key.
    WrongPassword,
    /// An error other than a wrong or missing password occurred.
    Other(Error),
}

impl FindKeyError {
    pub fn unwrap_other(self) -> Error {
        match self {
            FindKeyError::WrongPassword => {
                panic!("tried to unwrap_other when the error was WrongPassword")
            }
            FindKeyError::Other(err) => err,
        }
    }
}

impl<T: Into<Error>> From<T> for FindKeyError {
    fn from(err: T) -> Self {
        FindKeyError::Other(err.into())
    }
}

/// Models the interactions that can be done on a pgp key
pub trait Key {
    /// returns a list of names associated with the key
    fn user_id_names(&self) -> Vec<String>;

    /// returns the keys fingerprint
    fn fingerprint(&self) -> Result<[u8; 20]>;

    /// returns if the key isn't usable
    fn is_not_usable(&self) -> bool;
}

/// A key gotten from gpgme
pub struct GpgMeKey {
    /// The key, gotten from gpgme.
    key: gpgme::Key,
}

impl Key for GpgMeKey {
    fn user_id_names(&self) -> Vec<String> {
        self.key
            .user_ids()
            .map(|user_id| user_id.name().unwrap_or("?").to_owned())
            .collect()
    }

    fn fingerprint(&self) -> Result<[u8; 20]> {
        let fp = self.key.fingerprint()?;

        Ok(<[u8; 20]>::from_hex(fp)?)
    }

    fn is_not_usable(&self) -> bool {
        self.key.is_bad()
            || self.key.is_revoked()
            || self.key.is_expired()
            || self.key.is_disabled()
            || self.key.is_invalid()
    }
}

/// All operations that can be done through pgp, either with gpgme or sequoia.
pub trait Crypto {
    /// Reads a file and decrypts it
    /// # Errors
    /// Will return `Err` if decryption fails, for example if the current user isn't the
    /// recipient of the message.
    fn decrypt_string(&self, ciphertext: &[u8]) -> Result<String>;
    /// Encrypts a string
    /// # Errors
    /// Will return `Err` if encryption fails, for example if the current users key
    /// isn't capable of encrypting.
    fn encrypt_string(&self, plaintext: &str, recipients: &[Recipient]) -> Result<Vec<u8>>;

    /// Returns a gpg signature for the supplied string. Suitable to add to a gpg commit.
    /// # Errors
    /// Will return `Err` if signing fails, for example if the current users key
    /// isn't capable of signing.
    fn sign_string(
        &self,
        to_sign: &str,
        valid_gpg_signing_keys: &[[u8; 20]],
        strategy: &FindSigningFingerprintStrategy,
    ) -> Result<String>;

    /// Verifies is a signature is valid
    /// # Errors
    /// Will return `Err` if the verifican fails.
    fn verify_sign(
        &self,
        data: &[u8],
        sig: &[u8],
        valid_signing_keys: &[[u8; 20]],
    ) -> Result<SignatureStatus, VerificationError>;

    /// Returns true if a recipient is in the users keyring.
    fn is_key_in_keyring(&self, recipient: &Recipient) -> Result<bool>;

    /// Pull keys from the keyserver for those recipients.
    /// # Errors
    /// Will return `Err` on network errors and similar.
    fn pull_keys(&mut self, recipients: &[&Recipient], config_path: &Path) -> Result<String>;

    /// Import a key from text.
    /// # Errors
    /// Will return `Err` if the text wasn't able to be imported as a key.
    fn import_key(&mut self, key: &str, config_path: &Path) -> Result<String>;

    /// Return a key corresponding to the given key id.
    /// # Errors
    /// Will return `Err` if `key_id` didn't correspond to a key.
    fn get_key(&self, key_id: &str) -> Result<Box<dyn crate::crypto::Key>>;

    /// Returns a map from key fingerprints to OwnerTrustLevel's
    /// # Errors
    /// Will return `Err` on failure to obtain trust levels.
    fn get_all_trust_items(&self) -> Result<HashMap<[u8; 20], crate::signature::OwnerTrustLevel>>;

    /// Returns the type of this `CryptoImpl`, useful for serializing the store config
    fn implementation(&self) -> CryptoImpl;

    /// Returns the fingerprint of the user using ripasso
    fn own_fingerprint(&self) -> Option<[u8; 20]>;

    /// TODO
    fn find_key(&self, password: Option<String>) -> Result<(), FindKeyError>;
}

/// Used when the user configures gpgme to be used as a pgp backend.
#[non_exhaustive]
pub struct GpgMe {}

impl Crypto for GpgMe {
    fn decrypt_string(&self, ciphertext: &[u8]) -> Result<String> {
        let mut ctx = gpgme::Context::from_protocol(gpgme::Protocol::OpenPgp)?;
        let mut output = Vec::new();
        ctx.decrypt(ciphertext, &mut output)?;
        Ok(String::from_utf8(output)?)
    }

    fn encrypt_string(&self, plaintext: &str, recipients: &[Recipient]) -> Result<Vec<u8>> {
        let mut ctx = gpgme::Context::from_protocol(gpgme::Protocol::OpenPgp)?;
        ctx.set_armor(false);

        let mut keys = Vec::new();
        for recipient in recipients {
            if recipient.key_ring_status == KeyRingStatus::NotInKeyRing {
                return Err(Error::RecipientNotInKeyRing(recipient.key_id.clone()));
            }
            keys.push(ctx.get_key(recipient.key_id.clone())?);
        }

        let mut output = Vec::new();
        ctx.encrypt_with_flags(
            &keys,
            plaintext,
            &mut output,
            gpgme::EncryptFlags::NO_COMPRESS,
        )?;

        Ok(output)
    }

    fn sign_string(
        &self,
        to_sign: &str,
        valid_gpg_signing_keys: &[[u8; 20]],
        strategy: &FindSigningFingerprintStrategy,
    ) -> Result<String> {
        let mut ctx = gpgme::Context::from_protocol(gpgme::Protocol::OpenPgp)?;

        let config = git2::Config::open_default()?;

        let signing_key = match strategy {
            FindSigningFingerprintStrategy::GIT => config.get_string("user.signingkey")?,
            FindSigningFingerprintStrategy::GPG => {
                let mut key_opt: Option<gpgme::Key> = None;

                for key_id in valid_gpg_signing_keys {
                    let key_res = ctx.get_key(hex::encode_upper(key_id));

                    if let Ok(r) = key_res {
                        key_opt = Some(r);
                    }
                }

                if let Some(key) = key_opt {
                    key.fingerprint()?.to_owned()
                } else {
                    return Err(Error::Generic("no valid signing key"));
                }
            }
        };

        ctx.set_armor(true);
        let key = ctx.get_secret_key(signing_key)?;

        ctx.add_signer(&key)?;
        let mut output = Vec::new();
        let signature = ctx.sign_detached(to_sign, &mut output);

        if let Err(e) = signature {
            return Err(Error::Gpg(e));
        }

        Ok(String::from_utf8(output)?)
    }

    fn verify_sign(
        &self,
        data: &[u8],
        sig: &[u8],
        valid_signing_keys: &[[u8; 20]],
    ) -> Result<SignatureStatus, VerificationError> {
        let mut ctx = gpgme::Context::from_protocol(gpgme::Protocol::OpenPgp)
            .map_err(|e| VerificationError::InfrastructureError(format!("{e:?}")))?;

        let result = ctx
            .verify_detached(sig, data)
            .map_err(|e| VerificationError::InfrastructureError(format!("{e:?}")))?;

        let mut sig_sum = None;

        for (i, s) in result.signatures().enumerate() {
            let fpr = s
                .fingerprint()
                .map_err(|e| VerificationError::InfrastructureError(format!("{e:?}")))?;

            let fpr = <[u8; 20]>::from_hex(fpr)
                .map_err(|e| VerificationError::InfrastructureError(format!("{e:?}")))?;

            if !valid_signing_keys.contains(&fpr) {
                return Err(VerificationError::SignatureFromWrongRecipient);
            }
            if i == 0 {
                sig_sum = Some(s.summary());
            } else {
                return Err(VerificationError::TooManySignatures);
            }
        }

        match sig_sum {
            None => Err(VerificationError::MissingSignatures),
            Some(sig_sum) => {
                let sig_status: SignatureStatus = sig_sum.into();
                match sig_status {
                    SignatureStatus::Bad => Err(VerificationError::BadSignature),
                    SignatureStatus::Good | SignatureStatus::AlmostGood => Ok(sig_status),
                }
            }
        }
    }

    fn is_key_in_keyring(&self, recipient: &Recipient) -> Result<bool> {
        let mut ctx = gpgme::Context::from_protocol(gpgme::Protocol::OpenPgp)?;

        if let Some(fingerprint) = recipient.fingerprint {
            Ok(ctx.get_key(hex::encode(fingerprint)).is_ok())
        } else {
            Ok(false)
        }
    }

    fn pull_keys(&mut self, recipients: &[&Recipient], _config_path: &Path) -> Result<String> {
        let mut ctx = gpgme::Context::from_protocol(gpgme::Protocol::OpenPgp)?;

        let mut result_str = String::new();
        for recipient in recipients {
            let response = download_keys(&recipient.key_id)?;

            let result = ctx.import(response)?;

            write!(
                result_str,
                "{}: import result: {:?}\n\n",
                recipient.key_id, result
            )?;
        }

        Ok(result_str)
    }

    fn import_key(&mut self, key: &str, _config_path: &Path) -> Result<String> {
        let mut ctx = gpgme::Context::from_protocol(gpgme::Protocol::OpenPgp)?;

        let result = ctx.import(key)?;

        let result_str = format!("Import result: {result:?}\n\n");

        Ok(result_str)
    }

    fn get_key(&self, key_id: &str) -> Result<Box<dyn crate::crypto::Key>> {
        let mut ctx = gpgme::Context::from_protocol(gpgme::Protocol::OpenPgp)?;

        Ok(Box::new(GpgMeKey {
            key: ctx.get_key(key_id)?,
        }))
    }

    fn get_all_trust_items(&self) -> Result<HashMap<[u8; 20], crate::signature::OwnerTrustLevel>> {
        let mut ctx = gpgme::Context::from_protocol(gpgme::Protocol::OpenPgp)?;
        ctx.set_key_list_mode(gpgme::KeyListMode::SIGS)?;

        let keys = ctx.find_keys(vec![String::new()])?;

        let mut trusts = HashMap::new();
        for key_res in keys {
            let key = key_res?;
            trusts.insert(
                <[u8; 20]>::from_hex(key.fingerprint()?)?,
                crate::signature::OwnerTrustLevel::from(&key.owner_trust()),
            );
        }

        Ok(trusts)
    }

    fn implementation(&self) -> CryptoImpl {
        CryptoImpl::GpgMe
    }

    fn own_fingerprint(&self) -> Option<[u8; 20]> {
        None
    }

    fn find_key(&self, _password: Option<String>) -> Result<(), FindKeyError> {
        Ok(())
    }
}

/// Tries to download keys from keys.openpgp.org
fn download_keys(recipient_key_id: &str) -> Result<String> {
    let url = match recipient_key_id.len() {
        16 => format!("https://keys.openpgp.org/vks/v1/by-keyid/{recipient_key_id}"),
        18 if recipient_key_id.starts_with("0x") => format!(
            "https://keys.openpgp.org/vks/v1/by-keyid/{}",
            &recipient_key_id[2..]
        ),
        40 => format!("https://keys.openpgp.org/vks/v1/by-fingerprint/{recipient_key_id}"),
        42 if recipient_key_id.starts_with("0x") => format!(
            "https://keys.openpgp.org/vks/v1/by-fingerprint/{}",
            &recipient_key_id[2..]
        ),
        _ => return Err(Error::Generic("key id is not 16 or 40 hex chars")),
    };

    Ok(reqwest::blocking::get(url)?.text()?)
}

/// Internal helper struct for sequoia implementation.
struct Helper<'a> {
    /// The keypair for decrypting passwords
    keypair: Option<sequoia_openpgp::crypto::KeyPair>,
    /// all certs
    key_ring: &'a HashMap<[u8; 20], Arc<sequoia_openpgp::Cert>>,
    /// This is all the certificates that are allowed to sign something
    public_keys: Vec<Arc<sequoia_openpgp::Cert>>,
    /// context if talking to gpg_agent for example
    ctx: Option<sequoia_ipc::gnupg::Context>,
    /// to do verification or not
    do_signature_verification: bool,
}

impl<'a> VerificationHelper for Helper<'a> {
    fn get_certs(
        &mut self,
        handles: &[sequoia_openpgp::KeyHandle],
    ) -> sequoia_openpgp::Result<Vec<sequoia_openpgp::Cert>> {
        let mut certs = vec![];

        for handle in handles {
            for cert in &self.public_keys {
                for c in cert.keys() {
                    if c.key_handle().aliases(handle) {
                        certs.push(cert.as_ref().clone());
                    }
                }
            }
        }
        // Return public keys for signature verification here.
        Ok(certs)
    }

    fn check(&mut self, structure: MessageStructure) -> sequoia_openpgp::Result<()> {
        if !self.do_signature_verification {
            return Ok(());
        }

        for layer in structure {
            if let MessageLayer::SignatureGroup { results } = layer {
                if results.iter().any(Result::is_ok) {
                    return Ok(());
                }
            }
        }
        Err(anyhow::anyhow!("No valid signature"))
    }
}

fn find(
    key_ring: &HashMap<[u8; 20], Arc<sequoia_openpgp::Cert>>,
    recipient: &sequoia_openpgp::KeyID,
) -> Result<Arc<sequoia_openpgp::Cert>> {
    let bytes: &[u8; 8] = match recipient {
        sequoia_openpgp::KeyID::V4(bytes) => bytes,
        _ => return Err(Error::Generic("not an v4 keyid")),
    };

    for (key, value) in key_ring {
        if key[0..8] == *bytes {
            return Ok(value.clone());
        }
    }

    Err(Error::Generic("key not found in keyring"))
}

impl<'a> DecryptionHelper for Helper<'a> {
    fn decrypt<D>(
        &mut self,
        pkesks: &[sequoia_openpgp::packet::PKESK],
        _skesks: &[sequoia_openpgp::packet::SKESK],
        sym_algo: Option<SymmetricAlgorithm>,
        mut decrypt: D,
    ) -> sequoia_openpgp::Result<Option<sequoia_openpgp::Fingerprint>>
    where
        D: FnMut(SymmetricAlgorithm, &SessionKey) -> bool,
    {
        let pair = if let Some(pair) = &mut self.keypair {
            pair
        } else {
            // we don't know which key is the users own key, so lets try them all

            let mut selected_fingerprint: Option<sequoia_openpgp::Fingerprint> = None;
            for pkesk in pkesks {
                if let Ok(cert) = find(self.key_ring, pkesk.recipient()) {
                    let key = cert.primary_key();
                    let mut pair = sequoia_ipc::gnupg::KeyPair::new(
                        self.ctx
                            .as_ref()
                            .ok_or_else(|| anyhow::anyhow!("no context configured"))?,
                        &*key,
                    )?;
                    if pkesk
                        .decrypt(&mut pair, sym_algo)
                        .map_or(false, |(algo, session_key)| decrypt(algo, &session_key))
                    {
                        selected_fingerprint = Some(cert.fingerprint());
                        break;
                    }
                }
            }

            return Ok(selected_fingerprint);
        };

        for pkesk in pkesks {
            if pkesk
                .decrypt(pair, sym_algo)
                .map(|(algo, sk)| decrypt(algo, &sk))
                .unwrap_or(false)
            {
                return Ok(Some(pair.public().fingerprint()));
            }
        }

        Err(anyhow::anyhow!("no pkesks managed to descrypt ciphertext"))
    }
}

/// Intended for usage with slices containing a v4 fingerprint.
pub fn slice_to_20_bytes(b: &[u8]) -> Result<[u8; 20]> {
    if b.len() != 20 {
        return Err(Error::Generic("slice isn't 20 bytes"));
    }

    let mut f: [u8; 20] = [0; 20];
    f.copy_from_slice(&b[0..20]);

    Ok(f)
}

/// A pgp key produced with sequoia.
pub struct SequoiaKey {
    /// The pgp key
    cert: sequoia_openpgp::Cert,
}

impl Key for SequoiaKey {
    fn user_id_names(&self) -> Vec<String> {
        self.cert.userids().map(|ui| ui.to_string()).collect()
    }

    fn fingerprint(&self) -> Result<[u8; 20]> {
        slice_to_20_bytes(self.cert.fingerprint().as_bytes())
    }

    fn is_not_usable(&self) -> bool {
        let p = sequoia_openpgp::policy::StandardPolicy::new();

        let policy = match self.cert.with_policy(&p, None) {
            Err(_) => return true,
            Ok(p) => p,
        };

        self.cert.revocation_status(&p, None) != RevocationStatus::NotAsFarAsWeKnow
            || policy.alive().is_err()
    }
}

/// If the users configures to use sequoia as their pgp implementation.
pub struct Sequoia {
    /// key id of the user.
    user_key_id: [u8; 20],
    /// All certs in the keys directory
    key_ring: HashMap<[u8; 20], Arc<sequoia_openpgp::Cert>>,
    /// The keypair used for this store if it's been found and, if applicable, decrypted
    keypair: RefCell<Option<sequoia_openpgp::crypto::KeyPair>>,
    /// The home directory of the user, for gnupg context
    user_home: std::path::PathBuf,
}

impl Sequoia {
    /// creates the sequoia object
    /// # Errors
    /// If there is any problems reading the keys directory
    pub fn new(config_path: &Path, own_fingerprint: [u8; 20], user_home: &Path) -> Result<Self> {
        let mut key_ring: HashMap<[u8; 20], Arc<sequoia_openpgp::Cert>> = HashMap::new();

        let dir = config_path.join("share").join("ripasso").join("keys");
        if dir.exists() {
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_file() {
                    let data = fs::read(path)?;
                    let cert = Cert::from_bytes(&data)?;

                    let fingerprint = slice_to_20_bytes(cert.fingerprint().as_bytes())?;
                    key_ring.insert(fingerprint, Arc::new(cert));
                }
            }
        }

        Ok(Self {
            user_key_id: own_fingerprint,
            key_ring,
            keypair: RefCell::new(None),
            user_home: user_home.to_path_buf(),
        })
    }

    pub fn from_values(
        user_key_id: [u8; 20],
        key_ring: HashMap<[u8; 20], Arc<sequoia_openpgp::Cert>>,
        // TODO: this function is used for testing. add decryption_key to params.
        user_home: &Path,
    ) -> Self {
        Self {
            user_key_id,
            key_ring,
            keypair: RefCell::new(None),
            user_home: user_home.to_path_buf(),
        }
    }

    /// Converts a list of recipients to their sequoia certs
    /// # Errors
    /// errors if not all recipients have certs
    fn convert_recipients(&self, input: &[Recipient]) -> Result<Vec<Arc<sequoia_openpgp::Cert>>> {
        let mut result = vec![];

        for recipient in input {
            match recipient.fingerprint {
                Some(fp) => match self.key_ring.get(&fp) {
                    Some(cert) => result.push(cert.clone()),
                    None => {
                        return Err(Error::GenericDyn(format!(
                            "Recipient with key id {} not found",
                            recipient.key_id
                        )))
                    }
                },
                None => {
                    let kh: sequoia_openpgp::KeyHandle = recipient.key_id.parse()?;

                    for cert in self.key_ring.values() {
                        if cert.key_handle().aliases(&kh) {
                            result.push(cert.clone());
                        }
                    }
                }
            };
        }

        Ok(result)
    }

    /// Download keys from the internet and write them to the keys dir.
    /// # Errors
    /// Errors on download problems
    fn pull_and_write(&mut self, key_id: &str, keys_dir: &Path) -> Result<String> {
        let response = download_keys(key_id)?;

        self.write_cert(&response, keys_dir)
    }

    /// Writes a key to the keys directory, imported from a string.
    /// # Errors
    /// Errors if the string can't be parsed as a cert.
    fn write_cert(&mut self, cert_str: &str, keys_dir: &Path) -> Result<String> {
        let cert = Cert::from_bytes(cert_str.as_bytes())?;

        let fingerprint = slice_to_20_bytes(cert.fingerprint().as_bytes())?;

        let mut file = File::create(keys_dir.join(hex::encode(fingerprint)))?;

        cert.serialize(&mut file)?;

        self.key_ring.insert(fingerprint, Arc::new(cert));

        Ok("Downloaded ok".to_owned())
    }

    /// TODO
    fn own_decrypt_cert(&self) -> Result<Arc<sequoia_openpgp::Cert>> {
        let decrypt_cert = self
            .key_ring
            .get(&self.user_key_id)
            .ok_or(Error::Generic("no key for user found"))?;

        Ok(decrypt_cert.clone())
    }

    /// TODO
    fn own_decrypt_key(
        &self,
    ) -> Result<
        sequoia_openpgp::packet::Key<
            sequoia_openpgp::packet::key::SecretParts,
            sequoia_openpgp::packet::key::UnspecifiedRole,
        >,
    > {
        // TODO: just roll this into find_key

        let p = sequoia_openpgp::policy::StandardPolicy::new();

        let decrypt_key = self
            .own_decrypt_cert()?
            .keys()
            .secret()
            .with_policy(&p, None)
            .for_transport_encryption()
            .next()
            .ok_or(Error::Generic("no keys capable of encryption"))?
            .key()
            .clone();

        Ok(decrypt_key)
    }
}

impl Crypto for Sequoia {
    fn decrypt_string(&self, ciphertext: &[u8]) -> Result<String> {
        let p = sequoia_openpgp::policy::StandardPolicy::new();

        let mut sink: Vec<u8> = vec![];

        if let Some(keypair) = self.keypair.borrow().clone() {
            // Make a helper that that feeds the recipient's secret key to the
            // decryptor.
            let helper = Helper {
                keypair: Some(keypair),
                key_ring: &self.key_ring,
                public_keys: vec![],
                ctx: None,
                do_signature_verification: false,
            };

            // Now, create a decryptor with a helper using the given Certs.
            let mut decryptor = DecryptorBuilder::from_bytes(ciphertext)?
                .with_policy(&p, None, helper)
                .unwrap();

            // Decrypt the data.
            std::io::copy(&mut decryptor, &mut sink).unwrap();

            Ok(std::str::from_utf8(&sink).unwrap().to_owned())
        } else {
            // Make a helper that that feeds the recipient's secret key to the
            // decryptor.
            let helper = Helper {
                keypair: None,
                key_ring: &self.key_ring,
                public_keys: vec![],
                ctx: Some(sequoia_ipc::gnupg::Context::with_homedir(&self.user_home)?),
                do_signature_verification: false,
            };

            // Now, create a decryptor with a helper using the given Certs.
            let mut decryptor =
                DecryptorBuilder::from_bytes(ciphertext)?.with_policy(&p, None, helper)?;

            // Decrypt the data.
            std::io::copy(&mut decryptor, &mut sink)?;

            Ok(std::str::from_utf8(&sink)?.to_owned())
        }
    }

    fn encrypt_string(&self, plaintext: &str, recipients: &[Recipient]) -> Result<Vec<u8>> {
        let p = sequoia_openpgp::policy::StandardPolicy::new();

        let mut recipient_keys = vec![];
        let cr = self.convert_recipients(recipients)?;
        for r in &cr {
            for k in r
                .keys()
                .with_policy(&p, None)
                .supported()
                .alive()
                .revoked(false)
                .for_transport_encryption()
            {
                recipient_keys.push(k);
            }
        }

        let mut sink: Vec<u8> = vec![];

        // Start streaming an OpenPGP message.
        let message = Message::new(&mut sink);

        // We want to encrypt a literal data packet.
        let message = Encryptor::for_recipients(message, recipient_keys).build()?;

        // Emit a literal data packet.
        let mut message = LiteralWriter::new(message).build()?;

        // Encrypt the data.
        message.write_all(plaintext.as_bytes())?;

        // Finalize the OpenPGP message to make sure that all data is
        // written.
        message.finalize()?;

        Ok(sink)
    }

    fn sign_string(
        &self,
        to_sign: &str,
        _valid_gpg_signing_keys: &[[u8; 20]],
        _strategy: &FindSigningFingerprintStrategy,
    ) -> Result<String> {
        let p = sequoia_openpgp::policy::StandardPolicy::new();

        let tsk = self
            .key_ring
            .get(&self.user_key_id)
            .ok_or(Error::Generic("no key for user found"))?;

        // Get the keypair to do the signing from the Cert.
        let keypair = tsk
            .keys()
            .unencrypted_secret()
            .with_policy(&p, None)
            .alive()
            .revoked(false)
            .for_signing()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no cert valid for signing"))?
            .key()
            .clone()
            .into_keypair()?;

        let mut sink: Vec<u8> = vec![];

        // Start streaming an OpenPGP message.
        let message = Message::new(&mut sink);

        let message = Armorer::new(message)
            .kind(sequoia_openpgp::armor::Kind::Signature)
            .build()?;

        // We want to sign a literal data packet.
        let mut message = Signer::new(message, keypair).detached().build()?;

        // Sign the data.
        message.write_all(to_sign.as_bytes())?;

        // Finalize the OpenPGP message to make sure that all data is
        // written.
        message.finalize()?;

        Ok(std::str::from_utf8(&sink)?.to_owned())
    }

    fn verify_sign(
        &self,
        data: &[u8],
        sig: &[u8],
        valid_signing_keys: &[[u8; 20]],
    ) -> Result<SignatureStatus, VerificationError> {
        let p = sequoia_openpgp::policy::StandardPolicy::new();

        let recipients: Vec<Recipient> = if valid_signing_keys.is_empty() {
            self.key_ring
                .keys()
                .map(|k| Recipient::from(&hex::encode(k), &[], None, self))
                .collect::<Result<Vec<Recipient>>>()?
        } else {
            valid_signing_keys
                .iter()
                .map(|k| Recipient::from(&hex::encode_upper(k), &[], None, self))
                .collect::<Result<Vec<Recipient>>>()?
        };
        let senders = self.convert_recipients(&recipients)?;

        // Make a helper that that feeds the sender's public key to the
        // verifier.
        let helper = Helper {
            keypair: None,
            key_ring: &self.key_ring,
            public_keys: senders,
            ctx: None,
            do_signature_verification: true,
        };

        // Now, create a verifier with a helper using the given Certs.
        let mut verifier =
            DetachedVerifierBuilder::from_bytes(sig)?.with_policy(&p, None, helper)?;

        // Verify the data.
        verifier.verify_bytes(data)?;

        Ok(SignatureStatus::Good)
    }

    fn is_key_in_keyring(&self, recipient: &Recipient) -> Result<bool> {
        if let Some(fingerprint) = recipient.fingerprint {
            Ok(self.key_ring.contains_key(&fingerprint))
        } else {
            Ok(false)
        }
    }

    fn pull_keys(&mut self, recipients: &[&Recipient], config_path: &Path) -> Result<String> {
        let p = config_path.join("share").join("ripasso").join("keys");
        std::fs::create_dir_all(&p)?;

        let mut ret = String::new();
        for recipient in recipients {
            let res = self.pull_and_write(&recipient.key_id, &p);

            write!(ret, "{}: ", &recipient.key_id)?;
            match res {
                Ok(s) => ret.push_str(&s),
                Err(err) => write!(ret, "{err:?}")?,
            }
            ret.push('\n');
        }

        Ok(ret)
    }

    fn import_key(&mut self, key: &str, config_path: &Path) -> Result<String> {
        let p = config_path.join("share").join("ripasso").join("keys");
        std::fs::create_dir_all(&p)?;

        self.write_cert(key, &p)
    }

    fn get_key(&self, key_id: &str) -> Result<Box<dyn Key>> {
        let kh: KeyHandle = key_id.parse()?;
        for c in self.key_ring.values() {
            if c.key_handle() == kh {
                return Ok(Box::new(SequoiaKey {
                    cert: c.as_ref().clone(),
                }));
            }
        }

        Err(Error::GenericDyn(format!("no key found for {key_id}")))
    }

    fn get_all_trust_items(&self) -> Result<HashMap<[u8; 20], OwnerTrustLevel>> {
        let mut res: HashMap<[u8; 20], OwnerTrustLevel> = HashMap::new();

        for k in self.key_ring.keys() {
            res.insert(*k, OwnerTrustLevel::Ultimate);
        }

        Ok(res)
    }

    fn implementation(&self) -> CryptoImpl {
        CryptoImpl::Sequoia
    }

    fn own_fingerprint(&self) -> Option<[u8; 20]> {
        Some(self.user_key_id)
    }

    fn find_key(&self, password: Option<String>) -> Result<(), FindKeyError> {
        // move the password into a `Password` as soon as possible
        let password = password.map(sequoia_openpgp::crypto::Password::from);
        // save whether a password was given so we can drop `password` as soon as possible
        let password_is_none = password.is_none();

        if self.keypair.borrow().is_some() || !self.own_decrypt_cert()?.is_tsk() {
            return Ok(());
        }

        let mut decrypt_key = self.own_decrypt_key()?;

        if let Some(password) = password {
            decrypt_key = decrypt_key
                .decrypt_secret(&password)
                .map_err(|_| FindKeyError::WrongPassword)?;
        }

        match decrypt_key.into_keypair() {
            Ok(keypair) => {
                *self.keypair.borrow_mut() = Some(keypair);

                Ok(())
            }
            Err(_) => {
                assert!(password_is_none);

                Err(FindKeyError::WrongPassword)
            }
        }
    }
}

#[cfg(test)]
#[path = "tests/crypto.rs"]
mod crypto_tests;
