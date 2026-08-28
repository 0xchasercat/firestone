use std::{
    env,
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
};

struct Options {
    api_socket: PathBuf,
    log_file: PathBuf,
    record: PathBuf,
    body: PathBuf,
    behavior: String,
    console_log: Option<PathBuf>,
    descendant_pid: Option<PathBuf>,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fake-vmm: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = env::args().collect::<Vec<_>>();
    if matches!(
        arguments.get(1).map(String::as_str),
        Some("convert" | "create" | "info")
    ) {
        return run_qemu(&arguments);
    }

    let options = parse_options(&arguments)?;
    let mut environment = env::vars_os()
        .map(|(key, _)| key.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    environment.sort();
    fs::write(
        &options.record,
        format!("argv={arguments:?}\nenv={environment:?}\n"),
    )?;
    append(&options.log_file, b"fake VMM started\n")?;

    if options.behavior == "exit-before-api" {
        return Err("injected exit before API bind".into());
    }
    if options.behavior == "never-ready" {
        std::thread::sleep(std::time::Duration::from_secs(30));
        return Ok(());
    }
    if let Some(pid_path) = &options.descendant_pid {
        let child = Command::new("/bin/sleep")
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        fs::write(pid_path, format!("{}\n", child.id()))?;
    }

    let _ = fs::remove_file(&options.api_socket);
    let listener = UnixListener::bind(&options.api_socket)?;
    for connection in listener.incoming() {
        let mut stream = connection?;
        let request = read_request(&mut stream)?;
        append(
            &options.record,
            format!("{} {}\n", request.method, request.path).as_bytes(),
        )?;
        match request.path.as_str() {
            "/api/v1/vmm.ping" => {
                let body = format!(
                    "{{\"build_version\":\"v53.0\",\"version\":\"53.0.0\",\"pid\":{},\"features\":[]}}",
                    std::process::id()
                );
                response(&mut stream, "200 OK", body.as_bytes())?;
            }
            "/api/v1/vm.create" if options.behavior == "create-fail" => {
                response(
                    &mut stream,
                    "500 Internal Server Error",
                    b"injected create failure",
                )?;
            }
            "/api/v1/vm.create" => {
                fs::write(&options.body, &request.body)?;
                if let Some(console) = &options.console_log {
                    fs::write(console, b"current boot\n")?;
                }
                no_content(&mut stream)?;
            }
            "/api/v1/vm.boot" if options.behavior == "boot-fail" => {
                response(
                    &mut stream,
                    "500 Internal Server Error",
                    b"injected boot failure",
                )?;
            }
            "/api/v1/vm.boot" => {
                no_content(&mut stream)?;
                if options.behavior == "spontaneous" {
                    return Err("injected spontaneous VMM exit".into());
                }
            }
            "/api/v1/vm.power-button" if options.behavior == "power-fail" => {
                response(
                    &mut stream,
                    "500 Internal Server Error",
                    b"injected power failure",
                )?;
            }
            "/api/v1/vm.power-button" => {
                no_content(&mut stream)?;
                if options.behavior != "ignore-power" && options.behavior != "info-shutdown" {
                    return Ok(());
                }
            }
            "/api/v1/vm.info" => {
                let state = if options.behavior == "info-shutdown" {
                    "Shutdown"
                } else {
                    "Running"
                };
                let body = format!(
                    "{{\"config\":{{}},\"state\":\"{state}\",\"memory_actual_size\":1,\"device_tree\":null}}"
                );
                response(&mut stream, "200 OK", body.as_bytes())?;
            }
            "/api/v1/vmm.shutdown" => {
                stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n",
                )?;
                return Ok(());
            }
            other => {
                response(
                    &mut stream,
                    "404 Not Found",
                    format!("unexpected endpoint {other}").as_bytes(),
                )?;
            }
        }
    }
    Ok(())
}

