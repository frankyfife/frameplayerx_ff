//! In-app self-update for FramePlayerX.
//!
//! The update flow:
//!
//! 1. [`check`] fetches the release manifest (`manifest.json`) and its
//!    detached ed25519 signature (`manifest.json.sig`), verifies the
//!    signature against [`RELEASE_PUBLIC_KEY`], and compares versions with
//!    semver for the user's [`Channel`]. [`check_channel`] finds that
//!    manifest among FramePlayerX's GitHub releases.
//! 2. [`download`] streams the zip to disk with resume (HTTP `Range`) and
//!    verifies size and SHA-256 from the signed manifest.
//! 3. [`install()`] extracts into a staging directory beside the install
//!    directory, validates it, and swaps it in with renames, keeping one
//!    previous version for [`rollback`].
//!
//! The crate also carries the [`framedrop`] manifest used by the community
//! installers (Frame Control, FrameDrop) and the `fp-release` maintainer
//! tool that generates keys, manifests and signatures.

pub mod client;
pub mod error;
pub mod framedrop;
pub mod github;
pub mod install;
pub mod manifest;
pub mod url;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

pub use client::{Updater, agent_with_proxy, default_agent, download_with, hash_file};
pub use ed25519_dalek::{SigningKey, VerifyingKey};
pub use error::{Error, Result};
pub use install::{
    ArchiveInfo, extract_zip, inspect_zip, install, install_expecting, installed_version, rollback,
    validate_install_dir,
};
pub use manifest::{
    Artifact, Channel, ReleaseManifest, Update, select_update, sign_manifest, verify_manifest,
};
pub use semver::Version;

/// Where the latest stable release manifest is published. The signature is
/// expected at the same URL plus `.sig`. The stable channel falls back to
/// it when GitHub's release list can't be read (rate limits).
pub const DEFAULT_MANIFEST_URL: &str =
    "https://github.com/frankyfife/frameplayerx_ff/releases/latest/download/manifest.json";

/// GitHub's list of the most recent releases, pre-releases included. Both
/// channels pick the highest version for them here: it is current the
/// moment a release is published, where `releases/latest` can lag, and it
/// goes by the version rather than GitHub's pre-release flag.
pub const RELEASES_API_URL: &str =
    "https://api.github.com/repos/frankyfife/frameplayerx_ff/releases?per_page=10";

/// The ed25519 public key release manifests are signed with.
///
/// `None` in builds made before a release key exists: updates are then
/// refused with [`Error::NoPublicKey`]. To set it, run
/// `fp-release keygen`, keep the private key offline, and paste the line it
/// prints here.
pub const RELEASE_PUBLIC_KEY: Option<[u8; 32]> = Some([
    0x18, 0xd9, 0x34, 0xb7, 0xbc, 0x45, 0x3f, 0x73, 0xb0, 0xac, 0x24, 0xda, 0x21, 0xca, 0x79, 0xb2,
    0xcd, 0xd1, 0x84, 0xe1, 0x1a, 0x1b, 0xbc, 0x81, 0x2b, 0x22, 0x63, 0x61, 0x79, 0x37, 0xa9, 0x46,
]);

/// The compiled-in release key, parsed.
pub fn release_key() -> Result<VerifyingKey> {
    match RELEASE_PUBLIC_KEY {
        Some(bytes) => manifest::public_key_from_bytes(&bytes),
        None => Err(Error::NoPublicKey),
    }
}

/// Parses a semver version string (surrounding whitespace allowed).
pub fn parse_version(s: &str) -> Result<Version> {
    Version::parse(s.trim()).map_err(|e| Error::InvalidManifest(format!("bad version {s:?}: {e}")))
}

/// Checks `url` for an update newer than `current_version` on `channel`,
/// for this CPU architecture, trusting [`RELEASE_PUBLIC_KEY`].
///
/// Returns `Ok(None)` when up to date. Unsigned or badly signed manifests
/// are errors, never "no update".
pub fn check(url: &str, current_version: &str, channel: Channel) -> Result<Option<Update>> {
    let current = parse_version(current_version)?;
    Updater::new()?.check(url, &current, channel)
}

/// Checks FramePlayerX's GitHub releases for an update on `channel`: the
/// highest version among recent releases ([`RELEASES_API_URL`]), stable
/// versions only on the stable channel. Stable falls back to
/// [`DEFAULT_MANIFEST_URL`] when the list can't be read. Otherwise as
/// [`check`].
pub fn check_channel(current_version: &str, channel: Channel) -> Result<Option<Update>> {
    let current = parse_version(current_version)?;
    let updater = Updater::new()?;
    let url = channel_manifest_url(&updater, channel)?;
    updater.check(&url, &current, channel)
}

/// The manifest of the newest release on `channel` among recent releases
/// ([`RELEASES_API_URL`]). Stable falls back to [`DEFAULT_MANIFEST_URL`]
/// when the list can't be read; `releases/latest` never offers a beta.
pub fn channel_manifest_url(updater: &Updater, channel: Channel) -> Result<String> {
    match updater.newest_manifest_url(RELEASES_API_URL, channel) {
        Ok(url) => Ok(url),
        Err(_) if channel == Channel::Stable => Ok(DEFAULT_MANIFEST_URL.to_string()),
        Err(e) => Err(e),
    }
}

/// Downloads `update` into the directory `dest` with resume and SHA-256
/// verification. See [`Updater::download`].
pub fn download(
    update: &Update,
    dest: &Path,
    progress: impl FnMut(u64, u64),
    cancel: &AtomicBool,
) -> Result<PathBuf> {
    download_with(&default_agent(), update, dest, progress, cancel)
}

/// The directory holding the running executable, which is the install
/// directory for a FramePlayerX started from `frameplayerx.sh`.
pub fn current_install_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().map_err(|e| Error::io("cannot locate", "current exe", e))?;
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| Error::InvalidInstall("executable has no parent directory".into()))
}
