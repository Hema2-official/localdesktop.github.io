//! pacman's mirrors. The rootfs ships Arch Linux ARM's mirrorlist with only its redirector
//! enabled, which picks a mirror by the requester's location and doesn't always pick a near one;
//! a far one costs a round trip across the world for each of the thousand or so files a desktop's
//! install downloads. Setup times each mirror the list names once, and puts the fastest above the
//! rest of the list (`android::proot::mirrors`).

/// How many of the fastest mirrors go first.
pub const KEEP: usize = 3;
/// pacman's default number of downloads at once, and ours: with many small packages, waiting on
/// each request takes longer than the transfers.
const PARALLEL_DOWNLOADS_DEFAULT: &str = "ParallelDownloads = 5";
pub const PARALLEL_DOWNLOADS: u32 = 10;
/// How far a mirror's database may be behind the newest one seen, in seconds.
const BEHIND: u64 = 24 * 60 * 60;
/// Starts the lines setup puts at the top of the mirrorlist.
const MARK: &str = "# Local Desktop:";

/// How a mirror answered.
#[derive(Debug, Clone, PartialEq)]
pub struct Probe {
    pub server: String,
    /// How long its fetches took in all, in seconds.
    pub seconds: f64,
    /// When its `core` database last changed, in seconds since 1970.
    pub modified: Option<u64>,
}

/// The servers a mirrorlist names, enabled or commented out, in order and each once.
pub fn servers(list: &str) -> Vec<String> {
    let mut servers: Vec<String> = Vec::new();
    for line in list.lines() {
        let line = line.trim_start().trim_start_matches('#');
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        if key.trim() == "Server"
            && value.starts_with("http")
            && !servers.iter().any(|it| it == value)
        {
            servers.push(value.to_string());
        }
    }
    servers
}

/// A small file every mirror has: the `core` repository's database.
pub fn probe_url(server: &str, arch: &str) -> String {
    let base = server.replace("$arch", arch).replace("$repo", "core");
    format!("{}/core.db", base.trim_end_matches('/'))
}

/// The `count` fastest mirrors whose database isn't behind the others'. A mirror that didn't
/// say when its database changed can't be told behind, so it counts.
pub fn fastest(probes: &[Probe], count: usize) -> Vec<&Probe> {
    let newest = probes.iter().filter_map(|it| it.modified).max();
    let mut current: Vec<&Probe> = probes
        .iter()
        .filter(|it| match (it.modified, newest) {
            (Some(modified), Some(newest)) => modified + BEHIND >= newest,
            _ => true,
        })
        .collect();
    current.sort_by(|a, b| a.seconds.total_cmp(&b.seconds));
    current.truncate(count);
    current
}

/// The list without the lines setup put at its top.
pub fn stock_part(list: &str) -> &str {
    if !list.starts_with(MARK) {
        return list;
    }
    let mut rest = list;
    let mut marks = 0;
    while marks < 2 {
        let Some((line, after)) = rest.split_once('\n') else {
            return "";
        };
        if line.starts_with(MARK) {
            marks += 1;
        }
        rest = after;
    }
    rest.strip_prefix('\n').unwrap_or(rest)
}

/// `list` with `fastest` enabled first, above the list (without what an earlier ranking put
/// there), whose enabled servers pacman tries if those fail.
pub fn with_fastest_first(list: &str, fastest: &[&str]) -> String {
    let mut out = format!(
        "{MARK} the mirrors that answered fastest from this device when it was set up, \
         fastest first.\n"
    );
    for server in fastest {
        out.push_str(&format!("Server = {server}\n"));
    }
    out.push_str(&format!("{MARK} the list as Arch Linux ARM ships it.\n\n"));
    out.push_str(stock_part(list));
    out
}

/// `pacman.conf` with `ParallelDownloads` raised to ours, if it is pacman's default.
pub fn with_parallel_downloads(conf: &str) -> String {
    conf.split_inclusive('\n')
        .map(|line| {
            if line.trim_end() == PARALLEL_DOWNLOADS_DEFAULT {
                let end = &line[line.trim_end().len()..];
                format!("ParallelDownloads = {PARALLEL_DOWNLOADS}{end}")
            } else {
                line.to_string()
            }
        })
        .collect()
}

