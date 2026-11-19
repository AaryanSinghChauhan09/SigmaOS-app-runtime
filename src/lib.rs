// SPDX-License-Identifier: MIT OR Apache-2.0
//! SigmaOS-app-runtime — Sandboxed application runtime.
//!
//! Capability-based permission model, resource limits (CPU, memory, disk),
//! D-Bus activation descriptors, process lifecycle management, and app manifest
//! parsing. Inspired by Flatpak/Bubblewrap sandboxing and Omarchy's container
//! isolation approach.
//!
//! Principles: least privilege, fail-fast, deterministic lifecycle, zero `unsafe`.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

// ── Errors ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeError {
    EmptyAppId,
    EmptyExec,
    InvalidMemoryLimit(String),
    InvalidCpuLimit(String),
    DuplicateCapability(String),
    AppNotFound(String),
    AppAlreadyRunning(String),
    AppNotRunning(String),
    PermissionDenied { app: String, cap: String },
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyAppId => write!(f, "app id must not be empty"),
            Self::EmptyExec => write!(f, "exec command must not be empty"),
            Self::InvalidMemoryLimit(m) => write!(f, "invalid memory limit: {m}"),
            Self::InvalidCpuLimit(m) => write!(f, "invalid CPU limit: {m}"),
            Self::DuplicateCapability(c) => write!(f, "duplicate capability: {c}"),
            Self::AppNotFound(id) => write!(f, "app not found: {id}"),
            Self::AppAlreadyRunning(id) => write!(f, "app already running: {id}"),
            Self::AppNotRunning(id) => write!(f, "app not running: {id}"),
            Self::PermissionDenied { app, cap } => write!(f, "app {app} lacks capability: {cap}"),
        }
    }
}

impl std::error::Error for RuntimeError {}

// ── Capabilities ────────────────────────────────────────────────────────

/// Sandbox capabilities (least-privilege grants).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Capability {
    /// Access to the filesystem (scoped to sandbox).
    Filesystem,
    /// Network access.
    Network,
    /// Access to display server (Wayland/X11).
    Display,
    /// Audio playback/capture.
    Audio,
    /// D-Bus session bus access.
    DBusSession,
    /// D-Bus system bus access.
    DBusSystem,
    /// GPU/hardware acceleration.
    Gpu,
    /// Camera device access.
    Camera,
    /// USB device access.
    Usb,
    /// Custom capability.
    Custom(String),
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Filesystem => write!(f, "filesystem"),
            Self::Network => write!(f, "network"),
            Self::Display => write!(f, "display"),
            Self::Audio => write!(f, "audio"),
            Self::DBusSession => write!(f, "dbus-session"),
            Self::DBusSystem => write!(f, "dbus-system"),
            Self::Gpu => write!(f, "gpu"),
            Self::Camera => write!(f, "camera"),
            Self::Usb => write!(f, "usb"),
            Self::Custom(s) => write!(f, "custom:{s}"),
        }
    }
}

// ── Resource Limits ─────────────────────────────────────────────────────

/// Resource limits for a sandboxed app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceLimits {
    /// Memory limit in megabytes.
    pub memory_mb: u64,
    /// CPU quota as percentage (1–400 for multi-core).
    pub cpu_percent: u32,
    /// Disk quota in megabytes.
    pub disk_mb: u64,
    /// Max open file descriptors.
    pub max_fds: u32,
    /// Max child processes.
    pub max_pids: u32,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            memory_mb: 512,
            cpu_percent: 100,
            disk_mb: 1024,
            max_fds: 1024,
            max_pids: 64,
        }
    }
}

impl ResourceLimits {
    pub fn validate(&self) -> Result<(), RuntimeError> {
        if self.memory_mb == 0 {
            return Err(RuntimeError::InvalidMemoryLimit("must be > 0".into()));
        }
        if self.cpu_percent == 0 || self.cpu_percent > 400 {
            return Err(RuntimeError::InvalidCpuLimit(format!("{}% not in 1..=400", self.cpu_percent)));
        }
        Ok(())
    }
}

