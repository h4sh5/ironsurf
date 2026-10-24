//! Container registry (`containers.json`), including each container's
//! optional per-container User-Agent, the global userscript list
//! (`userscripts.json`), and the top-level [`Store`].

use ironsurf_common::{
    Container, ContainerId, Userscript, MAX_PROXY_NAME_CHARS, MAX_USER_AGENT_CHARS,
};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("invalid data in {path}: {source}")]
    Json {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("no such container: {0:?}")]
    NoSuchContainer(ContainerId),
    #[error("cannot delete the default container")]
    CannotDeleteDefault,
    #[error("container name must not be empty")]
    EmptyName,
    #[error("user agent too long: {length} chars (max {max})")]
    UserAgentTooLong { length: usize, max: usize },
    #[error("user agent must not contain control characters")]
    InvalidUserAgent,
    #[error("proxy name too long: {length} chars (max {max})")]
    ProxyNameTooLong { length: usize, max: usize },
    #[error("proxy name must not contain control characters")]
    InvalidProxyName,
    #[error("no such userscript: {0}")]
    NoSuchUserscript(u64),
    #[error("local storage quota exceeded for origin {origin} (max {max} bytes)")]
    QuotaExceeded { origin: String, max: usize },
    #[error("userscript name must not be empty")]
    EmptyUserscriptName,
    #[error("userscript name too long: {length} chars (max {max})")]
    UserscriptNameTooLong { length: usize, max: usize },
    #[error("userscript must have at least one URL pattern")]
    EmptyUserscriptPatterns,
    #[error("userscript code must not be empty")]
    EmptyUserscriptCode,
    #[error("userscript code too large: {size} bytes (max {max})")]
    UserscriptCodeTooLarge { size: usize, max: usize },
}

/// Upper bound on a userscript's JavaScript source (1 MiB).
pub const MAX_USERSCRIPT_CODE_BYTES: usize = 1024 * 1024;

/// Upper bound on a userscript's display name (in unicode chars).
pub const MAX_USERSCRIPT_NAME_CHARS: usize = 200;

impl StoreError {
    pub(crate) fn io(path: impl AsRef<Path>, source: io::Error) -> Self {
        StoreError::Io {
            path: path.as_ref().display().to_string(),
            source,
        }
    }
    pub(crate) fn json(path: impl AsRef<Path>, source: serde_json::Error) -> Self {
        StoreError::Json {
            path: path.as_ref().display().to_string(),
            source,
        }
    }
}

/// Open (and initialize if needed) the IronSurf data root.
pub struct Store {
    root: PathBuf,
    containers: Vec<Container>,
    /// Global userscript list (not per-container), from `userscripts.json`.
    userscripts: Vec<Userscript>,
    next_id: u32,
    default_id: ContainerId,
}

const DEFAULT_NAME: &str = "personal";
const DEFAULT_COLOR: &str = "#9d9ba4";

