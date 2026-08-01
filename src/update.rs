//! Update manifest parsing, version comparison, and payload verification.
//!
//! Pure and **network-free**: this module never touches the network or the
//! filesystem. The downloader (network) and the platform apply paths live
//! elsewhere (roadmap: distribution blocks 2 and 3). Here we only decide
//! *whether* an update applies and *whether* an already-downloaded payload
//! is authentic. Keeping the safety-critical verification logic pure makes
//! it unit-testable against fixtures with no I/O.
//!
//! # Security model
//!
//! Filex ships without OS code signing, and on Windows the updater runs as
//! **LocalSystem**. An unverified payload would therefore be a SYSTEM-level
//! RCE vector on every user's machine. Every downloaded artifact MUST pass
//! [`Manifest::verify_payload`] — an Ed25519 signature check over the
//! artifact bytes — before any install step runs. The public key is
//! embedded in the shipping binaries; the private key exists only in CI.
//! See `docs/design-distribution.md` §4.
//!
//! Wire encoding: the manifest is JSON, and the `sha256`, `signature`, and
//! the embedded public key are all lowercase **hex**. We use a raw Ed25519
//! detached signature over the full artifact bytes (not the minisign
//! envelope) so verification needs no external tooling and fixtures are
//! reproducible from a fixed seed.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ed25519_dalek::{Signature, VerifyingKey};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The version of the running build, from Cargo. Callers compare a fetched
/// manifest against this to decide whether an update applies.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

const ED25519_PUBLIC_KEY_LEN: usize = 32;
const ED25519_SIGNATURE_LEN: usize = 64;

/// A parsed update manifest (one per platform), served over HTTPS.
///
/// `minimum_version` is reserved for a future forced-reinstall path and is
/// **not enforced in v1** (see `docs/design-distribution.md` decision 8) —
/// it is parsed and preserved but no code acts on it yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Semver of the release this manifest describes, e.g. `"1.4.0"`.
    pub version: String,
    /// HTTPS URL of the platform artifact (MSI / tarball / dmg).
    pub url: String,
    /// Lowercase hex SHA-256 of the artifact — a fast corruption check run
    /// before the (authoritative) signature verification.
    pub sha256: String,
    /// Lowercase hex Ed25519 signature over the full artifact bytes.
    pub signature: String,
    /// Reserved; unused in v1. See the type-level note.
    #[serde(default)]
    pub minimum_version: Option<String>,
}

/// Everything that can go wrong parsing a manifest or verifying a payload.
///
/// Variants are deliberately fine-grained so the updater — and the tests —
/// can distinguish "this download was corrupted" from "this download was
/// *tampered with*" from "the manifest was malformed".
#[derive(Debug)]
pub enum UpdateError {
    /// The manifest JSON did not parse.
    Parse(String),
    /// A version string (`version`, `minimum_version`, or the supplied
    /// current version) was not valid semver.
    Version(String),
    /// The download's SHA-256 did not match the manifest. Corruption or a
    /// mismatched artifact — caught before signature verification.
    HashMismatch { expected: String, actual: String },
    /// A hex field (`sha256`, `signature`, or the public key) was not
    /// valid hexadecimal.
    BadHex(&'static str),
    /// The public key was the wrong length or not a valid Ed25519 key.
    InvalidPublicKey,
    /// The signature field decoded but was not 64 bytes.
    InvalidSignatureLength(usize),
    /// The signature is well-formed but does not verify against the payload
    /// and public key. **This is the tampering signal** — never proceed.
    SignatureVerification,
    /// The private key (release signing) was the wrong length.
    InvalidPrivateKey,
    /// OS randomness was unavailable during keypair generation.
    Entropy,
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpdateError::Parse(e) => write!(f, "malformed update manifest: {e}"),
            UpdateError::Version(e) => write!(f, "invalid version string: {e}"),
            UpdateError::HashMismatch { expected, actual } => write!(
                f,
                "artifact SHA-256 mismatch: manifest says {expected}, download is {actual}"
            ),
            UpdateError::BadHex(field) => write!(f, "manifest field `{field}` is not valid hex"),
            UpdateError::InvalidPublicKey => write!(f, "embedded public key is invalid"),
            UpdateError::InvalidSignatureLength(n) => {
                write!(f, "signature is {n} bytes, expected {ED25519_SIGNATURE_LEN}")
            }
            UpdateError::SignatureVerification => {
                write!(f, "signature verification failed — payload is not authentic")
            }
            UpdateError::InvalidPrivateKey => write!(f, "signing private key is invalid"),
            UpdateError::Entropy => write!(f, "OS randomness unavailable for keypair generation"),
        }
    }
}

impl std::error::Error for UpdateError {}

