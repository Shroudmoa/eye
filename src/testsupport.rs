//! Shared fixtures for unit tests.

use crate::event::{Meta, ShellInfo, Size, TerminalInfo, FORMAT_NAME, FORMAT_VERSION};

/// A minimal, valid header record.
pub fn meta() -> Meta {
    Meta {
        format: FORMAT_NAME.into(),
        version: FORMAT_VERSION,
        tool: "eye".into(),
        tool_version: env!("CARGO_PKG_VERSION").into(),
        created_at: "2026-09-29T00:00:00Z".into(),
        created_at_unix: 1_792_646_400.0,
        id: "0000000000000-1".into(),
        title: None,
        hostname: None,
        cwd: None,
        shell: ShellInfo {
            path: "/bin/bash".into(),
            name: "bash".into(),
            args: vec!["-i".into()],
            integration: "echo".into(),
        },
        terminal: TerminalInfo {
            term: "xterm-256color".into(),
            colorterm: None,
            size: Size::new(80, 24),
        },
        env: vec![],
    }
}
