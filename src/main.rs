// SPDX-License-Identifier: GPL-3.0-only
#[macro_use]
extern crate tracing;

mod comp;
mod notifications;
mod process;
mod service;
mod systemd;

use std::{
	os::fd::AsRawFd,
	sync::{Arc, Mutex},
};

use async_signals::Signals;
use color_eyre::{eyre::WrapErr, Result};
use comp::create_privileged_socket;
use cosmic_notifications_util::{DAEMON_NOTIFICATIONS_FD, PANEL_NOTIFICATIONS_FD};
use futures_util::StreamExt;
use launch_pad::{process::Process, ProcessManager};
use service::SessionRequest;
use tokio::{
	net::UnixStream,
	sync::{
		mpsc::{self, Receiver, Sender},
		oneshot,
	},
	time::{sleep, Duration},
};
use tokio_util::sync::CancellationToken;
use tracing::{metadata::LevelFilter, Instrument};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
use zbus::ConnectionBuilder;

use crate::notifications::notifications_process;
const XDP_COSMIC: Option<&'static str> = option_env!("XDP_COSMIC");

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
	color_eyre::install().wrap_err("failed to install color_eyre error handler")?;

	tracing_subscriber::registry()
		.with(tracing_journald::layer().wrap_err("failed to connect to journald")?)
		.with(fmt::layer())
		.with(
			EnvFilter::builder()
				.with_default_directive(LevelFilter::INFO.into())
				.from_env_lossy(),
		)
		.try_init()
		.wrap_err("failed to initialize logger")?;
	log_panics::init();

	let (session_tx, mut session_rx) = tokio::sync::mpsc::channel(10);
	let session_tx_clone = session_tx.clone();
	let _conn = ConnectionBuilder::session()?
		.name("com.system76.CosmicSession")?
		.serve_at(
			"/com/system76/CosmicSession",
			service::SessionService { session_tx },
		)?
		.build()
		.await?;

	loop {
		match start(session_tx_clone.clone(), &mut session_rx).await {
			Ok(Status::Exited) => {
				info!("Exited cleanly");
				break;
			}
			Ok(Status::Restarted) => {
				info!("Restarting");
			}
			Err(error) => {
				error!("Restarting after error: {:?}", error);
			}
		};
		// Drain the session channel.
		while session_rx.try_recv().is_ok() {}
	}
	Ok(())
}

#[derive(Debug)]
pub enum Status {
	Restarted,
	Exited,
}

