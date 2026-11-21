//! `--ssh` audits coordinated from Windows, through a fake `ssh.exe` that is this binary.
//!
//! The fake runs each destination's command with Git for Windows' `sh`, whose faked `uname`
//! names Linux, so the coordinator takes the other-platform path and runs the `token-max`
//! installed there: a copy of this build. Built without libtest, since as `ssh.exe` this binary
//! takes OpenSSH's arguments, which libtest would reject.

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() {
    windows::main();
}

#[cfg(windows)]
mod windows {
    use std::io::{BufRead, BufReader};
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use serde_json::Value;
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, GetCurrentProcess, OpenProcess, PROCESS_SYNCHRONIZE,
        WaitForSingleObject,
    };

    const BIN: &str = env!("CARGO_BIN_EXE_token-max");
    const VERSION: &str = env!("CARGO_PKG_VERSION");

    pub fn main() {
        let exe = std::env::current_exe().unwrap();
        if exe.file_stem() == Some("ssh".as_ref()) {
            std::process::exit(ssh());
        }
        let tests: [(&str, fn()); 4] = [
            (
                "a_remote_report_comes_back_whole",
                a_remote_report_comes_back_whole,
            ),
            (
                "remote_sessions_match_running_there",
                remote_sessions_match_running_there,
            ),
            (
                "a_failed_destination_fails_the_whole_report",
                a_failed_destination_fails_the_whole_report,
            ),
            (
                "cancelling_stops_every_destination",
                cancelling_stops_every_destination,
            ),
        ];
        for (name, test) in tests {
            println!("test {name} ...");
            test();
            println!("test {name} ... ok");
        }
    }

    /// Run the destination's command as OpenSSH does there: with that host's `sh`, home and
    /// `PATH`, in a job that dies with this process, as a remote command dies with its
    /// connection.
    fn ssh() -> i32 {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let at = args.iter().position(|a| a == "--").unwrap();
        let (host, command) = (&args[at + 1], &args[at + 2]);
        eprintln!("login noise from {host}");
        if host == "unreachable" {
            return 255;
        }
        let root = PathBuf::from(std::env::var_os("TEST_ROOT").unwrap());
        std::fs::write(
            root.join(format!("{host}.ssh")),
            std::process::id().to_string(),
        )
        .unwrap();
        record_interrupts(root.join(format!("{host}.interrupted")));
        kill_with_this_process();
        // As a file, so no Windows command line stands between the command and `sh`.
        let script = root.join(format!("scripts/{}.sh", std::process::id()));
        std::fs::write(&script, command).unwrap();
        let home = root.join("homes").join(host);
        let tools = PathBuf::from(std::env::var_os("TEST_TOOLS").unwrap());
        let path = std::env::join_paths([root.join("remote").join(host), tools.clone()]).unwrap();
        let status = Command::new(tools.join("sh.exe"))
            .arg(posix(&script))
            .env("HOME", &home)
            // Windows resolves the home from USERPROFILE and ignores HOME.
            .env("USERPROFILE", &home)
            .env("PATH", path)
            .env("TEST_ROOT", posix(&root))
            .current_dir(&home)
            .status()
            .unwrap();
        status.code().unwrap_or(1)
    }

    /// Create `file` on any Ctrl-C or Ctrl-Break that reaches this process, before it dies of it
    /// as OpenSSH does.
    fn record_interrupts(file: PathBuf) {
        static FILE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        unsafe extern "system" fn handler(_: u32) -> i32 {
            let _ = std::fs::write(FILE.get().unwrap(), "");
            // Not handled, so the default handler exits this process.
            0
        }
        FILE.set(file).unwrap();
        // SAFETY: the handler only writes a file this test owns.
        assert_ne!(unsafe { SetConsoleCtrlHandler(Some(handler), 1) }, 0);
    }

    /// Put this process, and so every process it starts, in a job that is killed once this one
    /// exits or is killed.
    fn kill_with_this_process() {
        // SAFETY: each call gets valid arguments; the job's handle is deliberately kept open
        // until this process exits, which is what kills the job.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            assert!(!job.is_null());
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            assert_ne!(
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    (&raw const limits).cast(),
                    size_of_val(&limits) as u32,
                ),
                0
            );
            assert_ne!(AssignProcessToJobObject(job, GetCurrentProcess()), 0);
        }
    }

    /// `path` with forward slashes, which `sh` reads without escaping.
    fn posix(path: &Path) -> String {
        path.to_str().unwrap().replace('\\', "/")
    }

    /// Whether process `pid` exits within a minute; killing one is asynchronous on Windows.
    fn exits(pid: u32) -> bool {
        // SAFETY: the handle is checked, waited on and closed here.
        unsafe {
            let process = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if process.is_null() {
                return true;
            }
            let waited = WaitForSingleObject(process, 60_000);
            CloseHandle(process);
            waited == WAIT_OBJECT_0
        }
    }

    /// Git for Windows' tools directory, with its `sh` and `uname`.
    fn tools() -> PathBuf {
        let path = std::env::var_os("PATH").unwrap();
        std::env::split_paths(&path)
            .flat_map(|dir| [dir.clone(), dir.join("../usr/bin")])
            .find(|dir| dir.join("sh.exe").is_file() && dir.join("uname.exe").is_file())
            .expect("the Windows SSH tests need Git for Windows' sh on PATH")
    }

    /// Destinations under one target scratch directory whose path has a space, removed on drop.
    struct Hosts {
        root: PathBuf,
        tools: PathBuf,
    }

    impl Hosts {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
                "ssh windows test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(root.join("bin")).unwrap();
            std::fs::create_dir_all(root.join("scripts")).unwrap();
            std::fs::copy(std::env::current_exe().unwrap(), root.join("bin/ssh.exe")).unwrap();
            Self {
                root,
                tools: tools(),
            }
        }

        /// A Linux destination with the synthetic transcript and this build installed, as the
        /// release installers put it.
        fn host(&self, name: &str) -> PathBuf {
            let home = self.home(name);
            let bin = home.join(".local/bin");
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::copy(BIN, bin.join("token-max.exe")).unwrap();
            home
        }

        /// A Linux destination whose installed `token-max` is this shell script instead.
        fn fake(&self, name: &str, body: &str) {
            let home = self.home(name);
            let bin = home.join(".local/bin");
            std::fs::create_dir_all(&bin).unwrap();
            let script =
                format!("#!/bin/sh\n[ \"$1\" = --version ] && echo {VERSION} && exit\n{body}\n");
            std::fs::write(bin.join("token-max"), script).unwrap();
        }

        fn home(&self, name: &str) -> PathBuf {
            let home = self.root.join("homes").join(name);
            let project = home.join(".claude/projects/my project");
            std::fs::create_dir_all(&project).unwrap();
            std::fs::copy(fixture("claude.jsonl"), project.join("main.jsonl")).unwrap();
            std::fs::write(
                home.join(".claude/settings.json"),
                r#"{"promptCacheTtl":"1h"}"#,
            )
            .unwrap();
            let remote = self.root.join("remote").join(name);
            std::fs::create_dir_all(&remote).unwrap();
            std::fs::write(
                remote.join("uname"),
                "#!/bin/sh\ncase \"$1\" in -s) echo Linux ;; -m) echo x86_64 ;; esac\n",
            )
            .unwrap();
            home
        }

        /// The coordinator running `name`, with an empty home of its own and only the test's
        /// environment.
        fn invoke(&self, name: &str, args: &[&str]) -> Command {
            let mut command = Command::new(BIN);
            let local = self.root.join("homes/local");
            std::fs::create_dir_all(&local).unwrap();
            command
                .args([
                    name,
                    "--prices-file",
                    fixture("prices.json").to_str().unwrap(),
                ])
                .args(args)
                .env_clear()
                .env("USERPROFILE", &local)
                .env("SystemRoot", std::env::var_os("SystemRoot").unwrap())
                .env("PATH", self.root.join("bin"))
                .env("TEST_ROOT", &self.root)
                .env("TEST_TOOLS", &self.tools)
                .stdin(Stdio::null());
            command
        }

        /// The first line `doctor --ssh-scan` prints: the header naming this build.
        fn header(&self) -> String {
            // Held open: a destination stops when its stdin closes.
            let mut child = Command::new(BIN)
                .args(["doctor", "--ssh-scan"])
                .env_clear()
                .env("USERPROFILE", self.root.join("homes/empty"))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut line = String::new();
            BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut line)
                .unwrap();
            drop(child.stdin.take());
            child.wait().unwrap();
            line.trim_end().to_string()
        }
    }

    impl Drop for Hosts {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test/fixtures")
            .join(name)
    }

    fn run(command: &mut Command) -> (i32, String, String) {
        let output = command.output().unwrap();
        (
            output.status.code().unwrap(),
            String::from_utf8(output.stdout).unwrap(),
            String::from_utf8(output.stderr).unwrap(),
        )
    }

    fn a_remote_report_comes_back_whole() {
        let hosts = Hosts::new();
        hosts.host("devbox");
        let (status, stdout, stderr) = run(&mut hosts.invoke(
            "doctor",
            &["--ssh", "devbox", "--details", "--since", "2026-11-01"],
        ));
        assert_eq!(status, 0, "{stderr}");
        // Login noise and transcript text stay out of the output.
        assert_eq!(stderr, "");
        assert!(!stdout.contains("SYNTHETIC_PRIVATE"));
        assert!(stdout.starts_with("token-max: devbox  "), "{stdout}");
        assert!(stdout.contains("; 2 requests;"), "{stdout}");
        assert!(
            stdout.contains("each percent is of devbox's spend"),
            "{stdout}"
        );
    }

    /// Each line of a session export, parsed.
    fn lines(stdout: &str) -> Vec<Value> {
        stdout
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn remote_sessions_match_running_there() {
        let hosts = Hosts::new();
        let home = hosts.host("devbox");
        let (status, stdout, stderr) = run(&mut hosts.invoke("sessions", &["--ssh", "devbox"]));
        assert_eq!(status, 0, "{stderr}");
        assert_eq!(stderr, "");
        let remote: Vec<Value> = lines(&stdout)
            .into_iter()
            .map(|mut line| {
                assert_eq!(line["host"], "devbox");
                line["host"] = Value::Null;
                line
            })
            .collect();
        assert!(!remote.is_empty());
        // What the destination's own run prints, read the same way.
        let (status, there, stderr) = run(hosts.invoke("sessions", &[]).env("USERPROFILE", &home));
        assert_eq!(status, 0, "{stderr}");
        assert_eq!(remote, lines(&there));
    }

    fn a_failed_destination_fails_the_whole_report() {
        let hosts = Hosts::new();
        hosts.host("devbox");
        let broken = hosts.host("broken");
        std::fs::write(
            broken.join(".claude/projects/my project/main.jsonl"),
            r#"{"timestamp":"2026-11-02T00:00:00Z","type":"assistant","private":"SYNTHETIC_PRIVATE_REPLY","message":{"usage":{"input_tokens":"bad"}}}
"#,
        )
        .unwrap();
        // Git for Windows' own `uname`, as a Windows destination answers.
        hosts.home("windows");
        std::fs::remove_file(hosts.root.join("remote/windows/uname")).unwrap();
        for (host, expected) in [
            (
                "unreachable",
                "unreachable: SSH connection failed; check that `ssh unreachable true` works",
            ),
            ("broken", "broken: "),
            ("windows", "windows: unsupported platform"),
        ] {
            let (status, stdout, stderr) =
                run(&mut hosts.invoke("doctor", &["--ssh", "devbox", "--ssh", host]));
            assert_eq!(status, 2, "{host}: {stderr}");
            assert_eq!(stdout, "", "{host}");
            assert!(
                stderr.starts_with(&format!("token-max: {expected}")),
                "{host}: {stderr}"
            );
            assert!(!stderr.contains("SYNTHETIC_PRIVATE") && !stderr.contains("noise"));
        }
    }

    fn cancelling_stops_every_destination() {
        let hosts = Hosts::new();
        let header = hosts.header();
        let ready = hosts.root.join("ready");
        // Mid-scan once it names the Windows process running it, which the test then watches.
        hosts.fake(
            "slow",
            &format!(
                "echo '{header}'; cat /proc/$$/winpid > \"$TEST_ROOT/ready.tmp\" && mv \"$TEST_ROOT/ready.tmp\" \"$TEST_ROOT/ready\"; exec sleep 600"
            ),
        );
        // Ctrl-Break, since Windows sends Ctrl-C only to every process on the console; token-max
        // handles both alike.
        let child = hosts
            .invoke("doctor", &["--ssh", "slow"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NEW_PROCESS_GROUP)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while !ready.exists() {
            assert!(Instant::now() < deadline, "the destination never started");
            std::thread::sleep(Duration::from_millis(10));
        }
        // SAFETY: this only signals the process group this test started.
        assert_ne!(
            unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.id()) },
            0,
            "{}",
            std::io::Error::last_os_error()
        );
        let output = child.wait_with_output().unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(output.status.code(), Some(2), "{stderr}");
        assert_eq!(output.stdout, b"");
        assert!(stderr.contains("cancelled"), "{stderr}");
        // As on Unix, only token-max hears the terminal, and it stops each `ssh` itself.
        assert!(!hosts.root.join("slow.interrupted").exists());
        for file in ["slow.ssh", "ready"] {
            let pid = std::fs::read_to_string(hosts.root.join(file)).unwrap();
            let pid: u32 = pid.trim().parse().unwrap();
            assert!(exits(pid), "{file}: {pid} still running");
        }
    }
}
