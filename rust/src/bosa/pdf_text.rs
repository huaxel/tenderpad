//! Sandboxed `pdftotext` runner shared by the evidence and classification commands.
//! It performs no OCR and never executes or converts embedded content.
use super::ooxml_text::TextSegment;
use std::error::Error;
#[cfg(target_os = "macos")]
use std::fs;
use std::path::Path;
#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};

#[cfg(target_os = "macos")]
const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;
#[cfg(target_os = "macos")]
const EXTRACTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

/// The parser runs with an empty environment, denied network, denied writes and
/// denied HOME reads except the selected PDF. Non-macOS hosts fail closed until
/// an equivalent OS boundary is implemented.
#[cfg(target_os = "macos")]
pub fn sandboxed_pdf_text(path: &Path) -> Result<String, Box<dyn Error>> {
    let canonical = fs::canonicalize(path)?;
    let text = canonical.to_str().ok_or("non-Unicode path unsupported")?;
    let home = std::env::var("HOME")?;
    if [text, home.as_str()]
        .iter()
        .any(|s| s.chars().any(|c| c == '"' || c == '\\' || c.is_control()))
    {
        return Err("unsafe sandbox path".into());
    }
    let profile=format!("(version 1) (allow default) (deny network*) (deny file-write*) (deny file-read-data (subpath \"/Users\")) (deny file-read-data (subpath \"/Volumes\")) (deny file-read-data (subpath \"/private/var\")) (deny file-read-data (subpath \"{home}\")) (allow file-read-data (literal \"{text}\"))");
    let parser = ["/opt/homebrew/bin/pdftotext", "/usr/local/bin/pdftotext"]
        .into_iter()
        .find(|p| Path::new(p).is_file())
        .ok_or("pdftotext is not installed")?;
    let mut command = Command::new("/usr/bin/sandbox-exec");
    command
        .arg("-p")
        .arg(profile)
        .arg(parser)
        .arg("-layout")
        .arg(&canonical)
        .arg("-")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    supervisor::extract_text(&mut command, MAX_TEXT_BYTES, EXTRACTION_TIMEOUT)
}
#[cfg(not(target_os = "macos"))]
pub fn sandboxed_pdf_text(_path: &Path) -> Result<String, Box<dyn Error>> {
    Err("PDF extraction requires a configured OS sandbox; unsupported on this host".into())
}

// Runtime PDF extraction remains macOS-only. Unix tests exercise the supervisor
// with harmless commands, without enabling a PDF parser on unsupported hosts.
#[cfg(any(target_os = "macos", all(test, unix)))]
mod supervisor {
    use std::error::Error;
    use std::io::{self, Read};
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::time::{Duration, Instant};

