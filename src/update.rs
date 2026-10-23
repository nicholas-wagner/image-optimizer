//! Once-a-day check against the public GitHub release.
//!
//! The check is anonymous. A token in the binary would be public, and one
//! shared token would spend a single 5,000/hour budget for every user.

use std::time::Duration;

use self_update::check_interval::UpdateCheckGuard;

const INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
pub const RELEASES_URL: &str = "https://github.com/nicholas-wagner/image-optimizer/releases/latest";

pub enum Check {
    /// The stamp file says a check already happened inside the interval.
    Skipped,
    Available(String),
    UpToDate,
    /// GitHub answered 403/429. The stamp is written so the next try waits a day.
    RateLimited,
    /// Network or other failure. The stamp is left alone so the next launch retries.
    Failed,
}

pub enum Install {
    Updated(String),
    Unwritable,
    Failed(String),
}

pub fn is_newer_than_running(version: &str) -> bool {
    match (parse_version(version), parse_version(env!("CARGO_PKG_VERSION"))) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

pub fn check_if_due() -> Check {
    let Some(guard) = guard() else {
        return Check::Failed;
    };
    match guard.should_check() {
        Ok(false) => return Check::Skipped,
        Ok(true) => {}
        Err(_) => return Check::Failed,
    }
    let updater = match updater() {
        Ok(updater) => updater,
        Err(_) => return Check::Failed,
    };
    match updater.is_update_available() {
        Ok(Some(release)) => {
            let version = release.version().to_string();
            if is_newer_than_running(&version) {
                let _ = guard.record_check();
                Check::Available(version)
            } else {
                let _ = guard.record_check();
                Check::UpToDate
            }
        }
        Ok(None) => {
            let _ = guard.record_check();
            Check::UpToDate
        }
        Err(err) if is_rate_limited(&err) => {
            let _ = guard.record_check();
            Check::RateLimited
        }
        Err(_) => Check::Failed,
    }
}

pub fn install() -> Install {
    let updater = match updater() {
        Ok(updater) => updater,
        Err(err) => return Install::Failed(err),
    };
    match updater.update() {
        Ok(self_update::VersionStatus::Updated(version)) => Install::Updated(version),
        Ok(self_update::VersionStatus::UpToDate(_)) => {
            Install::Failed("Already on the latest release.".into())
        }
        Ok(_) => Install::Failed("Already on the latest release.".into()),
        Err(err) if is_unwritable(&err) => Install::Unwritable,
        Err(err) => Install::Failed(short_message(&err)),
    }
}

fn updater() -> Result<self_update::backends::github::Update, String> {
    self_update::backends::github::Update::configure()
        .repo_owner("nicholas-wagner")
        .repo_name("image-optimizer")
        .bin_name("image-optimizer")
        .current_version(self_update::cargo_crate_version!())
        .update_strategy(self_update::update::UpdateStrategy::Latest)
        .check_install_path_writable(true)
        .timeout(Duration::from_secs(60))
        .unattended()
        .build()
        .map_err(|err| short_message(&err))
}

fn guard() -> Option<UpdateCheckGuard> {
    let bin = crate::tools::cache_bin_dir().ok()?;
    let stamp = bin.parent()?.join("update-check");
    Some(UpdateCheckGuard::new(stamp, INTERVAL))
}

fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let mut parts = text.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

fn is_rate_limited(err: &self_update::Error) -> bool {
    matches!(err, self_update::Error::RateLimited { .. })
}

fn is_unwritable(err: &self_update::Error) -> bool {
    matches!(err, self_update::Error::InstallPathNotWritable { .. })
}

fn short_message(err: &self_update::Error) -> String {
    let text = err.to_string();
    let line = text.lines().next().unwrap_or("").trim();
    let mut short: String = line.chars().take(140).collect();
    if line.chars().count() > 140 {
        short.push_str("...");
    }
    if short.is_empty() {
        "Could not reach GitHub.".into()
    } else {
        short
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release_with(names: &[&str]) -> self_update::update::Release {
        let mut builder = self_update::update::Release::builder();
        builder.version("0.2.4");
        for name in names {
            builder.asset(self_update::update::ReleaseAsset::new(
                *name,
                format!("https://example.invalid/{name}"),
            ));
        }
        builder.build().unwrap()
    }

    #[test]
    fn release_assets_match_the_shipped_names() {
        let release = release_with(&[
            "notes-linux.txt",
            "image-optimizer-x86_64",
            "image-optimizer-linux-x86_64",
            "image-optimizer-windows-x86_64.exe",
        ]);
        let linux = release
            .asset_for("x86_64-unknown-linux-gnu", None)
            .expect("linux asset");
        assert_eq!(linux.name(), "image-optimizer-linux-x86_64");
        let windows = release
            .asset_for("x86_64-pc-windows-gnu", None)
            .expect("windows asset");
        assert_eq!(windows.name(), "image-optimizer-windows-x86_64.exe");
    }

    #[test]
    fn an_asset_with_one_token_is_not_chosen() {
        let release = release_with(&["image-optimizer-x86_64", "notes-linux.txt"]);
        assert!(release
            .asset_for("x86_64-unknown-linux-gnu", None)
            .is_none());
        assert!(release.asset_for("x86_64-pc-windows-gnu", None).is_none());
    }

    #[test]
    fn a_fresh_stamp_skips_the_next_check() {
        let dir = std::env::temp_dir().join(format!(
            "image-optimizer-stamp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let stamp = dir.join("update-check");
        let guard = UpdateCheckGuard::new(&stamp, INTERVAL);
        assert!(guard.should_check().unwrap());
        guard.record_check().unwrap();
        assert!(!guard.should_check().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_higher_version_counts() {
        assert!(is_newer_than_running("9.0.0"));
        assert!(!is_newer_than_running(env!("CARGO_PKG_VERSION")));
        assert!(!is_newer_than_running("0.0.1"));
        assert!(!is_newer_than_running("nope"));
    }
}
