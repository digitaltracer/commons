//! `cargo xtask release`: a universal, Developer ID–signed, notarized and stapled Mac app and
//! dmg, plus the updater's `.app.tar.gz`, `.sig` and `latest.json`. It publishes nothing.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use base64::Engine;

use crate::{output, root, run};

const NOTARY_PROFILE: &str = "commons-notary";
const TARGET: &str = "universal-apple-darwin";
const DOWNLOADS: &str = "https://github.com/digitaltracer/commons/releases/download";
/// Tauri notarizes when these are set; the release notarizes with the keychain profile instead.
const TAURI_NOTARY_VARS: &[&str] = &[
    "APPLE_ID",
    "APPLE_PASSWORD",
    "APPLE_TEAM_ID",
    "APPLE_API_KEY",
    "APPLE_API_ISSUER",
    "APPLE_API_KEY_PATH",
];

struct Inputs {
    identity: String,
    team: String,
    key: PathBuf,
    pubkey: minisign_verify::PublicKey,
    overlay: Option<PathBuf>,
}

const USAGE: &str = "usage: cargo xtask release [patch | minor | major | <x.y.z>] \
                     [--config <tauri config overlay, for test builds>]";

pub fn release(args: &[String]) -> Result<()> {
    let mut bump = None;
    let mut overlay = None;
    let mut rest = args.iter();
    while let Some(a) = rest.next() {
        match a.as_str() {
            "--config" => {
                let path = rest.next().context(USAGE)?;
                overlay = Some(std::fs::canonicalize(path)?);
            }
            b if bump.is_none() && !b.starts_with('-') => bump = Some(b.to_string()),
            _ => bail!(USAGE),
        }
    }
    if !cfg!(target_os = "macos") {
        bail!("cargo xtask release builds the Mac app and runs only on macOS");
    }
    let inputs = preflight(overlay)?;
    if let Some(b) = bump {
        bump_version(&b)?;
    }
    let version = desktop_version()?;
    println!(
        "releasing Commons {version}, signed as {:?} (team {})",
        inputs.identity, inputs.team
    );

    let root = root();
    let dirty = output(
        Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&root),
    )?;
    if !dirty.trim().is_empty() {
        eprintln!("warning: the working tree has uncommitted changes, and they go into this build");
    }
    let out = root.join("target/release-mac").join(&version);
    let work = root.join("target/release-mac/work");
    for dir in [&out, &work] {
        if dir.exists() {
            std::fs::remove_dir_all(dir)?;
        }
        std::fs::create_dir_all(dir)?;
    }

    let mut build = Command::new("cargo");
    build
        .args(["tauri", "build", "--target", TARGET, "--bundles", "app"])
        .current_dir(root.join("apps/desktop"))
        .env("APPLE_SIGNING_IDENTITY", &inputs.identity);
    if let Some(o) = &inputs.overlay {
        build.arg("--config").arg(o);
    }
    for v in TAURI_NOTARY_VARS {
        build.env_remove(v);
    }
    run(&mut build)?;
    let built = root.join(format!("target/{TARGET}/release/bundle/macos/Commons.app"));
    check_signature(&built, &inputs.team)?;

    let app = out.join("Commons.app");
    run(Command::new("ditto").arg(&built).arg(&app))?;
    let zip = work.join("Commons.zip");
    run(Command::new("ditto")
        .args(["-c", "-k", "--keepParent"])
        .arg(&app)
        .arg(&zip))?;
    notarize(&zip)?;
    staple(&app)?;

    let dmg = out.join(format!("Commons_{version}_universal.dmg"));
    let stage = work.join("dmg");
    std::fs::create_dir_all(&stage)?;
    run(Command::new("ditto")
        .arg(&app)
        .arg(stage.join("Commons.app")))?;
    std::os::unix::fs::symlink("/Applications", stage.join("Applications"))?;
    run(Command::new("hdiutil")
        .args(["create", "-quiet", "-volname", "Commons", "-fs", "HFS+"])
        .args(["-format", "UDZO", "-ov", "-srcfolder"])
        .arg(&stage)
        .arg(&dmg))?;
    run(Command::new("codesign")
        .args(["--sign", &inputs.identity, "--timestamp"])
        .arg(&dmg))?;
    notarize(&dmg)?;
    staple(&dmg)?;

    let tarball = format!("Commons_{version}_universal.app.tar.gz");
    let tar_path = out.join(&tarball);
    run(Command::new("tar")
        .env("COPYFILE_DISABLE", "1")
        .arg("-czf")
        .arg(&tar_path)
        .arg("-C")
        .arg(&out)
        .arg("Commons.app"))?;
    println!("signing the update; the signer asks for the updater key's password");
    run(Command::new("cargo")
        .args(["tauri", "signer", "sign", "--app-version", &version])
        .arg("--private-key-path")
        .arg(&inputs.key)
        .arg(&tar_path)
        .env_remove("TAURI_SIGNING_PRIVATE_KEY")
        .current_dir(&root))?;
    let sig_path = out.join(format!("{tarball}.sig"));
    let signature = std::fs::read_to_string(&sig_path)
        .with_context(|| format!("reading {}", sig_path.display()))?;
    verify_update(&inputs.pubkey, &tar_path, &signature, &version)?;

    let pub_date = output(Command::new("date").args(["-u", "+%Y-%m-%dT%H:%M:%SZ"]))?;
    let url = format!("{DOWNLOADS}/v{version}/{tarball}");
    let platform = serde_json::json!({ "signature": signature.trim(), "url": url });
    // The universal tarball serves both architectures.
    let latest = serde_json::json!({
        "version": version,
        "pub_date": pub_date.trim(),
        "platforms": { "darwin-aarch64": platform, "darwin-x86_64": platform },
    });
    std::fs::write(
        out.join("latest.json"),
        serde_json::to_string_pretty(&latest)? + "\n",
    )?;

    assess(&app, &["-t", "exec"])?;
    assess(
        &dmg,
        &["-t", "open", "--context", "context:primary-signature"],
    )?;
    run(Command::new("xcrun")
        .args(["stapler", "validate"])
        .arg(&app))?;
    run(Command::new("xcrun")
        .args(["stapler", "validate"])
        .arg(&dmg))?;
    std::fs::remove_dir_all(&work)?;

    println!("\nCommons {version} is ready in {}:", out.display());
    for f in std::fs::read_dir(&out)? {
        println!("  {}", f?.file_name().to_string_lossy());
    }
    println!(
        "\nNothing was published. See docs/release.md, \"Publish\", for the GitHub release (tag v{version})."
    );
    Ok(())
}