impl Store {
    /// Create root/registry/profiles dirs; seed the default `personal`
    /// container on first run.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let root = root.into();
        Self::open_with(root, true)
    }

    fn open_with(root: PathBuf, create: bool) -> Result<Self, StoreError> {
        let registry_path = root.join("containers.json");
        let mut containers: Vec<Container> = Vec::new();
        if registry_path.exists() {
            let text =
                fs::read_to_string(&registry_path).map_err(|e| StoreError::io(&registry_path, e))?;
            containers =
                serde_json::from_str(&text).map_err(|e| StoreError::json(&registry_path, e))?;
        }
        let default_id = containers
            .iter()
            .find(|c| c.is_default)
            .map(|c| c.id)
            .unwrap_or(ContainerId(1));
        // Missing userscripts.json = empty list (never seeded, unlike the
        // registry); a corrupt file is a Json error, same as containers.
        let userscript_path = root.join("userscripts.json");
        let mut userscripts: Vec<Userscript> = Vec::new();
        if userscript_path.exists() {
            let text = fs::read_to_string(&userscript_path)
                .map_err(|e| StoreError::io(&userscript_path, e))?;
            userscripts =
                serde_json::from_str(&text).map_err(|e| StoreError::json(&userscript_path, e))?;
        }
        let mut store = Store {
            root,
            containers,
            userscripts,
            next_id: 0,
            default_id,
        };
        if create && store.containers.is_empty() {
            // First run: seed the default `personal` container.
            let id = store.new_id();
            store.containers.push(Container {
                id,
                name: DEFAULT_NAME.to_string(),
                color: DEFAULT_COLOR.to_string(),
                is_default: true,
                user_agent: None,
                proxy: None,
                private: false,
            });
            store.default_id = id;
            store.save()?;
        } else if create && !store.containers.iter().any(|c| c.is_default) {
            // Registry without a default (hand-edited): promote the first.
            store.containers[0].is_default = true;
            store.default_id = store.containers[0].id;
            store.save()?;
        }
        if create {
            let profiles = store.root.join("profiles");
            fs::create_dir_all(&profiles).map_err(|e| StoreError::io(profiles, e))?;
            for c in &store.containers {
                // Best-effort: make sure every registered container has a dir.
                let _ = store.ensure_profile_dir(c.id);
            }
        }
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn containers(&self) -> &[Container] {
        &self.containers
    }

    pub fn container(&self, id: ContainerId) -> Option<&Container> {
        self.containers.iter().find(|c| c.id == id)
    }

    pub fn default_container_id(&self) -> ContainerId {
        self.default_id
    }

    /// Create a container (and its profile dir). Returns the new id.
    pub fn create_container(&mut self, name: &str, color: &str) -> Result<ContainerId, StoreError> {
        let name = sanitize_name(name)?;
        let color = normalize_color(color);
        let id = self.new_id();
        self.ensure_profile_dir(id)?;
        self.containers.push(Container {
            id,
            name,
            color,
            is_default: false,
            user_agent: None,
            proxy: None,
            private: false,
        });
        self.save()?;
        Ok(id)
    }

    pub fn rename_container(&mut self, id: ContainerId, name: &str) -> Result<(), StoreError> {
        let name = sanitize_name(name)?;
        let old_dir = self.profile_dir_path(id);
        let container = self
            .containers
            .iter_mut()
            .find(|c| c.id == id)
            .ok_or(StoreError::NoSuchContainer(id))?;
        container.name = name;
        self.save()?;
        // The profile dir is named `<slug>-<id>`: carry the data across.
        let new_dir = self.profile_dir_path(id);
        if old_dir != new_dir && old_dir.exists() {
            if let Some(parent) = new_dir.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::remove_dir_all(&new_dir);
            let _ = fs::rename(&old_dir, &new_dir);
        }
        self.ensure_profile_dir(id)
    }

    pub fn set_container_color(&mut self, id: ContainerId, color: &str) -> Result<(), StoreError> {
        let color = normalize_color(color);
        let container = self
            .containers
            .iter_mut()
            .find(|c| c.id == id)
            .ok_or(StoreError::NoSuchContainer(id))?;
        container.color = color;
        self.save()
    }

    /// Set the per-container User-Agent for `id` (persisted in
    /// `containers.json`, like [`Store::set_container_color`]).
    ///
    /// `Some(v)` is stored trimmed; an all-blank `v` stores `None` (empty
    /// input means the tab's built-in default). `None` stores `None`.
    /// Validation happens before any mutation — a rejected call leaves the
    /// in-memory container and `containers.json` untouched:
    /// control characters are [`StoreError::InvalidUserAgent`], more than
    /// [`MAX_USER_AGENT_CHARS`] chars is [`StoreError::UserAgentTooLong`].
    pub fn set_container_user_agent(
        &mut self,
        id: ContainerId,
        user_agent: Option<&str>,
    ) -> Result<(), StoreError> {
        // Validate first (mirrors `upsert_userscript`), then mutate + save.
        let user_agent = match user_agent {
            Some(v) => {
                if v.chars().any(|c| c.is_control()) {
                    return Err(StoreError::InvalidUserAgent);
                }
                let trimmed = v.trim();
                // Blank input = the container has no override.
                if trimmed.is_empty() {
                    None
                } else {
                    let length = trimmed.chars().count();
                    if length > MAX_USER_AGENT_CHARS {
                        return Err(StoreError::UserAgentTooLong {
                            length,
                            max: MAX_USER_AGENT_CHARS,
                        });
                    }
                    Some(trimmed.to_string())
                }
            }
            None => None,
        };
        let container = self
            .containers
            .iter_mut()
            .find(|c| c.id == id)
            .ok_or(StoreError::NoSuchContainer(id))?;
        container.user_agent = user_agent;
        self.save()
    }

    /// Set the per-container **proxy** choice for `id` (persisted in
    /// `containers.json`, like [`Store::set_container_user_agent`]).
    ///
    /// The stored value is the *name* of one of the user's global proxy
    /// definitions (the definitions themselves live in the shell's
    /// `ui.json` settings); the shell resolves it to a URL and ships it as
    /// [`ironsurf_common::ToTab::SetProxy`]. `Some(v)` is stored trimmed;
    /// an all-blank `v` stores `None`, and `None` stores `None` — both
    /// mean "direct connections". A name is not resolved here: deleting a
    /// definition must not rewrite every container, and a dangling name
    /// resolves to direct by the shell's rule. Validation happens before
    /// any mutation — a rejected call leaves the container and
    /// `containers.json` untouched: control characters are
    /// [`StoreError::InvalidProxyName`], more than
    /// [`MAX_PROXY_NAME_CHARS`] chars is [`StoreError::ProxyNameTooLong`].
    pub fn set_container_proxy(
        &mut self,
        id: ContainerId,
        name: Option<&str>,
    ) -> Result<(), StoreError> {
        let proxy = match name {
            Some(v) => {
                if v.chars().any(|c| c.is_control()) {
                    return Err(StoreError::InvalidProxyName);
                }
                let trimmed = v.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    let length = trimmed.chars().count();
                    if length > MAX_PROXY_NAME_CHARS {
                        return Err(StoreError::ProxyNameTooLong {
                            length,
                            max: MAX_PROXY_NAME_CHARS,
                        });
                    }
                    Some(trimmed.to_string())
                }
            }
            None => None,
        };
        let container = self
            .containers
            .iter_mut()
            .find(|c| c.id == id)
            .ok_or(StoreError::NoSuchContainer(id))?;
        container.proxy = proxy;
        self.save()
    }

    /// Set the per-container **private browsing** flag for `id` (persisted
    /// in `containers.json`, like [`Store::set_container_proxy`]).
    ///
    /// While set, the shell sends `DNT: 1` on that container's traffic (via
    /// [`ironsurf_common::ToTab::SetDoNotTrack`] and the `--private` spawn
    /// flag) and records **no** browsing-history entries for its tabs.
    /// The flag itself is a plain `bool` — nothing to validate; an unknown
    /// id is [`StoreError::NoSuchContainer`] and leaves the file untouched.
    pub fn set_container_private(&mut self, id: ContainerId, private: bool) -> Result<(), StoreError> {
        let container = self
            .containers
            .iter_mut()
            .find(|c| c.id == id)
            .ok_or(StoreError::NoSuchContainer(id))?;
        container.private = private;
        self.save()
    }

    /// Delete a container and remove its profile dir recursively.
    /// Refuses to delete the default container.
    pub fn delete_container(&mut self, id: ContainerId) -> Result<(), StoreError> {
        if id == self.default_id {
            return Err(StoreError::CannotDeleteDefault);
        }
        let pos = self
            .containers
            .iter()
            .position(|c| c.id == id)
            .ok_or(StoreError::NoSuchContainer(id))?;
        // Resolve the dir while the name is still in the registry.
        let dir = self.profile_dir_path(id);
        self.containers.remove(pos);
        self.save()?;
        if dir.exists() {
            fs::remove_dir_all(&dir).map_err(|e| StoreError::io(dir, e))?;
        }
        Ok(())
    }

    /// Profile dir for `id`; guarantees the directory (and the reserved
    /// subdirs) exist.
    pub fn profile_dir(&self, id: ContainerId) -> PathBuf {
        let dir = self.profile_dir_path(id);
        let _ = self.ensure_profile_dir(id);
        dir
    }

    fn profile_dir_path(&self, id: ContainerId) -> PathBuf {
        let name = self
            .container(id)
            .map(|c| c.name.as_str())
            .unwrap_or("unknown");
        self.root
            .join("profiles")
            .join(format!("{}-{}", slug(name), id.0))
    }

    fn ensure_profile_dir(&self, id: ContainerId) -> Result<(), StoreError> {
        let dir = self.profile_dir_path(id);
        fs::create_dir_all(&dir).map_err(|e| StoreError::io(&dir, e))?;
        for sub in ["net-cache", "local-storage"] {
            let s = dir.join(sub);
            fs::create_dir_all(&s).map_err(|e| StoreError::io(&s, e))?;
        }
        Ok(())
    }

    fn new_id(&mut self) -> ContainerId {
        self.next_id += 1;
        while self.containers.iter().any(|c| c.id.0 == self.next_id) {
            self.next_id += 1;
        }
        ContainerId(self.next_id)
    }

    /// Atomically rewrite `containers.json` (temp file + rename).
    fn save(&self) -> Result<(), StoreError> {
        write_atomic(&self.root.join("containers.json"), &self.containers)
    }

    // ---- Userscripts (global, `userscripts.json`) ----

    /// Atomically rewrite `userscripts.json` (temp file + rename).
    fn save_userscripts(&self) -> Result<(), StoreError> {
        write_atomic(&self.root.join("userscripts.json"), &self.userscripts)
    }

    /// All stored userscripts, in storage order (loaded at `open`).
    pub fn userscripts(&self) -> Vec<Userscript> {
        self.userscripts.clone()
    }

    /// The id to hand out for a new userscript: max existing id + 1
    /// (`1` when the list is empty). Ids are caller-supplied, so this only
    /// needs to keep climbing across reloads.
    pub fn next_userscript_id(&self) -> u64 {
        self.userscripts
            .iter()
            .map(|s| s.id)
            .max()
            .map(|max| max + 1)
            .unwrap_or(1)
    }

    /// Insert `script` (by caller-supplied id) or replace the stored script
    /// with the same id. Validates name/patterns/code (see [`StoreError`]
    /// userscript variants); the name is stored trimmed.
    pub fn upsert_userscript(&mut self, script: Userscript) -> Result<(), StoreError> {
        let name = script.name.trim().to_string();
        if name.is_empty() {
            return Err(StoreError::EmptyUserscriptName);
        }
        let name_chars = name.chars().count();
        if name_chars > MAX_USERSCRIPT_NAME_CHARS {
            return Err(StoreError::UserscriptNameTooLong {
                length: name_chars,
                max: MAX_USERSCRIPT_NAME_CHARS,
            });
        }
        if script.patterns.is_empty() {
            return Err(StoreError::EmptyUserscriptPatterns);
        }
        if script.code.is_empty() {
            return Err(StoreError::EmptyUserscriptCode);
        }
        if script.code.len() > MAX_USERSCRIPT_CODE_BYTES {
            return Err(StoreError::UserscriptCodeTooLarge {
                size: script.code.len(),
                max: MAX_USERSCRIPT_CODE_BYTES,
            });
        }
        let script = Userscript { name, ..script };
        match self.userscripts.iter_mut().find(|s| s.id == script.id) {
            Some(existing) => *existing = script,
            None => self.userscripts.push(script),
        }
        self.save_userscripts()
    }

    /// Enable/disable the userscript with `id`. Unknown ids are
    /// [`StoreError::NoSuchUserscript`] (mirrors `NoSuchContainer`).
    pub fn set_userscript_enabled(&mut self, id: u64, enabled: bool) -> Result<(), StoreError> {
        let script = self
            .userscripts
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or(StoreError::NoSuchUserscript(id))?;
        script.enabled = enabled;
        self.save_userscripts()
    }

    /// Remove the userscript with `id`; unknown ids are
    /// [`StoreError::NoSuchUserscript`].
    pub fn delete_userscript(&mut self, id: u64) -> Result<(), StoreError> {
        let pos = self
            .userscripts
            .iter()
            .position(|s| s.id == id)
            .ok_or(StoreError::NoSuchUserscript(id))?;
        self.userscripts.remove(pos);
        self.save_userscripts()
    }
}