// ── App Manifest ────────────────────────────────────────────────────────

/// Application manifest describing a sandboxed app.
#[derive(Debug, Clone)]
pub struct AppManifest {
    /// Unique application identifier (reverse-domain style).
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Exec command.
    pub exec: String,
    /// D-Bus activation bus name (optional).
    pub dbus_name: Option<String>,
    /// Requested capabilities.
    pub capabilities: BTreeSet<Capability>,
    /// Resource limits.
    pub limits: ResourceLimits,
    /// Environment variables to inject.
    pub env: BTreeMap<String, String>,
}

impl AppManifest {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        exec: impl Into<String>,
    ) -> Result<Self, RuntimeError> {
        let id = id.into();
        let exec = exec.into();
        if id.trim().is_empty() { return Err(RuntimeError::EmptyAppId); }
        if exec.trim().is_empty() { return Err(RuntimeError::EmptyExec); }
        Ok(Self {
            id,
            name: name.into(),
            exec,
            dbus_name: None,
            capabilities: BTreeSet::new(),
            limits: ResourceLimits::default(),
            env: BTreeMap::new(),
        })
    }

    /// Grant a capability to this app.
    pub fn grant(&mut self, cap: Capability) -> Result<(), RuntimeError> {
        if !self.capabilities.insert(cap.clone()) {
            return Err(RuntimeError::DuplicateCapability(cap.to_string()));
        }
        Ok(())
    }

    /// Check if the app has a specific capability.
    pub fn has_capability(&self, cap: &Capability) -> bool {
        self.capabilities.contains(cap)
    }
}

// ── Process State ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    Stopped,
    Starting,
    Running,
    Paused,
    Stopping,
    Crashed,
}

// ── Runtime Manager ─────────────────────────────────────────────────────

/// Manages app lifecycles in the sandbox.
#[derive(Debug, Default)]
pub struct RuntimeManager {
    manifests: BTreeMap<String, AppManifest>,
    states: BTreeMap<String, ProcessState>,
}