/// Seconds since 1970 of an HTTP date, `Mon, 12 Oct 2026 11:49:32 GMT`.
pub fn http_date(value: &str) -> Option<u64> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut parts = value.split_whitespace().skip(1);
    let (day, month, year, clock) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    let month = MONTHS.iter().position(|it| *it == month)? + 1;
    crate::core::gpu_drivers::unix_seconds(&format!("{year}-{month}-{day}T{clock}Z"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALARM: &str = "\
#
# Arch Linux ARM repository mirrorlist
# Generated on 2026-04-11
#

## Geo-IP based mirror selection and load balancing
Server = http://mirror.archlinuxarm.org/$arch/$repo

### Mirrors by country

### Denmark
## Aalborg
# Server = http://dk.mirror.archlinuxarm.org/$arch/$repo

### Germany
## Coburg
# Server = http://de4.mirror.archlinuxarm.org/$arch/$repo

### Hungary
## Budapest
# Server = http://hu.mirror.archlinuxarm.org/$arch/$repo

### United States
## Florida
# Server = http://fl.us.mirror.archlinuxarm.org/$arch/$repo
";

    fn probe(server: &str, seconds: f64, modified: Option<u64>) -> Probe {
        Probe {
            server: format!("http://{server}.mirror.archlinuxarm.org/$arch/$repo"),
            seconds,
            modified,
        }
    }

    #[test]
    fn should_read_every_server_of_the_list() {
        let servers = servers(ALARM);
        assert_eq!(servers.len(), 5);
        assert_eq!(servers[0], "http://mirror.archlinuxarm.org/$arch/$repo");
        assert_eq!(servers[2], "http://de4.mirror.archlinuxarm.org/$arch/$repo");
        // A ranked list names the fastest twice; each counts once.
        let ranked = with_fastest_first(ALARM, &[&servers[2]]);
        assert_eq!(super::servers(&ranked).len(), 5);
    }

    #[test]
    fn should_fetch_the_core_database() {
        assert_eq!(
            probe_url("http://hu.mirror.archlinuxarm.org/$arch/$repo", "aarch64"),
            "http://hu.mirror.archlinuxarm.org/aarch64/core/core.db"
        );
    }

    #[test]
    fn should_put_the_fastest_current_mirrors_first() {
        let day = 24 * 60 * 60;
        let probes = [
            probe("fl.us", 0.66, Some(10 * day)),
            probe("de4", 0.12, Some(10 * day)),
            // Fast, but its database is two days old.
            probe("stale", 0.05, Some(8 * day)),
            probe("hu", 0.28, Some(10 * day - 3600)),
            probe("dk", 0.25, None),
        ];
        let fastest: Vec<&str> = fastest(&probes, 3)
            .iter()
            .map(|it| it.server.split('.').next().unwrap())
            .collect();
        assert_eq!(fastest, ["http://de4", "http://dk", "http://hu"]);
        assert!(super::fastest(&[], 3).is_empty());
    }

    #[test]
    fn should_keep_the_list_below_the_fastest() {
        let first = with_fastest_first(ALARM, &["http://de4/$arch/$repo", "http://hu/$arch/$repo"]);
        assert!(first.starts_with("# Local Desktop: the mirrors"));
        assert!(
            first.contains("\nServer = http://de4/$arch/$repo\nServer = http://hu/$arch/$repo\n")
        );
        assert!(first.ends_with(ALARM));
        assert_eq!(stock_part(&first), ALARM);
        // Ranking again replaces the earlier lines.
        let again = with_fastest_first(&first, &["http://dk/$arch/$repo"]);
        assert!(!again.contains("http://de4/$arch/$repo\n"));
        assert_eq!(stock_part(&again), ALARM);
        assert_eq!(stock_part(ALARM), ALARM);
    }

    #[test]
    fn should_raise_only_the_default_parallel_downloads() {
        let conf = "[options]\nParallelDownloads = 5\nCheckSpace\n";
        assert_eq!(
            with_parallel_downloads(conf),
            "[options]\nParallelDownloads = 10\nCheckSpace\n"
        );
        for kept in [
            "[options]\nParallelDownloads = 3\n",
            "[options]\n#ParallelDownloads = 5\n",
            "[options]\n",
        ] {
            assert_eq!(with_parallel_downloads(kept), kept);
        }
    }

    #[test]
    fn should_read_http_dates() {
        assert_eq!(
            http_date("Mon, 12 Oct 2026 11:49:32 GMT"),
            Some(1_791_805_772)
        );
        assert_eq!(http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(http_date("yesterday"), None);
    }
}
