//! The clipboard, for the board's `y`: the platform's own command when there
//! is one, else the terminal's clipboard sequence (OSC 52).
//!
//! A command runs on a thread of its own and is waited on for at most
//! [`LIMIT`], then killed: the board's loop only reads its result from a
//! channel when it's there, so a command that hangs never freezes it.

use std::env;
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

/// How long a copy command may take before it's killed and said to have
/// failed: room for `powershell.exe`, which can take a second or two to
/// start under WSL.
pub const LIMIT: Duration = Duration::from_secs(5);

/// `powershell.exe`'s arguments under WSL: its input read as UTF-8, set
/// explicitly. Left to itself it reads the console's code page (CP949 on
/// Korean Windows), as `clip.exe` does, and Hangul comes out as garbage.
pub const POWERSHELL: &[&str] = &[
    "-NoProfile",
    "-NonInteractive",
    "-Command",
    "[Console]::InputEncoding=[Text.Encoding]::UTF8; Set-Clipboard -Value ([Console]::In.ReadToEnd())",
];

/// Where `y` copies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Clipboard {
    /// A command that takes the text on its standard input.
    Command { program: String, args: Vec<String> },
    /// The terminal, by its clipboard sequence: whether it lands is the
    /// terminal's to say, and it says nothing.
    Terminal,
}

/// The clipboard here: the platform's command, when it's on `PATH`, else the
/// terminal's. Nothing is run to find out.
pub fn detect() -> Clipboard {
    let wsl = env::var_os("WSL_DISTRO_NAME").is_some()
        || fs::read_to_string("/proc/sys/kernel/osrelease")
            .is_ok_and(|r| r.to_lowercase().contains("microsoft"));
    let on_path = |program: &str| {
        env::var_os("PATH")
            .is_some_and(|path| env::split_paths(&path).any(|dir| dir.join(program).is_file()))
    };
    choose(
        Platform {
            mac: cfg!(target_os = "macos"),
            wsl,
            wayland: env::var_os("WAYLAND_DISPLAY").is_some(),
            x11: env::var_os("DISPLAY").is_some(),
        },
        on_path,
    )
}

/// What [`choose`] goes by.
#[derive(Debug, Clone, Copy, Default)]
pub struct Platform {
    pub mac: bool,
    pub wsl: bool,
    pub wayland: bool,
    pub x11: bool,
}

/// The clipboard for `platform`, `has` saying whether a program is there:
/// `pbcopy` on macOS; under WSL `win32yank.exe`, else `wl-copy` under WSLg,
/// else `powershell.exe` (never `clip.exe`, which reads its input in the
/// console's code page and garbles Hangul); `wl-copy` under Wayland, `xclip`
/// under X; else the terminal's.
pub fn choose(platform: Platform, has: impl Fn(&str) -> bool) -> Clipboard {
    let command = |program: &str, args: &[&str]| Clipboard::Command {
        program: program.into(),
        args: args.iter().map(|a| a.to_string()).collect(),
    };
    let wayland = platform.wayland && has("wl-copy");
    if platform.mac && has("pbcopy") {
        command("pbcopy", &[])
    } else if platform.wsl {
        if has("win32yank.exe") {
            command("win32yank.exe", &["-i", "--crlf"])
        } else if wayland {
            command("wl-copy", &[])
        } else if has("powershell.exe") {
            command("powershell.exe", POWERSHELL)
        } else {
            Clipboard::Terminal
        }
    } else if wayland {
        command("wl-copy", &[])
    } else if platform.x11 && has("xclip") {
        command("xclip", &["-selection", "clipboard"])
    } else {
        Clipboard::Terminal
    }
}

/// The terminal's clipboard sequence for `text`: OSC 52, to the clipboard.
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            out.push(match i <= chunk.len() {
                true => ABC[(n >> (18 - 6 * i) & 63) as usize] as char,
                false => '=',
            });
        }
    }
    out
}