/// Sets the app's version (`patch`, `minor`, `major` or an exact `x.y.z`, which must be
/// higher) and commits it with `Cargo.lock`. Refuses a dirty tree, so the commit holds
/// only the bump.
fn bump_version(how: &str) -> Result<()> {
    let root = root();
    let dirty = output(
        Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&root),
    )?;
    if !dirty.trim().is_empty() {
        bail!("commit or stash your changes first: the version bump is committed on its own");
    }
    let current = desktop_version()?;
    let next = next_version(&current, how)?;
    let tags = output(
        Command::new("git")
            .args(["tag", "--list", &format!("v{next}")])
            .current_dir(&root),
    )?;
    if !tags.trim().is_empty() {
        bail!("tag v{next} already exists");
    }
    let path = root.join("apps/desktop/Cargo.toml");
    let manifest = std::fs::read_to_string(&path)?;
    let old = format!("version = \"{current}\"");
    if !manifest.contains(&old) {
        bail!("couldn't find {old} in apps/desktop/Cargo.toml");
    }
    std::fs::write(
        &path,
        manifest.replacen(&old, &format!("version = \"{next}\""), 1),
    )?;
    run(Command::new("cargo")
        .args(["update", "--workspace", "--offline", "--quiet"])
        .current_dir(&root))?;
    run(Command::new("git")
        .args(["add", "apps/desktop/Cargo.toml", "Cargo.lock"])
        .current_dir(&root))?;
    run(Command::new("git")
        .args([
            "commit",
            "--quiet",
            "-m",
            &format!("chore(release): Commons {next}"),
        ])
        .current_dir(&root))?;
    println!("bumped Commons {current} -> {next} and committed it; push it before publishing");
    Ok(())
}

