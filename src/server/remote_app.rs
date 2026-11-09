use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use base::message_proto::{
    remote_app_request, Message, RemoteAppFrame, RemoteAppRequest, RemoteAppResponse,
    RemoteAppWindow, RemoteAppWindows,
};
use bytes::Bytes;

use super::{ConnInner, Subscriber};
use crate::platform::windows::remote_app::{self, App};

const MAX_APPS: usize = 8;
const MAX_WINDOWS: usize = 256;
const MAX_TITLE_CHARS: usize = 256;
const FRAME_INTERVAL: Duration = Duration::from_millis(100);
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const INPUT_ERROR_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Default)]
pub struct Sessions {
    sender: Option<SyncSender<QueuedRequest>>,
    stopped: Option<Arc<AtomicBool>>,
    input_enabled: Option<Arc<AtomicBool>>,
    release_input: Option<Arc<AtomicBool>>,
    input_epoch: Option<Arc<AtomicU64>>,
}

impl Sessions {
    pub fn handle(&mut self, mut conn: ConnInner, request: RemoteAppRequest, allow_input: bool) {
        if uuid::Uuid::parse_str(&request.app_id).is_err() {
            send_error(&mut conn, String::new(), "Invalid remote app ID".to_owned());
            return;
        }
        if self.sender.is_none() {
            let (sender, receiver) = mpsc::sync_channel(128);
            let stopped = Arc::new(AtomicBool::new(false));
            let input_enabled = Arc::new(AtomicBool::new(allow_input));
            let release_input = Arc::new(AtomicBool::new(false));
            let input_epoch = Arc::new(AtomicU64::new(0));
            let worker_stopped = stopped.clone();
            let worker_enabled = input_enabled.clone();
            let worker_release = release_input.clone();
            let worker_epoch = input_epoch.clone();
            let worker_conn = conn.clone();
            if let Err(e) = thread::Builder::new()
                .name("remote-app".to_owned())
                .spawn(move || {
                    run(
                        worker_conn,
                        receiver,
                        worker_stopped,
                        worker_enabled,
                        worker_release,
                        worker_epoch,
                    )
                })
            {
                send_error(
                    &mut conn,
                    request.app_id,
                    format!("Failed to start remote app: {e}"),
                );
                return;
            }
            self.sender = Some(sender);
            self.stopped = Some(stopped);
            self.input_enabled = Some(input_enabled);
            self.release_input = Some(release_input);
            self.input_epoch = Some(input_epoch);
        }
        self.set_input_enabled(allow_input);
        if let Some(sender) = &self.sender {
            let epoch = self
                .input_epoch
                .as_ref()
                .map_or(0, |epoch| epoch.load(Ordering::Acquire));
            match sender.try_send(QueuedRequest { request, epoch }) {
                Ok(()) => {}
                Err(TrySendError::Full(queued)) => {
                    let request = queued.request;
                    if is_release(&request) {
                        if let Some(epoch) = &self.input_epoch {
                            epoch.fetch_add(1, Ordering::AcqRel);
                        }
                        if let Some(release) = &self.release_input {
                            release.store(true, Ordering::Release);
                        }
                    } else if !matches!(request.union, Some(remote_app_request::Union::Pointer(_)))
                    {
                        send_error(
                            &mut conn,
                            request.app_id,
                            "Remote app request queue is full".to_owned(),
                        );
                    }
                }
                Err(TrySendError::Disconnected(queued)) => {
                    self.clear();
                    send_error(
                        &mut conn,
                        queued.request.app_id,
                        "Remote app dispatcher stopped".to_owned(),
                    );
                }
            }
        }
    }

    pub fn set_input_enabled(&self, enabled: bool) {
        let revoked = self.input_enabled.as_ref().map_or(false, |input_enabled| {
            input_enabled.swap(enabled, Ordering::AcqRel) && !enabled
        });
        if revoked {
            if let Some(epoch) = &self.input_epoch {
                epoch.fetch_add(1, Ordering::AcqRel);
            }
            if let Some(release) = &self.release_input {
                release.store(true, Ordering::Release);
            }
        }
    }

