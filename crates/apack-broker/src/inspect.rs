//! Live broker inspection (`apack-terminal inspect [--socket <path>] [--json]`).

use apack_core::auth::AuthConfig;
use apack_proto::messages::SessionSummary;
use apack_proto::types::MetricsSnapshot;
use serde::Serialize;

use crate::client::{BrokerClient, ClientError};

/// Combined diagnostics report returned by [`collect_report`].
#[derive(Debug, Clone, Serialize)]
pub struct InspectReport {
    /// Active sessions.
    pub sessions: Vec<InspectSession>,
    /// Broker-wide telemetry snapshot.
    pub metrics: InspectMetrics,
}

/// Serializable summary of one session for `inspect --json`.
#[derive(Debug, Clone, Serialize)]
pub struct InspectSession {
    /// Session ID.
    pub id: u64,
    /// Grid columns.
    pub cols: u16,
    /// Grid rows.
    pub rows: u16,
    /// Whether the process is running.
    pub alive: bool,
    /// Whether the session is kept across lease release.
    pub kept: bool,
    /// Whether this is a read-only rewound recording.
    pub rewound: bool,
    /// Terminal title.
    pub title: String,
    /// Last reported working directory.
    pub cwd: Option<String>,
    /// Leader PID, if live.
    pub pid: Option<i32>,
    /// Foreground process name, if available.
    pub foreground: Option<String>,
    /// Creation timestamp (ms since Unix epoch).
    pub created_ms: u64,
}

/// Serializable broker telemetry snapshot for `inspect --json`.
#[derive(Debug, Clone, Serialize)]
pub struct InspectMetrics {
    /// Number of live sessions.
    pub live_sessions: u32,
    /// Number of open rewound recording sessions.
    pub rewound_sessions: u32,
    /// Number of active client connections.
    pub connections: u32,
    /// Total scrollback bytes across all sessions.
    pub scrollback_bytes: u64,
    /// Deduplicated image store footprint in bytes.
    pub image_store_bytes: u64,
    /// Cumulative PTY bytes parsed.
    pub total_pty_bytes: u64,
}

/// Builds an [`InspectReport`] from wire [`SessionSummary`]s and [`MetricsSnapshot`].
pub fn build_report(sessions: &[SessionSummary], metrics: MetricsSnapshot) -> InspectReport {
    InspectReport {
        sessions: sessions
            .iter()
            .map(|s| InspectSession {
                id: s.id.get(),
                cols: s.size.cols.get(),
                rows: s.size.rows.get(),
                alive: s.alive,
                kept: s.kept,
                rewound: s.rewound,
                title: s.title.clone(),
                cwd: s.cwd.clone(),
                pid: s.pid.map(|p| p.pid()),
                foreground: s.foreground.as_ref().and_then(|f| f.name.clone()),
                created_ms: s.created_ms,
            })
            .collect(),
        metrics: InspectMetrics {
            live_sessions: metrics.live_sessions,
            rewound_sessions: metrics.rewound_sessions,
            connections: metrics.connections,
            scrollback_bytes: metrics.scrollback_bytes,
            image_store_bytes: metrics.image_store_bytes,
            total_pty_bytes: metrics.total_pty_bytes,
        },
    }
}

/// Connects to the broker at `socket_path` and collects a live [`InspectReport`].
pub fn collect_report(socket_path: &str, auth: &AuthConfig) -> Result<InspectReport, ClientError> {
    let mut client = BrokerClient::connect(socket_path, auth, "apack-inspect")?;
    let sessions = client.list_sessions()?;
    let metrics = client.metrics()?;
    let _ = client.send_fin();
    Ok(build_report(&sessions, metrics))
}

/// Formats an [`InspectReport`] as either pretty JSON (with trailing newline) or human-readable plain text.
pub fn format_report(report: &InspectReport, json: bool) -> String {
    if json {
        let mut s = serde_json::to_string_pretty(report).expect("InspectReport is serializable");
        s.push('\n');
        return s;
    }
    let mut out = format!(
        "sessions: {} live / {} rewound  connections: {}  scrollback: {} B  images: {} B  pty_bytes: {}\n",
        report.metrics.live_sessions,
        report.metrics.rewound_sessions,
        report.metrics.connections,
        report.metrics.scrollback_bytes,
        report.metrics.image_store_bytes,
        report.metrics.total_pty_bytes,
    );
    for s in &report.sessions {
        let mode = if s.rewound {
            "rewound"
        } else if s.kept {
            "kept"
        } else {
            "live"
        };
        let fg = s.foreground.as_deref().unwrap_or("-");
        out.push_str(&format!(
            "  #{:<4} {:>3}x{:<3} {:<7} pid={:<7} fg={:<16} title={:?}\n",
            s.id,
            s.cols,
            s.rows,
            mode,
            s.pid.map_or_else(|| String::from("-"), |p| p.to_string()),
            fg,
            s.title,
        ));
    }
    out
}