fn next_version(current: &str, how: &str) -> Result<String> {
    let parse = |v: &str| -> Result<[u64; 3]> {
        let parts: Vec<u64> = v
            .split('.')
            .map(str::parse)
            .collect::<Result<_, _>>()
            .with_context(|| format!("{v:?} is not a version like 1.2.3"))?;
        <[u64; 3]>::try_from(parts)
            .map_err(|_| anyhow::anyhow!("{v:?} is not a version like 1.2.3"))
    };
    let [ma, mi, pa] = parse(current)?;
    let next = match how {
        "patch" => [ma, mi, pa + 1],
        "minor" => [ma, mi + 1, 0],
        "major" => [ma + 1, 0, 0],
        exact => {
            let v = parse(exact)?;
            if v <= [ma, mi, pa] {
                bail!("{exact} is not higher than the current {current}");
            }
            v
        }
    };
    Ok(format!("{}.{}.{}", next[0], next[1], next[2]))
}

fn desktop_version() -> Result<String> {
    let manifest = std::fs::read_to_string(root().join("apps/desktop/Cargo.toml"))?;
    manifest
        .lines()
        .find_map(|l| l.strip_prefix("version = "))
        .map(|v| v.trim_matches('"').to_string())
        .context("no version in apps/desktop/Cargo.toml")
}

/// Fails before the long build when anything the release needs is missing.
fn preflight(overlay: Option<PathBuf>) -> Result<Inputs> {
    let identity = std::env::var("APPLE_SIGNING_IDENTITY")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .context(
            "APPLE_SIGNING_IDENTITY is not set. Export your \"Developer ID Application: Name (TEAMID)\" \
             identity; `security find-identity -v -p codesigning` lists them",
        )?;
    if !identity.starts_with("Developer ID Application: ") {
        bail!(
            "APPLE_SIGNING_IDENTITY must be a \"Developer ID Application: …\" identity, not {identity:?}"
        );
    }
    let team = identity
        .rsplit_once('(')
        .and_then(|(_, t)| t.strip_suffix(')'))
        .context("APPLE_SIGNING_IDENTITY should end with the team ID in parentheses")?
        .to_string();
    let identities =
        output(Command::new("security").args(["find-identity", "-v", "-p", "codesigning"]))?;
    if !identities.contains(&format!("\"{identity}\"")) {
        bail!("{identity:?} is not a valid code-signing identity in your keychain");
    }

    let installed = output(Command::new("rustup").args(["target", "list", "--installed"]))?;
    for t in ["aarch64-apple-darwin", "x86_64-apple-darwin"] {
        if !installed.lines().any(|l| l.trim() == t) {
            bail!("a universal build needs the {t} target: rustup target add {t}");
        }
    }

    let key = match std::env::var_os("TAURI_SIGNING_PRIVATE_KEY_PATH") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(std::env::var("HOME").context("HOME is not set")?)
            .join(".tauri/commons-updater.key"),
    };
    if !key.is_file() {
        bail!(
            "no updater key at {}. Generate one with: cargo tauri signer generate -w ~/.tauri/commons-updater.key",
            key.display()
        );
    }
    let pubkey = updater_pubkey(overlay.as_deref())?;
    let notary = Command::new("xcrun")
        .args([
            "notarytool",
            "history",
            "--keychain-profile",
            NOTARY_PROFILE,
        ])
        .stdout(Stdio::null())
        .output()
        .context("running xcrun notarytool")?;
    if !notary.status.success() {
        bail!(
            "the notary keychain profile {NOTARY_PROFILE:?} is missing or rejected: {}\n\
             Store it with: xcrun notarytool store-credentials {NOTARY_PROFILE} --apple-id <email> --team-id {team}",
            String::from_utf8_lossy(&notary.stderr).trim()
        );
    }

    Ok(Inputs {
        identity,
        team,
        key,
        pubkey,
        overlay,
    })
}

/// The public key the build embeds: the overlay's, else `tauri.conf.json`'s.
fn updater_pubkey(overlay: Option<&Path>) -> Result<minisign_verify::PublicKey> {
    let read = |p: &Path| -> Result<serde_json::Value> {
        Ok(serde_json::from_slice(
            &std::fs::read(p).with_context(|| format!("reading {}", p.display()))?,
        )?)
    };
    let base = read(&root().join("apps/desktop/tauri.conf.json"))?;
    let over = overlay.map(read).transpose()?;
    let b64 = over
        .as_ref()
        .and_then(|o| o["plugins"]["updater"]["pubkey"].as_str())
        .or_else(|| base["plugins"]["updater"]["pubkey"].as_str())
        .context("tauri.conf.json has no plugins.updater.pubkey")?;
    let text = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .context("plugins.updater.pubkey is not base64")?;
    minisign_verify::PublicKey::decode(std::str::from_utf8(&text)?)
        .map_err(|e| anyhow::anyhow!("plugins.updater.pubkey is not a minisign public key: {e}"))
}