    pub fn clear(&mut self) {
        if let Some(stopped) = self.stopped.take() {
            stopped.store(true, Ordering::Release);
        }
        self.sender = None;
        self.input_enabled = None;
        self.release_input = None;
        self.input_epoch = None;
    }
}

impl Drop for Sessions {
    fn drop(&mut self) {
        self.clear();
    }
}

fn is_release(request: &RemoteAppRequest) -> bool {
    match &request.union {
        Some(remote_app_request::Union::Pointer(pointer)) => pointer.kind == 2,
        Some(remote_app_request::Union::Key(key)) => !key.down,
        Some(remote_app_request::Union::Release(_)) => true,
        _ => false,
    }
}

fn is_input(request: &RemoteAppRequest) -> bool {
    matches!(
        request.union,
        Some(
            remote_app_request::Union::Pointer(_)
                | remote_app_request::Union::Key(_)
                | remote_app_request::Union::Resize(_)
        )
    )
}

struct QueuedRequest {
    request: RemoteAppRequest,
    epoch: u64,
}

struct AppState {
    app: App,
    frame_id: u64,
    layout_id: u64,
    awaiting_ack: bool,
    next_frame: Instant,
    last_input_error: Option<Instant>,
}

fn run(
    mut conn: ConnInner,
    receiver: Receiver<QueuedRequest>,
    stopped: Arc<AtomicBool>,
    input_enabled: Arc<AtomicBool>,
    release_input: Arc<AtomicBool>,
    input_epoch: Arc<AtomicU64>,
) {
    let mut apps: HashMap<String, AppState> = HashMap::new();
    let mut next_frame_id = 0u64;
    while !stopped.load(Ordering::Acquire) {
        if release_input.swap(false, Ordering::AcqRel) {
            for state in apps.values_mut() {
                if let Err(e) = state.app.release() {
                    hbb_common::throttled_log!(
                        Duration::from_secs(5),
                        warn,
                        "Failed to release remote application input: {e}"
                    );
                }
            }
        }
        if crate::privacy_mode::is_in_privacy_mode() {
            close_all(&mut conn, &mut apps);
        }
        match receiver.recv_timeout(POLL_INTERVAL) {
            Ok(queued) => {
                if stopped.load(Ordering::Acquire) {
                    break;
                }
                if queued.epoch != input_epoch.load(Ordering::Acquire)
                    && is_input(&queued.request)
                    && !is_release(&queued.request)
                {
                    continue;
                }
                if crate::privacy_mode::is_in_privacy_mode() {
                    close_all(&mut conn, &mut apps);
                }
                handle_request(&mut conn, &mut apps, queued.request, &input_enabled);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if stopped.load(Ordering::Acquire) {
            break;
        }
        if crate::privacy_mode::is_in_privacy_mode() {
            close_all(&mut conn, &mut apps);
            continue;
        }
        let mut closed = Vec::new();
        for (app_id, state) in &mut apps {
            if stopped.load(Ordering::Acquire) || crate::privacy_mode::is_in_privacy_mode() {
                break;
            }
            if state.awaiting_ack || Instant::now() < state.next_frame {
                continue;
            }
            state.next_frame = Instant::now() + FRAME_INTERVAL;
            match state.app.capture() {
                Ok(Some(frame)) => {
                    if stopped.load(Ordering::Acquire) || crate::privacy_mode::is_in_privacy_mode()
                    {
                        break;
                    }
                    let Some(frame_id) = next_frame_id.checked_add(1) else {
                        send_error(
                            &mut conn,
                            app_id.clone(),
                            "Remote app frame ID exhausted".to_owned(),
                        );
                        closed.push(app_id.clone());
                        continue;
                    };
                    next_frame_id = frame_id;
                    state.frame_id = frame_id;
                    state.layout_id = frame.layout_id;
                    state.awaiting_ack = true;
                    let mut response = RemoteAppResponse::new();
                    response.app_id = app_id.clone();
                    response.set_frame(RemoteAppFrame {
                        frame_id,
                        layout_id: frame.layout_id,
                        width: frame.width,
                        height: frame.height,
                        jpeg: Bytes::from(frame.jpeg),
                        title: frame.title.chars().take(MAX_TITLE_CHARS).collect(),
                        ..Default::default()
                    });
                    send_response(&mut conn, response);
                }
                Ok(None) => {}
                Err(e) => {
                    send_error(
                        &mut conn,
                        app_id.clone(),
                        format!("Remote app capture failed: {e}"),
                    );
                    closed.push(app_id.clone());
                }
            }
        }
        for app_id in closed {
            apps.remove(&app_id);
            send_closed(&mut conn, app_id);
        }
    }
}

fn handle_request(
    conn: &mut ConnInner,
    apps: &mut HashMap<String, AppState>,
    request: RemoteAppRequest,
    input_enabled: &AtomicBool,
) {
    let RemoteAppRequest {
        app_id,
        frame_id,
        layout_id,
        union,
        ..
    } = request;
    match union {
        Some(remote_app_request::Union::List(_)) => {
            if crate::privacy_mode::is_in_privacy_mode() {
                send_error(
                    conn,
                    app_id,
                    "Remote apps are unavailable in privacy mode".to_owned(),
                );
            } else {
                match remote_app::list_windows() {
                    Ok(windows) => {
                        if crate::privacy_mode::is_in_privacy_mode() {
                            send_error(
                                conn,
                                app_id,
                                "Remote apps are unavailable in privacy mode".to_owned(),
                            );
                            return;
                        }
                        let mut response = RemoteAppResponse::new();
                        response.app_id = app_id;
                        response.set_windows(RemoteAppWindows {
                            windows: windows
                                .into_iter()
                                .take(MAX_WINDOWS)
                                .map(|window| RemoteAppWindow {
                                    id: window.id,
                                    title: window.title.chars().take(MAX_TITLE_CHARS).collect(),
                                    width: window.width,
                                    height: window.height,
                                    ..Default::default()
                                })
                                .collect(),
                            ..Default::default()
                        });
                        send_response(conn, response);
                    }
                    Err(e) => send_error(conn, app_id, format!("Failed to list remote apps: {e}")),
                }
            }
        }
        Some(remote_app_request::Union::Start(window_id)) => {
            apps.remove(&app_id);
            if crate::privacy_mode::is_in_privacy_mode() {
                send_error(
                    conn,
                    app_id,
                    "Remote apps are unavailable in privacy mode".to_owned(),
                );
            } else if app_id.is_empty() {
                send_error(conn, app_id, "Remote app ID is required".to_owned());
            } else if apps.len() >= MAX_APPS {
                send_error(conn, app_id, "Too many remote apps".to_owned());
            } else {
                match App::new(window_id) {
                    Ok(app) => {
                        if crate::privacy_mode::is_in_privacy_mode() {
                            send_error(
                                conn,
                                app_id,
                                "Remote apps are unavailable in privacy mode".to_owned(),
                            );
                            return;
                        }
                        apps.insert(
                            app_id,
                            AppState {
                                app,
                                frame_id: 0,
                                layout_id: 0,
                                awaiting_ack: false,
                                next_frame: Instant::now(),
                                last_input_error: None,
                            },
                        );
                    }
                    Err(e) => send_error(conn, app_id, format!("Failed to start remote app: {e}")),
                }
            }
        }
        Some(remote_app_request::Union::Stop(_)) => {
            apps.remove(&app_id);
            send_closed(conn, app_id);
        }
        Some(remote_app_request::Union::Ack(_)) => {
            if let Some(state) = apps.get_mut(&app_id) {
                if state.awaiting_ack && state.frame_id == frame_id {
                    state.awaiting_ack = false;
                }
            }
        }
        Some(remote_app_request::Union::Release(_)) => {
            if let Some(state) = apps.get_mut(&app_id) {
                let result = state.app.release();
                report_input_error(conn, &app_id, state, result);
            }
        }
        Some(remote_app_request::Union::Pointer(pointer)) => {
            if let Some(state) = apps.get_mut(&app_id) {
                let is_up = pointer.kind == 2;
                let allowed = input_enabled.load(Ordering::Acquire);
                let fresh = state.layout_id != 0 && state.layout_id == layout_id;
                if is_up && (!allowed || !fresh) {
                    let result = state.app.release();
                    report_input_error(conn, &app_id, state, result);
                } else if allowed && fresh {
                    let result = state.app.pointer(
                        pointer.kind,
                        pointer.button,
                        pointer.x,
                        pointer.y,
                        pointer.scroll_x,
                        pointer.scroll_y,
                    );
                    if result.is_err() && is_up {
                        if let Err(e) = state.app.release() {
                            hbb_common::throttled_log!(
                                Duration::from_secs(5),
                                warn,
                                "Failed to release remote application input: {e}"
                            );
                        }
                    }
                    if pointer.kind != 0 {
                        report_input_error(conn, &app_id, state, result);
                    }
                }
            }
        }
        Some(remote_app_request::Union::Key(key)) => {
            if let Some(state) = apps.get_mut(&app_id) {
                let allowed = input_enabled.load(Ordering::Acquire);
                if !key.down && !allowed {
                    let result = state.app.release();
                    report_input_error(conn, &app_id, state, result);
                } else if allowed {
                    let result = state.app.key(key.virtual_key, key.down, &key.text);
                    if result.is_err() && !key.down {
                        if let Err(e) = state.app.release() {
                            hbb_common::throttled_log!(
                                Duration::from_secs(5),
                                warn,
                                "Failed to release remote application input: {e}"
                            );
                        }
                    }
                    report_input_error(conn, &app_id, state, result);
                }
            }
        }
        Some(remote_app_request::Union::Resize(size)) => {
            if let Some(state) = apps.get_mut(&app_id) {
                if input_enabled.load(Ordering::Acquire)
                    && state.layout_id != 0
                    && state.layout_id == layout_id
                {
                    let result = if size.width <= 0 || size.height <= 0 {
                        Err(hbb_common::anyhow::anyhow!(
                            "Invalid application window size"
                        ))
                    } else {
                        state.app.resize(size.width as u32, size.height as u32)
                    };
                    report_input_error(conn, &app_id, state, result);
                }
            }
        }
        Some(_) | None => {}
    }
}

fn report_input_error(
    conn: &mut ConnInner,
    app_id: &str,
    state: &mut AppState,
    result: hbb_common::ResultType<()>,
) {
    if let Err(e) = result {
        let now = Instant::now();
        if state.last_input_error.map_or(true, |last| {
            now.duration_since(last) >= INPUT_ERROR_INTERVAL
        }) {
            state.last_input_error = Some(now);
            send_error(
                conn,
                app_id.to_owned(),
                format!("Remote app input failed: {e}"),
            );
        }
    }
}

fn close_all(conn: &mut ConnInner, apps: &mut HashMap<String, AppState>) {
    for (app_id, _) in apps.drain() {
        send_closed(conn, app_id);
    }
}

fn send_response(conn: &mut ConnInner, response: RemoteAppResponse) {
    let mut message = Message::new();
    message.set_remote_app_response(response);
    conn.send(Arc::new(message));
}

pub fn send_error(conn: &mut ConnInner, app_id: String, message: String) {
    let mut response = RemoteAppResponse::new();
    response.app_id = app_id;
    response.set_error(message);
    send_response(conn, response);
}

fn send_closed(conn: &mut ConnInner, app_id: String) {
    let mut response = RemoteAppResponse::new();
    response.app_id = app_id;
    response.set_closed(true);
    send_response(conn, response);
}
