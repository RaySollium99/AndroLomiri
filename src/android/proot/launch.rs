use super::process::ArchProcess;
use crate::android::utils::application_context::get_application_context;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

static LAUNCH_RUNNING: AtomicBool = AtomicBool::new(false);

struct LaunchRunningGuard;

impl Drop for LaunchRunningGuard {
    fn drop(&mut self) {
        LAUNCH_RUNNING.store(false, Ordering::Release);
    }
}

pub fn launch() {
    if LAUNCH_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        log::info!("Skipping launch because the desktop session is already running");
        return;
    }

    thread::spawn(move || {
        let _guard = LaunchRunningGuard;

        // Clean up potential leftover files from previous sessions
        ArchProcess {
            command: "bash -c 'rm -rf /tmp/run /tmp/dbus-* /tmp/.X*-lock /tmp/.X11-unix* /var/run/dbus/* /run/dbus/* /var/run/user/* 2>/dev/null'".into(),
            user: None,
            log: None,
        }
        .run();

        // Kill dead processes that may have been left over from a previous session
        ArchProcess {
            command: "bash -c 'killall -9 -q dbus-daemon accounts-daemon lomiri Xwayland dbus-run-session gdbus pulseaudio 2>/dev/null || true'".into(),
            user: None,
            log: None,
        }
        .run();

        let local_config = get_application_context().local_config;
        let username = local_config.user.username;

        ArchProcess {
            command: local_config.command.launch,
            user: Some(username),
            log: Some(Arc::new(|it| log::trace!("{}", it))),
        }
        .run();
    });
}