fn check_signature(app: &Path, team: &str) -> Result<()> {
    run(Command::new("codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(app))?;
    let out = Command::new("codesign").args(["-dvv"]).arg(app).output()?;
    let info = String::from_utf8_lossy(&out.stderr);
    if !info.contains("(runtime)") {
        bail!(
            "{} is not signed with the hardened runtime:\n{info}",
            app.display()
        );
    }
    if !info.contains(&format!("TeamIdentifier={team}")) {
        bail!("{} is not signed by team {team}:\n{info}", app.display());
    }
    Ok(())
}

fn notarize(path: &Path) -> Result<()> {
    println!(
        "notarizing {} (this usually takes a few minutes)",
        path.display()
    );
    let out = Command::new("xcrun")
        .args(["notarytool", "submit"])
        .arg(path)
        .args([
            "--keychain-profile",
            NOTARY_PROFILE,
            "--wait",
            "--output-format",
            "json",
        ])
        .output()
        .context("running xcrun notarytool submit")?;
    let reply: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let status = reply["status"].as_str().unwrap_or("");
    if status == "Accepted" {
        println!(
            "notarized: submission {}",
            reply["id"].as_str().unwrap_or("?")
        );
        return Ok(());
    }
    if let Some(id) = reply["id"].as_str() {
        let _ = Command::new("xcrun")
            .args([
                "notarytool",
                "log",
                id,
                "--keychain-profile",
                NOTARY_PROFILE,
            ])
            .status();
    }
    bail!(
        "notarization of {} ended with {:?}: {}{}",
        path.display(),
        status,
        reply["message"].as_str().unwrap_or(""),
        String::from_utf8_lossy(&out.stderr).trim()
    )
}

fn staple(path: &Path) -> Result<()> {
    run(Command::new("xcrun")
        .args(["stapler", "staple", "-q"])
        .arg(path))
}

/// Gatekeeper's verdict, as a first-time download would get it.
fn assess(path: &Path, how: &[&str]) -> Result<()> {
    let out = Command::new("spctl")
        .args(["-a", "-vv"])
        .args(how)
        .arg(path)
        .output()?;
    let verdict = String::from_utf8_lossy(&out.stderr);
    print!("{verdict}");
    if !out.status.success() || !verdict.contains("source=Notarized Developer ID") {
        bail!("Gatekeeper does not accept {} as notarized", path.display());
    }
    Ok(())
}

/// Checks the update against the key the app ships with, so a mismatched key fails here and
/// not on every user's next update.
fn verify_update(
    pubkey: &minisign_verify::PublicKey,
    tarball: &Path,
    signature: &str,
    version: &str,
) -> Result<()> {
    let text = base64::engine::general_purpose::STANDARD.decode(signature.trim())?;
    let sig = minisign_verify::Signature::decode(std::str::from_utf8(&text)?)
        .map_err(|e| anyhow::anyhow!("bad update signature: {e}"))?;
    pubkey
        .verify(&std::fs::read(tarball)?, &sig, false)
        .map_err(|e| {
            anyhow::anyhow!(
                "the update signature does not verify with plugins.updater.pubkey ({e}): \
                 the updater key and the public key in the app do not match"
            )
        })?;
    if !sig
        .trusted_comment()
        .split('\t')
        .any(|f| f == format!("version:{version}"))
    {
        bail!("the update signature is not bound to version {version}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::next_version;

    #[test]
    fn bumps_and_exact_versions() {
        assert_eq!(next_version("0.1.0", "patch").unwrap(), "0.1.1");
        assert_eq!(next_version("0.1.9", "minor").unwrap(), "0.2.0");
        assert_eq!(next_version("0.4.2", "major").unwrap(), "1.0.0");
        assert_eq!(next_version("0.1.0", "0.3.0").unwrap(), "0.3.0");
        assert!(next_version("0.2.0", "0.1.5").is_err(), "not higher");
        assert!(next_version("0.2.0", "0.2.0").is_err(), "same");
        assert!(next_version("0.2.0", "two").is_err(), "not a version");
        assert!(next_version("0.2.0", "1.2").is_err(), "too short");
    }
}
