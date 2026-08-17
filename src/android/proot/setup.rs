use super::process::ArchProcess;
use crate::{
    android::{
        app::build::PolarBearBackend,
        backend::{
            wayland::{Compositor, TouchMode, WaylandBackend},
            webview::{ErrorVariant, WebviewBackend},
        },
        utils::application_context::get_application_context,
        utils::ndk::{density_dpi, long_press_timeout_ms, scale_factor, touch_slop_px},
        utils::ndk::run_in_jvm,
    },
    core::config::{
        CommandConfig, ARCH_FS_ARCHIVE, ARCH_FS_ROOT, DOCS_HOME_URL, PIPEWIRE_GUEST_RUNTIME_DIR,
        PULSE_GUEST_SERVER,
    },
};
use jni::objects::JObject;
use jni::sys::_jobject;
use pathdiff::diff_paths;
use smithay::utils::Clock;
use std::{
    fs::{self, File},
    io::{ErrorKind, Read, Write},
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
    process,
    sync::{
        mpsc::{self, Sender},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{SystemTime, UNIX_EPOCH},
};
use tar::Archive;
use winit::platform::android::activity::AndroidApp;
use xz2::read::XzDecoder;

#[derive(Debug)]
pub enum SetupMessage {
    Progress(String),
    Error(String),
}

pub struct SetupOptions {
    pub android_app: AndroidApp,
    pub mpsc_sender: Sender<SetupMessage>,
}

/// Setup is a process that should be done **only once** when the user installed the app.
/// The setup process consists of several stages.
/// Each stage is a function that takes the `SetupOptions` and returns a `StageOutput`.
type SetupStage = Box<dyn Fn(&SetupOptions) -> StageOutput + Send>;

/// Each stage should indicate whether the associated task is done previously or not.
/// Thus, it should return a finished status if the task is done, so that the setup process can move on to the next stage.
/// Otherwise, it should return a `JoinHandle`, so that the setup process can wait for the task to finish, but not block the main thread so that the setup progress can be reported to the user.
///
/// For coding agents: READ THIS BEFORE ADDING WORK HERE.
/// - Heavy/long work belongs inside the spawned thread of a returned `Some(JoinHandle)`, so it runs once at install and surfaces as setup progress.
/// - Simple/light tasks or important settings that must be run every launch (e.g. the Firefox config) can be done inline on the `None` path.
type StageOutput = Option<JoinHandle<()>>;

const PIPEWIRE_GUEST_LOCK_PACKAGES: &[&str] = &[
    "libpipewire",
    "pipewire",
    "pipewire-alsa",
    "pipewire-audio",
    "pipewire-jack",
    "pipewire-pulse",
    "pipewire-v4l2",
    "pipewire-zeroconf",
    "gst-plugin-pipewire",
    "wireplumber",
];

fn setup_arch_fs(options: &SetupOptions) -> StageOutput {
    let context = get_application_context();
    let temp_file = context.data_dir.join("ubuntu-fs.tar.xz");
    let fs_root = Path::new(ARCH_FS_ROOT);
    let extracted_dir = context.data_dir.join("ubuntu-resolute-aarch64");
    let mpsc_sender = options.mpsc_sender.clone();

    // Only run if the fs_root is missing or empty
    // TODO: Setup integration test to make sure on clean install, the fs_root is either non existent or empty
    let need_setup = fs_root.read_dir().map_or(true, |mut d| d.next().is_none());
    if need_setup {
        return Some(thread::spawn(move || {
            // Download if the archive doesn't exist
            loop {
                if !temp_file.exists() {
                    mpsc_sender
                        .send(SetupMessage::Progress(
                            "Downloading Ubuntu FS...".to_string(),
                        ))
                        .expect("Failed to send log message");

                    let response = reqwest::blocking::get(ARCH_FS_ARCHIVE)
                        .expect("Failed to download Ubuntu FS");

                    let total_size = response.content_length().unwrap_or(0);
                    let mut file = File::create(&temp_file)
                        .expect("Failed to create temp file for Ubuntu FS");

                    let mut downloaded = 0u64;
                    let mut buffer = [0u8; 8192];
                    let mut reader = response;
                    let mut last_percent = 0;

                    loop {
                        let n = reader
                            .read(&mut buffer)
                            .expect("Failed to read from response");
                        if n == 0 {
                            break;
                        }
                        file.write_all(&buffer[..n])
                            .expect("Failed to write to file");
                        downloaded += n as u64;
                        if total_size > 0 {
                            let percent = (downloaded * 100 / total_size).min(100) as u8;
                            if percent != last_percent {
                                let downloaded_mb = downloaded as f64 / 1024.0 / 1024.0;
                                let total_mb = total_size as f64 / 1024.0 / 1024.0;
                                mpsc_sender
                                    .send(SetupMessage::Progress(format!(
                                        "Downloading Ubuntu FS... {}% ({:.2} MB / {:.2} MB)",
                                        percent, downloaded_mb, total_mb
                                    )))
                                    .unwrap_or(());
                                last_percent = percent;
                            }
                        }
                    }
                }

                mpsc_sender
                    .send(SetupMessage::Progress(
                        "Extracting Ubuntu FS...".to_string(),
                    ))
                    .expect("Failed to send log message");

                // Ensure the extracted directory is clean
                let _ = fs::remove_dir_all(&extracted_dir);

                // Extract tar file directly to the final destination
                let tar_file =
                    File::open(&temp_file).expect("Failed to open downloaded Ubuntu FS file");
                let tar = XzDecoder::new(tar_file);
                let mut archive = Archive::new(tar);

                // Try to extract, if it fails, remove temp file and restart download
                if let Err(e) = archive.unpack(context.data_dir.clone()) {
                    // Clean up the failed extraction
                    let _ = fs::remove_dir_all(&extracted_dir);
                    let _ = fs::remove_file(&temp_file);

                    mpsc_sender
                        .send(SetupMessage::Error(format!(
                            "Failed to extract Ubuntu FS: {}. Restarting download...",
                            e
                        )))
                        .unwrap_or(());

                    // Continue the outer loop to retry the download
                    continue;
                }

                // If we get here, extraction was successful
                break;
            }

            // Move the extracted files to the final destination
            fs::rename(&extracted_dir, fs_root)
                .expect("Failed to rename extracted files to final destination");

            // Fix Ubuntu DNS resolution symlink
            // Ubuntu ships /etc/resolv.conf as a symlink to systemd-resolved, which bypasses
            // PRoot's /etc/resolv.conf interception. We replace it with a regular file.
            let resolv_conf = fs_root.join("etc/resolv.conf");
            let _ = fs::remove_file(&resolv_conf);
            let _ = fs::write(&resolv_conf, "nameserver 8.8.8.8\nnameserver 1.1.1.1\n");

            // Clean up the temporary file
            fs::remove_file(&temp_file).expect("Failed to remove temporary file");
        }));
    }
    None
}

fn simulate_linux_sysdata_stage(options: &SetupOptions) -> StageOutput {
    let fs_root = Path::new(ARCH_FS_ROOT);
    let mpsc_sender = options.mpsc_sender.clone();

    if !fs_root.join("proc/.version").exists() {
        return Some(thread::spawn(move || {
            mpsc_sender
                .send(SetupMessage::Progress(
                    "Simulating Linux system data...".to_string(),
                ))
                .expect(&format!("Failed to send log message"));

            // Create necessary directories - don't fail if they already exist
            let _ = fs::create_dir_all(fs_root.join("proc"));
            let _ = fs::create_dir_all(fs_root.join("sys"));
            let _ = fs::create_dir_all(fs_root.join("sys/.empty"));

            // Set permissions - only try to set permissions if we're on Unix and have the capability
            #[cfg(unix)]
            {
                // Try to set permissions, but don't fail if we can't
                let _ =
                    fs::set_permissions(fs_root.join("proc"), fs::Permissions::from_mode(0o700));
                let _ = fs::set_permissions(fs_root.join("sys"), fs::Permissions::from_mode(0o700));
                let _ = fs::set_permissions(
                    fs_root.join("sys/.empty"),
                    fs::Permissions::from_mode(0o700),
                );
            }

            // Create fake proc files
            let proc_files = [
                    ("proc/.loadavg", "0.12 0.07 0.02 2/165 765\n"),
                    ("proc/.stat", "cpu  1957 0 2877 93280 262 342 254 87 0 0\ncpu0 31 0 226 12027 82 10 4 9 0 0\n"),
                    ("proc/.uptime", "124.08 932.80\n"),
                    ("proc/.version", "Linux version 6.2.1 (proot@termux) (gcc (GCC) 12.2.1 20230201, GNU ld (GNU Binutils) 2.40) #1 SMP PREEMPT_DYNAMIC Wed, 01 Mar 2023 00:00:00 +0000\n"),
                    ("proc/.vmstat", "nr_free_pages 1743136\nnr_zone_inactive_anon 179281\nnr_zone_active_anon 7183\n"),
                    ("proc/.sysctl_entry_cap_last_cap", "40\n"),
                    ("proc/.sysctl_inotify_max_user_watches", "4096\n"),
                ];

            for (path, content) in proc_files {
                let _ = fs::write(fs_root.join(path), content)
                    .expect(&format!("Permission denied while writing to {}", path));
            }
        }));
    }
    None
}

fn setup_machine_id(_: &SetupOptions) -> StageOutput {
    let fs_root = Path::new(ARCH_FS_ROOT);
    let machine_id = fs_root.join("etc/machine-id");

    let existing = fs::read_to_string(&machine_id).unwrap_or_default();
    if !is_valid_machine_id(&existing) {
        if let Some(parent) = machine_id.parent() {
            fs::create_dir_all(parent).expect("Failed to create /etc for machine-id");
        }

        let _ = fs::set_permissions(&machine_id, fs::Permissions::from_mode(0o644));
        fs::write(&machine_id, format!("{}\n", generate_machine_id()))
            .expect("Failed to write machine-id");
        let _ = fs::set_permissions(&machine_id, fs::Permissions::from_mode(0o444));
        log::info!("Seeded guest /etc/machine-id");
    }

    let dbus_dir = fs_root.join("var/lib/dbus");
    fs::create_dir_all(&dbus_dir).expect("Failed to create /var/lib/dbus");
    let dbus_machine_id = dbus_dir.join("machine-id");
    match fs::symlink_metadata(&dbus_machine_id) {
        Ok(_) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {
            symlink("/etc/machine-id", &dbus_machine_id)
                .expect("Failed to symlink /var/lib/dbus/machine-id");
        }
        Err(err) => panic!("Failed to inspect /var/lib/dbus/machine-id: {}", err),
    }

    None
}

fn is_valid_machine_id(value: &str) -> bool {
    let value = value.trim();
    value.len() == 32
        && value.chars().all(|c| c.is_ascii_hexdigit())
        && value.chars().any(|c| c != '0')
}

fn generate_machine_id() -> String {
    if let Ok(uuid) = fs::read_to_string("/proc/sys/kernel/random/uuid") {
        let id = uuid.trim().replace('-', "").to_ascii_lowercase();
        if is_valid_machine_id(&id) {
            return id;
        }
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{:016x}{:016x}", nanos as u64, process::id() as u64)
}

fn install_dependencies(options: &SetupOptions) -> StageOutput {
    let SetupOptions {
        mpsc_sender,
        android_app: _,
    } = options;

    let context = get_application_context();
    let CommandConfig {
        check,
        install,
        launch: _,
    } = context.local_config.command;

    let installed = move || {
        ArchProcess {
            command: check.clone(),
            user: None,
            log: None,
        }
        .run()
        .status
        .success()
    };

    if installed() {
        return None;
    }

    clear_pipewire_package_lock_for_install();

    let mpsc_sender = mpsc_sender.clone();
    return Some(thread::spawn(move || {
        const MAX_INSTALL_ATTEMPTS: usize = 10;

        // Install dependencies until `check` succeeds.
        for attempt in 1..=MAX_INSTALL_ATTEMPTS {
            let output = ArchProcess {
                command: "rm -f /var/lib/pacman/db.lck".into(),
                user: None,
                log: None,
            }
            .run();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let sender = mpsc_sender.clone();
            ArchProcess {
                command: install.clone(),
                user: None,
                log: Some(Arc::new(move |it| {
                    sender
                        .send(SetupMessage::Progress(it))
                        .expect("Failed to send log message");
                })),
            }
            .run();

            mpsc_sender
                .send(SetupMessage::Progress(format!(
                    "Retrying installation... (attempt {}/{})",
                    attempt, MAX_INSTALL_ATTEMPTS
                )))
                .expect("Failed to send dependency install progress");

            if attempt == MAX_INSTALL_ATTEMPTS {
                let error_message = format!(
                    "Failed to install desktop dependencies after {} attempts. Please check your net connection and try restarting the app.",
                    MAX_INSTALL_ATTEMPTS
                );
                mpsc_sender
                    .send(SetupMessage::Error(error_message.clone()))
                    .unwrap_or(());
                panic!("{}", error_message);
            }
        }
    }));
}

fn clear_pipewire_package_lock_for_install() {
    let pacman_conf = Path::new(ARCH_FS_ROOT).join("etc/pacman.conf");
    let content = match fs::read_to_string(&pacman_conf) {
        Ok(content) => content,
        Err(error) => {
            log::warn!(
                "Skipping PipeWire pacman unlock before install; failed to read {}: {error}",
                pacman_conf.display()
            );
            return;
        }
    };

}

fn setup_pipewire_package_lock(_: &SetupOptions) -> StageOutput {
    let pacman_conf = Path::new(ARCH_FS_ROOT).join("etc/pacman.conf");
    let content = match fs::read_to_string(&pacman_conf) {
        Ok(content) => content,
        Err(error) => {
            log::warn!(
                "Skipping PipeWire pacman lock; failed to read {}: {error}",
                pacman_conf.display()
            );
            return None;
        }
    };
    
    None
}

fn setup_firefox_config(_: &SetupOptions) -> StageOutput {
    // Create the Firefox root directory if it doesn't exist
    let firefox_root = format!("{}/usr/lib/firefox", ARCH_FS_ROOT);
    let _ = fs::create_dir_all(&firefox_root).expect("Failed to create Firefox root directory");

    // Create the defaults/pref directory
    let pref_dir = format!("{}/defaults/pref", firefox_root);
    let _ = fs::create_dir_all(&pref_dir).expect("Failed to create Firefox pref directory");

    // Create autoconfig.js in defaults/pref
    let autoconfig_js = r#"pref("general.config.filename", "localdesktop.cfg");
pref("general.config.obscure_value", 0);
pref("general.config.sandbox_enabled", false);
"#;

    let _ = fs::write(format!("{}/autoconfig.js", pref_dir), autoconfig_js)
        .expect("Failed to write Firefox autoconfig.js");

    // Create localdesktop.cfg in the Firefox root directory
    let firefox_cfg = r#"// Auto updated by Local Desktop on each startup, do not edit manually
defaultPref("media.cubeb.sandbox", false);
defaultPref("security.sandbox.content.level", 0);
defaultPref("media.allow-audio-non-utility", true);
defaultPref("media.rdd-process.enabled", false);

try {
  var { SandboxUtils } = ChromeUtils.importESModule("resource://gre/modules/SandboxUtils.sys.mjs");
  SandboxUtils.maybeWarnAboutDisabledContentSandbox = () => {};
  SandboxUtils.observeContentSandboxPref = () => {};
} catch (_) {}
"#; // It is required that the first line of this file is a comment, even if you have nothing to comment. Docs: https://support.mozilla.org/en-US/kb/customizing-firefox-using-autoconfig

    let _ = fs::write(format!("{}/localdesktop.cfg", firefox_root), firefox_cfg)
        .expect("Failed to write Firefox configuration");

    None
}

#[derive(Debug)]
enum KvLine {
    Entry {
        key: String,
        value: String,
        prefix: String,
        delimiter: char,
    },
    Other(String),
}

fn parse_kv_lines(content: &str, delimiter: char) -> Vec<KvLine> {
    content
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('!') {
                return KvLine::Other(line.to_string());
            }
            if let Some((left, right)) = line.split_once(delimiter) {
                let key = left.trim().to_string();
                if key.is_empty() {
                    return KvLine::Other(line.to_string());
                }
                let prefix_len = line.len() - trimmed.len();
                let prefix = line[..prefix_len].to_string();
                let value = right.trim().to_string();
                KvLine::Entry {
                    key,
                    value,
                    prefix,
                    delimiter,
                }
            } else {
                KvLine::Other(line.to_string())
            }
        })
        .collect()
}

