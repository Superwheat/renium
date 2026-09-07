//! Authentication for privileged operations, not an isolation boundary against
//! arbitrary code already running as the OS user. Keys never enter Studio.
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use super::{Request, op};

const PROOF_FIELD: &str = "_authorization";
const PROOF_LIFETIME: Duration = Duration::from_secs(60);
const MAX_PROOFS: usize = 65536;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    time: u128,
    nonce: String,
    signature: String,
}

pub(crate) fn required(operation: u16) -> bool {
    // A file-reading or review operation must not become a side door into the
    // privileged boundary. Only the public, stateless capability handshake is exempt.
    operation != op::CAP
}

fn key_path(port: u16) -> Result<PathBuf> {
    Ok(crate::app::update::user_data_dir()?
        .join("private")
        .join(format!("control-{port}.key")))
}

pub(crate) fn random_id() -> Result<String> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("OS random source failed: {e}"))?;
    Ok(base64::encode_config(bytes, base64::URL_SAFE_NO_PAD))
}

fn message(request: &Request, proof: &Proof) -> Result<Vec<u8>> {
    let mut bytes = b"renium/privileged-request/v1\0".to_vec();
    bytes.extend(proof.time.to_le_bytes());
    bytes.extend(proof.nonce.as_bytes());
    bytes.push(0);
    serde_json::to_writer(&mut bytes, request)?;
    Ok(bytes)
}

fn sign(request: &Request, key: &SigningKey) -> Result<serde_json::Value> {
    let mut proof = Proof {
        time: crate::app::timing::current_millis(),
        nonce: random_id()?,
        signature: String::new(),
    };
    proof.signature = base64::encode(key.sign(&message(request, &proof)?).to_bytes());
    let mut wire = serde_json::to_value(request)?;
    wire["p"][PROOF_FIELD] = serde_json::to_value(proof)?;
    Ok(wire)
}

pub(crate) fn encode_request(request: &Request, port: u16) -> Result<String> {
    if !required(request.op) {
        return Ok(serde_json::to_string(request)?);
    }
    if request.p.get(PROOF_FIELD).is_some() {
        bail!("Authorization is supplied by the local Renium CLI, not request parameters");
    }
    let stored = match fs::read(key_path(port)?) {
        Ok(stored) => stored,
        // Preserve communication with older daemons for existing operations.
        // An updated daemon rejects unsigned requests; protected access NEVER
        // downgrades to this legacy transport.
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && request.op != op::PROPERTY_ACCESS =>
        {
            return Ok(serde_json::to_string(request)?);
        }
        Err(error) => {
            return Err(error).context(
                "Privileged authentication unavailable; restart the updated Renium daemon",
            );
        }
    };
    let seed: [u8; 32] = unprotect(&stored)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("Invalid private Renium credential"))?;
    Ok(serde_json::to_string(&sign(
        request,
        &SigningKey::from_bytes(&seed),
    )?)?)
}

pub(crate) struct Authority {
    key: VerifyingKey,
    seen: Mutex<HashMap<[u8; 64], Instant>>,
}

#[cfg(test)]
pub(crate) fn signed_fixture_request(state: &super::State, value: serde_json::Value) -> String {
    let key = SigningKey::from_bytes(&[19; 32]);
    state
        .authority
        .get_or_init(|| Authority::new(key.verifying_key()));
    let request = serde_json::from_value(value).unwrap();
    serde_json::to_string(&sign(&request, &key).unwrap()).unwrap()
}

impl Authority {
    pub(crate) fn initialize(port: u16) -> Result<Self> {
        let mut seed = [0; 32];
        getrandom::fill(&mut seed).map_err(|e| anyhow::anyhow!("OS random source failed: {e}"))?;
        let path = key_path(port)?;
        let directory = path
            .parent()
            .context("Missing private credential directory")?;
        fs::create_dir_all(directory)?;
        if fs::symlink_metadata(directory)?.file_type().is_symlink() {
            bail!("Renium private credential directory must not be a symbolic link");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        }
        crate::system::files::atomic_write_file(&path, &protect(&seed)?)?;
        Ok(Self::new(SigningKey::from_bytes(&seed).verifying_key()))
    }

