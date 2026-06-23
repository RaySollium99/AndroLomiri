use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Write,
};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(not(test))]
pub const ARCH_FS_ROOT: &str = "/data/data/app.polarbear/files/arch";
#[cfg(test)]
pub const ARCH_FS_ROOT: &str = "/data/local/tmp/arch";

pub const ARCH_FS_ARCHIVE: &str = "https://github.com/termux/proot-distro/releases/download/v4.18.0/ubuntu-noble-aarch64-pd-v4.18.0.tar.xz";

/// Project homepage, also the online documentation entry point.
pub const DOCS_HOME_URL: &str = "https://localdesktop.github.io/";

/// Download URL for the offline User Manual PDF matching the running version.
/// The release asset is dot-free/hyphenated (GitHub turns spaces into dots).
pub fn user_manual_url() -> String {
    format!(
        "https://github.com/localdesktop/localdesktop.github.io/releases/download/v{VERSION}/Local-Desktop-v{VERSION}-User-Manual.pdf"
    )
}

pub const WAYLAND_SOCKET_NAME: &str = "wayland-0";

pub const MAX_PANEL_LOG_ENTRIES: usize = 100;

pub const SENTRY_DSN: &str = "https://d8af27f864ade027ff81ecadea91b02e@o4509548388417536.ingest.de.sentry.io/4509548392480848";

/// PulseAudio Server Address and port
pub const PULSE_GUEST_SERVER: &str = "tcp:127.0.0.1:14713";

