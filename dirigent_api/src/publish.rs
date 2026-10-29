//! `dirigent_api publish`: sign an installer, upload it with its signature, then point the
//! channel at it. The channel file goes last, so no client ever sees a release whose files
//! aren't there yet. A version is published once: its files are never overwritten.

use crate::bucket::{self, Bucket};
use anyhow::{Context, Result, bail};
use dirigent_protocol::release::{self, Release, valid_name};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Installers and signatures never change once uploaded.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

pub struct Args {
    channel: String,
    platform: String,
    version: String,
    notes: String,
    file: PathBuf,
}

impl Args {
    pub fn parse(mut args: impl Iterator<Item = String>) -> Result<Self> {
        let (mut channel, mut platform, mut version, mut notes, mut file) = (None, None, None, String::new(), None);
        while let Some(arg) = args.next() {
            let mut value = || args.next().with_context(|| format!("{arg} needs a value"));
            match arg.as_str() {
                "--channel" => channel = Some(value()?),
                "--platform" => platform = Some(value()?),
                "--version" => version = Some(value()?),
                "--notes" => notes = value()?,
                a if a.starts_with("--") => bail!("unknown option {a}\n{}", crate::USAGE),
                _ => file = Some(PathBuf::from(arg)),
            }
        }
        let (Some(channel), Some(platform), Some(version), Some(file)) = (channel, platform, version, file) else {
            bail!("{}", crate::USAGE);
        };
        if !valid_name(&channel) || !valid_name(&platform) {
            bail!("channel and platform are lowercase letters, digits, - and _");
        }
        semver::Version::parse(&version).with_context(|| format!("version {version} is not semver"))?;
        Ok(Self { channel, platform, version, notes, file })
    }
}

pub fn run(bucket: &Bucket, args: Args) -> Result<()> {
    let creds = Bucket::credentials()?;
    let data = std::fs::read(&args.file).with_context(|| format!("read {}", args.file.display()))?;
    let signature = sign(&data, &args.version, &args.platform)?;
    let name = args.file.file_name().and_then(|n| n.to_str()).context("the file needs a name")?;
    let path = format!("releases/{}/{name}", args.version);
    if bucket.exists(&creds, &path)? {
        bail!("{path} is already published: bump the version");
    }
    bucket.put(&creds, &path, &data, "application/octet-stream", IMMUTABLE)?;
    bucket.put(&creds, &format!("{path}.minisig"), signature.as_bytes(), "text/plain", IMMUTABLE)?;
    let release = Release { version: args.version, url: bucket.url(&path), notes: args.notes };
    let channel = bucket::channel_path(&args.channel, &args.platform);
    bucket.put(&creds, &channel, serde_json::to_string_pretty(&release)?.as_bytes(), "application/json", "no-cache")?;
    tracing::info!(url = %release.url, "published {} to {channel}", release.version);
    Ok(())
}

/// A new signing key pair: the secret key (unencrypted: it lives in a CI secret) to `path`,
/// which must not exist yet, and the public key, for `release::KEYS`, printed.
pub fn keygen(path: &Path) -> Result<()> {
    let pair = minisign::KeyPair::generate_unencrypted_keypair()?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path).with_context(|| format!("create {}", path.display()))?;
    file.write_all(pair.sk.to_box(Some("dirigent release signing key"))?.into_string().as_bytes())?;
    println!("{}", pair.pk.to_base64());
    Ok(())
}

/// Sign `data` with `DIRIGENT_SIGNING_KEY` for `version` on `platform` (the trusted comment
/// the desktop checks), and check a key the desktop trusts verifies it: a release signed
/// with any other key would be refused by every install.
fn sign(data: &[u8], version: &str, platform: &str) -> Result<String> {
    let key = std::env::var("DIRIGENT_SIGNING_KEY").context("DIRIGENT_SIGNING_KEY is not set")?;
    let key = minisign::SecretKeyBox::from_string(&key)
        .and_then(|b| b.into_unencrypted_secret_key())
        .context("DIRIGENT_SIGNING_KEY is not an unencrypted minisign secret key")?;
    let comment = release::trusted_comment(version, platform);
    let signature = minisign::sign(None, &key, data, Some(&comment), None)?;
    let trusted = release::KEYS.iter().filter_map(|k| minisign::PublicKey::from_base64(k).ok()).any(|pk| {
        let sig = minisign::SignatureBox::from_string(&signature.to_string());
        sig.is_ok_and(|sig| minisign::verify(&pk, &sig, std::io::Cursor::new(data), true, false, false).is_ok())
    });
    if !trusted {
        bail!("DIRIGENT_SIGNING_KEY doesn't match a key in dirigent_protocol::release::KEYS");
    }
    Ok(signature.to_string())
}