    fn new(key: VerifyingKey) -> Self {
        Self {
            key,
            seen: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn verify(&self, request: &mut Request) -> Result<()> {
        let value = request
            .p
            .as_object_mut()
            .and_then(|p| p.remove(PROOF_FIELD))
            .context("Authenticated local Renium access is required")?;
        let proof: Proof = serde_json::from_value(value)
            .map_err(|_| anyhow::anyhow!("Invalid Renium authorization"))?;
        let now = crate::app::timing::current_millis();
        if proof.time > now.saturating_add(5000)
            || now.saturating_sub(proof.time) > PROOF_LIFETIME.as_millis()
            || proof.nonce.len() != 22
        {
            bail!("Renium authorization expired or is invalid");
        }
        let signature = base64::decode(&proof.signature)
            .ok()
            .and_then(|bytes| Signature::from_slice(&bytes).ok())
            .context("Invalid Renium authorization")?;
        self.key
            .verify_strict(&message(request, &proof)?, &signature)
            .map_err(|_| anyhow::anyhow!("Invalid Renium authorization"))?;
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        seen.retain(|_, time| time.elapsed() < PROOF_LIFETIME + Duration::from_secs(5));
        if seen.contains_key(&signature.to_bytes()) {
            bail!(
                "Renium authorization was already used; inspect the operation result before retrying"
            );
        }
        if seen.len() >= MAX_PROOFS {
            bail!("Too many privileged requests; wait for current authorizations to expire");
        }
        seen.insert(signature.to_bytes(), Instant::now());
        Ok(())
    }
}

// DPAPI ties the stored key to this Windows user, even if another account can
// read the file. Unix uses a private (0700) directory outside all project roots.
#[cfg(windows)]
fn crypt(bytes: &[u8], encrypt: bool) -> Result<Vec<u8>> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(bytes.len())?,
        pbData: bytes.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // No prompt/UI and no machine-wide protection: only this user's DPAPI key.
    let ok = unsafe {
        if encrypt {
            CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error())
            .context("Private Renium credential protection failed");
    }
    let result =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe {
        LocalFree(output.pbData.cast());
    }
    Ok(result)
}

fn protect(bytes: &[u8]) -> Result<Vec<u8>> {
    #[cfg(windows)]
    {
        crypt(bytes, true)
    }
    #[cfg(not(windows))]
    {
        Ok(bytes.to_vec())
    }
}

fn unprotect(bytes: &[u8]) -> Result<Vec<u8>> {
    #[cfg(windows)]
    {
        crypt(bytes, false)
    }
    #[cfg(not(windows))]
    {
        Ok(bytes.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request() -> Request {
        Request {
            v: super::super::PROTOCOL_VERSION,
            id: 42,
            op: op::PROPERTY_ACCESS,
            cx: Some(7),
            p: json!({"action":"mode","mode":"read-write"}),
        }
    }

    #[test]
    fn authorization_binds_all_parameters_and_rejects_unsigned_forged_and_replayed_requests() {
        let key = SigningKey::from_bytes(&[17; 32]);
        let authority = Authority::new(key.verifying_key());
        assert!(authority.verify(&mut request()).is_err());
        let wire = sign(&request(), &key).unwrap();
        for field in ["id", "op", "cx", "v"] {
            let mut altered = wire.clone();
            altered[field] = json!(99);
            assert!(
                authority
                    .verify(&mut serde_json::from_value(altered).unwrap())
                    .is_err()
            );
        }
        let mut altered = wire.clone();
        altered["p"]["mode"] = json!("read-only");
        assert!(
            authority
                .verify(&mut serde_json::from_value(altered).unwrap())
                .is_err()
        );
        let foreign = sign(&request(), &SigningKey::from_bytes(&[18; 32])).unwrap();
        assert!(
            authority
                .verify(&mut serde_json::from_value(foreign).unwrap())
                .is_err()
        );
        authority
            .verify(&mut serde_json::from_value(wire.clone()).unwrap())
            .unwrap();
        assert!(
            authority
                .verify(&mut serde_json::from_value(wire).unwrap())
                .is_err()
        );
    }

    #[test]
    fn private_credential_round_trip() {
        let seed = [23; 32];
        let encoded = protect(&seed).unwrap();
        #[cfg(windows)]
        assert_ne!(encoded, seed);
        assert_eq!(unprotect(&encoded).unwrap(), seed);
    }
}