impl Manifest {
    /// Parse a manifest from JSON.
    pub fn parse(json: &str) -> Result<Self, UpdateError> {
        serde_json::from_str(json).map_err(|e| UpdateError::Parse(e.to_string()))
    }

    /// The manifest's `version` as a parsed semver.
    pub fn version(&self) -> Result<Version, UpdateError> {
        parse_version(&self.version)
    }

    /// Whether this manifest describes a build newer than `current`.
    ///
    /// Both sides must be valid semver. Equal or older returns `false`.
    pub fn is_newer_than(&self, current: &str) -> Result<bool, UpdateError> {
        Ok(self.version()? > parse_version(current)?)
    }

    /// Verify a downloaded artifact against this manifest.
    ///
    /// Runs the cheap SHA-256 corruption check first, then the
    /// authoritative Ed25519 signature check. Returns `Ok(())` only if the
    /// payload is authentic; **the caller must not install unless this
    /// returns `Ok`**. `public_key_hex` is the embedded, trusted key.
    pub fn verify_payload(&self, data: &[u8], public_key_hex: &str) -> Result<(), UpdateError> {
        // 1. Fast integrity pre-check: catch a truncated/corrupted download
        //    before spending a signature verification on it.
        let actual = hex_encode(&Sha256::digest(data));
        if !actual.eq_ignore_ascii_case(&self.sha256) {
            return Err(UpdateError::HashMismatch {
                expected: self.sha256.to_ascii_lowercase(),
                actual,
            });
        }

        // 2. Authoritative check: Ed25519 signature over the artifact bytes.
        let key = decode_public_key(public_key_hex)?;
        let sig_bytes = hex_decode(&self.signature, "signature")?;
        let sig_array: [u8; ED25519_SIGNATURE_LEN] = sig_bytes
            .as_slice()
            .try_into()
            .map_err(|_| UpdateError::InvalidSignatureLength(sig_bytes.len()))?;
        let signature = Signature::from_bytes(&sig_array);

        // `verify_strict` rejects malleable/degenerate signatures.
        key.verify_strict(data, &signature)
            .map_err(|_| UpdateError::SignatureVerification)
    }
}

/// Generate a fresh Ed25519 keypair for release signing, returned as
/// `(public_hex, private_hex)`. Run **once** (via `filex-sign keygen`):
/// embed the public key in the shipping binaries, store the private key as
/// a CI secret and nowhere else. Uses OS randomness for the 32-byte seed.
pub fn generate_keypair() -> Result<(String, String), UpdateError> {
    use ed25519_dalek::SigningKey;
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).map_err(|_| UpdateError::Entropy)?;
    let signing = SigningKey::from_bytes(&seed);
    Ok((
        hex_encode(signing.verifying_key().as_bytes()),
        hex_encode(&seed),
    ))
}

/// Sign a release artifact: returns its lowercase-hex SHA-256 and the hex
/// Ed25519 signature over its bytes, from a hex-encoded private key. The
/// exact counterpart to [`Manifest::verify_payload`] — living in the same
/// module guarantees the wire format can't drift. Used by `filex-sign`.
pub fn sign_artifact(
    private_key_hex: &str,
    data: &[u8],
) -> Result<(String, String), UpdateError> {
    use ed25519_dalek::{Signer, SigningKey};
    let seed = hex_decode(private_key_hex, "private_key")?;
    let array: [u8; 32] = seed
        .as_slice()
        .try_into()
        .map_err(|_| UpdateError::InvalidPrivateKey)?;
    let signing = SigningKey::from_bytes(&array);
    let sha256 = hex_encode(&Sha256::digest(data));
    let signature = hex_encode(&signing.sign(data).to_bytes());
    Ok((sha256, signature))
}

fn parse_version(s: &str) -> Result<Version, UpdateError> {
    Version::parse(s).map_err(|e| UpdateError::Version(e.to_string()))
}

fn decode_public_key(public_key_hex: &str) -> Result<VerifyingKey, UpdateError> {
    let bytes = hex_decode(public_key_hex, "public_key")?;
    let array: [u8; ED25519_PUBLIC_KEY_LEN] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| UpdateError::InvalidPublicKey)?;
    VerifyingKey::from_bytes(&array).map_err(|_| UpdateError::InvalidPublicKey)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0x0f) as u32, 16).unwrap());
    }
    s
}

fn hex_decode(s: &str, field: &'static str) -> Result<Vec<u8>, UpdateError> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return Err(UpdateError::BadHex(field));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16).ok_or(UpdateError::BadHex(field))?;
        let lo = (bytes[i + 1] as char)
            .to_digit(16)
            .ok_or(UpdateError::BadHex(field))?;
        out.push(((hi << 4) | lo) as u8);
        i += 2;
    }
    Ok(out)
}