/// Runs `program` with `args` and `text` on its standard input, waiting for
/// it at most `limit`: past that it's killed. Its output goes nowhere.
pub fn run(program: &str, args: &[String], text: &str, limit: Duration) -> Result<(), String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("can't run {program}: {e}"))?;
    // Written apart, so a command that never reads can't hold the wait up.
    if let Some(mut stdin) = child.stdin.take() {
        let text = text.to_string();
        thread::spawn(move || {
            let _ = stdin.write_all(text.as_bytes());
        });
    }
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("{program} failed ({status})")),
            Ok(None) if start.elapsed() >= limit => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{program} didn't finish in {} s",
                    limit.as_secs_f32()
                ));
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(format!("{program}: {e}")),
        }
    }
}

/// [`run`] on a thread of its own, its result sent on `done` with `what` was
/// copied; it returns at once.
pub fn spawn(
    program: String,
    args: Vec<String>,
    text: String,
    what: String,
    limit: Duration,
    done: Sender<(String, Result<(), String>)>,
) {
    thread::spawn(move || {
        let result = run(&program, &args, &text, limit);
        let _ = done.send((what, result));
    });
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::mpsc;

    use super::*;

    /// A fresh, empty folder for one test, under the system's temp folder.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("halil-test-{}-clip-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A stand-in copy command: `sh -c <script>`, the scratch folder its $1.
    fn sh(script: &str, dir: &std::path::Path) -> (String, Vec<String>) {
        (
            "sh".into(),
            vec![
                "-c".into(),
                script.into(),
                "sh".into(),
                dir.display().to_string(),
            ],
        )
    }

    #[test]
    fn the_platforms_command_when_its_there_else_the_terminal() {
        let all = |_: &str| true;
        let none = |_: &str| false;
        let mac = Platform {
            mac: true,
            ..Default::default()
        };
        let wsl = Platform {
            wsl: true,
            x11: true,
            ..Default::default()
        };
        let wayland = Platform {
            wayland: true,
            x11: true,
            ..Default::default()
        };
        let x11 = Platform {
            x11: true,
            ..Default::default()
        };
        let program = |c: Clipboard| match c {
            Clipboard::Command { program, args } => format!("{program} {}", args.join(" ")),
            Clipboard::Terminal => "OSC 52".into(),
        };
        assert_eq!(program(choose(mac, all)).trim(), "pbcopy");
        assert_eq!(program(choose(mac, |p| p == "pbcopy")).trim(), "pbcopy");
        assert_eq!(program(choose(wayland, all)).trim(), "wl-copy");
        assert_eq!(program(choose(x11, all)), "xclip -selection clipboard");
        assert_eq!(
            program(choose(wayland, |p| p == "xclip")),
            "xclip -selection clipboard"
        );
        for platform in [mac, wsl, wayland, x11, Platform::default()] {
            assert_eq!(choose(platform, none), Clipboard::Terminal, "{platform:?}");
        }
        assert_eq!(
            choose(Platform::default(), all),
            Clipboard::Terminal,
            "over ssh"
        );
    }

    #[test]
    fn under_wsl_never_clip_exe_but_win32yank_then_wslgs_wl_copy_then_powershell_in_utf_8() {
        let program = |c: &Clipboard| match c {
            Clipboard::Command { program, .. } => program.clone(),
            Clipboard::Terminal => "OSC 52".into(),
        };
        // WSL, with WSLg's displays: every program there.
        let wslg = Platform {
            wsl: true,
            wayland: true,
            x11: true,
            ..Default::default()
        };
        let plain = Platform {
            wayland: false,
            ..wslg
        };
        let all = |_: &str| true;
        let only = |there: &'static [&'static str]| move |p: &str| there.contains(&p);
        for platform in [wslg, plain] {
            let chosen = choose(platform, all);
            assert_ne!(program(&chosen), "clip.exe", "{platform:?}");
            assert_eq!(
                chosen,
                Clipboard::Command {
                    program: "win32yank.exe".into(),
                    args: vec!["-i".into(), "--crlf".into()]
                },
                "{platform:?}"
            );
            // Nor with clip.exe the only one there.
            assert_eq!(
                choose(platform, only(&["clip.exe"])),
                Clipboard::Terminal,
                "{platform:?}"
            );
        }
        // No win32yank: under WSLg wl-copy, else PowerShell.
        let no_yank = only(&["clip.exe", "wl-copy", "powershell.exe", "xclip"]);
        assert_eq!(program(&choose(wslg, no_yank)), "wl-copy");
        assert_eq!(program(&choose(plain, no_yank)), "powershell.exe");
        // Only PowerShell: the command sets UTF-8 on its input.
        for platform in [wslg, plain] {
            let Clipboard::Command { program, args } =
                choose(platform, only(&["clip.exe", "powershell.exe"]))
            else {
                panic!("PowerShell under {platform:?}");
            };
            assert_eq!(program, "powershell.exe");
            assert_eq!(args, POWERSHELL);
        }
        // None of the three: the terminal, never xclip or clip.exe.
        for platform in [wslg, plain] {
            let none = only(&["clip.exe", "xclip"]);
            assert_eq!(choose(platform, none), Clipboard::Terminal, "{platform:?}");
            assert_eq!(choose(platform, |_| false), Clipboard::Terminal);
        }
    }

    /// Runs `program` as [`run`] does, again while the system says the
    /// stand-in just written is busy (another test's fork may hold it open
    /// for a moment on Linux).
    #[cfg(unix)]
    fn run_fresh(program: &str, args: &[String], text: &str) -> Result<(), String> {
        let start = Instant::now();
        loop {
            match run(program, args, text, LIMIT) {
                Err(e) if e.contains("os error 26") && start.elapsed() < LIMIT => {
                    thread::sleep(Duration::from_millis(20))
                }
                result => return result,
            }
        }
    }

    /// The fix's row. On WSL-shaped environments, the helper chosen is never
    /// `clip.exe`, and what reaches its standard input is the title's UTF-8
    /// bytes, unchanged; PowerShell's leg is told to read them as UTF-8. The
    /// helpers are stand-ins in this test's own folder, each recording its
    /// input and arguments: the real clipboard is never touched.
    #[cfg(unix)]
    #[test]
    fn under_wsl_the_chosen_helper_is_never_clip_exe_and_gets_the_titles_utf_8_bytes() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("wsl-helpers");
        let helpers = [
            "win32yank.exe",
            "wl-copy",
            "powershell.exe",
            "clip.exe",
            "xclip",
        ];
        for name in helpers {
            let path = dir.join(name);
            fs::write(
                &path,
                format!(
                    "#!/bin/sh\ncat > \"$(dirname \"$0\")/{name}.in\"\nprintf '%s\\n' \"$@\" > \"$(dirname \"$0\")/{name}.args\"\n"
                ),
            )
            .unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let wslg = Platform {
            wsl: true,
            wayland: true,
            x11: true,
            ..Default::default()
        };
        let plain = Platform {
            wayland: false,
            ..wslg
        };
        let title = "복사 하기 — pay rent\n장보기";
        // (where, which helpers are there, the one it must take)
        let rows: [(Platform, &[&str], &str); 5] = [
            (wslg, &helpers, "win32yank.exe"),
            (plain, &helpers, "win32yank.exe"),
            (
                wslg,
                &["wl-copy", "powershell.exe", "clip.exe", "xclip"],
                "wl-copy",
            ),
            (
                wslg,
                &["powershell.exe", "clip.exe", "xclip"],
                "powershell.exe",
            ),
            (
                plain,
                &["powershell.exe", "clip.exe", "xclip"],
                "powershell.exe",
            ),
        ];
        for (platform, there, want) in rows {
            for name in helpers {
                let _ = fs::remove_file(dir.join(format!("{name}.in")));
            }
            let chosen = choose(platform, |p| there.contains(&p));
            let Clipboard::Command { program, args } = chosen else {
                panic!("{platform:?} with {there:?}: {chosen:?}");
            };
            assert_ne!(program, "clip.exe", "{platform:?} with {there:?}");
            assert_eq!(program, want, "{platform:?} with {there:?}");

            let helper = dir.join(&program).display().to_string();
            assert_eq!(run_fresh(&helper, &args, title), Ok(()), "{program}");
            assert_eq!(
                fs::read(dir.join(format!("{program}.in"))).unwrap(),
                title.as_bytes(),
                "{program}: the title's UTF-8 bytes, unchanged"
            );
            assert!(!dir.join("clip.exe.in").exists(), "clip.exe never ran");

            let got = fs::read_to_string(dir.join(format!("{program}.args"))).unwrap();
            if program == "powershell.exe" {
                // The one place the encoding could go wrong again: the
                // input set to UTF-8, before the read, in what it was run
                // with.
                let set = "[Console]::InputEncoding=[Text.Encoding]::UTF8;";
                let read = "[Console]::In.ReadToEnd()";
                let script = got.lines().nth(3).unwrap_or_default();
                assert_eq!(
                    got.lines().take(3).collect::<Vec<_>>(),
                    ["-NoProfile", "-NonInteractive", "-Command"],
                    "{got}"
                );
                assert!(script.starts_with(set), "{got}");
                assert!(script.find(set) < script.find(read), "{got}");
            }
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn powershells_arguments_set_its_input_to_utf_8_before_reading_it() {
        let ["-NoProfile", "-NonInteractive", "-Command", script] = POWERSHELL else {
            panic!("{POWERSHELL:?}");
        };
        let set = "[Console]::InputEncoding=[Text.Encoding]::UTF8;";
        let read = "Set-Clipboard -Value ([Console]::In.ReadToEnd())";
        assert!(script.starts_with(set), "{script}");
        assert!(script.contains(read), "{script}");
        assert!(script.find(set) < script.find(read), "{script}");
    }

    #[test]
    fn osc_52_carries_the_text_in_base64() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"pay rent"), "cGF5IHJlbnQ=");
        assert_eq!(base64("장보기".as_bytes()), "7J6l67O06riw");
        assert_eq!(osc52("pay rent"), "\x1b]52;c;cGF5IHJlbnQ=\x07");
    }

    #[test]
    fn a_command_gets_the_text_on_its_input() {
        let dir = temp_dir("input");
        let (program, args) = sh("cat > \"$1/copied\"", &dir);
        assert_eq!(run(&program, &args, "pay rent\n장보기", LIMIT), Ok(()));
        assert_eq!(
            fs::read_to_string(dir.join("copied")).unwrap(),
            "pay rent\n장보기"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_command_that_fails_or_cant_run_says_so() {
        let dir = temp_dir("fails");
        let (program, args) = sh("cat > /dev/null; exit 3", &dir);
        let err = run(&program, &args, "x", LIMIT).unwrap_err();
        assert!(err.starts_with("sh failed") && err.contains('3'), "{err}");
        let missing = dir.join("no-such-copier").display().to_string();
        let err = run(&missing, &[], "x", LIMIT).unwrap_err();
        assert!(err.starts_with(&format!("can't run {missing}")), "{err}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_command_that_hangs_is_killed_past_the_limit_and_spawn_never_waits() {
        let dir = temp_dir("hangs");
        // It never reads its input and never ends, but for the kill.
        let (program, args) = sh("sleep 30; touch \"$1/finished\"", &dir);
        let start = Instant::now();
        let err = run(&program, &args, "x", Duration::from_millis(200)).unwrap_err();
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        assert_eq!(err, "sh didn't finish in 0.2 s");

        let (tx, rx) = mpsc::channel();
        let start = Instant::now();
        spawn(
            program,
            args,
            "x".into(),
            "\"x\"".into(),
            Duration::from_millis(200),
            tx,
        );
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "spawn returns at once"
        );
        assert!(rx.try_recv().is_err(), "nothing yet");
        let (what, result) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(what, "\"x\"");
        assert!(result.unwrap_err().contains("didn't finish"));
        assert!(!dir.join("finished").exists(), "killed");

        // More than a pipe holds, to a command that never reads it: the
        // writing can't hold the wait up either.
        let (program, args) = sh("exec sleep 30", &dir);
        let start = Instant::now();
        let big = "x".repeat(1 << 20);
        assert!(run(&program, &args, &big, Duration::from_millis(200)).is_err());
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