/// Make sure the config keys are all lowercase, and config values are single-line. Use \n for multi-line config values if needed
/// If a key exists multiple time, the first entry is applied
/// If a `try_` config exsists multiple time, the last entry is applied
/// But in general, it is **invalid** to have duplicated config keys inside a TOML file
pub const CONFIG_FILE: &str = "/etc/localdesktop/localdesktop.toml";

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct LocalConfig {
    #[serde(default)]
    pub user: UserConfig,

    /// What happens if we don't assign this `#[serde(default)]` attribute?
    /// The answer: If the user omits the `[command]` group, the WHOLE config fails to parse
    /// => The default `[user]` group is applied (with `username=root`) even if the `[user]` settings are completely valid.
    /// => So make sure that every config group has a `#[serde(default)]` attribute to avoid invalid sections breaking unrelated parts of the config.
    #[serde(default)]
    pub command: CommandConfig,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UserConfig {
    pub username: String,
}

impl Default for UserConfig {
    fn default() -> Self {
        Self {
            username: "root".to_string(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CommandConfig {
    #[serde(default = "default_check")]
    pub check: String,
    #[serde(default = "default_install")]
    pub install: String,
    #[serde(default = "default_launch")]
    pub launch: String,
}

fn default_check() -> String {
    "dpkg -s gcc && dpkg -s fonts-noto-core && dpkg -s dpkg-dev && dpkg -s lomiri && dpkg -s lomiri-desktop-session && dpkg -s dbus-x11 && dpkg -s mir-graphics-drivers-desktop && dpkg -s click && dpkg -s labwc && dpkg -s wlr-randr && dpkg -s xdg-desktop-portal && dpkg -s xdg-desktop-portal-gtk && dpkg -s evince && dpkg -s pulseaudio && dpkg -s lomiri-wallpapers && dpkg -s accountsservice"
    .to_string()
}

fn default_install() -> String {
    // Disgusting command, i am very well aware
    // Needed because installing click on PRoot-Distro is broken.
    // Workaround works so there's no need to do much for now
    // Fixing this is on Canonical and UBports.
    // (Tip: Add dpkg-dev to click's depedendencies to fix)
    // That's on them, leave me alone - RaySollium99
    "stdbuf -oL bash -c 'export DEBIAN_FRONTEND=noninteractive; echo -e \"#!/bin/sh\\nexit 101\" > /usr/sbin/policy-rc.d; chmod +x /usr/sbin/policy-rc.d; apt-get update; apt-get -y -o Dpkg::Progress-Fancy=0 -o APT::Color=0 full-upgrade; apt-get -y -o Dpkg::Progress-Fancy=0 -o APT::Color=0 install gcc dpkg-dev fonts-noto-core lomiri lomiri-desktop-session dbus-x11 mir-graphics-drivers-desktop labwc wlr-randr xdg-desktop-portal xdg-desktop-portal-gtk evince pulseaudio lomiri-wallpapers accountsservice'"
    .to_string()
}

fn default_launch() -> String {
    // Equally disgusting command, very well aware of that
    // That whole C library is needed because Mir's `poll()` is broken on PROOT
    // PKill/KillAll is needed because Android doesn't properly kill processes when you close the app
    // I'll 1000% move this to `src/android/proot` for M3, for now we just want Lomiri booting which this command does
    // Command and Fix is courtesy of The Slop Machine known as Gemini 3.1 Pro / Google Antigravity. - RaySollium99
    format!("bash -c 'killall -9 dbus-daemon dbus-run-session lomiri accounts-daemon 2>/dev/null; pkill -9 dbus-daemon; pkill -9 dbus-run-session; pkill -9 lomiri; rm -rf /tmp/run /tmp/dbus-* /tmp/.X11-unix /var/run/dbus/* /run/dbus/* /var/run/user/*; export PULSE_SERVER={PULSE_GUEST_SERVER}; mkdir -p /tmp/run /var/run/dbus /tmp/.X11-unix; chmod 700 /tmp/run; chmod 1777 /tmp/.X11-unix; ln -sf /tmp/wayland-0 /tmp/run/wayland-0; export XDG_RUNTIME_DIR=/tmp/run; export MIR_SERVER_WAYLAND_HOST=wayland-0; export XDG_SESSION_TYPE=wayland; export GALLIUM_DRIVER=softpipe; export LOMIRI_TESTING=1; export QT_WAYLAND_DISABLE_WINDOWDECORATION=1; echo -e \"#define _GNU_SOURCE\\n#include <poll.h>\\n#include <errno.h>\\n#include <dlfcn.h>\\ntypedef int (*poll_t)(struct pollfd *, nfds_t, int);\\nint poll(struct pollfd *fds, nfds_t nfds, int timeout) {{ static poll_t real_poll = 0; if (!real_poll) real_poll = (poll_t)dlsym(RTLD_NEXT, \\\"poll\\\"); int ret; do {{ ret = real_poll(fds, nfds, timeout); }} while (ret == -1 && errno == EINTR); return ret; }}\" > /tmp/poll_fix.c; if [ ! -f /tmp/poll_fix.so ]; then gcc -shared -fPIC /tmp/poll_fix.c -o /tmp/poll_fix.so -ldl; fi; export LD_PRELOAD=/tmp/poll_fix.so; dbus-daemon --system --nofork --nopidfile > /tmp/dbus-system.log 2>&1 & sleep 1; /usr/lib/accountsservice/accounts-daemon > /tmp/accounts.log 2>&1 & dbus-run-session lomiri > /tmp/launch.log 2>&1; echo \"LOMIRI_EXIT_CODE=$?\" >> /tmp/launch.log'")
    .to_string()
}

impl Default for CommandConfig {
    fn default() -> Self {
        Self {
            check: default_check(),
            install: default_install(),
            launch: default_launch(),
        }
    }
}

/// This function does 2 major tasks:
/// - Read config from `CONFIG_FILE`, and override configs with their `try_*` versions, and return the configs line by line
/// - Write back to the config file, with `try_*` configs commented out
///
/// **Important**: As each call to this function will comment out the `try_*` config, it is **non-idempotent**.
fn process_config_file(full_config_path: String) -> Vec<String> {
    let mut write_back_lines: Vec<String> = vec![];
    let mut effective_config: Vec<String> = vec![];

    if let Ok(content) = fs::read_to_string(&full_config_path) {
        for line in content.lines() {
            let trimmed = line.trim();

            if let Some((key, value)) = trimmed.split_once('=') {
                let key = key.trim();
                let value = value.trim();

                if key.starts_with("try_") {
                    // Comment out the `try_*` configs
                    write_back_lines.push(format!("# {}", trimmed));

                    // Prefer the `try_*` configs
                    let actual_key = key.trim_start_matches("try_");
                    if let Some(line_index) = effective_config
                        .iter()
                        .position(|line| line.starts_with(&format!("{}=", actual_key)))
                    {
                        // Config exists, overriding
                        effective_config[line_index] = format!("{}={}", actual_key, value);
                    } else {
                        // Config does not exist, appending
                        effective_config.push(format!("{}={}", actual_key, value));
                        // Make sure there are no spaces around = so that the check existing key logic works
                    }
                } else {
                    // Keep the config as is
                    write_back_lines.push(trimmed.to_string());

                    if effective_config
                        .iter()
                        .any(|line| line.starts_with(&format!("{}=", key)))
                    {
                        // If already overridden by try_ version, skip inserting
                    } else {
                        // Config does not exist, appending
                        effective_config.push(format!("{}={}", key, value)); // Make sure there are no spaces around = so that the check existing key logic works
                    }
                }
            } else {
                // Keep the line as is
                write_back_lines.push(trimmed.to_string());
                effective_config.push(trimmed.to_string());
            }
        }

        // Rewrite config with try_* lines commented out
        let _ = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&full_config_path)
            .and_then(|mut file| {
                for line in &write_back_lines {
                    writeln!(file, "{}", line)?;
                }
                Ok(())
            });
    }

    // Convert effective config back to lines
    effective_config
}

pub fn parse_config(full_config_path: String) -> LocalConfig {
    let lines = process_config_file(full_config_path);
    let content = lines.join("\n");
    if let Ok(config) = toml::from_str::<LocalConfig>(&content) {
        return config;
    }
    // Config malformed, use the default config and the user can modify it again
    let default_config = LocalConfig::default();
    default_config
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn with_config_file(content: &str, f: impl Fn(String)) -> () {
        let dir = tempdir().unwrap();
        let base_dir = dir.path().to_str().unwrap();
        let path = format!("{}/etc/localdesktop", base_dir);
        fs::create_dir_all(&path).unwrap();
        let file_path = format!("{}/localdesktop.toml", path);
        fs::write(&file_path, content).unwrap();
        f(file_path)
    }

    #[test]
    fn should_handle_configs_without_try() {
        with_config_file(
            r#"
                [user]
                username = "alice"

                [command]
                check = "check-cmd"
                install = "install-cmd"
                launch = "launch-cmd"
            "#,
            |full_config_path| {
                let config = parse_config(full_config_path);
                assert_eq!(config.user.username, "alice");
                assert_eq!(config.command.check, "check-cmd");
                assert_eq!(config.command.install, "install-cmd");
                assert_eq!(config.command.launch, "launch-cmd");
            },
        );
    }

    #[test]
    fn should_handle_configs_with_try() {
        with_config_file(
            r#"
                [user]
                username = "root"
                try_username = "testuser"

                [command]
                check = "check-cmd"
                try_check = "try-check"
                install = "install-cmd"
                launch = "launch-cmd"
            "#,
            |full_config_path| {
                let config = parse_config(full_config_path);
                assert_eq!(config.user.username, "testuser");
                assert_eq!(config.command.check, "try-check");
                assert_eq!(config.command.install, "install-cmd")
            },
        );
    }

    #[test]
    fn should_comment_out_try_configs() {
        with_config_file(
            r#"
                username = "root"
                try_username = "commented"

                check = "normal"
                try_check = "try"
            "#,
            |full_config_path| {
                let _ = parse_config(full_config_path.clone()); // This triggers rewriting the config file
                let content = fs::read_to_string(full_config_path).unwrap();

                assert!(
                    content.contains("# try_username = \"commented\""),
                    "❌ `try_username` is not commented out after being applied"
                );
                assert!(
                    content.contains("# try_check = \"try\""),
                    "❌ `try_check` is not commented out after being  applied"
                );
            },
        );
    }
}
