use std::{
    net::SocketAddr,
    process::Stdio,
    sync::{
        mpsc::{self, SyncSender},
        OnceLock,
    },
};

use axum::{
    extract::{
        ws::{Message, WebSocket},
        ConnectInfo, WebSocketUpgrade,
    },
    response::Response,
    routing::get,
    Router,
};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
    task::block_in_place,
};

static WS_SENDER: OnceLock<SyncSender<(WebSocket, SocketAddr)>> = OnceLock::new();

#[tokio::main]
async fn main() {
    // build our application with a route
    let app = Router::new().route("/", get(start));
    let (sender, receiver) = mpsc::sync_channel(0);
    WS_SENDER.set(sender).unwrap();

    // run our app with hyper, listening globally on port 80
    let listener = tokio::net::TcpListener::bind("0.0.0.0:80").await.unwrap();
    tokio::select! {
        result = tokio::spawn(async { axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await }) => {
            result.unwrap().unwrap();
        }
        _ = tokio::spawn(async move {
            let mut maybe_child: Option<(Child, u32)> = None;
            loop {
                let (mut ws, addr) = block_in_place(|| {
                    receiver.recv().unwrap()
                });

                let (child, pid) = if let Some(value) = &mut maybe_child {
                    value
                } else {
                    std::fs::write("settings.toml", format!("server_addr = \"{}\"", addr)).unwrap();
                    let tmp = match Command::new("/home/naj/.cargo/bin/lunabot")
                        .current_dir("/home/naj/lunaportal")
                        .stderr(Stdio::piped())
                        .stdout(Stdio::piped())
                        .spawn()
                    {
                        Ok(x) => x,
                        Err(e) => {
                            let _ = ws.send(format!("Failed to start: {e}").into()).await;
                            return;
                        }
                    };

                    let Some(pid) = tmp.id() else {
                        let output = match tmp.wait_with_output().await {
                            Ok(x) => x,
                            Err(e) => {
                                let _ = ws.send(format!("Failed to start: {e}").into()).await;
                                return;
                            }
                        };
                        let e = String::from_utf8_lossy(&output.stderr);
                        let _ = ws.send(format!("Failed to start: {e}").into()).await;
                        return;
                    };

                    maybe_child = Some((tmp, pid));
                    maybe_child.as_mut().unwrap()
                };

                let pid = *pid;
                let stdout = child.stdout.as_mut().unwrap();
                let stderr = child.stderr.as_mut().unwrap();

                let sigint = || {
                    Command::new("kill").arg("-2").arg(pid.to_string()).output()
                };

                let end = |mut ws: WebSocket, child: Child| async move {
                    match sigint().await {
                        Ok(output) => {
                            if output.status.success() {
                                tokio::select! {
                                    _ = child.wait_with_output() => { }
                                    _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                                        let _ = sigint().await;
                                    }
                                }
                            } else {
                                let e = String::from_utf8_lossy(&output.stderr);
                                eprintln!("kill failed to execute: {e}");
                                let _ = ws.send(format!("kill failed to execute: {e}").into()).await;
                            }
                        }
                        Err(e) => {
                            eprintln!("Failed to SIGINT the first time: {e}");
                            let _ = ws
                                .send(format!("Failed to SIGINT the first time: {e}").into())
                                .await;
                        }
                    }
                    let _ = ws.close().await;
                };

                macro_rules! send {
                    ($msg: expr) => {
                        if ws.send($msg.into()).await.is_err() {
                            break;
                        }
                    };
                }

                let mut stderr_buf = [0u8; 512];
                let mut stderr_msg = vec![];

                let mut stdout_buf = [0u8; 512];
                let mut stdout_msg = vec![];

                let mut mute = false;

                loop {
                    tokio::select! {
                        option = ws.recv() => {
                            let Some(Ok(msg)) = option else {
                                break;
                            };
                            if let Message::Close(_) = &msg {
                                break;
                            }
                            let Message::Text(msg) = msg else {
                                send!("Invalid message");
                                continue;
                            };
                            match msg.as_str() {
                                "mute" => {
                                    mute = true;
                                }
                                "unmute" => {
                                    mute = false;
                                }
                                "kill" => {
                                    let (child, _) = maybe_child.take().unwrap();
                                    end(ws, child).await;
                                    break;
                                }
                                _ => {
                                    send!("Invalid message");
                                }
                            }
                        }
                        result = stderr.read(&mut stderr_buf) => {
                            let Ok(n) = result else {
                                let (child, _) = maybe_child.take().unwrap();
                                match child.wait_with_output().await {
                                    Ok(output) => if !output.status.success() {
                                        let e = String::from_utf8_lossy(&output.stderr);
                                        let _ = ws.send(format!("{e}").into()).await;
                                    }
                                    Err(e) => {
                                        let _ = ws.send(format!("{e}").into()).await;
                                    }
                                }
                                let _ = ws.close().await;
                                break;
                            };
                            stderr_msg.extend_from_slice(stderr_buf.split_at(n).0);
                            let Ok(stderr_str) = std::str::from_utf8(&stderr_msg) else {
                                continue;
                            };
                            if let Some(n) = stderr_str.find('\n') {
                                if !mute {
                                    send!(stderr_str.split_at(n).0);
                                }
                                stderr_msg.drain(0..=n);
                            }
                        }
                        result = stdout.read(&mut stdout_buf) => {
                            let Ok(n) = result else {
                                let (child, _) = maybe_child.take().unwrap();
                                match child.wait_with_output().await {
                                    Ok(output) => if !output.status.success() {
                                        let e = String::from_utf8_lossy(&output.stderr);
                                        let _ = ws.send(format!("{e}").into()).await;
                                    }
                                    Err(e) => {
                                        let _ = ws.send(format!("{e}").into()).await;
                                    }
                                }
                                let _ = ws.close().await;
                                break;
                            };
                            stdout_msg.extend_from_slice(stdout_buf.split_at(n).0);
                            let Ok(stdout_str) = std::str::from_utf8(&stdout_msg) else {
                                continue;
                            };
                            if let Some(n) = stdout_str.find('\n') {
                                if !mute {
                                    send!(stdout_str.split_at(n).0);
                                }
                                stdout_msg.drain(0..=n);
                            }
                        }
                    }
                }
            }
        }) => {
            unreachable!();
        }
    }
}

pub async fn start(ws: WebSocketUpgrade, mut info: ConnectInfo<SocketAddr>) -> Response {
    ws.on_upgrade(move |mut ws| async move {
        info.0.set_port(43721);
        if let Err(e) = WS_SENDER.get().unwrap().try_send((ws, info.0)) {
            ws = match e {
                mpsc::TrySendError::Full((ws, _)) => ws,
                mpsc::TrySendError::Disconnected(_) => unreachable!(),
            };
            let _ = ws.send("Already started".into()).await;
        }
    })
}
