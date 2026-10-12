//! Which release of Mesa for Adreno (https://github.com/lfdevs/mesa-for-android-container) to
//! install, or to update to, from the project's GitHub releases: `setup_adreno_mesa` installs
//! it, `android::gpu_drivers` looks for newer ones.

use serde_json::Value;

/// How often `[graphics] driver_updates` looks for a newer release.
pub const CHECK_INTERVAL_SECS: u64 = 7 * 24 * 60 * 60;

/// A release with a build for Arch Linux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    /// The archive with the Arch packages.
    pub name: String,
    pub url: String,
    pub sha256: String,
    /// When it was published, in seconds since 1970.
    pub published: Option<u64>,
}

/// The release named `tag` in a GitHub releases listing, or without a name the newest regular
/// one (the `turnip-` releases only carry the Vulkan driver) other than `skip`.
pub fn release(releases: &Value, tag: Option<&str>, skip: Option<&str>) -> Option<Release> {
    releases
        .as_array()?
        .iter()
        .filter(|release| release["draft"] == false)
        .filter(|release| match tag {
            Some(tag) => release["tag_name"] == tag,
            None => {
                let name = release["tag_name"].as_str().unwrap_or_default();
                release["prerelease"] == false && name.starts_with("mesa-") && Some(name) != skip
            }
        })
        .find_map(arch_build)
}

fn arch_build(release: &Value) -> Option<Release> {
    let asset = release["assets"].as_array()?.iter().find(|asset| {
        asset["name"]
            .as_str()
            .is_some_and(|name| name.ends_with("_archlinux_arm64.tar"))
    })?;
    let name = asset["name"].as_str()?.to_string();
    // GitHub's own digest, or the checksum list in the release notes.
    let sha256 = asset["digest"]
        .as_str()
        .and_then(|it| it.strip_prefix("sha256:"))
        .map(str::to_string)
        .or_else(|| {
            release["body"].as_str()?.lines().find_map(|line| {
                let (hash, file) = line.trim().split_once(char::is_whitespace)?;
                (file.trim() == name).then(|| hash.to_string())
            })
        })?;
    Some(Release {
        tag: release["tag_name"].as_str()?.to_string(),
        name,
        url: asset["browser_download_url"].as_str()?.to_string(),
        sha256: sha256.to_lowercase(),
        published: release["published_at"].as_str().and_then(unix_seconds),
    })
}

/// Whether `newest` is newer than the drivers installed: the release named `installed`, or,
/// for drivers installed before the name was kept, the newest at `installed_at`.
pub fn is_newer(newest: &Release, installed: Option<&str>, installed_at: Option<u64>) -> bool {
    match (installed, newest.published, installed_at) {
        (Some(installed), _, _) => installed != newest.tag,
        (None, Some(published), Some(installed_at)) => published > installed_at,
        _ => false,
    }
}

/// Whether it's time to look for a newer release again.
pub fn due(last_checked: Option<u64>, now: u64) -> bool {
    last_checked.is_none_or(|it| now.saturating_sub(it) >= CHECK_INTERVAL_SECS)
}

/// Seconds since 1970 of a time as GitHub writes it, `2026-08-25T06:05:41Z`.
fn unix_seconds(time: &str) -> Option<u64> {
    let (date, clock) = time.strip_suffix('Z')?.split_once('T')?;
    let numbers = |text: &str, separator| -> Option<Vec<i64>> {
        text.split(separator).map(|it| it.parse().ok()).collect()
    };
    let (date, clock) = (numbers(date, '-')?, numbers(clock, ':')?);
    let (&[year, month, day], &[hour, minute, second]) = (date.as_slice(), clock.as_slice()) else {
        return None;
    };
    // Days since 1970 of a date in the Gregorian calendar (Howard Hinnant's days_from_civil).
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    u64::try_from(days * 86_400 + hour * 3_600 + minute * 60 + second).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn listing() -> Value {
        let asset = |version: &str| {
            json!([{
                "name": format!("mesa-for-android-container_{version}_archlinux_arm64.tar"),
                "browser_download_url": format!("https://example.org/{version}.tar"),
                "digest": "sha256:ABC123",
            }])
        };
        json!([
            { "tag_name": "turnip-26.3.0-devel-20260824", "draft": false, "prerelease": false,
              "published_at": "2026-08-25T06:11:53Z", "assets": [] },
            { "tag_name": "mesa-26.3.0-devel-20260824", "draft": false, "prerelease": false,
              "published_at": "2026-08-25T06:05:41Z", "assets": asset("26.3.0-devel-20260824") },
            { "tag_name": "mesa-26.2.0-devel-20260709", "draft": false, "prerelease": false,
              "published_at": "2026-07-09T15:06:56Z", "assets": asset("26.2.0-devel-20260709") },
        ])
    }

    #[test]
    fn should_pick_the_newest_mesa_release_or_the_one_named() {
        let newest = release(&listing(), None, None).unwrap();
        assert_eq!(newest.tag, "mesa-26.3.0-devel-20260824");
        assert_eq!(newest.sha256, "abc123");
        assert_eq!(newest.published, Some(1_787_637_941));
        let older = release(&listing(), Some("mesa-26.2.0-devel-20260709"), None).unwrap();
        assert_eq!(older.url, "https://example.org/26.2.0-devel-20260709.tar");
        // One that proved broken here: the one before it.
        let skipped = release(&listing(), None, Some("mesa-26.3.0-devel-20260824"));
        assert_eq!(skipped.unwrap().tag, "mesa-26.2.0-devel-20260709");
        assert_eq!(
            release(&listing(), Some("turnip-26.3.0-devel-20260824"), None),
            None
        );
        assert_eq!(
            release(&json!({ "message": "rate limited" }), None, None),
            None
        );
    }

    #[test]
    fn should_tell_a_newer_release() {
        let newest = release(&listing(), None, None).unwrap();
        assert!(!is_newer(&newest, Some("mesa-26.3.0-devel-20260824"), None));
        assert!(is_newer(&newest, Some("mesa-26.2.0-devel-20260709"), None));
        // Drivers installed before the release's name was kept: by the time.
        assert!(!is_newer(&newest, None, Some(1_790_000_000)));
        assert!(is_newer(&newest, None, Some(1_787_000_000)));
        assert!(!is_newer(&newest, None, None));
    }

    #[test]
    fn should_look_again_after_a_week() {
        assert!(due(None, 1_000));
        assert!(!due(Some(1_000), 1_000 + CHECK_INTERVAL_SECS - 1));
        assert!(due(Some(1_000), 1_000 + CHECK_INTERVAL_SECS));
    }

    #[test]
    fn should_read_githubs_times() {
        assert_eq!(unix_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(unix_seconds("2000-02-29T12:00:00Z"), Some(951_825_600));
        assert_eq!(unix_seconds("2026-08-25T06:05:41Z"), Some(1_787_637_941));
        assert_eq!(unix_seconds("2026-08-25 06:05:41"), None);
    }
}
