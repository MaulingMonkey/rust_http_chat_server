use std::io::{self, Read, Write, BufWriter};
use std::net::{TcpStream, TcpListener};
use std::process::Command;
use std::sync::{Mutex, Arc};
use std::sync::mpsc::{Sender, channel, RecvTimeoutError};
use std::time::Duration;

const MAX_REQUEST   : usize = 64 * 1024; // 64 KiB - N.B. stack allocated
const READ_TIMEOUT  : Duration = Duration::from_secs(10);
const WRITE_TIMEOUT : Duration = Duration::from_secs(10);
const SSE_TIMEOUT   : Duration = Duration::from_secs(10);

#[derive(Default)]
struct Common {
    listeners: Mutex<Vec<Sender<Arc<String>>>>,
}

fn main() -> io::Result<()> {
    let mut args = std::env::args();
    let _exe = args.next();
    let mut open = false;
    for arg in args {
        match &*arg {
            "--open"    => open = true,
            _           => panic!("unexpected argument: {arg:?}"),
        }
    }

    let common = Arc::new(Common::default());
    let listener = TcpListener::bind("127.0.0.1:80")?;
    if open {
        std::thread::spawn(||{
            let url = "http://localhost/";
            let mut cmd : Command;
            if cfg!(windows) {
                cmd = Command::new("cmd");
                cmd.args(&["/C", "start", "", url]);
            } else if cfg!(target_os = "macos") {
                cmd = Command::new("open");
                cmd.args(&[url]);
            } else if cfg!(target_os = "linux") {
                cmd = Command::new("xdg-open");
                cmd.args(&[url]);
            } else {
                // uhh... maybe?
                eprintln!("\u{001B}[33;1mwarning\u{001B}[37m:\u{001B}[0m `--open` not specifically implemented for this platform, attempting to use `xdg-open`");
                cmd = Command::new("xdg-open");
                cmd.args(&[url]);
            }
            cmd.status().unwrap();
        });
    }
    for stream in listener.incoming() {
        let stream = stream?;
        let common = Arc::clone(&common);
        std::thread::spawn(move || {
            if let Err(e) = handle_request(&common, &stream) {
                match e.kind() {
                    io::ErrorKind::TimedOut             => eprintln!("error handling connection: {:?}", e.kind()),
                    io::ErrorKind::ConnectionAborted    => eprintln!("error handling connection: {:?}", e.kind()),
                    _other                              => eprintln!("error handling connection: {e:?}"),
                }
            }
        });
    }
    Ok(())
}

