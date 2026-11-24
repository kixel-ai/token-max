//! Doctor's upgrade offer answered yes on a Windows console, which only a pseudo console gives
//! a child: `std::process::Command` cannot attach one.
//!
//! This binary is also the release the offer installs. With `TEST_STAND_IN_VERSION` set, which
//! doctor passes on to what it runs, it reports that version, or prints what it ran with and
//! exits [`windows::RERUN_CODE`]. Built without libtest, which would reject doctor's arguments.

mod support;

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() {
    windows::main();
}

#[cfg(windows)]
mod windows {
    use std::ffi::OsStr;
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use crate::support;

    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::{
        COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON,
    };
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{
        CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
        EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE,
        InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
        PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, STARTF_USESTDHANDLES,
        STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
    };

    const BIN: &str = env!("CARGO_BIN_EXE_token-max");
    const VERSION: &str = env!("CARGO_PKG_VERSION");
    const STAND_IN: &str = "TEST_STAND_IN_VERSION";
    /// The stand-in's exit code after a rerun, which no doctor run returns of its own.
    pub const RERUN_CODE: i32 = 42;

    pub fn main() {
        if let Some(version) = std::env::var_os(STAND_IN) {
            let args: Vec<String> = std::env::args().skip(1).collect();
            if args == ["--version"] {
                println!("{}", version.to_str().unwrap());
                return;
            }
            println!("ran {}", args.join(" "));
            std::process::exit(RERUN_CODE);
        }
        println!("test a_yes_on_a_console_replaces_the_running_binary_and_waits_on_the_rerun ...");
        a_yes_on_a_console_replaces_the_running_binary_and_waits_on_the_rerun();
        println!(
            "test a_yes_on_a_console_replaces_the_running_binary_and_waits_on_the_rerun ... ok"
        );
    }

    fn a_yes_on_a_console_replaces_the_running_binary_and_waits_on_the_rerun() {
        let mut releases = support::Releases::new();
        releases.binary = std::env::current_exe().unwrap();
        releases.publish("v99.0.0", false);
        let home = releases.root.join("home");
        let claude = home.join(".claude");
        std::fs::create_dir_all(claude.join("projects/project")).unwrap();
        std::fs::copy(
            fixture("claude.jsonl"),
            claude.join("projects/project/main.jsonl"),
        )
        .unwrap();
        // A copy, so the upgrade moves only it aside.
        let dir = home.join("bin");
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("token-max.exe");
        std::fs::copy(BIN, &exe).unwrap();
        let args = [
            "doctor".to_string(),
            "--claude-dir".into(),
            claude.to_str().unwrap().into(),
            "--prices-file".into(),
            fixture("prices.json").to_str().unwrap().into(),
            "--plain".into(),
        ];
        let mut env = vec![
            ("HOME".into(), home.to_str().unwrap().to_string()),
            // Windows resolves the home from USERPROFILE and ignores HOME.
            ("USERPROFILE".into(), home.to_str().unwrap().into()),
            ("TOKEN_MAX_RELEASES".into(), releases.url.clone()),
            (STAND_IN.to_string(), "99.0.0".into()),
        ];
        // Without it, Winsock cannot open a socket.
        env.extend(std::env::var("SystemRoot").map(|root| ("SystemRoot".into(), root)));

        // After the yes, what a failed upgrade's walk needs to end: skip how the profile pays,
        // then quit. A rerun never reads them.
        let (code, screen) = on_console(&exe, &args, &env, b"y\rs\rq\r");
        let screen = words(&screen);
        assert_eq!(code, RERUN_CODE as u32, "{screen}");
        for said in [
            format!(
                "token-max v99.0.0 is out; this is v{VERSION}, and its advice may be out of date."
            ),
            "Upgrade to v99.0.0 and rerun? [y]es, [n]o:".into(),
            "Downloading v99.0.0...".into(),
            "Upgraded to v99.0.0; rerunning.".into(),
            // The rerun has this run's arguments and this run's console.
            format!("ran {}", args.join(" ")),
        ] {
            assert!(screen.contains(&words(&said)), "{said:?} in {screen}");
        }
        // Windows' own tar unpacked the stand-in, and the binary it replaced moved aside while
        // it ran.
        let stand_in = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        assert!(std::fs::read(&exe).unwrap() == stand_in);
        let old = dir.join("token-max.exe.old");
        assert!(std::fs::read(&old).unwrap() == std::fs::read(BIN).unwrap());
        assert_eq!(names(&dir), ["token-max.exe", "token-max.exe.old"]);

        // The next run of a real build there deletes what the upgrade moved aside.
        std::fs::copy(BIN, &exe).unwrap();
        let mut next = Command::new(&exe);
        support::isolated(&mut next)
            .args(&args)
            .arg("--offline")
            .envs(env.iter().cloned())
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        let output = next.output().unwrap();
        assert!(matches!(output.status.code(), Some(0 | 1)), "{output:?}");
        assert_eq!(names(&dir), ["token-max.exe"]);
    }