/// A shared, cheap cancellation flag for an in-flight download.
///
/// Cloneable (it's an `Arc<AtomicBool>` inside), so the update loop can
/// hold one copy and hand another to the fetch. The concrete fetcher polls
/// [`CancelFlag::is_cancelled`] between chunks so a large download aborts
/// promptly when the user quits or a newer manifest supersedes it.
#[derive(Clone, Default)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation. Idempotent; safe to call from any thread.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Why a download-and-verify attempt did not yield a trusted payload.
#[derive(Debug)]
pub enum DownloadError {
    /// Cancelled via the [`CancelFlag`] before or during the fetch.
    Cancelled,
    /// The transport failed (DNS, TLS, HTTP status, truncated read, …).
    /// Carries the concrete fetcher's message.
    Network(String),
    /// The manifest was fetched but was malformed or had a bad version —
    /// distinct from a payload signature failure.
    Manifest(UpdateError),
    /// The bytes arrived but failed authentication — **do not install**.
    Verify(UpdateError),
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DownloadError::Cancelled => write!(f, "update download cancelled"),
            DownloadError::Network(e) => write!(f, "update download failed: {e}"),
            DownloadError::Manifest(e) => write!(f, "update manifest rejected: {e}"),
            DownloadError::Verify(e) => write!(f, "update payload rejected: {e}"),
        }
    }
}

impl std::error::Error for DownloadError {}

/// The result of a full update check.
pub enum UpdateAction {
    /// The manifest is not newer than the running build; nothing to do.
    UpToDate,
    /// A strictly-newer, **authenticated** payload is in hand — the caller
    /// may stage and install it. The bytes have already passed the verify
    /// gate, so reaching this variant *is* the safe-to-install signal.
    Apply { version: String, payload: Vec<u8> },
}

/// Fetch the manifest's artifact and return its bytes **only if authentic**.
///
/// This is the integrity gate every platform's apply path goes through. The
/// network is injected as `fetch` (keeping this function, and the library,
/// network-free and unit-testable with a mock — mirroring the injected
/// sender in `telemetry::drain`). The concrete HTTPS fetcher is
/// [`http_fetch`], enabled by the `updater` feature.
///
/// Order is deliberate: cancellation is honoured before and after the
/// fetch, and [`Manifest::verify_payload`] runs last. Bytes are returned
/// **only** on `Ok`; a caller must never touch an installer with anything
/// this function did not hand back.
pub fn download_and_verify<F>(
    fetch: F,
    manifest: &Manifest,
    public_key_hex: &str,
    cancel: &CancelFlag,
) -> Result<Vec<u8>, DownloadError>
where
    F: FnOnce(&str, &CancelFlag) -> Result<Vec<u8>, String>,
{
    if cancel.is_cancelled() {
        return Err(DownloadError::Cancelled);
    }

    let bytes = match fetch(&manifest.url, cancel) {
        Ok(bytes) => bytes,
        // A fetch error while the flag is set is a cancellation, not a
        // network fault — classify by the flag so callers don't log a
        // scary "download failed" for a clean user-initiated abort.
        Err(msg) => {
            return Err(if cancel.is_cancelled() {
                DownloadError::Cancelled
            } else {
                DownloadError::Network(msg)
            });
        }
    };

    if cancel.is_cancelled() {
        return Err(DownloadError::Cancelled);
    }

    manifest
        .verify_payload(&bytes, public_key_hex)
        .map_err(DownloadError::Verify)?;
    Ok(bytes)
}

/// Hard ceiling on a downloaded artifact, defending the SYSTEM-level
/// Windows service against an unbounded/hostile response body. Comfortably
/// above any real installer.
#[cfg(feature = "updater")]
const MAX_ARTIFACT_BYTES: usize = 500 * 1024 * 1024;

