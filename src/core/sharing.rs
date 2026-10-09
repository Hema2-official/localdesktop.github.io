//! Where what Android shares goes on the desktop, and how the desktop is told about it
//! (`guest::shared`).

/// The session user's home in the guest, from `/etc/passwd`.
pub fn home_from_passwd(passwd: &str, user: &str) -> Option<String> {
    passwd.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        (fields.len() >= 7 && fields[0] == user && fields[5].starts_with('/'))
            .then(|| fields[5].to_string())
    })
}

/// The user's Downloads folder: `XDG_DOWNLOAD_DIR` in `user-dirs.dirs` (`xdg-user-dirs`), where
/// the user has one, or `~/Downloads`.
pub fn download_directory(home: &str, user_dirs: Option<&str>) -> String {
    let home = home.trim_end_matches('/');
    let configured = user_dirs.into_iter().flat_map(str::lines).find_map(|line| {
        let value = line.trim().strip_prefix("XDG_DOWNLOAD_DIR=")?;
        let value = value.trim().trim_matches('"');
        if let Some(rest) = value.strip_prefix("$HOME") {
            Some(format!("{home}{rest}"))
        } else {
            value.starts_with('/').then(|| value.to_string())
        }
    });
    match configured {
        // xdg-user-dirs points a folder that isn't wanted at the home itself.
        Some(directory) if directory.trim_end_matches('/') != home => {
            directory.trim_end_matches('/').to_string()
        }
        _ => format!("{home}/Downloads"),
    }
}

/// `name` for the `n`th file of that name: "photo (2).jpg", as `ShareActivity` numbers them too.
pub fn numbered(name: &str, n: u32) -> String {
    match name.rfind('.') {
        Some(dot) if dot > 0 => format!("{} ({n}){}", &name[..dot], &name[dot..]),
        _ => format!("{name} ({n})"),
    }
}

/// `path` as a `file://` URI.
pub fn file_uri(path: &str) -> String {
    let mut uri = String::from("file://");
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                uri.push(byte as char)
            }
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    uri
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_find_the_users_home() {
        let passwd = "root:x:0:0::/root:/bin/bash\n\
                      alarm:x:1000:1000::/home/alarm:/bin/bash\n\
                      tester:x:1001:1001::/home/tester:/bin/bash\n";
        assert_eq!(
            home_from_passwd(passwd, "tester").as_deref(),
            Some("/home/tester")
        );
        assert_eq!(home_from_passwd(passwd, "root").as_deref(), Some("/root"));
        assert_eq!(home_from_passwd(passwd, "test"), None);
    }

    #[test]
    fn should_put_shares_in_the_downloads_folder() {
        assert_eq!(
            download_directory("/home/tester", None),
            "/home/tester/Downloads"
        );
        let german = "# This file is written by xdg-user-dirs-update\n\
                      XDG_DESKTOP_DIR=\"$HOME/Schreibtisch\"\n\
                      XDG_DOWNLOAD_DIR=\"$HOME/Downloads/Android\"\n";
        assert_eq!(
            download_directory("/home/tester/", Some(german)),
            "/home/tester/Downloads/Android"
        );
        let elsewhere = "XDG_DOWNLOAD_DIR=\"/data/incoming/\"\n";
        assert_eq!(
            download_directory("/root", Some(elsewhere)),
            "/data/incoming"
        );
        // Turned off, which points it at the home.
        let off = "XDG_DOWNLOAD_DIR=\"$HOME/\"\n";
        assert_eq!(download_directory("/root", Some(off)), "/root/Downloads");
    }

    #[test]
    fn should_number_names_before_the_extension() {
        assert_eq!(numbered("photo.jpg", 2), "photo (2).jpg");
        assert_eq!(numbered("archive.tar.gz", 3), "archive.tar (3).gz");
        assert_eq!(numbered("README", 2), "README (2)");
        assert_eq!(numbered(".hidden", 2), ".hidden (2)");
    }

    #[test]
    fn should_make_file_uris() {
        assert_eq!(
            file_uri("/home/tester/Downloads/Mein Foto (2).jpg"),
            "file:///home/tester/Downloads/Mein%20Foto%20%282%29.jpg"
        );
        assert_eq!(file_uri("/tmp/é"), "file:///tmp/%C3%A9");
    }
}