fn set_kv_value(lines: &mut Vec<KvLine>, key: &str, value: &str, delimiter: char) {
    let mut updated = false;
    for line in lines.iter_mut() {
        if let KvLine::Entry {
            key: entry_key,
            value: entry_value,
            ..
        } = line
        {
            if entry_key == key {
                *entry_value = value.to_string();
                updated = true;
            }
        }
    }
    if !updated {
        lines.push(KvLine::Entry {
            key: key.to_string(),
            value: value.to_string(),
            prefix: String::new(),
            delimiter,
        });
    }
}

fn render_kv_lines(lines: &[KvLine]) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in lines {
        match line {
            KvLine::Entry {
                key,
                value,
                prefix,
                delimiter,
            } => out.push(format!("{}{}{} {}", prefix, key, delimiter, value)),
            KvLine::Other(raw) => out.push(raw.to_string()),
        }
    }
    let mut content = out.join("\n");
    content.push('\n');
    content
}

fn upsert_kv_file(path: &Path, delimiter: char, updates: &[(&str, String)]) {
    let content = fs::read_to_string(path).unwrap_or_default();
    let mut lines = parse_kv_lines(&content, delimiter);
    for (key, value) in updates {
        set_kv_value(&mut lines, key, value, delimiter);
    }
    let content = render_kv_lines(&lines);
    fs::write(path, content).expect("Failed to write key/value file");
}

