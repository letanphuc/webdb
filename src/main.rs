use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use serde_json::json;

#[derive(Parser)]
#[command(name = "webdb", about = "WebOS device bridge over HTTP")]
struct Cli {
    #[arg(long, env = "WEBDB_HOST", default_value = "192.168.50.17")]
    host: String,

    #[arg(long, env = "WEBDB_PORT", default_value_t = 8080)]
    port: u16,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Shell {
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
    Push {
        local: PathBuf,
        remote: String,
    },
    Log {
        #[arg(short, long)]
        follow: bool,
    },
    Ota {
        image: PathBuf,
    },
}

#[derive(Deserialize)]
struct ShellResponse {
    rc: i32,
    output: String,
}

#[derive(Deserialize)]
struct LogResponse {
    messages: String,
}

#[derive(Deserialize)]
struct OtaResponse {
    ok: Option<bool>,
    bytes: Option<u64>,
    reboot_ms: Option<u32>,
    error: Option<i32>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let base = format!("http://{}:{}", cli.host, cli.port);

    match cli.command {
        Command::Shell { command } if command.is_empty() => attach_shell(&base),
        Command::Shell { command } => run_shell_command(&base, &command.join(" ")),
        Command::Push { local, remote } => push_file(&base, local, &remote),
        Command::Log { follow } => run_log(&base, follow),
        Command::Ota { image } => run_ota(&base, &image),
    }
}

fn run_shell_command(base: &str, command: &str) -> Result<()> {
    let response = post_shell(base, json!({ "cmd": command }))?;
    print!("{}", strip_ansi(&response.output));
    io::stdout().flush()?;

    if response.rc != 0 {
        bail!("remote shell command failed: {}", response.rc);
    }

    Ok(())
}

fn attach_shell(base: &str) -> Result<()> {
    let response = post_shell(base, json!({}))?;
    print!("{}", strip_ansi(&response.output));
    io::stdout().flush()?;

    let stdin = io::stdin();
    loop {
        print!("webdb> ");
        io::stdout().flush()?;

        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            break;
        }
        if line.trim() == "exit" || line.trim() == "quit" {
            break;
        }

        let response = post_shell(base, json!({ "input": line }))?;
        print!("{}", strip_ansi(&response.output));
        io::stdout().flush()?;
    }

    Ok(())
}

fn post_shell(base: &str, body: serde_json::Value) -> Result<ShellResponse> {
    let url = format!("{base}/shell");
    let response = ureq::post(&url)
        .set("Content-Type", "application/json")
        .send_json(body)
        .with_context(|| format!("POST {url}"))?;

    response
        .into_json()
        .with_context(|| format!("decode response from {url}"))
}

fn push_file(base: &str, local: PathBuf, remote: &str) -> Result<()> {
    let data = fs::read(&local).with_context(|| format!("read {}", local.display()))?;
    let url = format!("{base}/pushbin");
    let response = ureq::post(&url)
        .set("Content-Type", "application/octet-stream")
        .set("X-Webos-Path", remote)
        .send_bytes(&data)
        .with_context(|| format!("POST {url}"))?;

    let status = response.status();
    let body = response.into_string().unwrap_or_default();
    if !(200..300).contains(&status) {
        bail!("push failed: HTTP {status}: {body}");
    }

    print!("{body}");
    Ok(())
}

fn run_log(base: &str, follow: bool) -> Result<()> {
    loop {
        let messages = fetch_logs(base, follow)?;
        if !messages.is_empty() {
            print!("{}", messages);
            io::stdout().flush()?;
        }
        if !follow {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Ok(())
}

fn fetch_logs(base: &str, clear: bool) -> Result<String> {
    let url = format!("{base}/log");
    let response = ureq::post(&url)
        .set("Content-Type", "application/json")
        .send_json(json!({ "clear": clear }))
        .with_context(|| format!("POST {url}"))?;

    let body: LogResponse = response
        .into_json()
        .with_context(|| format!("decode response from {url}"))?;

    Ok(body.messages)
}

struct ProgressReader {
    inner: fs::File,
    total: u64,
    read: u64,
}

impl Read for ProgressReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n == 0 {
            let _ = io::stderr().write_all(b"\n");
            return Ok(0);
        }
        self.read += n as u64;
        let bar_width = 32;
        let filled = (self.read as usize * bar_width / self.total as usize).min(bar_width);
        let bar: String = (0..bar_width)
            .map(|i| if i < filled { '=' } else { ' ' })
            .collect();
        let pct = self.read * 100 / self.total;
        let _ = io::stderr().write_fmt(format_args!(
            "\r[{bar}] {pct:>3}% {}/{}",
            self.read, self.total
        ));
        let _ = io::stderr().flush();
        Ok(n)
    }
}

fn run_ota(base: &str, image: &PathBuf) -> Result<()> {
    let file = fs::File::open(image)
        .with_context(|| format!("open {}", image.display()))?;
    let total = file.metadata()
        .with_context(|| format!("stat {}", image.display()))?
        .len();

    let reader = ProgressReader { inner: file, total, read: 0 };
    let url = format!("{base}/ota");
    let response = ureq::post(&url)
        .set("Content-Type", "application/octet-stream")
        .set("Content-Length", &total.to_string())
        .send(reader)
        .with_context(|| format!("POST {url}"))?;

    let status = response.status();
    let body: OtaResponse = response
        .into_json()
        .with_context(|| format!("decode response from {url}"))?;

    if !(200..300).contains(&status) {
        bail!("OTA failed: HTTP {status}: error {}", body.error.unwrap_or(-1));
    }

    if body.ok == Some(true) {
        println!(
            "OTA complete: {} bytes written, rebooting in {} ms",
            body.bytes.unwrap_or(0),
            body.reboot_ms.unwrap_or(0)
        );
    } else {
        bail!("OTA failed: error {}", body.error.unwrap_or(-1));
    }

    Ok(())
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();

    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            out.push(ch);
            continue;
        }

        if chars.next() != Some('[') {
            continue;
        }

        for next in chars.by_ref() {
            if next.is_ascii_alphabetic() {
                break;
            }
        }
    }

    out
}
