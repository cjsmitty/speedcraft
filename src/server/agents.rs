//! Registry of remote agents the UI host can control, persisted as JSON
//! next to the config file with 0600 permissions (it holds tokens).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RemoteAgent {
    pub id: String,
    pub name: String,
    /// Base URL, e.g. http://127.0.0.1:7777 (via an SSH tunnel) or http://10.0.0.5:7777
    pub url: String,
    /// Bearer token. Never returned to the browser.
    #[serde(default)]
    pub token: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct RemoteAgentView {
    pub id: String,
    pub name: String,
    pub url: String,
    pub has_token: bool,
}

impl From<&RemoteAgent> for RemoteAgentView {
    fn from(a: &RemoteAgent) -> Self {
        RemoteAgentView {
            id: a.id.clone(),
            name: a.name.clone(),
            url: a.url.clone(),
            has_token: !a.token.is_empty(),
        }
    }
}

pub struct AgentRegistry {
    path: PathBuf,
    agents: Mutex<Vec<RemoteAgent>>,
}

impl AgentRegistry {
    pub fn open(path: &Path) -> Result<AgentRegistry> {
        let agents = if path.exists() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
        } else {
            Vec::new()
        };
        Ok(AgentRegistry {
            path: path.to_path_buf(),
            agents: Mutex::new(agents),
        })
    }

    pub fn list(&self) -> Vec<RemoteAgentView> {
        self.agents
            .lock()
            .unwrap()
            .iter()
            .map(RemoteAgentView::from)
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<RemoteAgent> {
        self.agents
            .lock()
            .unwrap()
            .iter()
            .find(|a| a.id == id)
            .cloned()
    }

    pub fn add(&self, name: &str, url: &str, token: &str) -> Result<RemoteAgentView> {
        let name = name.trim();
        let url = url.trim().trim_end_matches('/');
        if name.is_empty() {
            bail!("name is required");
        }
        let parsed = url::Url::parse(url).context("url must look like http://host:port")?;
        if !matches!(parsed.scheme(), "http" | "https") {
            bail!("url must be http or https");
        }
        let id = slug(name);
        let mut agents = self.agents.lock().unwrap();
        if agents.iter().any(|a| a.id == id) {
            bail!("an agent named '{name}' already exists");
        }
        let a = RemoteAgent {
            id,
            name: name.to_string(),
            url: url.to_string(),
            token: token.trim().to_string(),
        };
        let view = RemoteAgentView::from(&a);
        agents.push(a);
        self.save(&agents)?;
        Ok(view)
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        let mut agents = self.agents.lock().unwrap();
        let before = agents.len();
        agents.retain(|a| a.id != id);
        if agents.len() == before {
            bail!("no agent '{id}'");
        }
        self.save(&agents)
    }

    fn save(&self, agents: &[RemoteAgent]) -> Result<()> {
        let text = serde_json::to_string_pretty(agents)?;
        std::fs::write(&self.path, text)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}

fn slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        format!("agent-{}", rand::random::<u16>())
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_list_remove_persist() {
        let dir = std::env::temp_dir().join(format!(
            "speedcraft-agents-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("agents.json");
        let r = AgentRegistry::open(&p).unwrap();
        let v = r.add("VM One", "http://10.0.0.5:7777/", "secret").unwrap();
        assert_eq!(v.id, "vm-one");
        assert_eq!(v.url, "http://10.0.0.5:7777");
        assert!(v.has_token);
        assert!(r.add("VM One", "http://x:1", "").is_err());
        assert!(r.add("bad", "ftp://x", "").is_err());
        assert!(r.add("bad2", "not a url", "").is_err());
        let r2 = AgentRegistry::open(&p).unwrap();
        assert_eq!(r2.list().len(), 1);
        assert_eq!(r2.get("vm-one").unwrap().token, "secret");
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("secret"), "token persisted on disk (0600)");
        r2.remove("vm-one").unwrap();
        assert!(r2.remove("vm-one").is_err());
        assert!(AgentRegistry::open(&p).unwrap().list().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