/// Concrete HTTPS fetcher used by the app and the Windows index service.
///
/// Streams the body, polling `cancel` between chunks so an abort takes
/// effect promptly, and refuses a body larger than [`MAX_ARTIFACT_BYTES`].
/// Lives behind the `updater` feature so the default library build stays
/// network-free (the `lib` invariant in Cargo.toml). Pass it straight to
/// [`download_and_verify`].
#[cfg(feature = "updater")]
pub fn http_fetch(url: &str, cancel: &CancelFlag) -> Result<Vec<u8>, String> {
    use std::io::Read;

    if cancel.is_cancelled() {
        return Err("cancelled".to_string());
    }

    let resp = ureq::get(url).call().map_err(|e| e.to_string())?;
    let mut reader = resp.into_reader();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        if cancel.is_cancelled() {
            return Err("cancelled".to_string());
        }
        let n = reader.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        if buf.len() + n > MAX_ARTIFACT_BYTES {
            return Err(format!("artifact exceeds {MAX_ARTIFACT_BYTES} byte limit"));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(buf)
}

/// Run the full check → download → verify pipeline with the network
/// injected as `fetch` (used for both the manifest and the artifact, keyed
/// by URL). Returns [`UpdateAction::Apply`] **only** when a strictly-newer,
/// authentic payload is in hand — so the platform apply path (staging +
/// `msiexec`) can treat `Apply` as "safe to install" without re-checking.
///
/// Network-free and fully unit-testable: the real caller passes
/// [`http_fetch`]; tests pass a mock that dispatches on URL.
pub fn check_for_update<F>(
    fetch: F,
    manifest_url: &str,
    current: &str,
    public_key_hex: &str,
    cancel: &CancelFlag,
) -> Result<UpdateAction, DownloadError>
where
    F: Fn(&str, &CancelFlag) -> Result<Vec<u8>, String>,
{
    if cancel.is_cancelled() {
        return Err(DownloadError::Cancelled);
    }

    // 1. Fetch and parse the manifest.
    let manifest_bytes =
        fetch(manifest_url, cancel).map_err(|msg| classify_fetch_error(cancel, msg))?;
    let json = String::from_utf8(manifest_bytes)
        .map_err(|_| DownloadError::Manifest(UpdateError::Parse("manifest is not UTF-8".into())))?;
    let manifest = Manifest::parse(&json).map_err(DownloadError::Manifest)?;

    // 2. Is it newer? If not, stop before downloading anything.
    if !manifest
        .is_newer_than(current)
        .map_err(DownloadError::Manifest)?
    {
        return Ok(UpdateAction::UpToDate);
    }

    // 3. Download the artifact and run the verify gate. Same injected
    //    fetcher, keyed by the manifest's URL.
    let payload = download_and_verify(&fetch, &manifest, public_key_hex, cancel)?;
    Ok(UpdateAction::Apply {
        version: manifest.version,
        payload,
    })
}

fn classify_fetch_error(cancel: &CancelFlag, msg: String) -> DownloadError {
    if cancel.is_cancelled() {
        DownloadError::Cancelled
    } else {
        DownloadError::Network(msg)
    }
}

/// The `msiexec` arguments that silently apply a staged MSI.
///
/// `/qn` is fully silent (the SYSTEM service has no UI); `/norestart`
/// forbids an unattended machine reboot — the service restart is the MSI's
/// `ServiceControl` job, and the UI is coordinated separately (block 4).
/// Pure string logic so it's unit-testable off-Windows.
pub fn msiexec_args(msi_path: &str) -> Vec<String> {
    vec![
        "/i".into(),
        msi_path.into(),
        "/qn".into(),
        "/norestart".into(),
    ]
}

/// A lightweight "is a newer version available?" check for platforms that
/// do **not** self-install — macOS (`brew upgrade`) and Linux (re-download
/// the tarball). It fetches and parses the manifest and compares versions
/// only; it deliberately does **not** download or verify an artifact,
/// because the UI just shows a notice and the package manager performs the
/// actual (independently verified) install. Returns the new version string
/// when one is available.
pub fn check_for_newer_version<F>(
    fetch: F,
    manifest_url: &str,
    current: &str,
    cancel: &CancelFlag,
) -> Result<Option<String>, DownloadError>
where
    F: FnOnce(&str, &CancelFlag) -> Result<Vec<u8>, String>,
{
    if cancel.is_cancelled() {
        return Err(DownloadError::Cancelled);
    }
    let bytes = fetch(manifest_url, cancel).map_err(|msg| classify_fetch_error(cancel, msg))?;
    let json = String::from_utf8(bytes)
        .map_err(|_| DownloadError::Manifest(UpdateError::Parse("manifest is not UTF-8".into())))?;
    let manifest = Manifest::parse(&json).map_err(DownloadError::Manifest)?;
    if manifest
        .is_newer_than(current)
        .map_err(DownloadError::Manifest)?
    {
        Ok(Some(manifest.version))
    } else {
        Ok(None)
    }
}

// --- UI-facing status model (block 4) --------------------------------------
//
// Kept here, GPUI-free, so the presentation is unit-testable and the same
// model serves every platform's very different update UX (silent service +
// restart on Windows, `brew upgrade` on macOS, re-download on Linux). The
// GPUI banner (`src/ui/update_banner.rs`) only renders what these produce.

/// How the user acts on an available update — differs by platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateAffordance {
    /// Run a shell command; the string is the exact command to show/copy
    /// (macOS `brew upgrade filex`).
    RunCommand(String),
    /// Open a URL in the browser (Linux: the releases page to re-download).
    OpenUrl(String),
    /// The update is already applied in the background (Windows service);
    /// the user only needs to relaunch.
    Restart,
}