    // Observe exit without reaping: the leader's PID must remain pinned until
    // its process group is cleaned up. Never signal a number after wait().
    fn exited_without_reaping(pid: u32) -> io::Result<bool> {
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { info.si_pid() } == pid as i32)
    }

    struct OwnedChild {
        child: Child,
        reaped: bool,
    }

    impl OwnedChild {
        fn stop_and_reap(&mut self) -> io::Result<ExitStatus> {
            if self.reaped {
                return Err(io::Error::other("extractor already reaped"));
            }
            // An external reaper makes ownership uncertain. Refuse to signal
            // that PID/group; a cached numeric child ID is not authority.
            let exited = exited_without_reaping(self.child.id())?;
            let result = unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) };
            let group_error = (result != 0).then(io::Error::last_os_error);
            let direct = self.child.kill();
            if !exited {
                direct?;
            }
            let status = self.child.wait()?;
            self.reaped = true;
            if let Some(error) = group_error {
                // Darwin also returns EPERM when an exited group has no
                // signal-eligible members beyond the pinned zombie leader.
                // This is not proof of adversarial descendant containment.
                let empty_darwin_group = cfg!(target_os = "macos")
                    && exited
                    && error.raw_os_error() == Some(libc::EPERM);
                if error.raw_os_error() != Some(libc::ESRCH) && !empty_darwin_group {
                    return Err(error);
                }
            }
            Ok(status)
        }
    }

    impl Drop for OwnedChild {
        fn drop(&mut self) {
            if !self.reaped {
                // Retry interrupted ownership observation, but never turn
                // uncertain ownership into an unconditional group signal.
                for _ in 0..8 {
                    match self.stop_and_reap() {
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        _ => break,
                    }
                }
            }
        }
    }

    fn timed_out(budget: Duration) -> Box<dyn Error> {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("PDF extraction exceeded {} seconds", budget.as_secs_f64()),
        )
        .into()
    }

    pub(super) fn extract_text(
        command: &mut Command,
        max_bytes: usize,
        budget: Duration,
    ) -> Result<String, Box<dyn Error>> {
        let read_limit = max_bytes.checked_add(1).ok_or("invalid PDF text limit")?;
        let deadline = Instant::now()
            .checked_add(budget)
            .ok_or("invalid PDF time budget")?;
        command.process_group(0).stdout(Stdio::piped());
        let mut owned = OwnedChild {
            child: command.spawn()?,
            reaped: false,
        };
        let mut stdout = owned
            .child
            .stdout
            .take()
            .ok_or("missing extractor output")?;
        let flags = unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                < 0
        {
            return Err(io::Error::last_os_error().into());
        }
        // No blocking reader thread: EOF and leader exit are independent
        // events, and the same deadline covers both in either order.
        let mut bytes = Vec::new();
        let mut buffer = [0; 8192];
        let mut eof = false;
        loop {
            if Instant::now() >= deadline {
                return Err(timed_out(budget));
            }
            while !eof {
                if Instant::now() >= deadline {
                    return Err(timed_out(budget));
                }
                let available = (read_limit - bytes.len()).min(buffer.len());
                match stdout.read(&mut buffer[..available]) {
                    Ok(0) => eof = true,
                    Ok(n) => {
                        bytes.extend_from_slice(&buffer[..n]);
                        if bytes.len() > max_bytes {
                            return Err(
                                format!("PDF extracted text exceeds {max_bytes} bytes").into()
                            );
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error.into()),
                }
            }
            let exited = match exited_without_reaping(owned.child.id()) {
                Ok(exited) => exited,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            };
            if Instant::now() >= deadline {
                return Err(timed_out(budget));
            }
            if eof && exited {
                // First observe complete output, then clean descendants while
                // ownership is pinned. Forced deadline cleanup never yields Ok.
                let status = owned.stop_and_reap()?;
                if !status.success() {
                    return Err("sandboxed PDF extraction failed".into());
                }
                return Ok(String::from_utf8(bytes)?);
            }
            std::thread::sleep(
                Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        fn fixture(script: &str) -> Command {
            let mut command = Command::new("/bin/sh");
            command
                .args(["-c", script])
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            command
        }

        #[test]
        fn exited_leader_does_not_disable_pipe_deadline() {
            let started = Instant::now();
            let error = extract_text(
                &mut fixture("/bin/sleep 2 & printf ready; exit 0"),
                1024,
                Duration::from_millis(200),
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("PDF extraction exceeded"),
                "{error}"
            );
            assert!(started.elapsed() < Duration::from_millis(1500));
        }

        #[test]
        fn running_leader_needs_exit_even_when_pipe_is_closed() {
            for script in ["/bin/sleep 2", "exec 1>&-; /bin/sleep 2"] {
                let started = Instant::now();
                let error = extract_text(&mut fixture(script), 1024, Duration::from_millis(200))
                    .unwrap_err();
                assert!(
                    error.to_string().contains("PDF extraction exceeded"),
                    "{error}"
                );
                assert!(started.elapsed() < Duration::from_millis(1500));
            }
        }

        #[test]
        fn delayed_pipe_completion_keeps_complete_output() {
            let output = extract_text(
                &mut fixture("printf head; (/bin/sleep 0.05; printf tail) & exit 0"),
                1024,
                Duration::from_secs(2),
            )
            .unwrap();
            assert_eq!(output, "headtail");
        }

        #[test]
        fn exit_status_and_unicode_are_still_validated() {
            assert_eq!(
                extract_text(
                    &mut fixture("printf '\\303\\251'"),
                    16,
                    Duration::from_secs(2)
                )
                .unwrap(),
                "é"
            );
            let error = extract_text(
                &mut fixture("printf text; exit 7"),
                16,
                Duration::from_secs(2),
            )
            .unwrap_err();
            assert!(error
                .to_string()
                .contains("sandboxed PDF extraction failed"));
            let error = extract_text(&mut fixture("printf '\\377'"), 16, Duration::from_secs(2))
                .unwrap_err();
            assert!(error.to_string().contains("utf-8"));
        }

        #[test]
        fn output_limit_is_inclusive_and_checked_during_drain() {
            for limit in [0, 1, 32768, 2 * 1024 * 1024] {
                // BSD head rejects -c 0; an empty successful command is the
                // intended boundary fixture, not an extraction failure.
                let exact = if limit == 0 {
                    "exit 0".to_owned()
                } else {
                    format!("exec /usr/bin/head -c {limit} /dev/zero")
                };
                assert_eq!(
                    extract_text(&mut fixture(&exact), limit, Duration::from_secs(5))
                        .unwrap()
                        .len(),
                    limit
                );
                let excessive = format!("exec /usr/bin/head -c {} /dev/zero", limit + 1);
                let error = extract_text(&mut fixture(&excessive), limit, Duration::from_secs(5))
                    .unwrap_err();
                assert!(
                    error.to_string().contains("PDF extracted text exceeds"),
                    "{error}"
                );
            }
        }

        struct Scratch(std::path::PathBuf);
        impl Scratch {
            fn new() -> Self {
                use std::os::unix::fs::DirBuilderExt;
                let path = std::env::temp_dir()
                    .join(format!("pdf-supervisor-test-{}", uuid::Uuid::new_v4()));
                std::fs::DirBuilder::new()
                    .mode(0o700)
                    .create(&path)
                    .unwrap();
                Self(path)
            }
        }
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        fn assert_recorded_descendant_stopped(path: &std::path::Path) {
            let pid = std::fs::read_to_string(path)
                .unwrap()
                .trim()
                .parse::<u32>()
                .expect("complete fixture PID receipt")
                .to_string();
            for _ in 0..100 {
                let output = Command::new("/bin/ps")
                    .args(["-p", pid.trim(), "-o", "stat="])
                    .env_clear()
                    .env("PATH", "/usr/bin:/bin")
                    .output()
                    .unwrap();
                let state = String::from_utf8(output.stdout).unwrap();
                // A zombie is not a running descendant; PID 1 may reap it later.
                if !output.status.success() {
                    assert_eq!(output.status.code(), Some(1));
                    assert!(output.stderr.is_empty(), "process observer failed");
                    assert!(state.trim().is_empty());
                    return;
                }
                if state.trim().starts_with('Z') {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!(
                "recorded fixture descendant remains running: {}",
                pid.trim()
            );
        }

        #[test]
        fn owned_descendants_are_cleaned_on_success_timeout_and_limit_error() {
            let scratch = Scratch::new();
            for mode in ["success", "timeout", "limit"] {
                let receipt = scratch.0.join(mode);
                let tail = match mode {
                    "success" => "printf ok",
                    "timeout" => "/bin/sleep 5",
                    _ => "exec /usr/bin/head -c 65537 /dev/zero",
                };
                let script = format!("/bin/sleep 5 >/dev/null & printf '%s' $! > \"$1\"; {tail}");
                let mut command = fixture(&script);
                command.arg("fixture").arg(&receipt);
                let result = extract_text(&mut command, 16, Duration::from_millis(300));
                if mode == "success" {
                    assert_eq!(result.unwrap(), "ok");
                } else {
                    assert!(result.is_err());
                }
                assert_recorded_descendant_stopped(&receipt);
            }
        }

        #[test]
        fn guard_cleans_up_after_an_early_return() {
            let scratch = Scratch::new();
            let receipt = scratch.0.join("descendant");
            let mut command =
                fixture("/bin/sleep 5 >/dev/null & printf '%s' $! > \"$1\"; /bin/sleep 5");
            command.arg("fixture").arg(&receipt).process_group(0);
            let owned = OwnedChild {
                child: command.spawn().unwrap(),
                reaped: false,
            };
            for _ in 0..100 {
                if std::fs::read_to_string(&receipt)
                    .ok()
                    .and_then(|pid| pid.trim().parse::<u32>().ok())
                    .is_some()
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            // Positive observer control: the recorded descendant is alive
            // before guard drop, not merely absent due to a broken ps command.
            let pid = std::fs::read_to_string(&receipt).unwrap();
            let live = Command::new("/bin/ps")
                .args(["-p", pid.trim(), "-o", "stat="])
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .output()
                .unwrap();
            assert!(live.status.success());
            assert!(live.stderr.is_empty());
            assert!(!String::from_utf8(live.stdout)
                .unwrap()
                .trim()
                .starts_with('Z'));
            drop(owned);
            assert_recorded_descendant_stopped(&receipt);
        }

        #[test]
        fn cleanup_refuses_a_reaped_child_identity() {
            let mut command = fixture("exit 0");
            command.process_group(0);
            let mut owned = OwnedChild {
                child: command.spawn().unwrap(),
                reaped: false,
            };
            owned.child.wait().unwrap(); // Simulate an unexpected other reaper.
            assert_eq!(
                owned.stop_and_reap().unwrap_err().raw_os_error(),
                Some(libc::ECHILD)
            );
            // Drop also checks ownership; no signal follows this reaped ID.
        }

        #[test]
        fn invalid_limit_refuses_before_spawning() {
            let scratch = Scratch::new();
            let marker = scratch.0.join("must-not-exist");
            let mut command = fixture("printf spawned > \"$1\"");
            command.arg("fixture").arg(&marker);
            assert!(extract_text(&mut command, usize::MAX, Duration::from_secs(1)).is_err());
            assert!(!marker.exists());
        }
    }
}

/// Split `pdftotext` output into one segment per page that has text, located as
/// `page N`. Pages are separated by form feeds; numbering counts empty pages, so
/// `page 5` is always the fifth page of the file. The text is kept exactly as
/// extracted (the sandbox runs `pdftotext -layout`, so column padding is
/// preserved and a quote crossing columns will not verify as verbatim).
pub fn page_segments(text: &str) -> Vec<TextSegment> {
    let mut pages: Vec<&str> = text.split('\x0c').collect();
    if pages.last().is_some_and(|last| last.trim().is_empty()) {
        pages.pop();
    }
    pages
        .into_iter()
        .enumerate()
        .filter(|(_, page)| !page.trim().is_empty())
        .map(|(index, page)| TextSegment {
            location: format!("page {}", index + 1),
            text: page.to_owned(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bosa::citation::{verify_quote, CitationCheck};

    #[test]
    fn page_numbers_count_empty_pages() {
        let segments = page_segments("eerste pagina tekst\x0c \n\x0cderde pagina tekst\x0c");
        let got: Vec<_> = segments.iter().map(|s| s.location.as_str()).collect();
        assert_eq!(got, ["page 1", "page 3"]);
    }

    #[test]
    fn a_scan_yields_no_segments_and_empty_text_is_zero_pages() {
        assert!(page_segments("\x0c\x0c\x0c").is_empty());
        assert!(page_segments("").is_empty());
    }

    #[test]
    fn text_is_kept_verbatim_so_quotes_verify_against_the_page() {
        let segments = page_segments("Titel\x0cMinimale omzet  500.000 EUR\x0c");
        // Layout spacing is preserved: the double space is part of the source text.
        assert!(matches!(
            verify_quote(&segments, "Minimale omzet  500.000 EUR"),
            CitationCheck::Verbatim { .. }
        ));
        assert_eq!(
            verify_quote(&segments, "Minimale omzet 500.000 EUR"),
            CitationCheck::NotFound
        );
    }
}
