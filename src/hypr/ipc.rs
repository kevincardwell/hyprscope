use anyhow::{anyhow, bail, Context, Result};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use super::model::{Client, Monitor, Workspace};

/// A handle to the running compositor instance.
#[derive(Clone, Debug)]
pub struct Hypr {
    dir: PathBuf,
}

/// One line from the event socket, split into `name` and the raw payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub name: String,
    pub data: String,
}

impl Hypr {
    /// Locate the instance from the environment (`HYPRLAND_INSTANCE_SIGNATURE`).
    pub fn from_env() -> Result<Self> {
        let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").context("HYPRLAND_INSTANCE_SIGNATURE is not set; run hyprscope inside a Hyprland session")?;
        let runtime = std::env::var("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(format!("/run/user/{}", unsafe { libc_getuid() })));
        let dir = runtime.join("hypr").join(&sig);
        if !dir.join(".socket.sock").exists() {
            bail!("no Hyprland socket at {}", dir.display());
        }
        Ok(Self { dir })
    }

    fn request_sock(&self) -> PathBuf {
        self.dir.join(".socket.sock")
    }

    fn event_sock(&self) -> PathBuf {
        self.dir.join(".socket2.sock")
    }

    /// Send one raw request (e.g. `j/clients`) and return the reply.
    pub async fn request(&self, cmd: &str) -> Result<String> {
        let mut stream = UnixStream::connect(self.request_sock())
            .await
            .with_context(|| format!("connect {}", self.request_sock().display()))?;
        stream.write_all(cmd.as_bytes()).await?;
        stream.shutdown().await.ok();
        let mut out = String::new();
        stream.read_to_string(&mut out).await?;
        Ok(out)
    }

    async fn request_json<T: serde::de::DeserializeOwned>(&self, cmd: &str) -> Result<T> {
        let raw = self.request(&format!("j/{cmd}")).await?;
        serde_json::from_str(&raw).with_context(|| format!("parse reply to {cmd}: {}", raw.chars().take(200).collect::<String>()))
    }

    pub async fn clients(&self) -> Result<Vec<Client>> {
        self.request_json("clients").await
    }

    pub async fn workspaces(&self) -> Result<Vec<Workspace>> {
        self.request_json("workspaces").await
    }

    pub async fn monitors(&self) -> Result<Vec<Monitor>> {
        self.request_json("monitors").await
    }

    /// Address of the focused window, if any.
    pub async fn active_window(&self) -> Result<Option<String>> {
        let raw = self.request("j/activewindow").await?;
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
        Ok(v.get("address").and_then(|a| a.as_str()).map(str::to_string))
    }

    pub async fn version(&self) -> Result<String> {
        let raw = self.request("j/version").await?;
        let v: serde_json::Value = serde_json::from_str(&raw)?;
        v.get("tag")
            .and_then(|t| t.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("no version tag in reply"))
    }

    /// Config errors the compositor itself reported while loading.
    pub async fn config_errors(&self) -> Result<Vec<String>> {
        let raw = self.request("configerrors").await?;
        Ok(raw.lines().filter(|l| !l.trim().is_empty()).map(str::to_string).collect())
    }

    pub async fn focus(&self, address: &str) -> Result<()> {
        // Since the Lua config era, `dispatch` takes Lua: the old
        // `focuswindow address:…` form is rejected.
        let reply = self.request(&format!("dispatch hl.dsp.focus({{ window = \"address:{address}\" }})")).await?;
        if reply.trim() != "ok" {
            bail!("focus: {}", reply.trim());
        }
        Ok(())
    }

    /// Stream compositor events into a channel until the socket closes.
    pub fn subscribe(&self) -> mpsc::Receiver<Event> {
        let (tx, rx) = mpsc::channel(256);
        let path = self.event_sock();
        tokio::spawn(async move {
            let Ok(stream) = UnixStream::connect(&path).await else { return };
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some((name, data)) = line.split_once(">>") {
                    if tx
                        .send(Event {
                            name: name.to_string(),
                            data: data.to_string(),
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        });
        rx
    }
}

unsafe fn libc_getuid() -> u32 {
    extern "C" {
        fn getuid() -> u32;
    }
    getuid()
}
