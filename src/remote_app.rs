use crate::{
    client::{Data, Interface, LoginConfigHandler},
    flutter::sessions,
};
use base::message_proto::{
    remote_app_response, Message, RemoteAppKey, RemoteAppPointer, RemoteAppRequest,
    RemoteAppResponse, Resolution,
};
use hbb_common::{
    anyhow::anyhow,
    bail,
    base64::{engine::general_purpose::STANDARD, Engine as _},
    ResultType, SessionID,
};
use serde::Deserialize;
use std::sync::{Arc, RwLock};

#[derive(Deserialize)]
struct Request {
    app_id: String,
    command: String,
    #[serde(default)]
    frame_id: String,
    #[serde(default)]
    layout_id: String,
    #[serde(default)]
    window_id: String,
    #[serde(default)]
    kind: u32,
    #[serde(default)]
    button: u32,
    #[serde(default)]
    x: i32,
    #[serde(default)]
    y: i32,
    #[serde(default)]
    scroll_x: i32,
    #[serde(default)]
    scroll_y: i32,
    #[serde(default)]
    virtual_key: u32,
    #[serde(default)]
    down: bool,
    #[serde(default)]
    text: String,
    #[serde(default)]
    width: i32,
    #[serde(default)]
    height: i32,
}

fn parse_request(session_id: &SessionID, json: &str) -> ResultType<RemoteAppRequest> {
    if json.len() > 16 * 1024 {
        bail!("Remote application request is too large");
    }
    let request: Request = serde_json::from_str(json)?;
    if request.app_id != session_id.to_string() {
        bail!("Remote application session does not match");
    }
    let mut out = RemoteAppRequest {
        app_id: request.app_id,
        frame_id: if request.frame_id.is_empty() {
            0
        } else {
            request.frame_id.parse()?
        },
        layout_id: if request.layout_id.is_empty() {
            0
        } else {
            request.layout_id.parse()?
        },
        ..Default::default()
    };
    match request.command.as_str() {
        "list" => out.set_list(true),
        "start" => out.set_start(request.window_id.parse()?),
        "stop" => out.set_stop(true),
        "release" => out.set_release(true),
        "ack" => out.set_ack(true),
        "pointer" => out.set_pointer(RemoteAppPointer {
            kind: request.kind,
            button: request.button,
            x: request.x,
            y: request.y,
            scroll_x: request.scroll_x,
            scroll_y: request.scroll_y,
            ..Default::default()
        }),
        "key" => {
            if request.virtual_key > 255 || request.text.len() > 4096 {
                bail!("Invalid remote application key");
            }
            out.set_key(RemoteAppKey {
                virtual_key: request.virtual_key,
                down: request.down,
                text: request.text,
                ..Default::default()
            });
        }
        "resize" => out.set_resize(Resolution {
            width: request.width,
            height: request.height,
            ..Default::default()
        }),
        _ => bail!("Unknown remote application command"),
    }
    Ok(out)
}

pub(crate) fn request(session_id: &SessionID, json: &str) -> ResultType<()> {
    let request = parse_request(session_id, json)?;
    let session = sessions::get_session_by_session_id(session_id)
        .ok_or_else(|| anyhow!("Remote desktop session has closed"))?;
    if !session
        .connection_round_state
        .lock()
        .unwrap()
        .is_connected()
    {
        bail!("Remote desktop is not connected");
    }
    let supported = session
        .lc
        .read()
        .unwrap()
        .peer_info
        .as_ref()
        .and_then(|p| p.features.as_ref())
        .map_or(false, |f| f.remote_app);
    if !supported {
        bail!("This remote desktop does not support remote applications");
    }
    let mut message = Message::new();
    message.set_remote_app_request(request);
    session.send(Data::Message(message));
    Ok(())
}

pub(crate) fn local_error(session_id: &SessionID, error: &str) {
    if let Some(session) = sessions::get_session_by_session_id(session_id) {
        session.push_event_to(
            "remote_app",
            &[
                ("app_id", session_id.to_string()),
                ("kind", "error".to_owned()),
                ("message", error.to_owned()),
            ],
            &[session_id],
        );
    }
}

pub(crate) fn response(source: &Arc<RwLock<LoginConfigHandler>>, response: RemoteAppResponse) {
    let Ok(session_id) = SessionID::parse_str(&response.app_id) else {
        return;
    };
    let Some(session) = sessions::get_session_by_session_id(&session_id) else {
        return;
    };
    if !Arc::ptr_eq(source, &session.lc) {
        return;
    }
    let mut fields = vec![("app_id", response.app_id)];
    match response.union {
        Some(remote_app_response::Union::Windows(windows)) => {
            let windows: Vec<_> = windows.windows.iter().map(|w| serde_json::json!({
                "id": w.id.to_string(), "title": w.title, "width": w.width, "height": w.height,
            })).collect();
            fields.push(("kind", "windows".to_owned()));
            fields.push(("windows", serde_json::Value::Array(windows).to_string()));
        }
        Some(remote_app_response::Union::Frame(frame)) => {
            if frame.width == 0
                || frame.height == 0
                || frame.width > 8192
                || frame.height > 8192
                || u64::from(frame.width) * u64::from(frame.height) > 16 * 1024 * 1024
                || frame.jpeg.len() > 16 * 1024 * 1024
            {
                return;
            }
            fields.extend([
                ("kind", "frame".to_owned()),
                ("frame_id", frame.frame_id.to_string()),
                ("layout_id", frame.layout_id.to_string()),
                ("width", frame.width.to_string()),
                ("height", frame.height.to_string()),
                ("jpeg", STANDARD.encode(&frame.jpeg)),
                ("title", frame.title),
            ]);
        }
        Some(remote_app_response::Union::Error(error)) => {
            fields.extend([("kind", "error".to_owned()), ("message", error)]);
        }
        Some(remote_app_response::Union::Closed(_)) => {
            fields.push(("kind", "closed".to_owned()));
        }
        _ => return,
    }
    session.push_event_to("remote_app", &fields, &[&session_id]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_requests_are_scoped_to_the_ui_session() {
        let id = SessionID::new_v4();
        assert!(parse_request(&id, r#"{"app_id":"other","command":"stop"}"#).is_err());
        let json = serde_json::json!({"app_id":id.to_string(),"command":"start",
            "window_id":"18446744073709551600"})
        .to_string();
        assert_eq!(
            parse_request(&id, &json).unwrap().start(),
            18446744073709551600
        );
    }

    #[test]
    fn pointer_coordinates_and_frame_identity_survive_the_bridge() {
        let id = SessionID::new_v4();
        let json = serde_json::json!({"app_id":id.to_string(),"command":"pointer",
            "frame_id":"14","layout_id":"99","kind":1,"button":1,"x":65,"y":23})
        .to_string();
        let request = parse_request(&id, &json).unwrap();
        assert_eq!(request.frame_id, 14);
        assert_eq!(request.layout_id, 99);
        assert_eq!(
            (
                request.pointer().kind,
                request.pointer().x,
                request.pointer().y
            ),
            (1, 65, 23)
        );
    }
}