/// `[{...}]` written atomically via temp-file + rename.
pub(crate) fn write_atomic<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), StoreError> {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let json = serde_json::to_string_pretty(value).map_err(|e| StoreError::json(path, e))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json.as_bytes()).map_err(|e| StoreError::io(&tmp, e))?;
    fs::rename(&tmp, path).map_err(|e| StoreError::io(path, e))?;
    Ok(())
}

/// Strip/refuse junk: trim, reject empty, strip control/path characters.
fn sanitize_name(name: &str) -> Result<String, StoreError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(StoreError::EmptyName);
    }
    Ok(trimed_chars(trimmed))
}

fn trimed_chars(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control())
        .filter(|c| *c != '/' && *c != '\\' && *c != '\0')
        .collect::<String>()
        .trim()
        .to_string()
}

/// Lowercase hex `#rrggbb`; anything else falls back to the neutral gray.
fn normalize_color(color: &str) -> String {
    let c = color.trim().to_ascii_lowercase();
    let hex = c.strip_prefix('#').unwrap_or(&c);
    if hex.len() == 6 && hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
        format!("#{hex}")
    } else {
        DEFAULT_COLOR.to_string()
    }
}

/// Filesystem-safe slug: `[a-z0-9]+` joined with `-`, `unnamed` fallback.
pub(crate) fn slug(name: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true; // suppress leading dashes
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if matches!(c, '-' | '_' | ' ') && !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "unnamed".to_string()
    } else {
        out
    }
}