fn run_qemu(arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let executable = env::current_exe()?;
    let log = executable.with_extension("qemu.log");
    append(&log, format!("{}\n", arguments[1..].join(" ")).as_bytes())?;
    match arguments.get(1).map(String::as_str) {
        Some("convert") => {
            let source = Path::new(arguments.get(6).ok_or("missing convert source")?);
            let target = Path::new(arguments.get(7).ok_or("missing convert target")?);
            let mut output = vec![b'Q', b'F', b'I', 0xfb];
            output.extend_from_slice(b"CONVERTED");
            output.extend_from_slice(&fs::read(source)?);
            fs::write(target, output)?;
        }
        Some("create") => {
            let backing = arguments.get(7).ok_or("missing overlay backing")?;
            let target = Path::new(arguments.get(8).ok_or("missing overlay target")?);
            let size = arguments.get(9).ok_or("missing overlay size")?;
            let mut output = vec![b'Q', b'F', b'I', 0xfb];
            output.extend_from_slice(format!("OVERLAY\n{backing}\n{size}\n").as_bytes());
            fs::write(target, output)?;
        }
        Some("info") => {
            let path = Path::new(arguments.get(5).ok_or("missing info path")?);
            let data = fs::read(path)?;
            let suffix = data.get(4..).unwrap_or_default();
            if suffix.starts_with(b"OVERLAY\n") {
                let text = std::str::from_utf8(suffix)?;
                let mut lines = text.lines();
                let _ = lines.next();
                let backing = lines.next().ok_or("missing backing")?;
                let size = lines.next().ok_or("missing size")?.parse::<u64>()?;
                println!(
                    "{{\"format\":\"qcow2\",\"virtual-size\":{size},\"dirty-flag\":false,\"backing-filename\":{backing:?},\"backing-filename-format\":\"qcow2\",\"full-backing-filename\":{backing:?},\"format-specific\":{{\"type\":\"qcow2\",\"data\":{{\"corrupt\":false}}}}}}"
                );
            } else {
                println!(
                    "{{\"format\":\"qcow2\",\"virtual-size\":4,\"dirty-flag\":false,\"format-specific\":{{\"type\":\"qcow2\",\"data\":{{\"corrupt\":false}}}}}}"
                );
            }
        }
        _ => return Err("unknown qemu operation".into()),
    }
    Ok(())
}

fn parse_options(arguments: &[String]) -> Result<Options, Box<dyn std::error::Error>> {
    let mut api_socket = None;
    let mut log_file = None;
    let mut record = None;
    let mut body = None;
    let mut behavior = "normal".to_owned();
    let mut console_log = None;
    let mut descendant_pid = None;
    let mut index = 1;
    while index < arguments.len() {
        let key = arguments.get(index).ok_or("missing option")?;
        let value = arguments.get(index + 1).ok_or("missing option value")?;
        match key.as_str() {
            "--api-socket" => api_socket = Some(PathBuf::from(value)),
            "--log-file" => log_file = Some(PathBuf::from(value)),
            "--record" => record = Some(PathBuf::from(value)),
            "--body" => body = Some(PathBuf::from(value)),
            "--behavior" => behavior = value.clone(),
            "--console-log" => console_log = Some(PathBuf::from(value)),
            "--descendant-pid" => descendant_pid = Some(PathBuf::from(value)),
            _ => return Err(format!("unknown argument {key}").into()),
        }
        index += 2;
    }
    Ok(Options {
        api_socket: api_socket.ok_or("missing --api-socket")?,
        log_file: log_file.ok_or("missing --log-file")?,
        record: record.ok_or("missing --record")?,
        body: body.ok_or("missing --body")?,
        behavior,
        console_log,
        descendant_pid,
    })
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

fn read_request(stream: &mut UnixStream) -> Result<Request, Box<dyn std::error::Error>> {
    let mut bytes = Vec::new();
    let mut one = [0_u8; 1];
    while !bytes.ends_with(b"\r\n\r\n") {
        if stream.read(&mut one)? == 0 {
            return Err("request ended before headers".into());
        }
        bytes.push(one[0]);
        if bytes.len() > 64 * 1024 {
            return Err("request headers too large".into());
        }
    }
    let head = std::str::from_utf8(&bytes)?;
    let first = head.lines().next().ok_or("missing request line")?;
    let mut parts = first.split_ascii_whitespace();
    let method = parts.next().ok_or("missing method")?.to_owned();
    let path = parts.next().ok_or("missing path")?.to_owned();
    let content_length = head
        .lines()
        .find_map(|line| {
            line.strip_prefix("Content-Length:")
                .map(str::trim)
                .and_then(|value| value.parse::<usize>().ok())
        })
        .unwrap_or(0);
    let mut body = vec![0_u8; content_length];
    stream.read_exact(&mut body)?;
    Ok(Request { method, path, body })
}

fn no_content(stream: &mut UnixStream) -> std::io::Result<()> {
    stream.write_all(b"HTTP/1.1 204 No Content\r\nConnection: keep-alive\r\n\r\n")
}

fn response(stream: &mut UnixStream, status: &str, body: &[u8]) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)
}

fn append(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(bytes)
}