fn handle_request(common: &Common, mut stream: &TcpStream) -> io::Result<()> {
    // https://developer.mozilla.org/en-US/docs/Web/HTTP/Resources_and_specifications
    // https://datatracker.ietf.org/doc/html/rfc7230    Hypertext Transfer Protocol (HTTP/1.1): Message Syntax and Routing
    // https://datatracker.ietf.org/doc/html/rfc7231    Hypertext Transfer Protocol (HTTP/1.1): Semantics and Content
    // https://datatracker.ietf.org/doc/html/rfc6585    Additional HTTP Status Codes

    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;

    let mut request = [0u8; MAX_REQUEST];
    let mut read = 0;

    loop {
        if read == request.len() { return write!(BufWriter::new(stream), "HTTP/1.1 413 Payload Too Large\r\n\r\n") }
        let prev_read = read;
        let this_read = stream.read(&mut request[read..])?;
        if this_read == 0 { return write!(stream, "HTTP/1.0 400 Bad Request\r\n\r\n") }
        read += this_read;

        let crlfcrlf_search_start = prev_read.saturating_sub(3);
        if let Some(crlfcrlf_index) = request[crlfcrlf_search_start..].windows(4).position(|w| w == b"\r\n\r\n") {
            let crlf_index = request.windows(2).position(|w| w == b"\r\n").unwrap();
            let request_line = &request[..crlf_index];
            let request_line = String::from_utf8_lossy(request_line);
            let request_line = &*request_line;
            eprintln!("request: {request_line:?}");

            let crlfcrlf_index = crlfcrlf_index + crlfcrlf_search_start;
            let header_lines = &request[crlf_index+2..(crlf_index+2).max(crlfcrlf_index + crlfcrlf_search_start)];
            let header_lines = String::from_utf8_lossy(header_lines);
            let header_lines = header_lines.split("\r\n");

            // FIXME: should handle "Expect: 100-continue" header?
            let mut content_length = None;
            for header_line in header_lines {
                if let Some((key, value)) = header_line.split_once(": ") {
                    match key {
                        "Content-Length" => {
                            let length : usize = match value.parse() {
                                Ok(n) => n,
                                Err(_) => return write!(stream, "HTTP/1.0 400 Bad Request\r\n\r\n"),
                            };
                            content_length = Some(length);
                        },
                        _unknown => {},
                    }
                } else {
                    //dbg!(header_line);
                }
            }

            if let Some((method, (url, version))) = request_line.split_once(" ").map(|(m, u_v)| (m, u_v.split_once(" ").unwrap_or((u_v, "")))) {
                let response_version = match version {
                    "HTTP/0.9"                      => return write!(BufWriter::new(stream), "HTTP/1.0 426 Upgrade Required\r\nUpgrade: HTTP/1.1, HTTP/1.0\r\n\r\n"),
                    "HTTP/1.0"                      => "HTTP/1.0",
                    v if v.starts_with("HTTP/1.")   => "HTTP/1.1",
                    v if v.starts_with("HTTP/")     => "HTTP/1.1",
                    _                               => return write!(BufWriter::new(stream), "HTTP/1.0 505 HTTP Version Not Supported\r\n\r\n"),
                };

                let cargo_bin_name = env!("CARGO_BIN_NAME");
                let mut w = BufWriter::new(stream);

                return match url {
                    "/" => {
                        let index_html = include_str!("index.html");
                        let index_html_len = index_html.len();

                        // We "MUST" have a Date: header if reliable system time is available - but I've chosen to skip it.
                        let headers = format!("Server: {cargo_bin_name}\r\nContent-Type: text/html; charset=UTF-8\r\nContent-Length: {index_html_len}\r\n");
                        match method {
                            "GET"   => write!(w, "{response_version} 200 OK\r\n{headers}\r\n{index_html}"),
                            "HEAD"  => write!(w, "{response_version} 200 OK\r\n{headers}\r\n"),
                            _       => write!(w, "{response_version} 405 Method Not Allowed\r\nAllow: GET, HEAD\r\n\r\n"),
                        }
                    },
                    "/chat" => {
                        // We "MUST" have a Date: header if reliable system time is available - but I've chosen to skip it.
                        let headers = format!("Server: {cargo_bin_name}\r\nCache-Control: no-store\r\nContent-Type: text/event-stream; charset=UTF-8\r\n");
                        match method {
                            "HEAD" => write!(w, "{response_version} 200 OK\r\n{headers}\r\n"),
                            "GET" => {
                                let (sender, receiver) = channel();
                                common.listeners.lock().unwrap().push(sender);
                                write!(w, "{response_version} 200 OK\r\n{headers}\r\n")?;
                                loop {
                                    match receiver.recv_timeout(SSE_TIMEOUT) {
                                        Ok(msg) => {
                                            write!(w, "{msg}")?;
                                            while let Ok(msg) = receiver.try_recv() {
                                                write!(w, "{msg}")?;
                                            }
                                            w.flush()?;
                                        },
                                        Err(RecvTimeoutError::Disconnected) => return Ok(()),
                                        Err(RecvTimeoutError::Timeout) => write!(w, "event: ping\ndata: ping\n\n")?,
                                    }
                                }
                            },
                            "POST" => {
                                let message_start = crlfcrlf_index + 4;
                                loop {
                                    let message_len = read - message_start;
                                    if message_len >= content_length.unwrap_or(!0) { break }
                                    let this_read = stream.read(&mut request[read..])?;
                                    if this_read == 0 { break }
                                    read += this_read;
                                }
                                // TODO: cap request length based on Content-Length ?
                                let message = &request[message_start..read];
                                let message = String::from_utf8_lossy(message).to_owned();
                                let message = message.lines().map(|line| format!("data: {line}\n")).collect::<Vec<_>>().join("");
                                let message = Arc::new(format!("{message}\n"));
                                common.listeners.lock().unwrap().retain(|l| l.send(message.clone()).is_ok());
                                write!(w, "{response_version} 204 No Content\r\nServer: {cargo_bin_name}\r\n\r\n")
                            },
                            _ => write!(w, "{response_version} 405 Method Not Allowed\r\nAllow: GET, HEAD, POST\r\n\r\n"),
                        }
                    },
                    _ => write!(w, "HTTP/1.0 404 Not Found\r\n\r\n"),
                }
            } else {
                return write!(stream, "HTTP/1.0 400 Bad Request\r\n\r\n");
            }
        }
    }
}