fn setup_fake_bwrap(_: &SetupOptions) -> StageOutput {
    let fs_root = Path::new(ARCH_FS_ROOT);
    let wrapper_path = fs_root.join("usr/local/bin/bwrap");

    // bwrap (Bubblewrap) requires Linux user namespaces (CLONE_NEWUSER) which are
    // blocked by Android SELinux. We replace it with a shim that strips all
    // namespace/sandbox flags and directly exec's the target binary.
    // This unblocks glycin-svg (used by Onboard) which sandbox-loads SVG files via bwrap.
    let wrapper = r#"#!/bin/sh
# bwrap shim for proot/Android: namespaces are unavailable, exec directly.
# Strips all bwrap sandbox/namespace/bind flags, then exec's the target binary.
while [ $# -gt 0 ]; do
    case "$1" in
        # Three-argument flags (flag + src/key + dest/value)
        --ro-bind|--bind|--dev-bind|--bind-try|--ro-bind-try|--dev-bind-try|\
        --file|--bind-data|--ro-bind-data|--symlink|\
        --setenv|--chmod) shift 3 ;;
        # Two-argument flags (flag + single arg)
        --tmpfs|--proc|--dir|\
        --unsetenv|--perms|--cap-add|--cap-drop|\
        --seccomp|--add-seccomp-fd|--info-fd|--json-status-fd|\
        --block-fd|--userns-block-fd|--userns|--userns2|\
        --pidns|--chdir|--dev|--mqueue) shift 2 ;;
        # Zero-argument flags
        --unshare-all|--unshare-user|--unshare-user-try|--unshare-pid|\
        --unshare-ipc|--unshare-net|--unshare-uts|--unshare-cgroup|\
        --unshare-cgroup-try|--share-net|--remount-ro|\
        --as-pid-1|--die-with-parent|--new-session|--clearenv) shift ;;
        --) shift; break ;;
        *) break ;;
    esac