async fn start(
	session_tx: Sender<SessionRequest>,
	session_rx: &mut Receiver<SessionRequest>,
) -> Result<Status> {
	info!("Starting cosmic-session");

	let process_manager = ProcessManager::new().await;
	_ = process_manager.set_max_restarts(usize::MAX).await;
	_ = process_manager
		.set_restart_mode(launch_pad::RestartMode::ExponentialBackoff(
			Duration::from_millis(10),
		))
		.await;
	let token = CancellationToken::new();
	let (socket_tx, socket_rx) = mpsc::unbounded_channel();
	let (env_tx, env_rx) = oneshot::channel();
	let compositor_handle = comp::run_compositor(
		&process_manager,
		token.child_token(),
		socket_rx,
		env_tx,
		session_tx,
	)
	.wrap_err("failed to start compositor")?;
	sleep(Duration::from_millis(2000)).await;
	systemd::start_systemd_target()
		.await
		.wrap_err("failed to start systemd target")?;
	// Always stop the target when the process exits or panics.
	scopeguard::defer! {
		if let Err(error) = systemd::stop_systemd_target() {
			error!("failed to stop systemd target: {:?}", error);
		}
	}
	let env_vars = env_rx
		.await
		.expect("failed to receive environmental variables")
		.into_iter()
		.collect::<Vec<_>>();
	info!("got environmental variables: {:?}", env_vars);

	let (panel_notifications_fd, daemon_notifications_fd) =
		notifications::create_socket().expect("Failed to create notification socket");

	let mut daemon_env_vars = env_vars.clone();
	daemon_env_vars.push((
		DAEMON_NOTIFICATIONS_FD.to_string(),
		daemon_notifications_fd.as_raw_fd().to_string(),
	));
	let mut panel_env_vars = env_vars.clone();
	panel_env_vars.push((
		PANEL_NOTIFICATIONS_FD.to_string(),
		panel_notifications_fd.as_raw_fd().to_string(),
	));

	let panel_key = Arc::new(Mutex::new(None));
	let notif_key = Arc::new(Mutex::new(None));

	let notifications_span = info_span!(parent: None, "cosmic-notifications");
	let panel_span = info_span!(parent: None, "cosmic-panel");

	let mut guard = notif_key.lock().unwrap();
	*guard = Some(
		process_manager
			.start(notifications_process(
				notifications_span.clone(),
				"cosmic-notifications",
				notif_key.clone(),
				daemon_env_vars.clone(),
				daemon_notifications_fd,
				panel_span.clone(),
				"cosmic-panel",
				panel_key.clone(),
				panel_env_vars.clone(),
				socket_tx.clone(),
			))
			.await
			.expect("failed to start notifications daemon"),
	);
	drop(guard);

	let mut guard = panel_key.lock().unwrap();
	*guard = Some(
		process_manager
			.start(notifications_process(
				panel_span,
				"cosmic-panel",
				panel_key.clone(),
				panel_env_vars,
				panel_notifications_fd,
				notifications_span,
				"cosmic-notifications",
				notif_key,
				daemon_env_vars,
				socket_tx.clone(),
			))
			.await
			.expect("failed to start panel"),
	);
	drop(guard);

	let span = info_span!(parent: None, "cosmic-app-library");
	start_component(
		"cosmic-app-library",
		span,
		&process_manager,
		&env_vars,
		&socket_tx,
	)
	.await;

	let span = info_span!(parent: None, "cosmic-launcher");
	start_component(
		"cosmic-launcher",
		span,
		&process_manager,
		&env_vars,
		&socket_tx,
	)
	.await;

	let span = info_span!(parent: None, "cosmic-workspaces");
	start_component(
		"cosmic-workspaces",
		span,
		&process_manager,
		&env_vars,
		&socket_tx,
	)
	.await;

	let span = info_span!(parent: None, "cosmic-osd");
	start_component("cosmic-osd", span, &process_manager, &env_vars, &socket_tx).await;

	let span = info_span!(parent: None, "cosmic-bg");
	start_component("cosmic-bg", span, &process_manager, &env_vars, &socket_tx).await;

	let span = info_span!(parent: None, "xdg-desktop-portal-cosmic");
	start_component(
		XDP_COSMIC.unwrap_or("/usr/libexec/xdg-desktop-portal-cosmic"),
		span,
		&process_manager,
		&env_vars,
		&socket_tx,
	)
	.await;

	process_manager
		.start(Process::new().with_executable("cosmic-settings-daemon"))
		.await
		.expect("failed to start settings daemon");

	let mut signals = Signals::new(vec![libc::SIGTERM, libc::SIGINT]).unwrap();
	let mut status = Status::Exited;
	loop {
		let session_dbus_rx_next = session_rx.recv();
		tokio::select! {
			res = session_dbus_rx_next => {
				match res {
					Some(service::SessionRequest::Exit) => {
						info!("EXITING: session exited by request");
						break;
					}
					Some(service::SessionRequest::Restart) => {
						info!("RESTARTING: session restarted by request");
						status = Status::Restarted;
						break;
					}
					None => {
						warn!("exit channel dropped session");
						break;
					}
				}
			},
			signal = signals.next() => match signal {
				Some(libc::SIGTERM | libc::SIGINT) => {
					info!("EXITING: received request to terminate");
					break;
				}
				Some(signal) => unreachable!("EXITING: received unhandled signal {}", signal),
				None => break,
			}
		}
	}
	compositor_handle.abort();
	token.cancel();
	tokio::time::sleep(std::time::Duration::from_secs(2)).await;
	Ok(status)
}

async fn start_component(
	cmd: &str,
	span: tracing::Span,
	process_manager: &ProcessManager,
	env_vars: &[(String, String)],
	socket_tx: &mpsc::UnboundedSender<Vec<UnixStream>>,
) {
	let mut sockets = Vec::with_capacity(1);
	let (env_vars, fd) = create_privileged_socket(&mut sockets, env_vars).unwrap();
	if let Err(why) = socket_tx.send(sockets) {
		error!(?why, "Failed to send the privileged socket");
	}
	let socket_tx_clone = socket_tx.clone();
	let stdout_span = span.clone();
	let stderr_span = span.clone();
	let cmd_clone = cmd.to_string();
	process_manager
		.start(
			Process::new()
				.with_executable(cmd)
				.with_env(env_vars.iter().cloned())
				.with_on_stdout(move |_, _, line| {
					let stdout_span = stdout_span.clone();
					async move {
						info!("{}", line);
					}
					.instrument(stdout_span)
				})
				.with_on_stderr(move |_, _, line| {
					let stderr_span = stderr_span.clone();
					async move {
						warn!("{}", line);
					}
					.instrument(stderr_span)
				})
				.with_on_exit(move |mut pman, key, err_code, will_restart| {
					if let Some(err) = err_code {
						error!("{cmd_clone} exited with error {}", err.to_string());
					}

					let socket_tx_clone = socket_tx_clone.clone();
					async move {
						if !will_restart {
							return;
						}
						let mut sockets = Vec::with_capacity(1);
						let env_vars = Vec::with_capacity(1);
						let (env_vars, new_fd) =
							create_privileged_socket(&mut sockets, &env_vars).unwrap();

						if let Err(why) = socket_tx_clone.send(sockets) {
							error!(?why, "Failed to send the privileged socket");
						}
						if let Err(why) = pman.update_process_env(&key, env_vars).await {
							error!(?why, "Failed to update environment variables");
						}
						if let Err(why) = pman.update_process_fds(&key, move || vec![new_fd]).await
						{
							error!(?why, "Failed to update fds");
						}
					}
				})
				.with_fds(move || vec![fd]),
		)
		.await
		.expect(&format!("failed to start {}", cmd));
}
