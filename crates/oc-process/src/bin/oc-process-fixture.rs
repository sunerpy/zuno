use std::fs::OpenOptions;
use std::io::{self, Write as _};
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

fn main() -> ExitCode {
    if let Some(code) = oc_process::run_guard_from_args() {
        return code;
    }
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("oc-process fixture failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> io::Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    let mode = arguments
        .next()
        .and_then(|value| value.into_string().ok())
        .ok_or_else(|| io::Error::other("fixture mode is missing"))?;
    let ready = arguments
        .next()
        .ok_or_else(|| io::Error::other("fixture ready path is missing"))?;
    let stop = arguments.next();
    match mode.as_str() {
        "parent" => parent(Path::new(&ready), stop.as_deref().map(Path::new)),
        "payload" => payload(Path::new(&ready)),
        "grandchild" => grandchild(),
        _ => Err(io::Error::other("invalid fixture mode")),
    }
}

fn parent(ready: &Path, stop: Option<&Path>) -> io::Result<()> {
    let executable = std::env::current_exe()?;
    oc_process::activate_guard_executable(&executable)?;
    let (program, arguments) =
        oc_process::guarded_argv(&executable, ["payload".as_ref(), ready.as_os_str()]);
    let mut child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    let stop = stop.ok_or_else(|| io::Error::other("fixture stop path is missing"))?;
    while !stop.exists() {
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "guard exited before stop request: {status}"
            )));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.kill()?;
    let _status = child.wait()?;
    Ok(())
}

fn payload(ready: &Path) -> io::Result<()> {
    let executable = std::env::current_exe()?;
    let child = Command::new(executable)
        .arg("grandchild")
        .arg(ready)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let mut file = OpenOptions::new().create(true).append(true).open(ready)?;
    writeln!(file, "{} {}", std::process::id(), child.id())?;
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

fn grandchild() -> io::Result<()> {
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}
