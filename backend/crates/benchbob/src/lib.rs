//! BenchBob's server: the dashboard, REST API, SSE status stream, WebSockets and MCP on
//! `$PORT` (default 80), dual-stack. See backend/README.md.
//!
//! A library too, for the tests and the CLI.

pub mod app;
pub mod docs;
pub mod health;
pub mod onboard_leds;
pub mod sse;
pub mod status;
pub mod ui;
pub mod update;

use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::Request;
use benchbob_boards::{Board, Boards, pick, read_detect_input};
use benchbob_core::Registry;
use benchbob_core::paths::{app_root, env, state_path};

use crate::app::App;

/// Listen on [::]:port with IPV6_V6ONLY off (IPv4 arrives as mapped addresses); plain
/// IPv4 if the kernel has no IPv6.
pub fn listen(port: u16) -> anyhow::Result<std::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let v6 = (|| -> std::io::Result<Socket> {
        let s = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
        s.set_only_v6(false)?;
        s.set_reuse_address(true)?;
        s.bind(&SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0)).into())?;
        Ok(s)
    })();
    let s = match v6 {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("IPv6 listen failed ({e}); IPv4 only");
            let s = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
            s.set_reuse_address(true)?;
            s.bind(&SocketAddr::from(([0, 0, 0, 0], port)).into())?;
            s
        }
    };
    s.listen(1024)?;
    s.set_nonblocking(true)?;
    Ok(s.into())
}

/// Every panel's driver. A new panel crate is one line here.
pub fn drivers(board: &Arc<Board>) -> Vec<Arc<dyn benchbob_core::Driver>> {
    let i2c = benchbob_i2c::I2c::new();
    // 1-Wire lists the DS2482 bridges the I²C scan found and rescans after binding one.
    let onewire = benchbob_onewire::OneWire::new(Some(i2c.handle()));
    // LED strips claim their pins in the GPIO panel and follow its output control.
    let gpio = benchbob_gpio::Gpio::new(board.clone());
    let leds = benchbob_leds::Leds::new(gpio.handle());
    vec![
        Arc::new(benchbob_ble::Ble::new()),
        Arc::new(benchbob_camera::Camera::new()),
        Arc::new(benchbob_can::Can::new()),
        Arc::new(benchbob_drives::Drives::new()),
        Arc::new(gpio),
        Arc::new(benchbob_gps::Gps::new()),
        Arc::new(i2c),
        Arc::new(leds),
        Arc::new(benchbob_netdiag::Netdiag::new()),
        Arc::new(onewire),
        Arc::new(benchbob_probes::Probes::new()),
        Arc::new(benchbob_sdr::Sdr::new()),
        Arc::new(benchbob_serial::Serial::new()),
        Arc::new(benchbob_ups::Ups::new()),
        Arc::new(benchbob_usb::Usb::new()),
        Arc::new(benchbob_wifi::Wifi::new()),
    ]
}

pub async fn serve() -> anyhow::Result<()> {
    let port: u16 = env("PORT").and_then(|p| p.parse().ok()).filter(|p| *p > 0).unwrap_or(80);
    let boards = Arc::new(Boards::load_default());
    let board = Arc::new(pick(&boards, &read_detect_input(), env("BENCHBOB_BOARD").as_deref()));
    tracing::info!(
        "[boards] {} → {}{}{} ({} boards from {})",
        board.name,
        board.id,
        if board.forced { " (forced)" } else { "" },
        if board.tested { "" } else { " (untested board)" },
        boards.defs.len(),
        boards.dirs.join(", ")
    );
    for e in &boards.errors {
        tracing::warn!("[boards] {}: {}", e.file, e.error);
    }
    let mut registry = Registry::new(state_path("peripherals.json"));
    for d in drivers(&board) {
        registry.register(d);
    }
    let app = Arc::new(App {
        registry,
        health: health::Health::new(&board.def.health),
        board,
        boards,
        sampler: Default::default(),
        sse: Default::default(),
        update: Default::default(),
        ui: ui::Ui::new(app_root()),
        onboard_leds: Default::default(),
    });

    // A peripheral change pushes only the peripherals part, as its own event: a full
    // status re-samples CPU/IO rates and spawns processes, and every full message adds a
    // point to the dashboard's sparklines. The 2 s tick still carries everything.
    {
        let a = app.clone();
        app.registry.hub().on_changed(Arc::new(move || {
            if a.sse.count() > 0 {
                a.sse.broadcast(&a.periph(), Some("periph"));
            }
        }));
    }
    app.update.start();
    app.registry.start().await;

    {
        let a = app.clone();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(Duration::from_secs(2));
            loop {
                t.tick().await;
                if a.sse.count() > 0 {
                    let s = a.status().await;
                    a.sse.broadcast(&s, None);
                }
            }
        });
    }
    {
        let a = app.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{SignalKind, signal};
            let (mut term, mut int) = (signal(SignalKind::terminate()).unwrap(), signal(SignalKind::interrupt()).unwrap());
            tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} }
            tracing::info!("stopping drivers");
            a.onboard_leds.release_all().await;
            a.registry.shutdown().await;
            std::process::exit(0);
        });
    }

    let listener = tokio::net::TcpListener::from_std(listen(port)?)?;
    let router = Router::new().fallback(handler).with_state(app);
    tracing::info!("BenchBob running on http://[::]:{port} (dual-stack: IPv6 + IPv4)");
    axum::serve(listener, router).await?;
    Ok(())
}

async fn handler(State(app): State<Arc<App>>, req: Request<Body>) -> axum::response::Response {
    app.handle(req).await
}
