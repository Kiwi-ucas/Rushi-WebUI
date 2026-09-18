use std::path::PathBuf;

/// Configuration for the rushi-web server.
///
/// In production this is populated from `config.toml` (`[web]` section)
/// or CLI flags. The fields below mirror what the server needs at
/// runtime; the kernel config.toml is read separately by the loop
/// process, not by this server.
#[derive(Debug, Clone)]
pub struct WebConfig {
    /// Bind address.
    pub host: String,
    /// Bind port.
    pub port: u16,
    /// Root directory that holds one sub-directory per session.
    pub sessions_root: PathBuf,
    /// Command to spawn for the agent loop (e.g. `["rushi", "run"]`).
    /// The session name is appended as the last argument.
    pub loop_cmd: Vec<String>,
    /// Directories to scan for UI-extension manifests (`ext.toml`).
    pub ext_dirs: Vec<PathBuf>,
}
