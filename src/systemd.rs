// SPDX-License-Identifier: GPL-3.0-only

use color_eyre::{eyre::WrapErr, Result};

pub async fn start_systemd_target() -> Result<()> {
	let manager = systemd_client::manager::build_blocking_proxy()
		.wrap_err("failed to connect to org.freedesktop.systemd1.Manager")?;
	manager
		.start_unit("cosmic-session.target", "replace")
		.wrap_err("failed to start cosmic-session.target")?;
	Ok(())
}