/// The UI's view of the updater. The workspace holds one and renders it;
/// background tasks advance it. `Idle`/`Failed` render nothing (a failed
/// check is silent — we retry on the next launch).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UpdateStatus {
    #[default]
    Idle,
    /// A newer version is available; `affordance` says how to get it.
    Available {
        version: String,
        affordance: UpdateAffordance,
    },
    /// (Windows) the service is downloading in the background.
    Downloading { version: String },
    /// The last check failed; rendered as nothing.
    Failed,
}

/// What the banner should display for a status, or `None` when the banner
/// should be hidden entirely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BannerContent {
    /// The one-line message, e.g. `"Filex 1.4.0 is available"`.
    pub message: String,
    /// The primary action's label, or `None` for a message-only banner
    /// (e.g. while downloading).
    pub action_label: Option<String>,
}

/// Map a status to what the banner shows. Pure — the single place the
/// update UX wording lives.
pub fn banner_content(status: &UpdateStatus) -> Option<BannerContent> {
    match status {
        UpdateStatus::Idle | UpdateStatus::Failed => None,
        UpdateStatus::Downloading { version } => Some(BannerContent {
            message: format!("Downloading Filex {version}…"),
            action_label: None,
        }),
        UpdateStatus::Available {
            version,
            affordance,
        } => {
            let (message, action_label) = match affordance {
                UpdateAffordance::RunCommand(cmd) => (
                    format!("Filex {version} is available"),
                    Some(format!("Copy “{cmd}”")),
                ),
                UpdateAffordance::OpenUrl(_) => {
                    (format!("Filex {version} is available"), Some("Get update".to_string()))
                }
                UpdateAffordance::Restart => {
                    (format!("Filex {version} is ready"), Some("Restart".to_string()))
                }
            };
            Some(BannerContent {
                message,
                action_label,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use std::cell::Cell;

    /// Deterministic signing key for fixtures — no RNG, so tests are
    /// reproducible. The seed is arbitrary; only used in tests.
    fn test_key() -> (SigningKey, String) {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let pk_hex = hex_encode(sk.verifying_key().as_bytes());
        (sk, pk_hex)
    }

    fn sign_hex(sk: &SigningKey, data: &[u8]) -> String {
        hex_encode(&sk.sign(data).to_bytes())
    }

    /// Build a manifest whose sha256 + signature are correct for `data`.
    fn valid_manifest(sk: &SigningKey, version: &str, data: &[u8]) -> Manifest {
        Manifest {
            version: version.to_string(),
            url: "https://example.test/filex.msi".to_string(),
            sha256: hex_encode(&Sha256::digest(data)),
            signature: sign_hex(sk, data),
            minimum_version: None,
        }
    }

    #[test]
    fn hex_round_trips() {
        let bytes = [0x00, 0x0f, 0xa5, 0xff, 0x10];
        assert_eq!(hex_encode(&bytes), "000fa5ff10");
        assert_eq!(hex_decode("000fa5ff10", "x").unwrap(), bytes);
    }

    #[test]
    fn hex_decode_rejects_odd_length_and_non_hex() {
        assert!(matches!(hex_decode("abc", "x"), Err(UpdateError::BadHex("x"))));
        assert!(matches!(hex_decode("zz", "x"), Err(UpdateError::BadHex("x"))));
    }

    #[test]
    fn parses_manifest_with_optional_minimum_version_defaulting() {
        let json = r#"{
            "version": "1.4.0",
            "url": "https://example.test/filex.msi",
            "sha256": "aa",
            "signature": "bb"
        }"#;
        let m = Manifest::parse(json).unwrap();
        assert_eq!(m.version, "1.4.0");
        assert_eq!(m.minimum_version, None);
    }

    #[test]
    fn parse_rejects_missing_required_field() {
        // No `url`.
        let json = r#"{ "version": "1.0.0", "sha256": "aa", "signature": "bb" }"#;
        assert!(matches!(Manifest::parse(json), Err(UpdateError::Parse(_))));
    }

    #[test]
    fn manifest_json_round_trips() {
        let (sk, _) = test_key();
        let m = valid_manifest(&sk, "2.0.0", b"payload");
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(Manifest::parse(&json).unwrap(), m);
    }

    #[test]
    fn is_newer_than_compares_semver() {
        let (sk, _) = test_key();
        let m = valid_manifest(&sk, "1.4.0", b"x");
        assert!(m.is_newer_than("1.3.9").unwrap());
        assert!(m.is_newer_than("1.4.0").unwrap() == false);
        assert!(m.is_newer_than("1.4.1").unwrap() == false);
        assert!(m.is_newer_than("2.0.0").unwrap() == false);
    }

    #[test]
    fn is_newer_than_rejects_bad_semver() {
        let (sk, _) = test_key();
        let m = valid_manifest(&sk, "not-a-version", b"x");
        assert!(matches!(m.is_newer_than("1.0.0"), Err(UpdateError::Version(_))));

        let good = valid_manifest(&sk, "1.0.0", b"x");
        assert!(matches!(good.is_newer_than("garbage"), Err(UpdateError::Version(_))));
    }

    #[test]
    fn verify_accepts_authentic_payload() {
        let (sk, pk) = test_key();
        let data = b"the real installer bytes";
        let m = valid_manifest(&sk, "1.0.0", data);
        assert!(m.verify_payload(data, &pk).is_ok());
    }

    #[test]
    fn keygen_sign_verify_round_trips() {
        // The full release loop with a freshly generated key: what
        // `filex-sign` produces must be exactly what the client accepts.
        let (public_hex, private_hex) = generate_keypair().unwrap();
        let artifact = b"an installer built in CI";
        let (sha256, signature) = sign_artifact(&private_hex, artifact).unwrap();

        let manifest = Manifest {
            version: "1.2.3".into(),
            url: "https://example.test/Filex.msi".into(),
            sha256,
            signature,
            minimum_version: None,
        };
        assert!(manifest.verify_payload(artifact, &public_hex).is_ok());
        // A different key must not verify against this public key.
        let (_, other_priv) = generate_keypair().unwrap();
        let (_, other_sig) = sign_artifact(&other_priv, artifact).unwrap();
        let mut forged = manifest.clone();
        forged.signature = other_sig;
        assert!(matches!(
            forged.verify_payload(artifact, &public_hex),
            Err(UpdateError::SignatureVerification)
        ));
    }

    #[test]
    fn sign_artifact_rejects_bad_private_key() {
        assert!(matches!(
            sign_artifact("aabb", b"x"),
            Err(UpdateError::InvalidPrivateKey)
        ));
        assert!(matches!(
            sign_artifact("nothex", b"x"),
            Err(UpdateError::BadHex("private_key"))
        ));
    }

    #[test]
    fn verify_rejects_tampered_payload_after_hash_is_forced_to_match() {
        // Sign the real data, but hand verify_payload different bytes whose
        // sha256 we also overwrite to match — so the *only* thing standing
        // between the attacker and install is the signature. It must hold.
        let (sk, pk) = test_key();
        let real = b"the real installer bytes";
        let mut m = valid_manifest(&sk, "1.0.0", real);

        let malicious = b"malware payload of the same story";
        m.sha256 = hex_encode(&Sha256::digest(malicious)); // attacker fixes the hash
        assert!(matches!(
            m.verify_payload(malicious, &pk),
            Err(UpdateError::SignatureVerification)
        ));
    }

    #[test]
    fn verify_rejects_hash_mismatch_before_signature() {
        let (sk, pk) = test_key();
        let data = b"payload";
        let mut m = valid_manifest(&sk, "1.0.0", data);
        m.sha256 = hex_encode(&Sha256::digest(b"different"));
        assert!(matches!(
            m.verify_payload(data, &pk),
            Err(UpdateError::HashMismatch { .. })
        ));
    }

    #[test]
    fn verify_rejects_wrong_public_key() {
        let (sk, _) = test_key();
        let data = b"payload";
        let m = valid_manifest(&sk, "1.0.0", data);

        let other = SigningKey::from_bytes(&[9u8; 32]);
        let wrong_pk = hex_encode(other.verifying_key().as_bytes());
        assert!(matches!(
            m.verify_payload(data, &wrong_pk),
            Err(UpdateError::SignatureVerification)
        ));
    }

    #[test]
    fn verify_rejects_malformed_signature_and_key() {
        let (sk, pk) = test_key();
        let data = b"payload";
        let mut m = valid_manifest(&sk, "1.0.0", data);

        // Non-hex signature.
        m.signature = "nothex!!".to_string();
        assert!(matches!(
            m.verify_payload(data, &pk),
            Err(UpdateError::BadHex("signature"))
        ));

        // Right hex, wrong length.
        let mut m2 = valid_manifest(&sk, "1.0.0", data);
        m2.signature = "aabb".to_string();
        assert!(matches!(
            m2.verify_payload(data, &pk),
            Err(UpdateError::InvalidSignatureLength(2))
        ));

        // Bad public key length.
        let m3 = valid_manifest(&sk, "1.0.0", data);
        assert!(matches!(
            m3.verify_payload(data, "aabbcc"),
            Err(UpdateError::InvalidPublicKey)
        ));
    }

    // --- download_and_verify (the integrity gate) --------------------------

    #[test]
    fn download_returns_bytes_only_when_authentic() {
        let (sk, pk) = test_key();
        let data = b"authentic installer".to_vec();
        let m = valid_manifest(&sk, "1.0.0", &data);
        let cancel = CancelFlag::new();

        let got = download_and_verify(|_url, _c| Ok(data.clone()), &m, &pk, &cancel).unwrap();
        assert_eq!(got, data);
    }

    #[test]
    fn download_rejects_tampered_bytes_from_the_source() {
        // The source hands back different bytes than were signed — a
        // compromised mirror/MITM. The gate must refuse and surface Verify,
        // never the bytes.
        let (sk, pk) = test_key();
        let signed = b"authentic installer";
        let m = valid_manifest(&sk, "1.0.0", signed);
        let cancel = CancelFlag::new();

        let result = download_and_verify(
            |_url, _c| Ok(b"malware".to_vec()),
            &m,
            &pk,
            &cancel,
        );
        assert!(matches!(result, Err(DownloadError::Verify(_))));
    }

    #[test]
    fn download_propagates_network_errors() {
        let (sk, pk) = test_key();
        let m = valid_manifest(&sk, "1.0.0", b"x");
        let cancel = CancelFlag::new();

        let result =
            download_and_verify(|_url, _c| Err("dns failure".to_string()), &m, &pk, &cancel);
        match result {
            Err(DownloadError::Network(msg)) => assert_eq!(msg, "dns failure"),
            other => panic!("expected Network, got {other:?}"),
        }
    }

    #[test]
    fn download_does_not_fetch_when_already_cancelled() {
        let (sk, pk) = test_key();
        let m = valid_manifest(&sk, "1.0.0", b"x");
        let cancel = CancelFlag::new();
        cancel.cancel();

        let fetched = Cell::new(false);
        let result = download_and_verify(
            |_url, _c| {
                fetched.set(true);
                Ok(b"x".to_vec())
            },
            &m,
            &pk,
            &cancel,
        );
        assert!(matches!(result, Err(DownloadError::Cancelled)));
        assert!(!fetched.get(), "fetch must not run once cancelled");
    }

    #[test]
    fn download_classifies_midflight_cancel_as_cancelled_not_network() {
        // The source observes the flag mid-stream and errors; because the
        // flag is set, the gate reports Cancelled, not a network fault.
        let (sk, pk) = test_key();
        let m = valid_manifest(&sk, "1.0.0", b"x");
        let cancel = CancelFlag::new();

        let result = download_and_verify(
            |_url, c| {
                c.cancel();
                Err("aborted".to_string())
            },
            &m,
            &pk,
            &cancel,
        );
        assert!(matches!(result, Err(DownloadError::Cancelled)));
    }

    #[test]
    fn download_treats_cancel_after_successful_fetch_as_cancelled() {
        // Bytes came back fine, but cancellation was requested during the
        // fetch — we must not proceed to verify/install a superseded build.
        let (sk, pk) = test_key();
        let data = b"authentic".to_vec();
        let m = valid_manifest(&sk, "1.0.0", &data);
        let cancel = CancelFlag::new();

        let result = download_and_verify(
            |_url, c| {
                c.cancel();
                Ok(data.clone())
            },
            &m,
            &pk,
            &cancel,
        );
        assert!(matches!(result, Err(DownloadError::Cancelled)));
    }

    // --- check_for_update (full pipeline) ----------------------------------

    const MANIFEST_URL: &str = "https://example.test/windows/latest.json";
    const ARTIFACT_URL: &str = "https://example.test/Filex.msi";

    /// A URL-dispatching fetcher: manifest JSON at `MANIFEST_URL`, artifact
    /// bytes at `ARTIFACT_URL`, else a network error.
    fn router(
        manifest_json: String,
        artifact: Vec<u8>,
    ) -> impl Fn(&str, &CancelFlag) -> Result<Vec<u8>, String> {
        move |url, _cancel| match url {
            u if u == MANIFEST_URL => Ok(manifest_json.clone().into_bytes()),
            u if u == ARTIFACT_URL => Ok(artifact.clone()),
            other => Err(format!("no route for {other}")),
        }
    }

    fn manifest_json(sk: &SigningKey, version: &str, artifact: &[u8]) -> String {
        let mut m = valid_manifest(sk, version, artifact);
        m.url = ARTIFACT_URL.to_string();
        serde_json::to_string(&m).unwrap()
    }

    #[test]
    fn check_applies_a_newer_authentic_release() {
        let (sk, pk) = test_key();
        let artifact = b"the new installer".to_vec();
        let json = manifest_json(&sk, "9.9.9", &artifact);
        let cancel = CancelFlag::new();

        let action =
            check_for_update(router(json, artifact.clone()), MANIFEST_URL, "1.0.0", &pk, &cancel)
                .unwrap();
        match action {
            UpdateAction::Apply { version, payload } => {
                assert_eq!(version, "9.9.9");
                assert_eq!(payload, artifact);
            }
            _ => panic!("expected Apply"),
        }
    }

    #[test]
    fn check_reports_up_to_date_and_skips_artifact_download() {
        let (sk, pk) = test_key();
        // Manifest version equals current → no download attempted. Route the
        // artifact to an error so a stray download would fail the test.
        let json = manifest_json(&sk, "2.0.0", b"unused");
        let cancel = CancelFlag::new();

        let action = check_for_update(
            move |url, _c| {
                if url == MANIFEST_URL {
                    Ok(json.clone().into_bytes())
                } else {
                    Err("artifact must not be fetched".into())
                }
            },
            MANIFEST_URL,
            "2.0.0",
            &pk,
            &cancel,
        )
        .unwrap();
        assert!(matches!(action, UpdateAction::UpToDate));
    }

    #[test]
    fn check_rejects_tampered_artifact_without_applying() {
        // Newer manifest, but the artifact route serves bytes that weren't
        // the signed ones. The pipeline must surface Verify and never yield
        // Apply — this is the "verify fails ⇒ msiexec never runs" guarantee.
        let (sk, pk) = test_key();
        let signed = b"the real installer".to_vec();
        let json = manifest_json(&sk, "9.9.9", &signed);
        let cancel = CancelFlag::new();

        let result = check_for_update(
            router(json, b"malware".to_vec()),
            MANIFEST_URL,
            "1.0.0",
            &pk,
            &cancel,
        );
        assert!(matches!(result, Err(DownloadError::Verify(_))));
    }

    #[test]
    fn check_rejects_a_malformed_manifest() {
        let (_, pk) = test_key();
        let cancel = CancelFlag::new();
        let result = check_for_update(
            |_url, _c| Ok(b"not json".to_vec()),
            MANIFEST_URL,
            "1.0.0",
            &pk,
            &cancel,
        );
        assert!(matches!(result, Err(DownloadError::Manifest(_))));
    }

    #[test]
    fn msiexec_args_are_silent_and_no_restart() {
        assert_eq!(
            msiexec_args(r"C:\Temp\Filex-1.4.0.msi"),
            vec!["/i", r"C:\Temp\Filex-1.4.0.msi", "/qn", "/norestart"]
        );
    }

    // --- check_for_newer_version (notice-only, no download) ----------------

    #[test]
    fn newer_version_check_reports_version_and_skips_download() {
        let (sk, _) = test_key();
        // Manifest for a newer version; note the check must NOT hit the
        // artifact URL — there's only a manifest fetch here.
        let json = manifest_json(&sk, "3.1.0", b"unused");
        let cancel = CancelFlag::new();
        let got = check_for_newer_version(
            |_url, _c| Ok(json.clone().into_bytes()),
            MANIFEST_URL,
            "3.0.0",
            &cancel,
        )
        .unwrap();
        assert_eq!(got, Some("3.1.0".to_string()));
    }

    #[test]
    fn newer_version_check_returns_none_when_current() {
        let (sk, _) = test_key();
        let json = manifest_json(&sk, "3.0.0", b"unused");
        let cancel = CancelFlag::new();
        let got = check_for_newer_version(
            |_url, _c| Ok(json.clone().into_bytes()),
            MANIFEST_URL,
            "3.0.0",
            &cancel,
        )
        .unwrap();
        assert_eq!(got, None);
    }

    // --- banner_content (presentation) -------------------------------------

    #[test]
    fn banner_hidden_for_idle_and_failed() {
        assert_eq!(banner_content(&UpdateStatus::Idle), None);
        assert_eq!(banner_content(&UpdateStatus::Failed), None);
    }

    #[test]
    fn banner_run_command_offers_copy() {
        let status = UpdateStatus::Available {
            version: "1.4.0".into(),
            affordance: UpdateAffordance::RunCommand("brew upgrade filex".into()),
        };
        let content = banner_content(&status).unwrap();
        assert_eq!(content.message, "Filex 1.4.0 is available");
        assert_eq!(content.action_label.as_deref(), Some("Copy “brew upgrade filex”"));
    }

    #[test]
    fn banner_restart_offers_restart_and_downloading_has_no_action() {
        let ready = UpdateStatus::Available {
            version: "2.0.0".into(),
            affordance: UpdateAffordance::Restart,
        };
        let content = banner_content(&ready).unwrap();
        assert_eq!(content.message, "Filex 2.0.0 is ready");
        assert_eq!(content.action_label.as_deref(), Some("Restart"));

        let downloading = UpdateStatus::Downloading { version: "2.0.0".into() };
        let content = banner_content(&downloading).unwrap();
        assert_eq!(content.message, "Downloading Filex 2.0.0…");
        assert_eq!(content.action_label, None);
    }
}
