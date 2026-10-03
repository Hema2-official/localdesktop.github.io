---
title: Using a Non-root User
---

A fresh install asks for a user name next to the choice of desktop; with the field left empty, Local Desktop logs in as root, as installs before it did. Some programs are better off, or only work, as a normal user:

- Chromium and Electron-based programs like VS Code are safer without root.
- AUR helpers like paru or yay refuse to run as root.

## Tell Local Desktop who to log in as

The name chosen at install time is in the config file. To change it later, or to add a user to an install that runs as root, edit the file (create it if it isn't there):

```toml title="/etc/localdesktop/localdesktop.toml"
[user]
username = "teddy"
```

_(Replace `teddy` with the name you want: lowercase letters, digits, `_` and `-`, starting with a letter.)_

The next start creates the user if it doesn't exist yet:

- a home directory, `/home/teddy`, with the usual starting files (`/etc/skel`);
- `sudo` without a password, as a member of the `wheel` group (the rule is in `/etc/sudoers.d/localdesktop`);
- no password of its own, so there is nothing to type. SSH logs in with keys; to log in with a password instead, set one with `passwd`.

The desktop, the SSH server and the shared clipboard then run as that user. Root's files stay where they were, in `/root`.

A user you created yourself (`useradd -m teddy`) is used as is, and gets the same `sudo` rule.

If the desktop doesn't come up with the new user, delete the `[user]` section or fix the name and restart. `try_username` tries a name once, see [Configurations](./4-configurations.md#special-try_-configs).
