//! Start the local model when the cloud endpoint is unavailable.
//!
//! On this machine Ollama runs inside WSL, so "start the fallback" means
//! `wsl.exe` + `ollama serve` + one empty generation to pull the weights into
//! memory. Only two strings reach the shell: the port parsed out of the endpoint
//! and the model name after validation. Nothing the model wrote is ever
//! interpolated, so this cannot become a command injection.

use std::net::{TcpStream, ToSocketAddrs};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const START_TIMEOUT: Duration = Duration::from_secs(30);
const LOAD_TIMEOUT: Duration = Duration::from_secs(90);

/// Make sure something answers on the endpoint, starting WSL and Ollama if not.
pub fn ensure(endpoint: &str, model: &str) {
    let Some(address) = address_of(endpoint) else {
        tracing::warn!("fallback endpoint {endpoint} is not loopback; not starting anything");
        return;
    };

    // Already serving: Ollama loads the model on the first request by itself.
    if reachable(&address) {
        return;
    }

    if model.is_empty()
        || model
            .chars()
            .any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '-')))
    {
        tracing::warn!("refusing to start Ollama for an unexpected model name: {model}");
        return;
    }

    tracing::info!("starting Ollama in WSL for {model}");
    if !start_server(&address) || !wait_until(&address, START_TIMEOUT) {
        return;
    }
    load_model(model, &address);
}

/// `host:port` for a loopback endpoint, `None` for anything else.
fn address_of(endpoint: &str) -> Option<String> {
    let authority = endpoint.split_once("://")?.1.split('/').next()?;
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port.parse::<u16>().ok()?),
        None => (authority, 11434),
    };
    let host = host.trim_matches(|c| c == '[' || c == ']');
    if !matches!(host, "localhost" | "127.0.0.1" | "::1") {
        return None;
    }
    Some(format!("{host}:{port}"))
}

fn reachable(address: &str) -> bool {
    let Ok(addrs) = address.to_socket_addrs() else {
        return false;
    };
    addrs.into_iter().any(|addr| {
        TcpStream::connect_timeout(&addr, Duration::from_millis(600)).is_ok()
    })
}

fn wait_until(address: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if reachable(address) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    tracing::warn!("Ollama did not answer within {}s", timeout.as_secs());
    false
}

/// Starts `ollama serve` in the foreground of one `wsl.exe` and leaves it running.
///
/// WSL kills everything a `wsl.exe` invocation started once that process exits,
/// so `nohup ... &` and friends do not survive here; the server stays up only
/// while this child is alive. Dropping the handle does not stop it, and it goes
/// away when Windows tears the process down - which is the behaviour we want for
/// a service the app itself started.
fn start_server(address: &str) -> bool {
    let log = std::env::temp_dir().join("jarvis-ollama.log");
    let file = match std::fs::File::create(&log) {
        Ok(file) => file,
        Err(error) => {
            tracing::warn!("could not write {}: {error}", log.display());
            return false;
        }
    };
    let (out, err) = match (file.try_clone(), file) {
        (Ok(out), err) => (out, err),
        (Err(error), _) => {
            tracing::warn!("could not open the Ollama log twice: {error}");
            return false;
        }
    };

    // Serve on the endpoint the user configured, not on Ollama's default, so a
    // fallback pointing at another port still works.
    let script = format!("OLLAMA_HOST={address} ollama serve");
    match Command::new("wsl.exe")
        .args(["-e", "bash", "-lc", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
    {
        Ok(_) => true,
        Err(error) => {
            tracing::warn!("could not run wsl.exe: {error}. Is WSL installed?");
            false
        }
    }
}

/// One short generation loads the weights and keeps them for the real request.
/// A prompt is passed on purpose: `ollama run model` with no prompt opens its
/// REPL and would hang. The timeout stops a model that is missing (and would
/// ask to be pulled) from stalling the turn forever.
fn load_model(model: &str, address: &str) {
    let script = format!("OLLAMA_HOST={address} ollama run '{model}' 'hi' >/dev/null 2>&1");
    let Ok(mut child) = Command::new("wsl.exe")
        .args(["-e", "bash", "-lc", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        tracing::warn!("could not load {model} in WSL");
        return;
    };

    let deadline = Instant::now() + LOAD_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(250));
            }
            _ => {
                let _ = child.kill();
                tracing::warn!("loading {model} took longer than {}s", LOAD_TIMEOUT.as_secs());
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_endpoints_are_ours_to_start() {
        assert_eq!(
            address_of("http://localhost:11434/v1").as_deref(),
            Some("localhost:11434")
        );
        assert_eq!(
            address_of("http://127.0.0.1:11434/v1").as_deref(),
            Some("127.0.0.1:11434")
        );
        assert_eq!(address_of("http://[::1]:11434/v1").as_deref(), Some("::1:11434"));
        assert_eq!(address_of("http://ollama.lan:11434/v1"), None);
        assert_eq!(address_of("https://api.openai.com/v1"), None);
    }
}