done
exec "$@"
"#;

    let _ = fs::create_dir_all(
        wrapper_path
            .parent()
            .expect("Failed to read bwrap wrapper parent directory"),
    );
    fs::write(&wrapper_path, wrapper).expect("Failed to write bwrap wrapper");
    fs::set_permissions(&wrapper_path, fs::Permissions::from_mode(0o755))
        .expect("Failed to mark bwrap wrapper executable");

    None
}

fn setup_chromium_no_sandbox(_: &SetupOptions) -> StageOutput {
    let fs_root = Path::new(ARCH_FS_ROOT);

    // Chromium's sandbox needs CLONE_NEWUSER, which Android SELinux blocks, so every
    // Chromium/Electron app has to be started with --no-sandbox. Electron apps pick that up
    // from ELECTRON_DISABLE_SANDBOX (exported by startxfce4-localdesktop), but Chromium itself
    // only takes the flag, and its desktop entry hardcodes an absolute path that a
    // /usr/local/bin wrapper cannot intercept. So shadow the affected application entries in
    // the user's own XDG directory, re-running every session to catch newly installed apps.
    write_executable(
        &fs_root.join("usr/local/bin/localdesktop-no-sandbox-entries"),
        r#"#!/bin/sh
target_dir="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
mkdir -p "$target_dir" || exit 0

for src in /usr/share/applications/*.desktop /usr/local/share/applications/*.desktop; do
    [ -f "$src" ] || continue

    prog=$(sed -n 's/^Exec=//p' "$src" | head -n1 | awk '{print $1}')
    [ -n "$prog" ] || continue
    case "$prog" in
        /*) bin="$prog" ;;
        *) bin=$(command -v "$prog" 2>/dev/null) || continue ;;
    esac
    bin=$(readlink -f "$bin" 2>/dev/null)
    [ -n "$bin" ] || continue

    # Every Chromium/Electron build ships the setuid sandbox helper next to its binary,
    # or one level up when the launcher lives in a bin/ subdirectory.
    dir=$(dirname "$bin")
    [ -e "$dir/chrome-sandbox" ] || [ -e "$dir/../chrome-sandbox" ] || continue

    dst="$target_dir/$(basename "$src")"
    # Leave alone anything the user wrote themselves.
    if [ -e "$dst" ] && ! grep -q '^X-LocalDesktop-NoSandbox=' "$dst"; then
        continue
    fi

    awk '
        /^\[Desktop Entry\]/ && !seen { print; print "X-LocalDesktop-NoSandbox=true"; seen = 1; next }
        /^Exec=/ && !/--no-sandbox/ { sub(/^Exec=[^ ]+/, "& --no-sandbox") }
        { print }
    ' "$src" > "$dst"
done
"#,
    );

    // Same flag for terminal launches, following the /usr/local/bin PATH-priority pattern.
    write_executable(
        &fs_root.join("usr/local/bin/chromium"),
        r#"#!/bin/sh
[ -x /usr/bin/chromium ] || { echo "chromium is not installed" >&2; exit 127; }
exec /usr/bin/chromium --no-sandbox "$@"
"#,
    );

    None
}

fn read_android_density_dpi(android_app: AndroidApp) -> i32 {
    let mut density_dpi: i32 = 160;
    run_in_jvm(
        |env, app| {
            let activity = unsafe { JObject::from_raw(app.activity_as_ptr() as *mut _jobject) };
            let resources = env
                .call_method(
                    activity,
                    "getResources",
                    "()Landroid/content/res/Resources;",
                    &[],
                )
                .expect("Failed to call getResources")
                .l()
                .expect("Failed to read getResources result");
            let metrics = env
                .call_method(
                    resources,
                    "getDisplayMetrics",
                    "()Landroid/util/DisplayMetrics;",
                    &[],
                )
                .expect("Failed to call getDisplayMetrics")
                .l()
                .expect("Failed to read getDisplayMetrics result");
            density_dpi = env
                .get_field(&metrics, "densityDpi", "I")
                .expect("Failed to read densityDpi")
                .i()
                .expect("Failed to convert densityDpi");
        },
        android_app,
    );
    density_dpi
}

/// Map Android density to a whole-number UI scale factor (same baseline as the old LXQt setup).
fn android_ui_scale(density_dpi: i32) -> i32 {
    ((density_dpi as f32) / 160.0 * 1.1).max(1.0).round() as i32
}

fn chroot_home_dir(fs_root: &Path, username: &str) -> PathBuf {
    if username == "root" {
        fs_root.join("root")
    } else {
        fs_root.join(format!("home/{username}"))
    }
}

fn write_executable(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(path, contents).expect("Failed to write executable script");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .expect("Failed to mark executable script");
}

// Sets up APT Policy to prevent services from starting during install.
fn setup_apt_policy(_: &SetupOptions) -> StageOutput {
    let fs_root = Path::new(ARCH_FS_ROOT);
    let sbin = fs_root.join("usr/sbin");
    let _ = fs::create_dir_all(&sbin);
    write_executable(&sbin.join("policy-rc.d"), "#!/bin/sh\nexit 101\n");
    None
}

// Sets up Lomiri Session script
fn setup_lomiri_session(options: &SetupOptions) -> StageOutput {
    let fs_root = Path::new(ARCH_FS_ROOT);
    let bin_dir = fs_root.join("usr/local/bin");
    let _ = fs::create_dir_all(&bin_dir);

    // Read the Android density and compute the grid unit
    let density_dpi = read_android_density_dpi(options.android_app.clone());
    let ui_scale = android_ui_scale(density_dpi);
    let grid_unit = ui_scale * 16;

    write_executable(&bin_dir.join("androlomiri-session"), &format!(r#"#!/bin/bash
mkdir -p /tmp/run /var/run/dbus /tmp/.X11-unix
chmod 700 /tmp/run
chmod 1777 /tmp/.X11-unix
ln -sf /tmp/wayland-0 /tmp/run/wayland-0

export XDG_RUNTIME_DIR=/tmp/run
export MIR_SERVER_WAYLAND_HOST=wayland-0
export XDG_SESSION_TYPE=wayland
export GALLIUM_DRIVER=softpipe
export QT_WAYLAND_DISABLE_WINDOWDECORATION=1

export LOMIRI_TESTING=1
export DEVICEINFO_DEVICE_NAME=androlomiri
export GRID_UNIT_PX={grid_unit}

export LD_PRELOAD=/usr/local/lib/libpoll_fix.so

dbus-daemon --system --nofork --nopidfile > /tmp/dbus-system.log 2>&1 &
sleep 1

/usr/lib/accountsservice/accounts-daemon > /tmp/accounts.log 2>&1 &

dbus-run-session /usr/bin/lomiri-session > /tmp/launch.log 2>&1
echo "LOMIRI_EXIT_CODE=$?" >> /tmp/launch.log
'
"#));

    None
}

// Sets up poll() fix for Android
// Required because Android's libc does not handle EINTR properly
// poll() is badly implemented on Mir, making it not work on Android by default.
fn setup_poll_fix(options: &SetupOptions) -> StageOutput {
    let fs_root = Path::new(ARCH_FS_ROOT);
    let poll_fix_so = fs_root.join("usr/local/lib/libpoll_fix.so");
    if poll_fix_so.exists() {
        return None;
    }

    let mpsc_sender = options.mpsc_sender.clone();
    let poll_fix_src = fs_root.join("usr/local/src/poll_fix.c");
    let _ = fs::create_dir_all(poll_fix_src.parent().unwrap());
    fs::write(&poll_fix_src, r#"#define _GNU_SOURCE
#include <poll.h>
#include <errno.h>
#include <dlfcn.h>
typedef int (*poll_t)(struct pollfd *, nfds_t, int);
int poll(struct pollfd *fds, nfds_t nfds, int timeout) {
    static poll_t real_poll = 0;
    if (!real_poll) real_poll = (poll_t)dlsym(RTLD_NEXT, "poll");
    int ret;
    do { ret = real_poll(fds, nfds, timeout); } while (ret == -1 && errno == EINTR);
    return ret;
}
"#).expect("Failed to write poll_fix.c");

    Some(thread::spawn(move || {
        ArchProcess {
            command: "gcc -shared -fPIC /usr/local/src/poll_fix.c -o /usr/local/lib/libpoll_fix.so -ldl".into(),
            user: None,
            log: None,
        }.run();
    }))
}

// Setup Lomiri DeviceInfo properly
fn setup_lomiri_deviceinfo(options: &SetupOptions) -> StageOutput {
    let fs_root = Path::new(ARCH_FS_ROOT);
    let density_dpi = read_android_density_dpi(options.android_app.clone());
    let ui_scale = android_ui_scale(density_dpi);
    let grid_unit = ui_scale * 16;

    let deviceinfo_yaml = format!(r#"androlomiri:
  Names:
    - androlomiri
  PrettyName: AndroLomiri
  DeviceType: phone
  GridUnit: {grid_unit}
  SupportedOrientations:
    - Portrait
    - Landscape
    - InvertedLandscape
    - InvertedPortrait
"#);

    let deviceinfo_dir = fs_root.join("etc/deviceinfo");
    let _ = fs::create_dir_all(&deviceinfo_dir);
    fs::write(deviceinfo_dir.join("androlomiri.yaml"), &deviceinfo_yaml)
        .expect("Failed to write androlomiri.yaml");
    
    let devices_dir = fs_root.join("etc/deviceinfo/devices");
    let _ = fs::create_dir_all(&devices_dir);
    fs::write(devices_dir.join("androlomiri.yaml"), deviceinfo_yaml)
        .expect("Failed to write devices/androlomiri.yaml");

    None
}

fn fix_xkb_symlink(options: &SetupOptions) -> StageOutput {
    let fs_root = Path::new(ARCH_FS_ROOT);
    let xkb_path = fs_root.join("usr/share/X11/xkb");
    let mpsc_sender = options.mpsc_sender.clone();

    if let Ok(meta) = fs::symlink_metadata(&xkb_path) {
        if meta.file_type().is_symlink() {
            if let Ok(target) = fs::read_link(&xkb_path) {
                if target.is_absolute() {
                    log::info!(
                        "Absolute symlink target detected: {} -> {}. This is a problem because libxkbcommon is loaded in NDK, whose / is not Ubuntu FS root!",
                        xkb_path.display(),
                        target.display()
                    );
                    // Compute the relative path from /usr/share/X11/xkb to /usr/share/xkeyboard-config-2
                    // Both are inside the chroot, so strip the fs_root prefix
                    let xkb_inside = Path::new("/usr/share/X11/xkb");
                    let target_inside = Path::new("/usr/share/xkeyboard-config-2");
                    let rel_target = diff_paths(target_inside, xkb_inside.parent().unwrap())
                        .unwrap_or_else(|| target_inside.to_path_buf());
                    log::info!(
                        "Fixing with new relative symlink: {} -> {}",
                        xkb_path.display(),
                        rel_target.display()
                    );
                    // Remove the old symlink
                    let _ = fs::remove_file(&xkb_path);
                    // Create the new relative symlink
                    if let Err(e) = symlink(&rel_target, &xkb_path) {
                        mpsc_sender
                            .send(SetupMessage::Error(format!(
                                "Failed to create relative symlink for xkb: {}",
                                e
                            )))
                            .unwrap_or(());
                    }
                }
            }
        }
    }
    None
}

pub fn setup(android_app: AndroidApp) -> PolarBearBackend {
    let (sender, receiver) = mpsc::channel();
    let progress = Arc::new(Mutex::new(0));

    if ArchProcess::is_supported(&android_app) {
        sender
            .send(SetupMessage::Progress(
                "✅ Your device is supported!".to_string(),
            ))
            .unwrap_or(());
    } else {
        log::info!("PRoot support check failed, showing Device Unsupported page");
        return PolarBearBackend::WebView(WebviewBackend {
            socket_port: 0,
            progress,
            error: ErrorVariant::Unsupported,
        });
    }

    let options = SetupOptions {
        android_app: android_app.clone(),
        mpsc_sender: sender.clone(),
    };

    let stages: Vec<SetupStage> = vec![
        Box::new(setup_arch_fs),                // Step 1. Setup Ubuntu FS (extract)
        Box::new(simulate_linux_sysdata_stage), // Step 2. Simulate Linux system data
        Box::new(setup_apt_policy), // Step 3. Setup APT policy to prevent services from starting during install
        Box::new(install_dependencies),         // Step 4. Install dependencies
        Box::new(setup_machine_id),             // Step 5. Seed /etc/machine-id for D-Bus clients
        Box::new(setup_pipewire_package_lock), // Step 6. Hold guest PipeWire packages for the Android-side PipeWire POC
        Box::new(setup_firefox_config),        // Step 7. Setup Firefox config
        Box::new(setup_fake_bwrap), // Step 8. Replace bwrap with a no-sandbox shim (Android has no user namespaces)
        Box::new(setup_poll_fix), // Step 9. Setup poll() fix for Android
        Box::new(setup_chromium_no_sandbox), // Step 10. Make Chromium/Electron apps launchable without a terminal
        Box::new(setup_lomiri_session), // Step 11. Setup Lomiri session script
        Box::new(setup_lomiri_deviceinfo), // Step 12. Setup Lomiri DeviceInfo properly
        Box::new(fix_xkb_symlink),          // Step 13. Fix xkb symlink
    ];

    let handle_stage_error = |e: Box<dyn std::any::Any + Send>, sender: &Sender<SetupMessage>| {
        let error_msg = if let Some(e) = e.downcast_ref::<String>() {
            format!("Stage execution failed: {}", e)
        } else if let Some(e) = e.downcast_ref::<&str>() {
            format!("Stage execution failed: {}", e)
        } else {
            "Stage execution failed: Unknown error".to_string()
        };
        sender
            .send(SetupMessage::Error(error_msg.clone()))
            .unwrap_or(());
    };

    let fully_installed = 'outer: loop {
        for (i, stage) in stages.iter().enumerate() {
            if let Some(handle) = stage(&options) {
                let progress_clone = progress.clone();
                let sender_clone = sender.clone();
                thread::spawn(move || {
                    let progress = progress_clone;
                    let progress_value = ((i) as u16 * 100 / stages.len() as u16) as u16;
                    *progress.lock().unwrap() = progress_value;

                    // Wait for the current stage to finish
                    if let Err(e) = handle.join() {
                        handle_stage_error(e, &sender_clone);
                        return;
                    }

                    // Process the remaining stages in the same loop
                    for (j, next_stage) in stages.iter().enumerate().skip(i + 1) {
                        let progress_value = ((j) as u16 * 100 / stages.len() as u16) as u16;
                        *progress.lock().unwrap() = progress_value;
                        if let Some(next_handle) = next_stage(&options) {
                            if let Err(e) = next_handle.join() {
                                handle_stage_error(e, &sender_clone);
                                return;
                            }

                            // Increment progress and send it
                            let next_progress_value =
                                ((j + 1) as u16 * 100 / stages.len() as u16) as u16;
                            *progress.lock().unwrap() = next_progress_value;
                        }
                    }

                    // All stages are done, we need to replace the WebviewBackend with the WaylandBackend
                    // Or, easier, just restart the whole app
                    *progress.lock().unwrap() = 100;
                    sender_clone
                        .send(SetupMessage::Progress(
                            "Installation finished, please restart the app".to_string(),
                        ))
                        .expect("Failed to send installation finished message");
                });

                // Setup is still running in the background, but we need to return control
                // so that the main thread can continue to report progress to the user
                break 'outer false;
            }
        }

        // All stages were done previously, no need to wait for anything
        break 'outer true;
    };

    if fully_installed {
        PolarBearBackend::Wayland(WaylandBackend {
            compositor: Compositor::build().expect("Failed to build compositor"),
            graphic_renderer: None,
            clock: Clock::new(),
            key_counter: 0,
            guest_scale_factor: scale_factor(&android_app),
            touch_points: std::collections::HashMap::new(),
            scroll_centroid: None,
            touch_mode: TouchMode::Undecided,
            touch_down_position: None,
            touch_down_time: None,
            touch_slop_px: touch_slop_px(&android_app),
            long_press_timeout_ms: long_press_timeout_ms(&android_app),
            pointer_pressed: false,
            android_app,
        })
    } else {
        PolarBearBackend::WebView(WebviewBackend::build(receiver, progress))
    }
}
