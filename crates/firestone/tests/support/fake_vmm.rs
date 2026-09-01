use std::{
    env,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitCode, Stdio},
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
    if arguments.iter().any(|argument| argument == "--foreground")
        || arguments.get(1).is_some_and(|argument| argument == "--socket-path")
    {
        return run_fake_sidecar(&arguments);
    }
    if matches!(
        arguments.get(1).map(String::as_str),
        Some("convert" | "create" | "info" | "resize")
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
    if options.behavior == "delayed-ready" {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    if let Some(pid_path) = &options.descendant_pid {
        let child_pid = if options.behavior == "thread-descendant" {
            let (sender, receiver) = std::sync::mpsc::sync_channel(1);
            std::thread::spawn(move || match spawn_descendant() {
                Ok(mut child) => {
                    let _ = sender.send(Ok(child.id()));
                    let _ = child.wait();
                }
                Err(error) => {
                    let _ = sender.send(Err(error));
                }
            });
            receiver.recv()??
        } else {
            spawn_descendant()?.id()
        };
        fs::write(pid_path, format!("{child_pid}\n"))?;
    }


    let pty = FakePty::open()?;
    let _ = fs::remove_file(&options.api_socket);
    let mut sidecar_connections = Vec::new();
    let mut reported_vcpus: u64 = 0;
    let mut reported_ram: u64 = 1;
    let listener = UnixListener::bind(&options.api_socket)?;
    for connection in listener.incoming() {
        let mut stream = connection?;
        let request = read_request(&mut stream)?;
        if request.path == "/api/v1/vm.power-button"
            && options.behavior == "slow-power"
        {
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
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
                let _ = response(&mut stream, "200 OK", body.as_bytes());
            }
            "/api/v1/vm.create" if options.behavior == "create-fail" => {
                response(
                    &mut stream,
                    "500 Internal Server Error",
                    b"injected create failure",
                )?;
            }
            "/api/v1/vm.create" => {
                reported_vcpus = json_u64(&request.body, "\"boot_vcpus\":").unwrap_or(0);
                reported_ram = json_u64(&request.body, "\"size\":").unwrap_or(1);
                fs::write(&options.body, &request.body)?;
                sidecar_connections.extend(connect_sidecars(&request.body)?);
                start_vsock(&request.body)?;
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
            "/api/v1/vm.resize" if options.behavior == "resize-fail" => {
                response(
                    &mut stream,
                    "500 Internal Server Error",
                    b"injected resize failure",
                )?;
            }
            "/api/v1/vm.resize" => {
                fs::write(
                    options.body.with_extension("resize"),
                    &request.body,
                )?;
                if let Some(vcpus) = json_u64(&request.body, "\"desired_vcpus\":") {
                    reported_vcpus = vcpus;
                }
                if let Some(ram) = json_u64(&request.body, "\"desired_ram\":") {
                    reported_ram = ram;
                }
                no_content(&mut stream)?;
            }
            "/api/v1/vm.info" => {
                let state = if options.behavior == "info-shutdown" {
                    "Shutdown"
                } else {
                    "Running"
                };
                let body = format!(
                    "{{\"config\":{{\"console\":{{\"mode\":\"Pty\",\"file\":{:?}}},\"cpus\":{{\"boot_vcpus\":{reported_vcpus},\"max_vcpus\":{reported_vcpus}}},\"memory\":{{\"size\":{reported_ram}}}}},\"state\":\"{state}\",\"memory_actual_size\":{reported_ram},\"device_tree\":null}}",
                    pty.path
                );
                response(&mut stream, "200 OK", body.as_bytes())?;
            }
            "/api/v1/vm.counters" => {
                // Mirrors verified Cloud Hypervisor v53 shapes: block devices keyed
                // by id, latency counters saturated to u64::MAX-family sentinels when
                // a device has never been written, and no net entries under passt
                // vhost-user networking.
                let body = concat!(
                    "{\"_disk0\":{\"read_bytes\":4096,\"read_latency_avg\":37,",
                    "\"read_latency_max\":81,\"read_latency_min\":11,\"read_ops\":2,",
                    "\"write_bytes\":8192,\"write_latency_avg\":9223372036854775815,",
                    "\"write_latency_max\":18446744073709551615,",
                    "\"write_latency_min\":18446744073709551615,\"write_ops\":3},",
                    "\"_disk1\":{\"read_bytes\":0,\"read_latency_avg\":9223372036854775815,",
                    "\"read_latency_max\":18446744073709551615,",
                    "\"read_latency_min\":18446744073709551615,\"read_ops\":0,",
                    "\"write_bytes\":0,\"write_latency_avg\":9223372036854775815,",
                    "\"write_latency_max\":18446744073709551615,",
                    "\"write_latency_min\":18446744073709551615,\"write_ops\":0}}"
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
fn option_value(arguments: &[String], option: &str) -> Option<PathBuf> {
    arguments
        .windows(2)
        .find(|pair| pair[0] == option)
        .map(|pair| PathBuf::from(&pair[1]))
}

fn fake_sidecar_name(socket: &Path, passt: bool) -> Result<String, Box<dyn std::error::Error>> {
    if passt {
        return Ok("passt".to_owned());
    }
    let file = socket
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or("fake virtiofsd socket has no UTF-8 file name")?;
    let index = file
        .strip_prefix("fs")
        .and_then(|value| value.strip_suffix(".sock"))
        .ok_or("fake virtiofsd socket does not use fsN.sock")?;
    Ok(format!("virtiofsd-{index}"))
}

fn run_fake_sidecar(arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let passt = arguments.iter().any(|argument| argument == "--foreground");
    let socket = option_value(arguments, if passt { "--socket" } else { "--socket-path" })
        .ok_or("fake sidecar has no socket option")?;
    let name = fake_sidecar_name(&socket, passt)?;
    if passt
        && arguments
            .get(arguments.len().saturating_sub(2)..)
            != Some(["--repair-path".to_owned(), "none".to_owned()].as_slice())
    {
        return Err("passt repair-path pair is not final".into());
    }
    let mut environment = env::vars_os()
        .map(|(key, _)| key.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    environment.sort();
    if let Some(record) = env::var_os("FIRESTONE_FAKE_SIDECAR_RECORD") {
        append(
            Path::new(&record),
            format!(
                "launch {name} pid={} argv={:?} env={environment:?}\n",
                std::process::id(),
                &arguments[1..]
            )
            .as_bytes(),
        )?;
    }
    if env::var("FIRESTONE_FAKE_SIDECAR_FAIL").as_deref() == Ok(name.as_str()) {
        return Err(format!("injected {name} failure before readiness").into());
    }
    if env::var("FIRESTONE_FAKE_SIDECAR_NEVER_READY").as_deref() == Ok(name.as_str()) {
        std::thread::sleep(std::time::Duration::from_secs(30));
        return Ok(());
    }

    let _ = fs::remove_file(&socket);
    let pid_file = (!passt).then(|| PathBuf::from(format!("{}.pid", socket.display())));
    if let Some(pid_file) = pid_file.as_ref() {
        let _ = fs::remove_file(pid_file);
    }
    if env::var("FIRESTONE_FAKE_SIDECAR_BAD_READY").as_deref() == Ok(name.as_str()) {
        fs::write(&socket, b"not a socket")?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o700))?;
        if let Some(pid_file) = pid_file.as_ref() {
            fs::write(pid_file, format!("{}\n", std::process::id()))?;
            fs::set_permissions(pid_file, fs::Permissions::from_mode(0o600))?;
        }
        std::thread::sleep(std::time::Duration::from_secs(30));
        return Ok(());
    }
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o700))?;
    if let Some(pid_file) = pid_file.as_ref() {
        fs::write(pid_file, format!("{}\n", std::process::id()))?;
        fs::set_permissions(pid_file, fs::Permissions::from_mode(0o600))?;
    }
    eprintln!("fake {name} ready");
    if env::var("FIRESTONE_FAKE_SIDECAR_EXIT_AFTER_READY").as_deref() == Ok(name.as_str()) {
        std::thread::sleep(std::time::Duration::from_millis(100));
        return Err(format!("injected {name} exit after readiness").into());
    }

    let (mut stream, _) = listener.accept()?;
    if let Some(record) = env::var_os("FIRESTONE_FAKE_SIDECAR_RECORD") {
        append(Path::new(&record), format!("connect {name}\n").as_bytes())?;
    }
    if env::var("FIRESTONE_FAKE_SIDECAR_EXIT_AFTER_CONNECT").as_deref() == Ok(name.as_str()) {
        return Err(format!("injected {name} exit after VMM connection").into());
    }
    let mut buffer = [0_u8; 4096];
    while stream.read(&mut buffer)? != 0 {}
    Ok(())
}

fn json_u64(body: &[u8], marker: &str) -> Option<u64> {
    let text = std::str::from_utf8(body).ok()?;
    let start = text.find(marker)? + marker.len();
    let rest = &text[start..];
    let end = rest
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn json_string_values(text: &str, marker: &str) -> Vec<PathBuf> {
    let mut values = Vec::new();
    let mut remaining = text;
    while let Some(start) = remaining.find(marker) {
        let tail = &remaining[start + marker.len()..];
        let Some(end) = tail.find('"') else {
            break;
        };
        values.push(PathBuf::from(&tail[..end]));
        remaining = &tail[end + 1..];
    }
    values
}

fn connect_sidecars(config: &[u8]) -> Result<Vec<UnixStream>, Box<dyn std::error::Error>> {
    let text = std::str::from_utf8(config)?;
    let mut paths = json_string_values(text, "\"vhost_socket\":\"");
    if let Some(fs_start) = text.find("\"fs\":[") {
        let fs_tail = &text[fs_start..];
        let fs_end = fs_tail.find("],\"memory\"").unwrap_or(fs_tail.len());
        paths.extend(json_string_values(
            &fs_tail[..fs_end],
            "\"socket\":\"",
        ));
    }
    paths
        .into_iter()
        .map(|path| UnixStream::connect(&path).map_err(Into::into))
        .collect()
}

fn start_vsock(config: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let config = std::str::from_utf8(config)?;
    let Some(vsock) = config.find("\"vsock\"") else {
        return Ok(());
    };
    let tail = &config[vsock..];
    let marker = "\"socket\":\"";
    let start = tail.find(marker).ok_or("VmConfig vsock has no socket")? + marker.len();
    let tail = &tail[start..];
    let end = tail.find('"').ok_or("unterminated VmConfig vsock socket")?;
    let path = PathBuf::from(&tail[..end]);
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    std::thread::Builder::new()
        .name("fake-vsock".to_owned())
        .spawn(move || {
            for connection in listener.incoming() {
                let Ok(mut stream) = connection else {
                    break;
                };
                let mut request = Vec::new();
                let mut byte = [0_u8; 1];
                while request.len() < 64 && !request.ends_with(b"\n") {
                    match stream.read(&mut byte) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => request.push(byte[0]),
                    }
                }
                if request == b"CONNECT 22\n" {
                    let _ = stream.write_all(b"OK 1024\n");
                }
            }
        })?;
    Ok(())
}

struct FakePty {
    child: Child,
    path: String,
}

impl FakePty {
    fn open() -> Result<Self, Box<dyn std::error::Error>> {
        let script = "import os,pty,signal,tty\nm,s=pty.openpty()\ntty.setraw(s)\nprint(os.ttyname(s), flush=True)\nsignal.pause()\n";
        let mut child = Command::new("python3")
            .arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdout = child.stdout.take().ok_or("fake PTY helper has no stdout")?;
        let mut path = String::new();
        BufReader::new(stdout).read_line(&mut path)?;
        let path = path.trim_end().to_owned();
        if path.is_empty() {
            let _ = child.kill();
            let _ = child.wait();
            return Err("fake PTY helper returned an empty path".into());
        }
        Ok(Self { child, path })
    }
}

impl Drop for FakePty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_descendant() -> Result<std::process::Child, std::io::Error> {
    #[cfg(target_os = "linux")]
    let mut command = {
        let mut command = Command::new("setsid");
        command.arg("/bin/sleep").arg("60");
        command
    };
    #[cfg(not(target_os = "linux"))]
    let mut command = {
        let mut command = Command::new("/bin/sleep");
        command.arg("60");
        command
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

fn run_qemu(arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let executable = env::current_exe()?;
    let log = executable.with_extension("qemu.log");
    append(&log, format!("{}\n", arguments[1..].join(" ")).as_bytes())?;
    match arguments.get(1).map(String::as_str) {
        Some("convert") => {
            if let Some(index) = arguments.iter().position(|argument| argument == "-B") {
                // Overlay copy: qemu-img convert -f qcow2 -O qcow2 -o backing_fmt=qcow2
                //               -B <backing> <src> <dest>
                let backing = arguments.get(index + 1).ok_or("missing convert backing")?;
                let source = Path::new(arguments.get(index + 2).ok_or("missing convert source")?);
                let target = Path::new(arguments.get(index + 3).ok_or("missing convert target")?);
                let data = fs::read(source)?;
                let suffix = data.get(4..).unwrap_or_default();
                let text = std::str::from_utf8(suffix)?;
                let size = text
                    .lines()
                    .nth(2)
                    .ok_or("overlay source has no recorded size")?;
                let mut output = vec![b'Q', b'F', b'I', 0xfb];
                output.extend_from_slice(format!("OVERLAY\n{backing}\n{size}\n").as_bytes());
                fs::write(target, output)?;
                return Ok(());
            }
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
        Some("resize") => {
            let target = Path::new(arguments.get(4).ok_or("missing resize target")?);
            let size = arguments.get(5).ok_or("missing resize size")?;
            let data = fs::read(target)?;
            let suffix = data.get(4..).unwrap_or_default();
            let text = std::str::from_utf8(suffix)?;
            let mut lines = text.lines();
            let _ = lines.next();
            let backing = lines.next().ok_or("missing backing")?;
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
