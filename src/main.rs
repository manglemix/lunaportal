
use std::{net::SocketAddr, process::Stdio, sync::{Arc, OnceLock}};

use axum::{
    extract::{ws::{Message, WebSocket}, ConnectInfo, WebSocketUpgrade},
    response::Response,
    routing::get,
    Router,
};
use tokio::{io::AsyncReadExt, process::{Child, Command}, sync::{mpsc, Mutex, Notify}};

static PROC_LOCK: Mutex<()> = Mutex::const_new(());
static END_RECV: OnceLock<Mutex<mpsc::UnboundedReceiver<Arc<Notify>>>> = OnceLock::new();

#[tokio::main]
async fn main() {
    // build our application with a route
    let app = Router::new().route("/", get(start));
    let (sender, receiver) = mpsc::unbounded_channel();
    END_RECV.set(Mutex::new(receiver)).unwrap();

    // run our app with hyper, listening globally on port 80
    let listener = tokio::net::TcpListener::bind("0.0.0.0:80").await.unwrap();
    tokio::select! {
        result = tokio::spawn(async { axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await }) => {
            result.unwrap().unwrap();
        }
        result = tokio::signal::ctrl_c() => {
            result.unwrap();
            let notify = Arc::new(Notify::new());
            sender.send(notify.clone()).unwrap();
            notify.notified().await;
        }
    }
}

pub async fn start(ws: WebSocketUpgrade, mut info: ConnectInfo<SocketAddr>) -> Response {
    ws.on_upgrade(move |mut ws| async move {
        let _lock;
        loop {
            match PROC_LOCK.try_lock() {
                Ok(x) => {
                    _lock = x;
                    break;
                }
                Err(_) => {
                    let _ = ws.send("Already started".into()).await;
                    return;
                }
            };
        }

        info.0.set_port(43721);
        std::fs::write("settings.toml", format!("server_addr = \"{}\"", info.0)).unwrap();

        let mut child = match Command::new("lunabot")
            // .env("SERVER_ADDR", info.0.to_string())
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn() {
                Ok(x) => x,
                Err(e) => {
                    let _ = ws.send(format!("Failed to start: {e}").into()).await;
                    return;
                }
            };
        
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        
        let Some(pid) = child.id() else {
            let output = match child.wait_with_output().await {
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

        let sigint = || {
            Command::new("kill")
                .arg("-2")
                .arg(pid.to_string())
                .output()
        };

        let end = |mut ws: WebSocket, child: Child| async move {
            match sigint().await {
                    Ok(output) => if output.status.success() {
                        tokio::select! {
                            result = child.wait_with_output() => {
                                match result {
                                    Ok(output) => if !output.status.success() {
                                        let e = String::from_utf8_lossy(&output.stderr);
                                        let _ = ws.send(format!("Failed to kill: {e}").into()).await;
                                    }
                                    Err(e) => {
                                        let _ = ws.send(format!("Failed to kill: {e}").into()).await;
                                    }
                                }
                            }
                            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                                let _ = sigint().await;
                            }
                        }
                    }
                 else {
                    let e = String::from_utf8_lossy(&output.stderr);
                    let _ = ws.send(format!("Failed to kill: {e}").into()).await;
                }
                Err(e) => {
                    let _ = ws.send(format!("Failed to kill: {e}").into()).await;
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

        let mut end_recv = END_RECV.get().unwrap().lock().await;

        loop {
            tokio::select! {
                option = ws.recv() => {
                    let Some(Ok(msg)) = option else {
                        end(ws, child).await;
                        break;
                    };
                    let Message::Text(msg) = msg else {
                        send!("Invalid message");
                        continue;
                    };
                    match msg.as_str() {
                        
                        _ => {
                            send!("Invalid message");
                        }
                    }
                }
                result = stderr.read(&mut stderr_buf) => {
                    let Ok(n) = result else {
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
                        send!(stderr_str.split_at(n).0);
                        stderr_msg.drain(0..=n);
                    }
                }
                result = stdout.read(&mut stdout_buf) => {
                    let Ok(n) = result else {
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
                        send!(stdout_str.split_at(n).0);
                        stdout_msg.drain(0..=n);
                    }
                }
                result = end_recv.recv() => {
                    let notify = result.unwrap();
                    end(ws, child).await;
                    notify.notify_one();
                    break;
                }
            }
        }
    })
}