impl RuntimeManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an app manifest.
    pub fn register(&mut self, manifest: AppManifest) -> Result<(), RuntimeError> {
        manifest.limits.validate()?;
        self.states.insert(manifest.id.clone(), ProcessState::Stopped);
        self.manifests.insert(manifest.id.clone(), manifest);
        Ok(())
    }

    /// Start an app by id.
    pub fn start(&mut self, id: &str) -> Result<(), RuntimeError> {
        let state = self.states.get(id).ok_or_else(|| RuntimeError::AppNotFound(id.into()))?;
        if *state == ProcessState::Running || *state == ProcessState::Starting {
            return Err(RuntimeError::AppAlreadyRunning(id.into()));
        }
        self.states.insert(id.into(), ProcessState::Running);
        Ok(())
    }

    /// Stop an app by id.
    pub fn stop(&mut self, id: &str) -> Result<(), RuntimeError> {
        let state = self.states.get(id).ok_or_else(|| RuntimeError::AppNotFound(id.into()))?;
        if *state == ProcessState::Stopped {
            return Err(RuntimeError::AppNotRunning(id.into()));
        }
        self.states.insert(id.into(), ProcessState::Stopped);
        Ok(())
    }

    /// Get current state of an app.
    pub fn state(&self, id: &str) -> Option<ProcessState> {
        self.states.get(id).copied()
    }

    /// Check if an app has permission for an action.
    pub fn check_permission(&self, id: &str, cap: &Capability) -> Result<(), RuntimeError> {
        let manifest = self.manifests.get(id)
            .ok_or_else(|| RuntimeError::AppNotFound(id.into()))?;
        if !manifest.has_capability(cap) {
            return Err(RuntimeError::PermissionDenied { app: id.into(), cap: cap.to_string() });
        }
        Ok(())
    }

    /// List all registered app ids.
    pub fn list_apps(&self) -> Vec<&str> {
        self.manifests.keys().map(|s| s.as_str()).collect()
    }

    /// Count of running apps.
    pub fn running_count(&self) -> usize {
        self.states.values().filter(|s| **s == ProcessState::Running).count()
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manifest_creation() {
        let m = AppManifest::new("org.sigma.test", "Test App", "/usr/bin/test").unwrap();
        assert_eq!(m.id, "org.sigma.test");
        assert!(m.capabilities.is_empty());
    }

    #[test]
    fn test_empty_id_rejected() {
        assert_eq!(AppManifest::new("", "X", "cmd").unwrap_err(), RuntimeError::EmptyAppId);
    }

    #[test]
    fn test_empty_exec_rejected() {
        assert_eq!(AppManifest::new("x", "X", "").unwrap_err(), RuntimeError::EmptyExec);
    }

    #[test]
    fn test_capability_grant() {
        let mut m = AppManifest::new("x", "X", "cmd").unwrap();
        m.grant(Capability::Network).unwrap();
        assert!(m.has_capability(&Capability::Network));
        assert!(!m.has_capability(&Capability::Camera));
    }

    #[test]
    fn test_duplicate_capability_rejected() {
        let mut m = AppManifest::new("x", "X", "cmd").unwrap();
        m.grant(Capability::Audio).unwrap();
        assert!(m.grant(Capability::Audio).is_err());
    }

    #[test]
    fn test_resource_limits_validation() {
        let mut lim = ResourceLimits::default();
        assert!(lim.validate().is_ok());
        lim.memory_mb = 0;
        assert!(lim.validate().is_err());
    }

    #[test]
    fn test_lifecycle_start_stop() {
        let mut mgr = RuntimeManager::new();
        let m = AppManifest::new("app1", "App 1", "app1").unwrap();
        mgr.register(m).unwrap();
        assert_eq!(mgr.state("app1"), Some(ProcessState::Stopped));

        mgr.start("app1").unwrap();
        assert_eq!(mgr.state("app1"), Some(ProcessState::Running));
        assert_eq!(mgr.running_count(), 1);

        mgr.stop("app1").unwrap();
        assert_eq!(mgr.state("app1"), Some(ProcessState::Stopped));
    }

    #[test]
    fn test_double_start_rejected() {
        let mut mgr = RuntimeManager::new();
        mgr.register(AppManifest::new("app1", "A", "a").unwrap()).unwrap();
        mgr.start("app1").unwrap();
        assert!(mgr.start("app1").is_err());
    }

    #[test]
    fn test_stop_already_stopped() {
        let mut mgr = RuntimeManager::new();
        mgr.register(AppManifest::new("app1", "A", "a").unwrap()).unwrap();
        assert!(mgr.stop("app1").is_err());
    }

    #[test]
    fn test_permission_check() {
        let mut mgr = RuntimeManager::new();
        let mut m = AppManifest::new("app1", "A", "a").unwrap();
        m.grant(Capability::Display).unwrap();
        mgr.register(m).unwrap();

        assert!(mgr.check_permission("app1", &Capability::Display).is_ok());
        assert!(mgr.check_permission("app1", &Capability::Network).is_err());
    }

    #[test]
    fn test_unknown_app() {
        let mgr = RuntimeManager::new();
        assert!(mgr.state("nope").is_none());
        assert!(mgr.check_permission("nope", &Capability::Network).is_err());
    }

    #[test]
    fn test_list_apps() {
        let mut mgr = RuntimeManager::new();
        mgr.register(AppManifest::new("b", "B", "b").unwrap()).unwrap();
        mgr.register(AppManifest::new("a", "A", "a").unwrap()).unwrap();
        let apps = mgr.list_apps();
        assert_eq!(apps, vec!["a", "b"]); // BTreeMap = sorted
    }
}