    /// Run `program` with only `env`, on a console of its own that is sent `keys` up front,
    /// and return its exit code and everything the console drew.
    fn on_console(
        program: &Path,
        args: &[String],
        env: &[(String, String)],
        keys: &[u8],
    ) -> (u32, String) {
        // SAFETY: every out-pointer is valid, each handle is owned once, and the attribute list
        // outlives the CreateProcessW call that reads it.
        unsafe {
            let (mut input, mut console_in) = (std::ptr::null_mut(), std::ptr::null_mut());
            let (mut console_out, mut output) = (std::ptr::null_mut(), std::ptr::null_mut());
            assert_ne!(
                CreatePipe(&mut console_in, &mut input, std::ptr::null(), 0),
                0
            );
            assert_ne!(
                CreatePipe(&mut output, &mut console_out, std::ptr::null(), 0),
                0
            );
            let mut console: HPCON = 0;
            // Wide enough that no line wraps.
            let size = COORD { X: 500, Y: 50 };
            assert_eq!(
                CreatePseudoConsole(size, console_in, console_out, 0, &mut console),
                0
            );
            // The console holds its own copies of its ends.
            CloseHandle(console_in);
            CloseHandle(console_out);
            let mut output = File::from(OwnedHandle::from_raw_handle(output));
            // The console stops drawing when nothing reads it, so read alongside.
            let reader = std::thread::spawn(move || {
                let mut drawn = vec![];
                // A broken pipe once the console closes.
                let _ = output.read_to_end(&mut drawn);
                String::from_utf8_lossy(&drawn).into_owned()
            });
            let mut input = File::from(OwnedHandle::from_raw_handle(input));
            input.write_all(keys).unwrap();

            let mut size = 0;
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size);
            let mut list = vec![0u8; size];
            let attributes: LPPROC_THREAD_ATTRIBUTE_LIST = list.as_mut_ptr().cast();
            assert_ne!(
                InitializeProcThreadAttributeList(attributes, 1, 0, &mut size),
                0
            );
            assert_ne!(
                UpdateProcThreadAttribute(
                    attributes,
                    0,
                    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                    console as *const std::ffi::c_void,
                    size_of::<HPCON>(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                ),
                0
            );
            let mut startup: STARTUPINFOEXW = std::mem::zeroed();
            startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
            // Invalid handles, or the child takes this process's redirected ones, such as
            // cargo's pipes, instead of the console's.
            startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            startup.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
            startup.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
            startup.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
            startup.lpAttributeList = attributes;
            let mut line = wide(&command_line(program, args));
            let mut block = environment(env);
            let mut process: PROCESS_INFORMATION = std::mem::zeroed();
            assert_ne!(
                CreateProcessW(
                    std::ptr::null(),
                    line.as_mut_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                    EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                    block.as_mut_ptr().cast(),
                    std::ptr::null(),
                    &startup.StartupInfo,
                    &mut process,
                ),
                0,
                "{}",
                std::io::Error::last_os_error()
            );
            DeleteProcThreadAttributeList(attributes);
            CloseHandle(process.hThread);
            WaitForSingleObject(process.hProcess, INFINITE);
            let mut code = 0;
            assert_ne!(GetExitCodeProcess(process.hProcess, &mut code), 0);
            CloseHandle(process.hProcess);
            // Closing draws the last frame and ends the output.
            ClosePseudoConsole(console);
            drop(input);
            (code, reader.join().unwrap())
        }
    }

    /// The text a console drew, its control sequences replaced by spaces and its whitespace
    /// collapsed, since a console moves its cursor rather than draw every blank.
    fn words(drawn: &str) -> String {
        let mut text = String::new();
        let mut chars = drawn.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                text.push(c);
                continue;
            }
            text.push(' ');
            match chars.next() {
                // CSI: parameters, then one final byte.
                Some('[') => while chars.next().is_some_and(|c| !('@'..='~').contains(&c)) {},
                // OSC: up to BEL or ST.
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// `program` and `args` as one command line, quoted as the C runtime and Rust parse it.
    fn command_line(program: &Path, args: &[String]) -> String {
        let mut line = String::new();
        for arg in std::iter::once(program.to_str().unwrap()).chain(args.iter().map(String::as_str))
        {
            if !line.is_empty() {
                line.push(' ');
            }
            if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
                line.push_str(arg);
                continue;
            }
            line.push('"');
            let mut backslashes = 0;
            for c in arg.chars() {
                if c == '\\' {
                    backslashes += 1;
                    continue;
                }
                let escapes = if c == '"' {
                    backslashes * 2 + 1
                } else {
                    backslashes
                };
                line.extend(std::iter::repeat_n('\\', escapes));
                backslashes = 0;
                line.push(c);
            }
            line.extend(std::iter::repeat_n('\\', backslashes * 2));
            line.push('"');
        }
        line
    }

    /// `env` as CreateProcessW's environment block: sorted `name=value` strings, each
    /// terminated, then one more terminator.
    fn environment(env: &[(String, String)]) -> Vec<u16> {
        let mut env = env.to_vec();
        env.sort_by_key(|(name, _)| name.to_uppercase());
        let mut block: Vec<u16> = env
            .iter()
            .flat_map(|(name, value)| wide(&format!("{name}={value}")))
            .collect();
        block.push(0);
        block
    }

    /// `text` as a terminated UTF-16 string.
    fn wide(text: &str) -> Vec<u16> {
        OsStr::new(text).encode_wide().chain([0]).collect()
    }

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test/fixtures")
            .join(name)
    }

    /// Every name in `dir`, sorted.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }
}
