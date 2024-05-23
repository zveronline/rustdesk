use super::{input_service::*, *};
#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
use crate::clipboard_file::*;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use crate::common::update_clipboard;
#[cfg(target_os = "android")]
use crate::keyboard::client::map_key_to_control_key;
#[cfg(target_os = "linux")]
use crate::platform::linux::is_x11;
#[cfg(target_os = "linux")]
use crate::platform::linux_desktop_manager;
#[cfg(any(target_os = "windows", target_os = "linux"))]
use crate::platform::WallPaperRemover;
#[cfg(windows)]
use crate::portable_service::client as portable_client;
use crate::{
    client::{
        new_voice_call_request, new_voice_call_response, start_audio_thread, MediaData, MediaSender,
    },
    common::{get_default_sound_input, set_sound_input},
    display_service, ipc, privacy_mode, video_service, VERSION,
};
#[cfg(any(target_os = "android", target_os = "ios"))]
use crate::{common::DEVICE_NAME, flutter::connection_manager::start_channel};
use cidr_utils::cidr::IpCidr;
#[cfg(target_os = "linux")]
use hbb_common::platform::linux::run_cmds;
#[cfg(target_os = "android")]
use hbb_common::protobuf::EnumOrUnknown;
use hbb_common::{
    config::{self, Config},
    fs::{self, can_enable_overwrite_detection},
    futures::{SinkExt, StreamExt},
    get_time, get_version_number,
    message_proto::{option_message::BoolOption, permission_info::Permission},
    password_security::{self as password, ApproveMode},
    sleep, timeout,
    tokio::{
        net::TcpStream,
        sync::mpsc,
        time::{self, Duration, Instant},
    },
    tokio_util::codec::{BytesCodec, Framed},
};
#[cfg(any(target_os = "android", target_os = "ios"))]
use scrap::android::{call_main_service_key_event, call_main_service_pointer_input};
use serde_derive::Serialize;
use serde_json::{json, value::Value};
use sha2::{Digest, Sha256};
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use std::sync::atomic::Ordering;
use std::{
    num::NonZeroI64,
    path::PathBuf,
    sync::{atomic::AtomicI64, mpsc as std_mpsc},
};
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use system_shutdown;

#[cfg(windows)]
use crate::virtual_display_manager;
#[cfg(not(any(target_os = "ios")))]
use std::collections::HashSet;
pub type Sender = mpsc::UnboundedSender<(Instant, Arc<Message>)>;

lazy_static::lazy_static! {
    static ref LOGIN_FAILURES: [Arc::<Mutex<HashMap<String, (i32, i32, i32)>>>; 2] = Default::default();
    static ref SESSIONS: Arc::<Mutex<HashMap<String, Session>>> = Default::default();
    static ref ALIVE_CONNS: Arc::<Mutex<Vec<i32>>> = Default::default();
    pub static ref AUTHED_CONNS: Arc::<Mutex<Vec<(i32, AuthConnType)>>> = Default::default();
    static ref SWITCH_SIDES_UUID: Arc::<Mutex<HashMap<String, (Instant, uuid::Uuid)>>> = Default::default();
    static ref WAKELOCK_SENDER: Arc::<Mutex<std::sync::mpsc::Sender<(usize, usize)>>> = Arc::new(Mutex::new(start_wakelock_thread()));
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
lazy_static::lazy_static! {
    static ref WALLPAPER_REMOVER: Arc<Mutex<Option<WallPaperRemover>>> = Default::default();
}
pub static CLICK_TIME: AtomicI64 = AtomicI64::new(0);
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub static MOUSE_MOVE_TIME: AtomicI64 = AtomicI64::new(0);

#[cfg(all(feature = "flutter", feature = "plugin_framework"))]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
lazy_static::lazy_static! {
    static ref PLUGIN_BLOCK_INPUT_TXS: Arc<Mutex<HashMap<String, std_mpsc::Sender<MessageInput>>>> = Default::default();
    static ref PLUGIN_BLOCK_INPUT_TX_RX: (Arc<Mutex<std_mpsc::Sender<bool>>>, Arc<Mutex<std_mpsc::Receiver<bool>>>) = {
        let (tx, rx) = std_mpsc::channel();
        (Arc::new(Mutex::new(tx)), Arc::new(Mutex::new(rx)))
    };
}

// Block input is required for some special cases, such as privacy mode.
#[cfg(all(feature = "flutter", feature = "plugin_framework"))]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn plugin_block_input(peer: &str, block: bool) -> bool {
    if let Some(tx) = PLUGIN_BLOCK_INPUT_TXS.lock().unwrap().get(peer) {
        let _ = tx.send(if block {
            MessageInput::BlockOnPlugin(peer.to_string())
        } else {
            MessageInput::BlockOffPlugin(peer.to_string())
        });
        match PLUGIN_BLOCK_INPUT_TX_RX
            .1
            .lock()
            .unwrap()
            .recv_timeout(std::time::Duration::from_millis(3_000))
        {
            Ok(b) => b == block,
            Err(..) => {
                log::error!("plugin_block_input timeout");
                false
            }
        }
    } else {
        false
    }
}

#[derive(Clone, Default)]
pub struct ConnInner {
    id: i32,
    tx: Option<Sender>,
    tx_video: Option<Sender>,
}

enum MessageInput {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    Mouse((MouseEvent, i32)),
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    Key((KeyEvent, bool)),
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    Pointer((PointerDeviceEvent, i32)),
    BlockOn,
    BlockOff,
    #[cfg(all(feature = "flutter", feature = "plugin_framework"))]
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    BlockOnPlugin(String),
    #[cfg(all(feature = "flutter", feature = "plugin_framework"))]
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    BlockOffPlugin(String),
}

#[derive(Clone, Debug)]
struct Session {
    name: String,
    session_id: u64,
    last_recv_time: Arc<Mutex<Instant>>,
    random_password: String,
    tfa: bool,
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
struct StartCmIpcPara {
    rx_to_cm: mpsc::UnboundedReceiver<ipc::Data>,
    tx_from_cm: mpsc::UnboundedSender<ipc::Data>,
    rx_desktop_ready: mpsc::Receiver<()>,
    tx_cm_stream_ready: mpsc::Sender<()>,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum AuthConnType {
    Remote,
    FileTransfer,
    PortForward,
}

pub struct Connection {
    inner: ConnInner,
    display_idx: usize,
    stream: super::Stream,
    server: super::ServerPtrWeak,
    hash: Hash,
    read_jobs: Vec<fs::TransferJob>,
    timer: crate::RustDeskInterval,
    file_timer: crate::RustDeskInterval,
    file_transfer: Option<(String, bool)>,
    port_forward_socket: Option<Framed<TcpStream, BytesCodec>>,
    port_forward_address: String,
    tx_to_cm: mpsc::UnboundedSender<ipc::Data>,
    authorized: bool,
    require_2fa: Option<totp_rs::TOTP>,
    keyboard: bool,
    clipboard: bool,
    audio: bool,
    file: bool,
    restart: bool,
    recording: bool,
    block_input: bool,
    last_test_delay: Option<Instant>,
    network_delay: u32,
    lock_after_session_end: bool,
    show_remote_cursor: bool,
    // by peer
    ip: String,
    // by peer
    disable_keyboard: bool,
    // by peer
    disable_clipboard: bool,
    // by peer
    disable_audio: bool,
    // by peer
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    enable_file_transfer: bool,
    // by peer
    audio_sender: Option<MediaSender>,
    // audio by the remote peer/client
    tx_input: std_mpsc::Sender<MessageInput>,
    // handle input messages
    video_ack_required: bool,
    server_audit_conn: String,
    server_audit_file: String,
    lr: LoginRequest,
    last_recv_time: Arc<Mutex<Instant>>,
    chat_unanswered: bool,
    file_transferred: bool,
    #[cfg(windows)]
    portable: PortableState,
    from_switch: bool,
    voice_call_request_timestamp: Option<NonZeroI64>,
    audio_input_device_before_voice_call: Option<String>,
    options_in_login: Option<OptionMessage>,
    #[cfg(not(any(target_os = "ios")))]
    pressed_modifiers: HashSet<rdev::Key>,
    #[cfg(target_os = "linux")]
    linux_headless_handle: LinuxHeadlessHandle,
    closed: bool,
    delay_response_instant: Instant,
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    start_cm_ipc_para: Option<StartCmIpcPara>,
    auto_disconnect_timer: Option<(Instant, u64)>,
    authed_conn_id: Option<self::raii::AuthedConnID>,
    file_remove_log_control: FileRemoveLogControl,
    last_supported_encoding: Option<SupportedEncoding>,
    services_subed: bool,
    delayed_read_dir: Option<(String, bool)>,
    #[cfg(target_os = "macos")]
    retina: Retina,
    follow_remote_cursor: bool,
    follow_remote_window: bool,
    multi_ui_session: bool,
}

impl ConnInner {
    pub fn new(id: i32, tx: Option<Sender>, tx_video: Option<Sender>) -> Self {
        Self { id, tx, tx_video }
    }
}

impl Subscriber for ConnInner {
    #[inline]
    fn id(&self) -> i32 {
        self.id
    }

    #[inline]
    fn send(&mut self, msg: Arc<Message>) {
        // Send SwitchDisplay on the same channel as VideoFrame to avoid send order problems.
        let tx_by_video = match &msg.union {
            Some(message::Union::VideoFrame(_)) => true,
            Some(message::Union::Misc(misc)) => match &misc.union {
                Some(misc::Union::SwitchDisplay(_)) => true,
                _ => false,
            },
            _ => false,
        };
        let tx = if tx_by_video {
            self.tx_video.as_mut()
        } else {
            self.tx.as_mut()
        };
        tx.map(|tx| {
            allow_err!(tx.send((Instant::now(), msg)));
        });
    }
}

const TEST_DELAY_TIMEOUT: Duration = Duration::from_secs(1);
const SEC30: Duration = Duration::from_secs(30);
const H1: Duration = Duration::from_secs(3600);
const MILLI1: Duration = Duration::from_millis(1);
const SEND_TIMEOUT_VIDEO: u64 = 12_000;
const SEND_TIMEOUT_OTHER: u64 = SEND_TIMEOUT_VIDEO * 10;
const SESSION_TIMEOUT: Duration = Duration::from_secs(30);

impl Connection {
    pub async fn start(
        addr: SocketAddr,
        stream: super::Stream,
        id: i32,
        server: super::ServerPtrWeak,
    ) {
        let _raii_id = raii::ConnectionID::new(id);
        let hash = Hash {
            salt: Config::get_salt(),
            challenge: Config::get_auto_password(6),
            ..Default::default()
        };
        let (tx_from_cm_holder, mut rx_from_cm) = mpsc::unbounded_channel::<ipc::Data>();
        // holding tx_from_cm_holder to avoid cpu burning of rx_from_cm.recv when all sender closed
        let tx_from_cm = tx_from_cm_holder.clone();
        let (tx_to_cm, rx_to_cm) = mpsc::unbounded_channel::<ipc::Data>();
        let (tx, mut rx) = mpsc::unbounded_channel::<(Instant, Arc<Message>)>();
        let (tx_video, mut rx_video) = mpsc::unbounded_channel::<(Instant, Arc<Message>)>();
        let (tx_input, _rx_input) = std_mpsc::channel();
        let mut hbbs_rx = crate::hbbs_http::sync::signal_receiver();
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        let (tx_cm_stream_ready, _rx_cm_stream_ready) = mpsc::channel(1);
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        let (_tx_desktop_ready, rx_desktop_ready) = mpsc::channel(1);
        #[cfg(target_os = "linux")]
        let linux_headless_handle =
            LinuxHeadlessHandle::new(_rx_cm_stream_ready, _tx_desktop_ready);

        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        let tx_cloned = tx.clone();
        let mut conn = Self {
            inner: ConnInner {
                id,
                tx: Some(tx),
                tx_video: Some(tx_video),
            },
            require_2fa: crate::auth_2fa::get_2fa(None),
            display_idx: *display_service::PRIMARY_DISPLAY_IDX,
            stream,
            server,
            hash,
            read_jobs: Vec::new(),
            timer: crate::rustdesk_interval(time::interval(SEC30)),
            file_timer: crate::rustdesk_interval(time::interval(SEC30)),
            file_transfer: None,
            port_forward_socket: None,
            port_forward_address: "".to_owned(),
            tx_to_cm,
            authorized: false,
            keyboard: Connection::permission("enable-keyboard"),
            clipboard: Connection::permission("enable-clipboard"),
            audio: Connection::permission("enable-audio"),
            // to-do: make sure is the option correct here
            file: Connection::permission(config::keys::OPTION_ENABLE_FILE_TRANSFER),
            restart: Connection::permission("enable-remote-restart"),
            recording: Connection::permission("enable-record-session"),
            block_input: Connection::permission("enable-block-input"),
            last_test_delay: None,
            network_delay: 0,
            lock_after_session_end: false,
            show_remote_cursor: false,
            follow_remote_cursor: false,
            follow_remote_window: false,
            multi_ui_session: false,
            ip: "".to_owned(),
            disable_audio: false,
            #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
            enable_file_transfer: false,
            disable_clipboard: false,
            disable_keyboard: false,
            tx_input,
            video_ack_required: false,
            server_audit_conn: "".to_owned(),
            server_audit_file: "".to_owned(),
            lr: Default::default(),
            last_recv_time: Arc::new(Mutex::new(Instant::now())),
            chat_unanswered: false,
            file_transferred: false,
            #[cfg(windows)]
            portable: Default::default(),
            from_switch: false,
            audio_sender: None,
            voice_call_request_timestamp: None,
            audio_input_device_before_voice_call: None,
            options_in_login: None,
            #[cfg(not(any(target_os = "ios")))]
            pressed_modifiers: Default::default(),
            #[cfg(target_os = "linux")]
            linux_headless_handle,
            closed: false,
            delay_response_instant: Instant::now(),
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            start_cm_ipc_para: Some(StartCmIpcPara {
                rx_to_cm,
                tx_from_cm,
                rx_desktop_ready,
                tx_cm_stream_ready,
            }),
            auto_disconnect_timer: None,
            authed_conn_id: None,
            file_remove_log_control: FileRemoveLogControl::new(id),
            last_supported_encoding: None,
            services_subed: false,
            delayed_read_dir: None,
            #[cfg(target_os = "macos")]
            retina: Retina::default(),
        };
        let addr = hbb_common::try_into_v4(addr);
        if !conn.on_open(addr).await {
            conn.closed = true;
            // sleep to ensure msg got received.
            sleep(1.).await;
            return;
        }
        #[cfg(target_os = "android")]
        start_channel(rx_to_cm, tx_from_cm);
        if !conn.keyboard {
            conn.send_permission(Permission::Keyboard, false).await;
        }
        if !conn.clipboard {
            conn.send_permission(Permission::Clipboard, false).await;
        }
        if !conn.audio {
            conn.send_permission(Permission::Audio, false).await;
        }
        if !conn.file {
            conn.send_permission(Permission::File, false).await;
        }
        if !conn.restart {
            conn.send_permission(Permission::Restart, false).await;
        }
        if !conn.recording {
            conn.send_permission(Permission::Recording, false).await;
        }
        if !conn.block_input {
            conn.send_permission(Permission::BlockInput, false).await;
        }
        let mut test_delay_timer =
            crate::rustdesk_interval(time::interval_at(Instant::now(), TEST_DELAY_TIMEOUT));
        let mut last_recv_time = Instant::now();

        conn.stream.set_send_timeout(
            if conn.file_transfer.is_some() || conn.port_forward_socket.is_some() {
                SEND_TIMEOUT_OTHER
            } else {
                SEND_TIMEOUT_VIDEO
            },
        );

        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        std::thread::spawn(move || Self::handle_input(_rx_input, tx_cloned));
        let mut second_timer = crate::rustdesk_interval(time::interval(Duration::from_secs(1)));

        loop {
            tokio::select! {
                // biased; // video has higher priority // causing test_delay_timer failed while transferring big file

                Some(data) = rx_from_cm.recv() => {
                    match data {
                        ipc::Data::Authorize => {
                            conn.require_2fa.take();
                            conn.send_logon_response().await;
                            if conn.port_forward_socket.is_some() {
                                break;
                            }
                        }
                        ipc::Data::Close => {
                            conn.chat_unanswered = false; // seen
                            conn.file_transferred = false; //seen
                            conn.send_close_reason_no_retry("").await;
                            conn.on_close("connection manager", true).await;
                            break;
                        }
                        ipc::Data::CmErr(e) => {
                            if e != "expected" {
                                // cm closed before connection
                                conn.on_close(&format!("connection manager error: {}", e), false).await;
                                break;
                            }
                        }
                        ipc::Data::ChatMessage{text} => {
                            let mut misc = Misc::new();
                            misc.set_chat_message(ChatMessage {
                                text,
                                ..Default::default()
                            });
                            let mut msg_out = Message::new();
                            msg_out.set_misc(misc);
                            conn.send(msg_out).await;
                            conn.chat_unanswered = false;
                        }
                        ipc::Data::SwitchPermission{name, enabled} => {
                            log::info!("Change permission {} -> {}", name, enabled);
                            if &name == "keyboard" {
                                conn.keyboard = enabled;
                                conn.send_permission(Permission::Keyboard, enabled).await;
                                if let Some(s) = conn.server.upgrade() {
                                    s.write().unwrap().subscribe(
                                        NAME_CURSOR,
                                        conn.inner.clone(), enabled || conn.show_remote_cursor);
                                }
                            } else if &name == "clipboard" {
                                conn.clipboard = enabled;
                                conn.send_permission(Permission::Clipboard, enabled).await;
                                if let Some(s) = conn.server.upgrade() {
                                    s.write().unwrap().subscribe(
                                        super::clipboard_service::NAME,
                                        conn.inner.clone(), conn.clipboard_enabled() && conn.peer_keyboard_enabled());
                                }
                            } else if &name == "audio" {
                                conn.audio = enabled;
                                conn.send_permission(Permission::Audio, enabled).await;
                                if let Some(s) = conn.server.upgrade() {
                                    s.write().unwrap().subscribe(
                                        super::audio_service::NAME,
                                        conn.inner.clone(), conn.audio_enabled());
                                }
                            } else if &name == "file" {
                                conn.file = enabled;
                                conn.send_permission(Permission::File, enabled).await;
                            } else if &name == "restart" {
                                conn.restart = enabled;
                                conn.send_permission(Permission::Restart, enabled).await;
                            } else if &name == "recording" {
                                conn.recording = enabled;
                                conn.send_permission(Permission::Recording, enabled).await;
                            } else if &name == "block_input" {
                                conn.block_input = enabled;
                                conn.send_permission(Permission::BlockInput, enabled).await;
                            }
                        }
                        ipc::Data::RawMessage(bytes) => {
                            allow_err!(conn.stream.send_raw(bytes).await);
                        }
                        #[cfg(any(target_os="windows", target_os="linux", target_os = "macos"))]
                        ipc::Data::ClipboardFile(clip) => {
                            allow_err!(conn.stream.send(&clip_2_msg(clip)).await);
                        }
                        ipc::Data::PrivacyModeState((_, state, impl_key)) => {
                            let msg_out = match state {
                                privacy_mode::PrivacyModeState::OffSucceeded => {
                                    crate::common::make_privacy_mode_msg(
                                        back_notification::PrivacyModeState::PrvOffSucceeded,
                                        impl_key,
                                    )
                                }
                                privacy_mode::PrivacyModeState::OffByPeer => {
                                    crate::common::make_privacy_mode_msg(
                                        back_notification::PrivacyModeState::PrvOffByPeer,
                                        impl_key,
                                    )
                                }
                                privacy_mode::PrivacyModeState::OffUnknown => {
                                     crate::common::make_privacy_mode_msg(
                                        back_notification::PrivacyModeState::PrvOffUnknown,
                                        impl_key,
                                    )
                                }
                            };
                            conn.send(msg_out).await;
                        }
                        #[cfg(windows)]
                        ipc::Data::DataPortableService(ipc::DataPortableService::RequestStart) => {
                            if let Err(e) = portable_client::start_portable_service(portable_client::StartPara::Direct) {
                                log::error!("Failed to start portable service from cm: {:?}", e);
                            }
                        }
                        ipc::Data::SwitchSidesBack => {
                            let mut misc = Misc::new();
                            misc.set_switch_back(SwitchBack::default());
                            let mut msg = Message::new();
                            msg.set_misc(misc);
                            conn.send(msg).await;
                        }
                        ipc::Data::VoiceCallResponse(accepted) => {
                            conn.handle_voice_call(accepted).await;
                        }
                        ipc::Data::CloseVoiceCall(_reason) => {
                            log::debug!("Close the voice call from the ipc.");
                            conn.close_voice_call().await;
                            // Notify the peer that we closed the voice call.
                            let msg = new_voice_call_request(false);
                            conn.send(msg).await;
                        }
                        _ => {}
                    }
                },
                res = conn.stream.next() => {
                    if let Some(res) = res {
                        match res {
                            Err(err) => {
                                conn.on_close(&err.to_string(), true).await;
                                break;
                            },
                            Ok(bytes) => {
                                last_recv_time = Instant::now();
                                *conn.last_recv_time.lock().unwrap() = Instant::now();
                                if let Ok(msg_in) = Message::parse_from_bytes(&bytes) {
                                    if !conn.on_message(msg_in).await {
                                        break;
                                    }
                                    if conn.port_forward_socket.is_some() && conn.authorized {
                                        log::info!("Port forward, last_test_delay is none: {}", conn.last_test_delay.is_none());
                                        // Avoid TestDelay reply injection into rdp data stream
                                        if conn.last_test_delay.is_none() {
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        conn.on_close("Reset by the peer", true).await;
                        break;
                    }
                },
                _ = conn.file_timer.tick() => {
                    if !conn.read_jobs.is_empty() {
                        conn.send_to_cm(ipc::Data::FileTransferLog(("transfer".to_string(), fs::serialize_transfer_jobs(&conn.read_jobs))));
                        match fs::handle_read_jobs(&mut conn.read_jobs, &mut conn.stream).await {
                            Ok(log) => {
                                if !log.is_empty() {
                                    conn.send_to_cm(ipc::Data::FileTransferLog(("transfer".to_string(), log)));
                                }
                            }
                            Err(err) =>  {
                                conn.on_close(&err.to_string(), false).await;
                                break;
                            }
                        }
                    } else {
                        conn.file_timer = crate::rustdesk_interval(time::interval_at(Instant::now() + SEC30, SEC30));
                    }
                }
                Ok(conns) = hbbs_rx.recv() => {
                    if conns.contains(&id) {
                        conn.send_close_reason_no_retry("Closed manually by web console").await;
                        conn.on_close("web console", true).await;
                        break;
                    }
                }
                Some((instant, value)) = rx_video.recv() => {
                    if !conn.video_ack_required {
                        video_service::notify_video_frame_fetched(id, Some(instant.into()));
                    }
                    if let Err(err) = conn.stream.send(&value as &Message).await {
                        conn.on_close(&err.to_string(), false).await;
                        break;
                    }
                },
                Some((instant, value)) = rx.recv() => {
                    let latency = instant.elapsed().as_millis() as i64;
                    #[allow(unused_mut)]
                    let mut msg = value;

                    if latency > 1000 {
                        match &msg.union {
                            Some(message::Union::AudioFrame(_)) => {
                                // log::info!("audio frame latency {}", instant.elapsed().as_secs_f32());
                                continue;
                            }
                            _ => {}
                        }
                    }
                    match &msg.union {
                        Some(message::Union::Misc(m)) => {
                            match &m.union {
                                Some(misc::Union::StopService(_)) => {
                                    conn.send_close_reason_no_retry("").await;
                                    conn.on_close("stop service", false).await;
                                    break;
                                }
                                _ => {},
                            }
                        }
                        Some(message::Union::PeerInfo(_pi)) => {
                            conn.refresh_video_display(None);
                            #[cfg(target_os = "macos")]
                            conn.retina.set_displays(&_pi.displays);
                        }
                        Some(message::Union::CursorPosition(pos)) => {
                            #[cfg(not(any(target_os = "android", target_os = "ios")))]
                            {
                                if conn.follow_remote_cursor {
                                    conn.handle_cursor_switch_display(pos.clone()).await;
                                }
                            }
                            #[cfg(target_os = "macos")]
                            if let Some(new_msg) = conn.retina.on_cursor_pos(&pos, conn.display_idx) {
                                msg = Arc::new(new_msg);
                            }
                        }
                        _ => {}
                    }
                    let msg: &Message = &msg;
                    if let Err(err) = conn.stream.send(msg).await {
                        conn.on_close(&err.to_string(), false).await;
                        break;
                    }
                },
                _ = second_timer.tick() => {
                    #[cfg(windows)]
                    conn.portable_check();
                    if let Some((instant, minute)) = conn.auto_disconnect_timer.as_ref() {
                        if instant.elapsed().as_secs() > minute * 60 {
                            conn.send_close_reason_no_retry("Connection failed due to inactivity").await;
                            conn.on_close("auto disconnect", true).await;
                            break;
                        }
                    }
                    conn.file_remove_log_control.on_timer().drain(..).map(|x| conn.send_to_cm(x)).count();
                    #[cfg(feature = "hwcodec")]
                    conn.update_supported_encoding();
                }
                _ = test_delay_timer.tick() => {
                    if last_recv_time.elapsed() >= SEC30 {
                        conn.on_close("Timeout", true).await;
                        break;
                    }
                    // The control end will jump out of the loop after receiving LoginResponse and will not reply to the TestDelay
                    if conn.last_test_delay.is_none() && !(conn.port_forward_socket.is_some() && conn.authorized) {
                        conn.last_test_delay = Some(Instant::now());
                        let mut msg_out = Message::new();
                        msg_out.set_test_delay(TestDelay{
                            last_delay: conn.network_delay,
                            target_bitrate: video_service::VIDEO_QOS.lock().unwrap().bitrate(),
                            ..Default::default()
                        });
                        conn.send(msg_out.into()).await;
                    }
                    video_service::VIDEO_QOS.lock().unwrap().user_delay_response_elapsed(conn.inner.id(), conn.delay_response_instant.elapsed().as_millis());
                }
            }
        }

        if let Some(video_privacy_conn_id) = privacy_mode::get_privacy_mode_conn_id() {
            if video_privacy_conn_id == id {
                let _ = Self::turn_off_privacy_to_msg(id);
            }
        }
        #[cfg(all(feature = "flutter", feature = "plugin_framework"))]
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        crate::plugin::handle_listen_event(
            crate::plugin::EVENT_ON_CONN_CLOSE_SERVER.to_owned(),
            conn.lr.my_id.clone(),
        );
        video_service::notify_video_frame_fetched(id, None);
        if conn.authorized {
            password::update_temporary_password();
        }
        if let Err(err) = conn.try_port_forward_loop(&mut rx_from_cm).await {
            conn.on_close(&err.to_string(), false).await;
        }

        conn.post_conn_audit(json!({
            "action": "close",
        }));
        if let Some(s) = conn.server.upgrade() {
            let mut s = s.write().unwrap();
            s.remove_connection(&conn.inner);
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            try_stop_record_cursor_pos();
        }
        conn.on_close("End", true).await;
        log::info!("#{} connection loop exited", id);
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    fn handle_input(receiver: std_mpsc::Receiver<MessageInput>, tx: Sender) {
        let mut block_input_mode = false;
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        {
            rdev::set_mouse_extra_info(enigo::ENIGO_INPUT_EXTRA_VALUE);
            rdev::set_keyboard_extra_info(enigo::ENIGO_INPUT_EXTRA_VALUE);
        }
        #[cfg(target_os = "macos")]
        reset_input_ondisconn();
        loop {
            match receiver.recv_timeout(std::time::Duration::from_millis(500)) {
                Ok(v) => match v {
                    MessageInput::Mouse((msg, id)) => {
                        handle_mouse(&msg, id);
                    }
                    MessageInput::Key((mut msg, press)) => {
                        // todo: press and down have similar meanings.
                        if press && msg.mode.enum_value() == Ok(KeyboardMode::Legacy) {
                            msg.down = true;
                        }
                        handle_key(&msg);
                        if press && msg.mode.enum_value() == Ok(KeyboardMode::Legacy) {
                            msg.down = false;
                            handle_key(&msg);
                        }
                    }
                    MessageInput::Pointer((msg, id)) => {
                        handle_pointer(&msg, id);
                    }
                    MessageInput::BlockOn => {
                        let (ok, msg) = crate::platform::block_input(true);
                        if ok {
                            block_input_mode = true;
                        } else {
                            Self::send_block_input_error(
                                &tx,
                                back_notification::BlockInputState::BlkOnFailed,
                                msg,
                            );
                        }
                    }
                    MessageInput::BlockOff => {
                        let (ok, msg) = crate::platform::block_input(false);
                        if ok {
                            block_input_mode = false;
                        } else {
                            Self::send_block_input_error(
                                &tx,
                                back_notification::BlockInputState::BlkOffFailed,
                                msg,
                            );
                        }
                    }
                    #[cfg(all(feature = "flutter", feature = "plugin_framework"))]
                    #[cfg(not(any(target_os = "android", target_os = "ios")))]
                    MessageInput::BlockOnPlugin(_peer) => {
                        let (ok, _msg) = crate::platform::block_input(true);
                        if ok {
                            block_input_mode = true;
                        }
                        let _r = PLUGIN_BLOCK_INPUT_TX_RX
                            .0
                            .lock()
                            .unwrap()
                            .send(block_input_mode);
                    }
                    #[cfg(all(feature = "flutter", feature = "plugin_framework"))]
                    #[cfg(not(any(target_os = "android", target_os = "ios")))]
                    MessageInput::BlockOffPlugin(_peer) => {
                        let (ok, _msg) = crate::platform::block_input(false);
                        if ok {
                            block_input_mode = false;
                        }
                        let _r = PLUGIN_BLOCK_INPUT_TX_RX
                            .0
                            .lock()
                            .unwrap()
                            .send(block_input_mode);
                    }
                },
                Err(err) => {
                    #[cfg(not(any(target_os = "android", target_os = "ios")))]
                    if block_input_mode {
                        let _ = crate::platform::block_input(true);
                    }
                    if std_mpsc::RecvTimeoutError::Disconnected == err {
                        break;
                    }
                }
            }
        }
        #[cfg(target_os = "linux")]
        clear_remapped_keycode();
        log::info!("Input thread exited");
    }

    async fn try_port_forward_loop(
        &mut self,
        rx_from_cm: &mut mpsc::UnboundedReceiver<Data>,
    ) -> ResultType<()> {
        let mut last_recv_time = Instant::now();
        if let Some(mut forward) = self.port_forward_socket.take() {
            log::info!("Running port forwarding loop");
            self.stream.set_raw();
            let mut hbbs_rx = crate::hbbs_http::sync::signal_receiver();
            loop {
                tokio::select! {
                    Some(data) = rx_from_cm.recv() => {
                        match data {
                            ipc::Data::Close => {
                                bail!("Close requested from connection manager");
                            }
                            ipc::Data::CmErr(e) => {
                                log::error!("Connection manager error: {e}");
                                bail!("{e}");
                            }
                            _ => {}
                        }
                    }
                    res = forward.next() => {
                        if let Some(res) = res {
                            last_recv_time = Instant::now();
                            self.stream.send_bytes(res?.into()).await?;
                        } else {
                            bail!("Forward reset by the peer");
                        }
                    },
                    res = self.stream.next() => {
                        if let Some(res) = res {
                            last_recv_time = Instant::now();
                            timeout(SEND_TIMEOUT_OTHER, forward.send(res?)).await??;
                        } else {
                            bail!("Stream reset by the peer");
                        }
                    },
                    _ = self.timer.tick() => {
                        if last_recv_time.elapsed() >= H1 {
                            bail!("Timeout");
                        }
                    }
                    Ok(conns) = hbbs_rx.recv() => {
                        if conns.contains(&self.inner.id) {
                            // todo: check reconnect
                            bail!("Closed manually by the web console");
                        }
                    }
                }
            }
        }
        Ok(())
    }

    async fn send_permission(&mut self, permission: Permission, enabled: bool) {
        let mut misc = Misc::new();
        misc.set_permission_info(PermissionInfo {
            permission: permission.into(),
            enabled,
            ..Default::default()
        });
        let mut msg_out = Message::new();
        msg_out.set_misc(misc);
        self.send(msg_out).await;
    }

    async fn check_privacy_mode_on(&mut self) -> bool {
        if privacy_mode::is_in_privacy_mode() {
            self.send_login_error("Someone turns on privacy mode, exit")
                .await;
            false
        } else {
            true
        }
    }

    async fn check_whitelist(&mut self, addr: &SocketAddr) -> bool {
        let whitelist: Vec<String> = Config::get_option("whitelist")
            .split(",")
            .filter(|x| !x.is_empty())
            .map(|x| x.to_owned())
            .collect();
        if !whitelist.is_empty()
            && whitelist
                .iter()
                .filter(|x| x == &"0.0.0.0")
                .next()
                .is_none()
            && whitelist
                .iter()
                .filter(|x| IpCidr::from_str(x).map_or(false, |y| y.contains(addr.ip())))
                .next()
                .is_none()
        {
            self.send_login_error("Your ip is blocked by the peer")
                .await;
            Self::post_alarm_audit(
                AlarmAuditType::IpWhitelist, //"ip whitelist",
                json!({ "ip":addr.ip() }),
            );
            return false;
        }
        true
    }

    async fn on_open(&mut self, addr: SocketAddr) -> bool {
        log::debug!("#{} Connection opened from {}.", self.inner.id, addr);
        if !self.check_whitelist(&addr).await {
            return false;
        }
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        if crate::is_server() && Config::get_option("allow-only-conn-window-open") == "Y" {
            if !crate::check_process("", !crate::platform::is_root()) {
                self.send_login_error("The main window is not open").await;
                return false;
            }
        }
        self.ip = addr.ip().to_string();
        let mut msg_out = Message::new();
        msg_out.set_hash(self.hash.clone());
        self.send(msg_out).await;
        self.get_api_server();
        self.post_conn_audit(json!({
            "ip": addr.ip(),
            "action": "new",
        }));
        true
    }

    fn get_api_server(&mut self) {
        self.server_audit_conn = crate::get_audit_server(
            Config::get_option("api-server"),
            Config::get_option("custom-rendezvous-server"),
            "conn".to_owned(),
        );
        self.server_audit_file = crate::get_audit_server(
            Config::get_option("api-server"),
            Config::get_option("custom-rendezvous-server"),
            "file".to_owned(),
        );
    }

    fn post_conn_audit(&self, v: Value) {
        if self.server_audit_conn.is_empty() {
            return;
        }
        let url = self.server_audit_conn.clone();
        let mut v = v;
        v["id"] = json!(Config::get_id());
        v["uuid"] = json!(crate::encode64(hbb_common::get_uuid()));
        v["conn_id"] = json!(self.inner.id);
        v["session_id"] = json!(self.lr.session_id);
        tokio::spawn(async move {
            allow_err!(Self::post_audit_async(url, v).await);
        });
    }

    fn post_file_audit(
        &self,
        r#type: FileAuditType,
        path: &str,
        files: Vec<(String, i64)>,
        info: Value,
    ) {
        if self.server_audit_file.is_empty() {
            return;
        }
        let url = self.server_audit_file.clone();
        let file_num = files.len();
        let mut files = files;
        files.sort_by(|a, b| b.1.cmp(&a.1));
        files.truncate(10);
        let is_file = files.len() == 1 && files[0].0.is_empty();
        let mut info = info;
        info["ip"] = json!(self.ip.clone());
        info["name"] = json!(self.lr.my_name.clone());
        info["num"] = json!(file_num);
        info["files"] = json!(files);
        let v = json!({
            "id":json!(Config::get_id()),
            "uuid":json!(crate::encode64(hbb_common::get_uuid())),
            "peer_id":json!(self.lr.my_id),
            "type": r#type as i8,
            "path":path,
            "is_file":is_file,
            "info":json!(info).to_string(),
        });
        tokio::spawn(async move {
            allow_err!(Self::post_audit_async(url, v).await);
        });
    }

    pub fn post_alarm_audit(typ: AlarmAuditType, info: Value) {
        let url = crate::get_audit_server(
            Config::get_option("api-server"),
            Config::get_option("custom-rendezvous-server"),
            "alarm".to_owned(),
        );
        if url.is_empty() {
            return;
        }
        let mut v = Value::default();
        v["id"] = json!(Config::get_id());
        v["uuid"] = json!(crate::encode64(hbb_common::get_uuid()));
        v["typ"] = json!(typ as i8);
        v["info"] = serde_json::Value::String(info.to_string());
        tokio::spawn(async move {
            allow_err!(Self::post_audit_async(url, v).await);
        });
    }

    #[inline]
    async fn post_audit_async(url: String, v: Value) -> ResultType<String> {
        crate::post_request(url, v.to_string(), "").await
    }

    async fn send_logon_response(&mut self) {
        if self.authorized {
            return;
        }
        if self.require_2fa.is_some() && !self.is_recent_session(true) && !self.from_switch {
            self.send_login_error(crate::client::REQUIRE_2FA).await;
            return;
        }
        self.authorized = true;
        let (conn_type, auth_conn_type) = if self.file_transfer.is_some() {
            (1, AuthConnType::FileTransfer)
        } else if self.port_forward_socket.is_some() {
            (2, AuthConnType::PortForward)
        } else {
            (0, AuthConnType::Remote)
        };
        self.authed_conn_id = Some(self::raii::AuthedConnID::new(
            self.inner.id(),
            auth_conn_type,
        ));
        self.post_conn_audit(
            json!({"peer": ((&self.lr.my_id, &self.lr.my_name)), "type": conn_type}),
        );
        #[allow(unused_mut)]
        let mut username = crate::platform::get_active_username();
        let mut res = LoginResponse::new();
        let mut pi = PeerInfo {
            username: username.clone(),
            version: VERSION.to_owned(),
            ..Default::default()
        };

        #[cfg(not(target_os = "android"))]
        {
            pi.hostname = whoami::hostname();
            pi.platform = whoami::platform().to_string();
        }
        #[cfg(target_os = "android")]
        {
            pi.hostname = DEVICE_NAME.lock().unwrap().clone();
            pi.platform = "Android".into();
        }
        #[cfg(all(target_os = "macos", not(feature = "unix-file-copy-paste")))]
        let platform_additions = serde_json::Map::new();
        #[cfg(any(
            target_os = "windows",
            target_os = "linux",
            all(target_os = "macos", feature = "unix-file-copy-paste")
        ))]
        let mut platform_additions = serde_json::Map::new();
        #[cfg(target_os = "linux")]
        {
            if crate::platform::current_is_wayland() {
                platform_additions.insert("is_wayland".into(), json!(true));
            }
            #[cfg(target_os = "linux")]
            if crate::platform::is_headless_allowed() {
                if linux_desktop_manager::is_headless() {
                    platform_additions.insert("headless".into(), json!(true));
                }
            }
        }
        #[cfg(target_os = "windows")]
        {
            platform_additions.insert(
                "is_installed".into(),
                json!(crate::platform::is_installed()),
            );
            if crate::platform::is_installed() {
                platform_additions.extend(virtual_display_manager::get_platform_additions());
            }
            platform_additions.insert(
                "supported_privacy_mode_impl".into(),
                json!(privacy_mode::get_supported_privacy_mode_impl()),
            );
        }

        #[cfg(any(
            target_os = "windows",
            all(
                any(target_os = "linux", target_os = "macos"),
                feature = "unix-file-copy-paste"
            )
        ))]
        {
            platform_additions.insert("has_file_clipboard".into(), json!(true));
        }

        #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
        if !platform_additions.is_empty() {
            pi.platform_additions = serde_json::to_string(&platform_additions).unwrap_or("".into());
        }

        if self.port_forward_socket.is_some() {
            let mut msg_out = Message::new();
            res.set_peer_info(pi);
            msg_out.set_login_response(res);
            self.send(msg_out).await;
            return;
        }
        #[cfg(target_os = "linux")]
        if !self.file_transfer.is_some() && !self.port_forward_socket.is_some() {
            let mut msg = "".to_string();
            if crate::platform::linux::is_login_screen_wayland() {
                msg = crate::client::LOGIN_SCREEN_WAYLAND.to_owned()
            } else {
                let dtype = crate::platform::linux::get_display_server();
                if dtype != crate::platform::linux::DISPLAY_SERVER_X11
                    && dtype != crate::platform::linux::DISPLAY_SERVER_WAYLAND
                {
                    msg = format!(
                        "Unsupported display server type \"{}\", x11 or wayland expected",
                        dtype
                    );
                }
            }
            if !msg.is_empty() {
                res.set_error(msg);
                let mut msg_out = Message::new();
                msg_out.set_login_response(res);
                self.send(msg_out).await;
                return;
            }
        }
        #[allow(unused_mut)]
        let mut sas_enabled = false;
        #[cfg(windows)]
        if crate::platform::is_root() {
            sas_enabled = true;
        }
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        if self.file_transfer.is_some() {
            if crate::platform::is_prelogin() || self.tx_to_cm.send(ipc::Data::Test).is_err() {
                username = "".to_owned();
            }
        }
        #[cfg(all(feature = "flutter", feature = "plugin_framework"))]
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        PLUGIN_BLOCK_INPUT_TXS
            .lock()
            .unwrap()
            .insert(self.lr.my_id.clone(), self.tx_input.clone());

        pi.username = username;
        pi.sas_enabled = sas_enabled;
        pi.features = Some(Features {
            privacy_mode: privacy_mode::is_privacy_mode_supported(),
            ..Default::default()
        })
        .into();

        let mut sub_service = false;
        #[allow(unused_mut)]
        let mut wait_session_id_confirm = false;
        #[cfg(windows)]
        self.handle_windows_specific_session(&mut pi, &mut wait_session_id_confirm);
        if self.file_transfer.is_some() {
            res.set_peer_info(pi);
        } else {
            let supported_encoding = scrap::codec::Encoder::supported_encoding();
            self.last_supported_encoding = Some(supported_encoding.clone());
            log::info!("peer info supported_encoding: {:?}", supported_encoding);
            pi.encoding = Some(supported_encoding).into();
            if let Some(msg_out) = super::display_service::is_inited_msg() {
                self.send(msg_out).await;
            }

            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            {
                #[cfg(not(windows))]
                let displays = display_service::try_get_displays();
                #[cfg(windows)]
                let displays = display_service::try_get_displays_add_amyuni_headless();
                pi.resolutions = Some(SupportedResolutions {
                    resolutions: displays
                        .map(|displays| {
                            displays
                                .get(self.display_idx)
                                .map(|d| crate::platform::resolutions(&d.name()))
                                .unwrap_or(vec![])
                        })
                        .unwrap_or(vec![]),
                    ..Default::default()
                })
                .into();
            }

            try_activate_screen();

            match super::display_service::update_get_sync_displays().await {
                Err(err) => {
                    res.set_error(format!("{}", err));
                }
                Ok(displays) => {
                    // For compatibility with old versions, we need to send the displays to the peer.
                    // But the displays may be updated later, before creating the video capturer.
                    #[cfg(target_os = "macos")]
                    {
                        self.retina.set_displays(&displays);
                    }
                    pi.displays = displays;
                    pi.current_display = self.display_idx as _;
                    res.set_peer_info(pi);
                    sub_service = true;

                    #[cfg(target_os = "linux")]
                    {
                        // use rdp_input when uinput is not available in wayland. Ex: flatpak
                        if !is_x11() && !crate::is_server() {
                            let _ = setup_rdp_input().await;
                        }
                    }
                }
            }
            self.on_remote_authorized();
        }
        let mut msg_out = Message::new();
        msg_out.set_login_response(res);
        self.send(msg_out).await;
        if let Some(o) = self.options_in_login.take() {
            self.update_options(&o).await;
        }
        if let Some((dir, show_hidden)) = self.file_transfer.clone() {
            let dir = if !dir.is_empty() && std::path::Path::new(&dir).is_dir() {
                &dir
            } else {
                ""
            };
            if !wait_session_id_confirm {
                self.read_dir(dir, show_hidden);
            } else {
                self.delayed_read_dir = Some((dir.to_owned(), show_hidden));
            }
        } else if sub_service {
            if !wait_session_id_confirm {
                self.try_sub_services();
            }
        }
    }

    fn try_sub_services(&mut self) {
        let is_remote = self.file_transfer.is_none() && self.port_forward_socket.is_none();
        if is_remote && !self.services_subed {
            self.services_subed = true;
            if let Some(s) = self.server.upgrade() {
                let mut noperms = Vec::new();
                if !self.peer_keyboard_enabled() && !self.show_remote_cursor {
                    noperms.push(NAME_CURSOR);
                }
                if !self.show_remote_cursor {
                    noperms.push(NAME_POS);
                }
                if !self.follow_remote_window {
                    noperms.push(NAME_WINDOW_FOCUS);
                }
                if !self.clipboard_enabled() || !self.peer_keyboard_enabled() {
                    noperms.push(super::clipboard_service::NAME);
                }
                if !self.audio_enabled() {
                    noperms.push(super::audio_service::NAME);
                }
                let mut s = s.write().unwrap();
                #[cfg(not(any(target_os = "android", target_os = "ios")))]
                let _h = try_start_record_cursor_pos();
                self.auto_disconnect_timer = Self::get_auto_disconenct_timer();
                s.try_add_primay_video_service();
                s.add_connection(self.inner.clone(), &noperms);
            }
        }
    }

    #[cfg(windows)]
    fn handle_windows_specific_session(
        &mut self,
        pi: &mut PeerInfo,
        wait_session_id_confirm: &mut bool,
    ) {
        let sessions = crate::platform::get_available_sessions(true);
        if let Some(current_sid) = crate::platform::get_current_process_session_id() {
            if crate::platform::is_installed()
                && crate::platform::is_share_rdp()
                && raii::AuthedConnID::remote_and_file_conn_count() == 1
                && sessions.len() > 1
                && sessions.iter().any(|e| e.sid == current_sid)
                && get_version_number(&self.lr.version) >= get_version_number("1.2.4")
            {
                pi.windows_sessions = Some(WindowsSessions {
                    sessions,
                    current_sid,
                    ..Default::default()
                })
                .into();
                *wait_session_id_confirm = true;
            }
        }
    }

    fn on_remote_authorized(&self) {
        self.update_codec_on_login();
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        if !Config::get_option("allow-remove-wallpaper").is_empty() {
            // multi connections set once
            let mut wallpaper = WALLPAPER_REMOVER.lock().unwrap();
            if wallpaper.is_none() {
                match crate::platform::WallPaperRemover::new() {
                    Ok(remover) => {
                        *wallpaper = Some(remover);
                    }
                    Err(e) => {
                        log::info!("create wallpaper remover failed: {:?}", e);
                    }
                }
            }
        }
    }

    fn peer_keyboard_enabled(&self) -> bool {
        self.keyboard && !self.disable_keyboard
    }

    fn clipboard_enabled(&self) -> bool {
        self.clipboard && !self.disable_clipboard
    }

    fn audio_enabled(&self) -> bool {
        self.audio && !self.disable_audio
    }

    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    fn file_transfer_enabled(&self) -> bool {
        self.file && self.enable_file_transfer
    }

    fn try_start_cm(&mut self, peer_id: String, name: String, authorized: bool) {
        self.send_to_cm(ipc::Data::Login {
            id: self.inner.id(),
            is_file_transfer: self.file_transfer.is_some(),
            port_forward: self.port_forward_address.clone(),
            peer_id,
            name,
            authorized,
            keyboard: self.keyboard,
            clipboard: self.clipboard,
            audio: self.audio,
            file: self.file,
            file_transfer_enabled: self.file,
            restart: self.restart,
            recording: self.recording,
            block_input: self.block_input,
            from_switch: self.from_switch,
        });
    }

    #[inline]
    fn send_to_cm(&mut self, data: ipc::Data) {
        self.tx_to_cm.send(data).ok();
    }

    #[inline]
    fn send_fs(&mut self, data: ipc::FS) {
        self.send_to_cm(ipc::Data::FS(data));
    }

    async fn send_login_error<T: std::string::ToString>(&mut self, err: T) {
        let mut msg_out = Message::new();
        let mut res = LoginResponse::new();
        res.set_error(err.to_string());
        msg_out.set_login_response(res);
        self.send(msg_out).await;
    }

    #[inline]
    pub fn send_block_input_error(
        s: &Sender,
        state: back_notification::BlockInputState,
        details: String,
    ) {
        let mut misc = Misc::new();
        let mut back_notification = BackNotification {
            details,
            ..Default::default()
        };
        back_notification.set_block_input_state(state);
        misc.set_back_notification(back_notification);
        let mut msg_out = Message::new();
        msg_out.set_misc(misc);
        s.send((Instant::now(), Arc::new(msg_out))).ok();
    }

    #[inline]
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    fn input_mouse(&self, msg: MouseEvent, conn_id: i32) {
        self.tx_input.send(MessageInput::Mouse((msg, conn_id))).ok();
    }

    #[inline]
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    fn input_pointer(&self, msg: PointerDeviceEvent, conn_id: i32) {
        self.tx_input
            .send(MessageInput::Pointer((msg, conn_id)))
            .ok();
    }

    #[inline]
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    fn input_key(&self, msg: KeyEvent, press: bool) {
        // to-do: if is the legacy mode, and the key is function key "LockScreen".
        // Switch to the primary display.
        self.tx_input.send(MessageInput::Key((msg, press))).ok();
    }

    fn validate_one_password(&self, password: String) -> bool {
        if password.len() == 0 {
            return false;
        }
        let mut hasher = Sha256::new();
        hasher.update(password);
        hasher.update(&self.hash.salt);
        let mut hasher2 = Sha256::new();
        hasher2.update(&hasher.finalize()[..]);
        hasher2.update(&self.hash.challenge);
        hasher2.finalize()[..] == self.lr.password[..]
    }

    fn validate_password(&mut self) -> bool {
        if password::temporary_enabled() {
            let password = password::temporary_password();
            if self.validate_one_password(password.clone()) {
                SESSIONS.lock().unwrap().insert(
                    self.lr.my_id.clone(),
                    Session {
                        name: self.lr.my_name.clone(),
                        session_id: self.lr.session_id,
                        last_recv_time: self.last_recv_time.clone(),
                        random_password: password,
                        tfa: false,
                    },
                );
                return true;
            }
        }
        if password::permanent_enabled() {
            if self.validate_one_password(Config::get_permanent_password()) {
                return true;
            }
        }
        false
    }

    fn is_recent_session(&mut self, tfa: bool) -> bool {
        SESSIONS
            .lock()
            .unwrap()
            .retain(|_, s| s.last_recv_time.lock().unwrap().elapsed() < SESSION_TIMEOUT);
        let session = SESSIONS
            .lock()
            .unwrap()
            .get(&self.lr.my_id)
            .map(|s| s.to_owned());
        // last_recv_time is a mutex variable shared with connection, can be updated lively.
        if let Some(mut session) = session {
            if session.name == self.lr.my_name
                && session.session_id == self.lr.session_id
                && !self.lr.password.is_empty()
                && (tfa && session.tfa
                    || !tfa && self.validate_one_password(session.random_password.clone()))
            {
                session.last_recv_time = self.last_recv_time.clone();
                SESSIONS
                    .lock()
                    .unwrap()
                    .insert(self.lr.my_id.clone(), session);
                return true;
            }
        }
        false
    }

    pub fn permission(enable_prefix_option: &str) -> bool {
        #[cfg(feature = "flutter")]
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        {
            let access_mode = Config::get_option("access-mode");
            if access_mode == "full" {
                return true;
            } else if access_mode == "view" {
                return false;
            }
        }
        return Config::get_option(enable_prefix_option).is_empty();
    }

    fn update_codec_on_login(&self) {
        use scrap::codec::{Encoder, EncodingUpdate::*};
        if let Some(o) = self.lr.clone().option.as_ref() {
            if let Some(q) = o.supported_decoding.clone().take() {
                Encoder::update(Update(self.inner.id(), q));
            } else {
                Encoder::update(NewOnlyVP9(self.inner.id()));
            }
        } else {
            Encoder::update(NewOnlyVP9(self.inner.id()));
        }
    }

    async fn handle_login_request_without_validation(&mut self, lr: &LoginRequest) {
        self.lr = lr.clone();
        if let Some(o) = lr.option.as_ref() {
            self.options_in_login = Some(o.clone());
        }
        self.video_ack_required = lr.video_ack_required;
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    fn try_start_cm_ipc(&mut self) {
        if let Some(p) = self.start_cm_ipc_para.take() {
            tokio::spawn(async move {
                #[cfg(windows)]
                let tx_from_cm_clone = p.tx_from_cm.clone();
                if let Err(err) = start_ipc(
                    p.rx_to_cm,
                    p.tx_from_cm,
                    p.rx_desktop_ready,
                    p.tx_cm_stream_ready,
                )
                .await
                {
                    log::error!("ipc to connection manager exit: {}", err);
                    #[cfg(windows)]
                    if !crate::platform::is_prelogin() {
                        allow_err!(tx_from_cm_clone.send(Data::CmErr(err.to_string())));
                    }
                }
            });
            #[cfg(all(windows, feature = "flutter"))]
            std::thread::spawn(move || {
                if crate::is_server() && !crate::check_process("--tray", false) {
                    crate::platform::run_as_user(vec!["--tray"]).ok();
                }
            });
        }
    }

    async fn on_message(&mut self, msg: Message) -> bool {
        if let Some(message::Union::LoginRequest(lr)) = msg.union {
            self.handle_login_request_without_validation(&lr).await;
            if self.authorized {
                return true;
            }
            match lr.union {
                Some(login_request::Union::FileTransfer(ft)) => {
                    if !Connection::permission(config::keys::OPTION_ENABLE_FILE_TRANSFER) {
                        self.send_login_error("No permission of file transfer")
                            .await;
                        sleep(1.).await;
                        return false;
                    }
                    self.file_transfer = Some((ft.dir, ft.show_hidden));
                }
                Some(login_request::Union::PortForward(mut pf)) => {
                    if !Connection::permission("enable-tunnel") {
                        self.send_login_error("No permission of IP tunneling").await;
                        sleep(1.).await;
                        return false;
                    }
                    let mut is_rdp = false;
                    if pf.host == "RDP" && pf.port == 0 {
                        pf.host = "localhost".to_owned();
                        pf.port = 3389;
                        is_rdp = true;
                    }
                    if pf.host.is_empty() {
                        pf.host = "localhost".to_owned();
                    }
                    let mut addr = format!("{}:{}", pf.host, pf.port);
                    self.port_forward_address = addr.clone();
                    match timeout(3000, TcpStream::connect(&addr)).await {
                        Ok(Ok(sock)) => {
                            self.port_forward_socket = Some(Framed::new(sock, BytesCodec::new()));
                        }
                        _ => {
                            if is_rdp {
                                addr = "RDP".to_owned();
                            }
                            self.send_login_error(format!(
                                "Failed to access remote {}, please make sure if it is open",
                                addr
                            ))
                            .await;
                            return false;
                        }
                    }
                }
                _ => {
                    if !self.check_privacy_mode_on().await {
                        return false;
                    }
                }
            }

            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            self.try_start_cm_ipc();

            #[cfg(not(target_os = "linux"))]
            let err_msg = "".to_owned();
            #[cfg(target_os = "linux")]
            let err_msg = self
                .linux_headless_handle
                .try_start_desktop(lr.os_login.as_ref());

            // If err is LOGIN_MSG_DESKTOP_SESSION_NOT_READY, just keep this msg and go on checking password.
            if !err_msg.is_empty() && err_msg != crate::client::LOGIN_MSG_DESKTOP_SESSION_NOT_READY
            {
                self.send_login_error(err_msg).await;
                return true;
            }

            if !hbb_common::is_ip_str(&lr.username)
                && !hbb_common::is_domain_port_str(&lr.username)
                && lr.username != Config::get_id()
            {
                self.send_login_error(crate::client::LOGIN_MSG_OFFLINE)
                    .await;
                return false;
            } else if password::approve_mode() == ApproveMode::Click
                || password::approve_mode() == ApproveMode::Both && !password::has_valid_password()
            {
                self.try_start_cm(lr.my_id, lr.my_name, false);
                if hbb_common::get_version_number(&lr.version)
                    >= hbb_common::get_version_number("1.2.0")
                {
                    self.send_login_error(crate::client::LOGIN_MSG_NO_PASSWORD_ACCESS)
                        .await;
                }
                return true;
            } else if password::approve_mode() == ApproveMode::Password
                && !password::has_valid_password()
            {
                self.send_login_error("Connection not allowed").await;
                return false;
            } else if self.is_recent_session(false) {
                if err_msg.is_empty() {
                    #[cfg(target_os = "linux")]
                    self.linux_headless_handle.wait_desktop_cm_ready().await;
                    self.send_logon_response().await;
                    self.try_start_cm(lr.my_id.clone(), lr.my_name.clone(), self.authorized);
                } else {
                    self.send_login_error(err_msg).await;
                }
            } else if lr.password.is_empty() {
                if err_msg.is_empty() {
                    self.try_start_cm(lr.my_id, lr.my_name, false);
                } else {
                    self.send_login_error(
                        crate::client::LOGIN_MSG_DESKTOP_SESSION_NOT_READY_PASSWORD_EMPTY,
                    )
                    .await;
                }
            } else {
                let (failure, res) = self.check_failure(0).await;
                if !res {
                    return true;
                }
                if !self.validate_password() {
                    self.update_failure(failure, false, 0);
                    if err_msg.is_empty() {
                        self.send_login_error(crate::client::LOGIN_MSG_PASSWORD_WRONG)
                            .await;
                        self.try_start_cm(lr.my_id, lr.my_name, false);
                    } else {
                        self.send_login_error(
                            crate::client::LOGIN_MSG_DESKTOP_SESSION_NOT_READY_PASSWORD_WRONG,
                        )
                        .await;
                    }
                } else {
                    self.update_failure(failure, true, 0);
                    if err_msg.is_empty() {
                        #[cfg(target_os = "linux")]
                        self.linux_headless_handle.wait_desktop_cm_ready().await;
                        self.send_logon_response().await;
                        self.try_start_cm(lr.my_id, lr.my_name, self.authorized);
                    } else {
                        self.send_login_error(err_msg).await;
                    }
                }
            }
        } else if let Some(message::Union::Auth2fa(tfa)) = msg.union {
            let (failure, res) = self.check_failure(1).await;
            if !res {
                return true;
            }
            if let Some(totp) = self.require_2fa.as_ref() {
                if let Ok(res) = totp.check_current(&tfa.code) {
                    if res {
                        self.update_failure(failure, true, 1);
                        self.require_2fa.take();
                        self.send_logon_response().await;
                        self.try_start_cm(
                            self.lr.my_id.to_owned(),
                            self.lr.my_name.to_owned(),
                            self.authorized,
                        );
                        let session = SESSIONS
                            .lock()
                            .unwrap()
                            .get(&self.lr.my_id)
                            .map(|s| s.to_owned());
                        if let Some(mut session) = session {
                            session.tfa = true;
                            SESSIONS
                                .lock()
                                .unwrap()
                                .insert(self.lr.my_id.clone(), session);
                        } else {
                            SESSIONS.lock().unwrap().insert(
                                self.lr.my_id.clone(),
                                Session {
                                    name: self.lr.my_name.clone(),
                                    session_id: self.lr.session_id,
                                    last_recv_time: self.last_recv_time.clone(),
                                    random_password: "".to_owned(),
                                    tfa: true,
                                },
                            );
                        }
                    } else {
                        self.update_failure(failure, false, 1);
                        self.send_login_error(crate::client::LOGIN_MSG_2FA_WRONG)
                            .await;
                    }
                }
            }
        } else if let Some(message::Union::TestDelay(t)) = msg.union {
            if t.from_client {
                let mut msg_out = Message::new();
                msg_out.set_test_delay(t);
                self.inner.send(msg_out.into());
            } else {
                if let Some(tm) = self.last_test_delay {
                    self.last_test_delay = None;
                    let new_delay = tm.elapsed().as_millis() as u32;
                    video_service::VIDEO_QOS
                        .lock()
                        .unwrap()
                        .user_network_delay(self.inner.id(), new_delay);
                    self.network_delay = new_delay;
                }
                self.delay_response_instant = Instant::now();
            }
        } else if let Some(message::Union::SwitchSidesResponse(_s)) = msg.union {
            #[cfg(feature = "flutter")]
            if let Some(lr) = _s.lr.clone().take() {
                self.handle_login_request_without_validation(&lr).await;
                SWITCH_SIDES_UUID
                    .lock()
                    .unwrap()
                    .retain(|_, v| v.0.elapsed() < Duration::from_secs(10));
                let uuid_old = SWITCH_SIDES_UUID.lock().unwrap().remove(&lr.my_id);
                if let Ok(uuid) = uuid::Uuid::from_slice(_s.uuid.to_vec().as_ref()) {
                    if let Some((_instant, uuid_old)) = uuid_old {
                        if uuid == uuid_old {
                            self.from_switch = true;
                            self.send_logon_response().await;
                            self.try_start_cm(
                                lr.my_id.clone(),
                                lr.my_name.clone(),
                                self.authorized,
                            );
                            #[cfg(not(any(target_os = "android", target_os = "ios")))]
                            self.try_start_cm_ipc();
                        }
                    }
                }
            }
        } else if self.authorized {
            if self.port_forward_socket.is_some() {
                return true;
            }
            match msg.union {
                #[allow(unused_mut)]
                Some(message::Union::MouseEvent(mut me)) => {
                    #[cfg(any(target_os = "android", target_os = "ios"))]
                    if let Err(e) = call_main_service_pointer_input("mouse", me.mask, me.x, me.y) {
                        log::debug!("call_main_service_pointer_input fail:{}", e);
                    }
                    #[cfg(not(any(target_os = "android", target_os = "ios")))]
                    if self.peer_keyboard_enabled() {
                        if is_left_up(&me) {
                            CLICK_TIME.store(get_time(), Ordering::SeqCst);
                        } else {
                            MOUSE_MOVE_TIME.store(get_time(), Ordering::SeqCst);
                        }
                        #[cfg(target_os = "macos")]
                        self.retina.on_mouse_event(&mut me, self.display_idx);
                        self.input_mouse(me, self.inner.id());
                    }
                    self.update_auto_disconnect_timer();
                }
                Some(message::Union::PointerDeviceEvent(pde)) => {
                    #[cfg(any(target_os = "android", target_os = "ios"))]
                    if let Err(e) = match pde.union {
                        Some(pointer_device_event::Union::TouchEvent(touch)) => match touch.union {
                            Some(touch_event::Union::PanStart(pan_start)) => {
                                call_main_service_pointer_input(
                                    "touch",
                                    4,
                                    pan_start.x,
                                    pan_start.y,
                                )
                            }
                            Some(touch_event::Union::PanUpdate(pan_update)) => {
                                call_main_service_pointer_input(
                                    "touch",
                                    5,
                                    pan_update.x,
                                    pan_update.y,
                                )
                            }
                            Some(touch_event::Union::PanEnd(pan_end)) => {
                                call_main_service_pointer_input("touch", 6, pan_end.x, pan_end.y)
                            }
                            _ => Ok(()),
                        },
                        _ => Ok(()),
                    } {
                        log::debug!("call_main_service_pointer_input fail:{}", e);
                    }
                    #[cfg(not(any(target_os = "android", target_os = "ios")))]
                    if self.peer_keyboard_enabled() {
                        MOUSE_MOVE_TIME.store(get_time(), Ordering::SeqCst);
                        self.input_pointer(pde, self.inner.id());
                    }
                    self.update_auto_disconnect_timer();
                }
                #[cfg(any(target_os = "ios"))]
                Some(message::Union::KeyEvent(..)) => {}
                #[cfg(any(target_os = "android"))]
                Some(message::Union::KeyEvent(mut me)) => {
                    let is_press = (me.press || me.down) && !crate::is_modifier(&me);

                    let key = match me.mode.enum_value() {
                        Ok(KeyboardMode::Map) => {
                            Some(crate::keyboard::keycode_to_rdev_key(me.chr()))
                        }
                        Ok(KeyboardMode::Translate) => {
                            if let Some(key_event::Union::Chr(code)) = me.union {
                                Some(crate::keyboard::keycode_to_rdev_key(code & 0x0000FFFF))
                            } else {
                                None
                            }
                        }
                        _ => None,
                    }
                    .filter(crate::keyboard::is_modifier);

                    if let Some(key) = key {
                        if is_press {
                            self.pressed_modifiers.insert(key);
                        } else {
                            self.pressed_modifiers.remove(&key);
                        }
                    }

                    let mut modifiers = vec![];

                    for key in self.pressed_modifiers.iter() {
                        if let Some(control_key) = map_key_to_control_key(key) {
                            modifiers.push(EnumOrUnknown::new(control_key));
                        }
                    }

                    me.modifiers = modifiers;

                    let encode_result = me.write_to_bytes();

                    match encode_result {
                        Ok(data) => {
                            let result = call_main_service_key_event(&data);
                            if let Err(e) = result {
                                log::debug!("call_main_service_key_event fail: {}", e);
                            }
                        }
                        Err(e) => {
                            log::debug!("encode key event fail: {}", e);
                        }
                    }
                }
                #[cfg(not(any(target_os = "android", target_os = "ios")))]
                Some(message::Union::KeyEvent(me)) => {
                    if self.peer_keyboard_enabled() {
                        if is_enter(&me) {
                            CLICK_TIME.store(get_time(), Ordering::SeqCst);
                        }
                        // handle all down as press
                        // fix unexpected repeating key on remote linux, seems also fix abnormal alt/shift, which
                        // make sure all key are released
                        let is_press = if cfg!(target_os = "linux") {
                            (me.press || me.down) && !crate::is_modifier(&me)
                        } else {
                            me.press
                        };

                        let key = match me.mode.enum_value() {
                            Ok(KeyboardMode::Map) => {
                                Some(crate::keyboard::keycode_to_rdev_key(me.chr()))
                            }
                            Ok(KeyboardMode::Translate) => {
                                if let Some(key_event::Union::Chr(code)) = me.union {
                                    Some(crate::keyboard::keycode_to_rdev_key(code & 0x0000FFFF))
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        }
                        .filter(crate::keyboard::is_modifier);

                        if let Some(key) = key {
                            if is_press {
                                self.pressed_modifiers.insert(key);
                            } else {
                                self.pressed_modifiers.remove(&key);
                            }
                        }

                        if is_press {
                            match me.union {
                                Some(key_event::Union::Unicode(_))
                                | Some(key_event::Union::Seq(_)) => {
                                    self.input_key(me, false);
                                }
                                _ => {
                                    self.input_key(me, true);
                                }
                            }
                        } else {
                            self.input_key(me, false);
                        }
                    }
                    self.update_auto_disconnect_timer();
                }
                Some(message::Union::Clipboard(_cb)) =>
                {
                    #[cfg(not(any(target_os = "android", target_os = "ios")))]
                    if self.clipboard {
                        update_clipboard(_cb, None);
                    }
                }
                Some(message::Union::Cliprdr(_clip)) =>
                {
                    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
                    if let Some(clip) = msg_2_clip(_clip) {
                        log::debug!("got clipfile from client peer");
                        self.send_to_cm(ipc::Data::ClipboardFile(clip))
                    }
                }
                Some(message::Union::FileAction(fa)) => {
                    if self.file_transfer.is_some() {
                        if self.delayed_read_dir.is_some() {
                            if let Some(file_action::Union::ReadDir(rd)) = fa.union {
                                self.delayed_read_dir = Some((rd.path, rd.include_hidden));
                            }
                            return true;
                        }
                        match fa.union {
                            Some(file_action::Union::ReadDir(rd)) => {
                                self.read_dir(&rd.path, rd.include_hidden);
                            }
                            Some(file_action::Union::AllFiles(f)) => {
                                match fs::get_recursive_files(&f.path, f.include_hidden) {
                                    Err(err) => {
                                        self.send(fs::new_error(f.id, err, -1)).await;
                                    }
                                    Ok(files) => {
                                        self.send(fs::new_dir(f.id, f.path, files)).await;
                                    }
                                }
                            }
                            Some(file_action::Union::Send(s)) => {
                                // server to client
                                let id = s.id;
                                let od = can_enable_overwrite_detection(get_version_number(
                                    &self.lr.version,
                                ));
                                let path = s.path.clone();
                                match fs::TransferJob::new_read(
                                    id,
                                    "".to_string(),
                                    path.clone(),
                                    s.file_num,
                                    s.include_hidden,
                                    false,
                                    od,
                                ) {
                                    Err(err) => {
                                        self.send(fs::new_error(id, err, 0)).await;
                                    }
                                    Ok(mut job) => {
                                        self.send(fs::new_dir(id, path, job.files().to_vec()))
                                            .await;
                                        let mut files = job.files().to_owned();
                                        job.is_remote = true;
                                        job.conn_id = self.inner.id();
                                        self.read_jobs.push(job);
                                        self.file_timer =
                                            crate::rustdesk_interval(time::interval(MILLI1));
                                        self.post_file_audit(
                                            FileAuditType::RemoteSend,
                                            &s.path,
                                            files
                                                .drain(..)
                                                .map(|f| (f.name, f.size as _))
                                                .collect(),
                                            json!({}),
                                        );
                                    }
                                }
                                self.file_transferred = true;
                            }
                            Some(file_action::Union::Receive(r)) => {
                                // client to server
                                // note: 1.1.10 introduced identical file detection, which breaks original logic of send/recv files
                                // whenever got send/recv request, check peer version to ensure old version of rustdesk
                                let od = can_enable_overwrite_detection(get_version_number(
                                    &self.lr.version,
                                ));
                                self.send_fs(ipc::FS::NewWrite {
                                    path: r.path.clone(),
                                    id: r.id,
                                    file_num: r.file_num,
                                    files: r
                                        .files
                                        .to_vec()
                                        .drain(..)
                                        .map(|f| (f.name, f.modified_time))
                                        .collect(),
                                    overwrite_detection: od,
                                    total_size: r.total_size,
                                    conn_id: self.inner.id(),
                                });
                                self.post_file_audit(
                                    FileAuditType::RemoteReceive,
                                    &r.path,
                                    r.files
                                        .to_vec()
                                        .drain(..)
                                        .map(|f| (f.name, f.size as _))
                                        .collect(),
                                    json!({}),
                                );
                                self.file_transferred = true;
                            }
                            Some(file_action::Union::RemoveDir(d)) => {
                                self.send_fs(ipc::FS::RemoveDir {
                                    path: d.path.clone(),
                                    id: d.id,
                                    recursive: d.recursive,
                                });
                                self.file_remove_log_control.on_remove_dir(d);
                            }
                            Some(file_action::Union::RemoveFile(f)) => {
                                self.send_fs(ipc::FS::RemoveFile {
                                    path: f.path.clone(),
                                    id: f.id,
                                    file_num: f.file_num,
                                });
                                self.file_remove_log_control.on_remove_file(f);
                            }
                            Some(file_action::Union::Create(c)) => {
                                self.send_fs(ipc::FS::CreateDir {
                                    path: c.path.clone(),
                                    id: c.id,
                                });
                                self.send_to_cm(ipc::Data::FileTransferLog((
                                    "create_dir".to_string(),
                                    serde_json::to_string(&FileActionLog {
                                        id: c.id,
                                        conn_id: self.inner.id(),
                                        path: c.path,
                                        dir: true,
                                    })
                                    .unwrap_or_default(),
                                )));
                            }
                            Some(file_action::Union::Cancel(c)) => {
                                self.send_fs(ipc::FS::CancelWrite { id: c.id });
                                if let Some(job) = fs::get_job_immutable(c.id, &self.read_jobs) {
                                    self.send_to_cm(ipc::Data::FileTransferLog((
                                        "transfer".to_string(),
                                        fs::serialize_transfer_job(job, false, true, ""),
                                    )));
                                }
                                fs::remove_job(c.id, &mut self.read_jobs);
                            }
                            Some(file_action::Union::SendConfirm(r)) => {
                                if let Some(job) = fs::get_job(r.id, &mut self.read_jobs) {
                                    job.confirm(&r);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Some(message::Union::FileResponse(fr)) => match fr.union {
                    Some(file_response::Union::Block(block)) => {
                        self.send_fs(ipc::FS::WriteBlock {
                            id: block.id,
                            file_num: block.file_num,
                            data: block.data,
                            compressed: block.compressed,
                        });
                    }
                    Some(file_response::Union::Done(d)) => {
                        self.send_fs(ipc::FS::WriteDone {
                            id: d.id,
                            file_num: d.file_num,
                        });
                    }
                    Some(file_response::Union::Digest(d)) => self.send_fs(ipc::FS::CheckDigest {
                        id: d.id,
                        file_num: d.file_num,
                        file_size: d.file_size,
                        last_modified: d.last_modified,
                        is_upload: true,
                    }),
                    Some(file_response::Union::Error(e)) => {
                        self.send_fs(ipc::FS::WriteError {
                            id: e.id,
                            file_num: e.file_num,
                            err: e.error,
                        });
                    }
                    _ => {}
                },
                Some(message::Union::Misc(misc)) => match misc.union {
                    Some(misc::Union::SwitchDisplay(s)) => {
                        self.handle_switch_display(s).await;
                    }
                    Some(misc::Union::CaptureDisplays(displays)) => {
                        let add = displays.add.iter().map(|d| *d as usize).collect::<Vec<_>>();
                        let sub = displays.sub.iter().map(|d| *d as usize).collect::<Vec<_>>();
                        let set = displays.set.iter().map(|d| *d as usize).collect::<Vec<_>>();
                        self.capture_displays(&add, &sub, &set).await;
                    }
                    #[cfg(windows)]
                    Some(misc::Union::ToggleVirtualDisplay(t)) => {
                        self.toggle_virtual_display(t).await;
                    }
                    Some(misc::Union::TogglePrivacyMode(t)) => {
                        self.toggle_privacy_mode(t).await;
                    }
                    Some(misc::Union::ChatMessage(c)) => {
                        self.send_to_cm(ipc::Data::ChatMessage { text: c.text });
                        self.chat_unanswered = true;
                        self.update_auto_disconnect_timer();
                    }
                    Some(misc::Union::Option(o)) => {
                        self.update_options(&o).await;
                    }
                    Some(misc::Union::RefreshVideo(r)) => {
                        if r {
                            // Refresh all videos.
                            // Compatibility with old versions and sciter(remote).
                            self.refresh_video_display(None);
                        }
                        self.update_auto_disconnect_timer();
                    }
                    Some(misc::Union::RefreshVideoDisplay(display)) => {
                        self.refresh_video_display(Some(display as usize));
                        self.update_auto_disconnect_timer();
                    }
                    Some(misc::Union::VideoReceived(_)) => {
                        video_service::notify_video_frame_fetched(
                            self.inner.id,
                            Some(Instant::now().into()),
                        );
                    }
                    Some(misc::Union::CloseReason(_)) => {
                        self.on_close("Peer close", true).await;
                        SESSIONS.lock().unwrap().remove(&self.lr.my_id);
                        return false;
                    }

                    Some(misc::Union::RestartRemoteDevice(_)) => {
                        #[cfg(not(any(target_os = "android", target_os = "ios")))]
                        if self.restart {
                            // force_reboot, not work on linux vm and macos 14
                            #[cfg(any(target_os = "linux", target_os = "windows"))]
                            match system_shutdown::force_reboot() {
                                Ok(_) => log::info!("Restart by the peer"),
                                Err(e) => log::error!("Failed to restart: {}", e),
                            }
                            #[cfg(any(target_os = "linux", target_os = "macos"))]
                            match system_shutdown::reboot() {
                                Ok(_) => log::info!("Restart by the peer"),
                                Err(e) => log::error!("Failed to restart: {}", e),
                            }
                        }
                    }
                    #[cfg(windows)]
                    Some(misc::Union::ElevationRequest(r)) => match r.union {
                        Some(elevation_request::Union::Direct(_)) => {
                            self.handle_elevation_request(portable_client::StartPara::Direct)
                                .await;
                        }
                        Some(elevation_request::Union::Logon(r)) => {
                            self.handle_elevation_request(portable_client::StartPara::Logon(
                                r.username, r.password,
                            ))
                            .await;
                        }
                        _ => {}
                    },
                    Some(misc::Union::AudioFormat(format)) => {
                        if !self.disable_audio {
                            // Drop the audio sender previously.
                            drop(std::mem::replace(&mut self.audio_sender, None));
                            self.audio_sender = Some(start_audio_thread());
                            self.audio_sender
                                .as_ref()
                                .map(|a| allow_err!(a.send(MediaData::AudioFormat(format))));
                        }
                    }
                    #[cfg(feature = "flutter")]
                    Some(misc::Union::SwitchSidesRequest(s)) => {
                        if let Ok(uuid) = uuid::Uuid::from_slice(&s.uuid.to_vec()[..]) {
                            crate::run_me(vec![
                                "--connect",
                                &self.lr.my_id,
                                "--switch_uuid",
                                uuid.to_string().as_ref(),
                            ])
                            .ok();
                            self.on_close("switch sides", false).await;
                            return false;
                        }
                    }
                    #[cfg(not(any(target_os = "android", target_os = "ios")))]
                    Some(misc::Union::ChangeResolution(r)) => self.change_resolution(None, &r),
                    #[cfg(not(any(target_os = "android", target_os = "ios")))]
                    Some(misc::Union::ChangeDisplayResolution(dr)) => {
                        self.change_resolution(Some(dr.display as _), &dr.resolution)
                    }
                    #[cfg(all(feature = "flutter", feature = "plugin_framework"))]
                    #[cfg(not(any(target_os = "android", target_os = "ios")))]
                    Some(misc::Union::PluginRequest(p)) => {
                        let msg =
                            crate::plugin::handle_client_event(&p.id, &self.lr.my_id, &p.content);
                        self.send(msg).await;
                    }
                    Some(misc::Union::AutoAdjustFps(fps)) => video_service::VIDEO_QOS
                        .lock()
                        .unwrap()
                        .user_auto_adjust_fps(self.inner.id(), fps),
                    Some(misc::Union::ClientRecordStatus(status)) => video_service::VIDEO_QOS
                        .lock()
                        .unwrap()
                        .user_record(self.inner.id(), status),
                    #[cfg(windows)]
                    Some(misc::Union::SelectedSid(sid)) => {
                        if let Some(current_process_sid) =
                            crate::platform::get_current_process_session_id()
                        {
                            let sessions = crate::platform::get_available_sessions(false);
                            if crate::platform::is_installed()
                                && crate::platform::is_share_rdp()
                                && raii::AuthedConnID::remote_and_file_conn_count() == 1
                                && sessions.len() > 1
                                && current_process_sid != sid
                                && sessions.iter().any(|e| e.sid == sid)
                            {
                                std::thread::spawn(move || {
                                    let _ = ipc::connect_to_user_session(Some(sid));
                                });
                                return false;
                            }
                            if self.file_transfer.is_some() {
                                if let Some((dir, show_hidden)) = self.delayed_read_dir.take() {
                                    self.read_dir(&dir, show_hidden);
                                }
                            } else {
                                self.try_sub_services();
                            }
                        }
                    }
                    Some(misc::Union::MessageQuery(mq)) => {
                        if let Some(msg_out) =
                            video_service::make_display_changed_msg(mq.switch_display as _, None)
                        {
                            self.send(msg_out).await;
                        }
                    }
                    _ => {}
                },
                Some(message::Union::AudioFrame(frame)) => {
                    if !self.disable_audio {
                        if let Some(sender) = &self.audio_sender {
                            allow_err!(sender.send(MediaData::AudioFrame(Box::new(frame))));
                        } else {
                            log::warn!(
                                "Processing audio frame without the voice call audio sender."
                            );
                        }
                    }
                }
                Some(message::Union::VoiceCallRequest(request)) => {
                    if request.is_connect {
                        self.voice_call_request_timestamp = Some(
                            NonZeroI64::new(request.req_timestamp)
                                .unwrap_or(NonZeroI64::new(get_time()).unwrap()),
                        );
                        // Notify the connection manager.
                        self.send_to_cm(Data::VoiceCallIncoming);
                    } else {
                        self.close_voice_call().await;
                    }
                }
                Some(message::Union::VoiceCallResponse(_response)) => {
                    // TODO: Maybe we can do a voice call from cm directly.
                }
                _ => {}
            }
        }
        true
    }

    fn update_failure(&self, (mut failure, time): ((i32, i32, i32), i32), remove: bool, i: usize) {
        if remove {
            if failure.0 != 0 {
                LOGIN_FAILURES[i].lock().unwrap().remove(&self.ip);
            }
            return;
        }
        if failure.0 == time {
            failure.1 += 1;
            failure.2 += 1;
        } else {
            failure.0 = time;
            failure.1 = 1;
            failure.2 += 1;
        }
        LOGIN_FAILURES[i]
            .lock()
            .unwrap()
            .insert(self.ip.clone(), failure);
    }

    async fn check_failure(&mut self, i: usize) -> (((i32, i32, i32), i32), bool) {
        let failure = LOGIN_FAILURES[i]
            .lock()
            .unwrap()
            .get(&self.ip)
            .map(|x| x.clone())
            .unwrap_or((0, 0, 0));
        let time = (get_time() / 60_000) as i32;
        let res = if failure.2 > 30 {
            self.send_login_error("Too many wrong attempts").await;
            Self::post_alarm_audit(
                AlarmAuditType::ExceedThirtyAttempts,
                json!({
                            "ip": self.ip,
                            "id": self.lr.my_id.clone(),
                            "name": self.lr.my_name.clone(),
                }),
            );
            false
        } else if time == failure.0 && failure.1 > 6 {
            self.send_login_error("Please try 1 minute later").await;
            Self::post_alarm_audit(
                AlarmAuditType::SixAttemptsWithinOneMinute,
                json!({
                            "ip": self.ip,
                            "id": self.lr.my_id.clone(),
                            "name": self.lr.my_name.clone(),
                }),
            );
            false
        } else {
            true
        };
        ((failure, time), res)
    }

    fn refresh_video_display(&self, display: Option<usize>) {
        video_service::refresh();
        self.server.upgrade().map(|s| {
            s.read().unwrap().set_video_service_opt(
                display,
                video_service::OPTION_REFRESH,
                super::service::SERVICE_OPTION_VALUE_TRUE,
            );
        });
    }

    async fn handle_switch_display(&mut self, s: SwitchDisplay) {
        #[cfg(windows)]
        if portable_client::running()
            && *CONN_COUNT.lock().unwrap() > 1
            && s.display != (*display_service::PRIMARY_DISPLAY_IDX as i32)
        {
            log::info!("Switch to non-primary display is not supported in the elevated mode when there are multiple connections.");
            let mut msg_out = Message::new();
            let res = MessageBox {
                msgtype: "nook-nocancel-hasclose".to_owned(),
                title: "Prompt".to_owned(),
                text: "switch_display_elevated_connections_tip".to_owned(),
                link: "".to_owned(),
                ..Default::default()
            };
            msg_out.set_message_box(res);
            self.send(msg_out).await;
            return;
        }

        let display_idx = s.display as usize;
        if self.display_idx != display_idx {
            if let Some(server) = self.server.upgrade() {
                self.switch_display_to(display_idx, server.clone());

                #[cfg(not(any(target_os = "android", target_os = "ios")))]
                if s.width != 0 && s.height != 0 {
                    self.change_resolution(
                        None,
                        &Resolution {
                            width: s.width,
                            height: s.height,
                            ..Default::default()
                        },
                    );
                }
            }

            // Send display changed message.
            // 1. For compatibility with old versions ( < 1.2.4 ).
            // 2. Sciter version.
            // 3. Update `SupportedResolutions`.
            if let Some(msg_out) = video_service::make_display_changed_msg(self.display_idx, None) {
                self.send(msg_out).await;
            }
        }
    }

    fn switch_display_to(&mut self, display_idx: usize, server: Arc<RwLock<Server>>) {
        let new_service_name = video_service::get_service_name(display_idx);
        let old_service_name = video_service::get_service_name(self.display_idx);
        let mut lock = server.write().unwrap();
        if display_idx != *display_service::PRIMARY_DISPLAY_IDX {
            if !lock.contains(&new_service_name) {
                lock.add_service(Box::new(video_service::new(display_idx)));
            }
        }
        // For versions greater than 1.2.4, a `CaptureDisplays` message will be sent immediately.
        // Unnecessary capturers will be removed then.
        if !crate::common::is_support_multi_ui_session(&self.lr.version) {
            lock.subscribe(&old_service_name, self.inner.clone(), false);
        }
        lock.subscribe(&new_service_name, self.inner.clone(), true);
        self.display_idx = display_idx;
    }

    #[cfg(windows)]
    async fn handle_elevation_request(&mut self, para: portable_client::StartPara) {
        let mut err;
        if !self.keyboard {
            err = "No permission".to_string();
        } else {
            err = "No need to elevate".to_string();
            if !crate::platform::is_installed() && !portable_client::running() {
                err = portable_client::start_portable_service(para)
                    .err()
                    .map_or("".to_string(), |e| e.to_string());
            }
        }

        let mut misc = Misc::new();
        misc.set_elevation_response(err);
        let mut msg = Message::new();
        msg.set_misc(misc);
        self.send(msg).await;
        self.update_auto_disconnect_timer();
    }

    async fn capture_displays(&mut self, add: &[usize], sub: &[usize], set: &[usize]) {
        #[cfg(windows)]
        if portable_client::running() && (add.len() > 0 || set.len() > 1) {
            log::info!("Capturing multiple displays is not supported in the elevated mode.");
            let mut msg_out = Message::new();
            let res = MessageBox {
                msgtype: "nook-nocancel-hasclose".to_owned(),
                title: "Prompt".to_owned(),
                text: "capture_display_elevated_connections_tip".to_owned(),
                link: "".to_owned(),
                ..Default::default()
            };
            msg_out.set_message_box(res);
            self.send(msg_out).await;
            return;
        }

        if let Some(sever) = self.server.upgrade() {
            let mut lock = sever.write().unwrap();
            for display in add.iter() {
                let service_name = video_service::get_service_name(*display);
                if !lock.contains(&service_name) {
                    lock.add_service(Box::new(video_service::new(*display)));
                }
            }
            for display in set.iter() {
                let service_name = video_service::get_service_name(*display);
                if !lock.contains(&service_name) {
                    lock.add_service(Box::new(video_service::new(*display)));
                }
            }
            if !add.is_empty() {
                lock.capture_displays(self.inner.clone(), add, true, false);
            } else if !sub.is_empty() {
                lock.capture_displays(self.inner.clone(), sub, false, true);
            } else {
                lock.capture_displays(self.inner.clone(), set, true, true);
            }
            self.multi_ui_session = lock.get_subbed_displays_count(self.inner.id()) > 1;
            if self.follow_remote_window {
                lock.subscribe(
                    NAME_WINDOW_FOCUS,
                    self.inner.clone(),
                    !self.multi_ui_session,
                );
            }
            drop(lock);
        }
    }

    #[cfg(windows)]
    async fn toggle_virtual_display(&mut self, t: ToggleVirtualDisplay) {
        let make_msg = |text: String| {
            let mut msg_out = Message::new();
            let res = MessageBox {
                msgtype: "nook-nocancel-hasclose".to_owned(),
                title: "Virtual display".to_owned(),
                text,
                link: "".to_owned(),
                ..Default::default()
            };
            msg_out.set_message_box(res);
            msg_out
        };

        if t.on {
            if !virtual_display_manager::is_virtual_display_supported() {
                self.send(make_msg("idd_not_support_under_win10_2004_tip".to_string()))
                    .await;
            } else {
                if let Err(e) = virtual_display_manager::plug_in_monitor(t.display as _, Vec::new())
                {
                    log::error!("Failed to plug in virtual display: {}", e);
                    self.send(make_msg(format!(
                        "Failed to plug in virtual display: {}",
                        e
                    )))
                    .await;
                }
            }
        } else {
            if let Err(e) = virtual_display_manager::plug_out_monitor(t.display) {
                log::error!("Failed to plug out virtual display {}: {}", t.display, e);
                self.send(make_msg(format!(
                    "Failed to plug out virtual displays: {}",
                    e
                )))
                .await;
            }
        }
    }

    async fn toggle_privacy_mode(&mut self, t: TogglePrivacyMode) {
        if t.on {
            self.turn_on_privacy(t.impl_key).await;
        } else {
            self.turn_off_privacy(t.impl_key).await;
        }
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    fn change_resolution(&mut self, d: Option<usize>, r: &Resolution) {
        if self.keyboard {
            if let Ok(displays) = display_service::try_get_displays() {
                let display_idx = d.unwrap_or(self.display_idx);
                if let Some(display) = displays.get(display_idx) {
                    let name = display.name();
                    #[cfg(windows)]
                    if let Some(_ok) =
                        virtual_display_manager::rustdesk_idd::change_resolution_if_is_virtual_display(
                            &name,
                            r.width as _,
                            r.height as _,
                        )
                    {
                        return;
                    }
                    let mut record_changed = true;
                    #[cfg(windows)]
                    if virtual_display_manager::amyuni_idd::is_my_display(&name) {
                        record_changed = false;
                    }
                    if record_changed {
                        display_service::set_last_changed_resolution(
                            &name,
                            (display.width() as _, display.height() as _),
                            (r.width, r.height),
                        );
                    }
                    if let Err(e) =
                        crate::platform::change_resolution(&name, r.width as _, r.height as _)
                    {
                        log::error!(
                            "Failed to change resolution '{}' to ({},{}): {:?}",
                            &name,
                            r.width,
                            r.height,
                            e
                        );
                    }
                }
            }
        }
    }

    pub async fn handle_voice_call(&mut self, accepted: bool) {
        if let Some(ts) = self.voice_call_request_timestamp.take() {
            let msg = new_voice_call_response(ts.get(), accepted);
            if accepted {
                // Backup the default input device.
                let audio_input_device = Config::get_option("audio-input");
                log::debug!("Backup the sound input device {}", audio_input_device);
                self.audio_input_device_before_voice_call = Some(audio_input_device);
                // Switch to default input device
                let default_sound_device = get_default_sound_input();
                if let Some(device) = default_sound_device {
                    set_sound_input(device);
                }
                self.send_to_cm(Data::StartVoiceCall);
            } else {
                self.send_to_cm(Data::CloseVoiceCall("".to_owned()));
            }
            self.send(msg).await;
        } else {
            log::warn!("Possible a voice call attack.");
        }
    }

    pub async fn close_voice_call(&mut self) {
        // Restore to the prior audio device.
        if let Some(sound_input) =
            std::mem::replace(&mut self.audio_input_device_before_voice_call, None)
        {
            set_sound_input(sound_input);
        }
        // Notify the connection manager that the voice call has been closed.
        self.send_to_cm(Data::CloseVoiceCall("".to_owned()));
    }

    async fn update_options(&mut self, o: &OptionMessage) {
        log::info!("Option update: {:?}", o);
        if let Ok(q) = o.image_quality.enum_value() {
            let image_quality;
            if let ImageQuality::NotSet = q {
                if o.custom_image_quality > 0 {
                    image_quality = o.custom_image_quality;
                } else {
                    image_quality = -1;
                }
            } else {
                image_quality = q.value();
            }
            if image_quality > 0 {
                video_service::VIDEO_QOS
                    .lock()
                    .unwrap()
                    .user_image_quality(self.inner.id(), image_quality);
            }
        }
        if o.custom_fps > 0 {
            video_service::VIDEO_QOS
                .lock()
                .unwrap()
                .user_custom_fps(self.inner.id(), o.custom_fps as _);
        }
        if let Some(q) = o.supported_decoding.clone().take() {
            scrap::codec::Encoder::update(scrap::codec::EncodingUpdate::Update(self.inner.id(), q));
        }
        if let Ok(q) = o.lock_after_session_end.enum_value() {
            if q != BoolOption::NotSet {
                self.lock_after_session_end = q == BoolOption::Yes;
            }
        }
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        if let Ok(q) = o.show_remote_cursor.enum_value() {
            if q != BoolOption::NotSet {
                self.show_remote_cursor = q == BoolOption::Yes;
                if let Some(s) = self.server.upgrade() {
                    s.write().unwrap().subscribe(
                        NAME_CURSOR,
                        self.inner.clone(),
                        self.peer_keyboard_enabled() || self.show_remote_cursor,
                    );
                    s.write().unwrap().subscribe(
                        NAME_POS,
                        self.inner.clone(),
                        self.show_remote_cursor,
                    );
                }
            }
        }
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        if let Ok(q) = o.follow_remote_cursor.enum_value() {
            if q != BoolOption::NotSet {
                self.follow_remote_cursor = q == BoolOption::Yes;
            }
        }
        if let Ok(q) = o.follow_remote_window.enum_value() {
            if q != BoolOption::NotSet {
                self.follow_remote_window = q == BoolOption::Yes;
                if let Some(s) = self.server.upgrade() {
                    s.write().unwrap().subscribe(
                        NAME_WINDOW_FOCUS,
                        self.inner.clone(),
                        self.follow_remote_window,
                    );
                }
            }
        }
        if let Ok(q) = o.disable_audio.enum_value() {
            if q != BoolOption::NotSet {
                self.disable_audio = q == BoolOption::Yes;
                if let Some(s) = self.server.upgrade() {
                    s.write().unwrap().subscribe(
                        super::audio_service::NAME,
                        self.inner.clone(),
                        self.audio_enabled(),
                    );
                }
            }
        }
        #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
        if let Ok(q) = o.enable_file_transfer.enum_value() {
            if q != BoolOption::NotSet {
                self.enable_file_transfer = q == BoolOption::Yes;
                self.send_to_cm(ipc::Data::ClipboardFileEnabled(
                    self.file_transfer_enabled(),
                ));
            }
        }
        if let Ok(q) = o.disable_clipboard.enum_value() {
            if q != BoolOption::NotSet {
                self.disable_clipboard = q == BoolOption::Yes;
                if let Some(s) = self.server.upgrade() {
                    s.write().unwrap().subscribe(
                        super::clipboard_service::NAME,
                        self.inner.clone(),
                        self.clipboard_enabled() && self.peer_keyboard_enabled(),
                    );
                }
            }
        }
        if let Ok(q) = o.disable_keyboard.enum_value() {
            if q != BoolOption::NotSet {
                self.disable_keyboard = q == BoolOption::Yes;
                if let Some(s) = self.server.upgrade() {
                    s.write().unwrap().subscribe(
                        super::clipboard_service::NAME,
                        self.inner.clone(),
                        self.clipboard_enabled() && self.peer_keyboard_enabled(),
                    );
                    s.write().unwrap().subscribe(
                        NAME_CURSOR,
                        self.inner.clone(),
                        self.peer_keyboard_enabled() || self.show_remote_cursor,
                    );
                }
            }
        }
        // For compatibility with old versions ( < 1.2.4 ).
        if hbb_common::get_version_number(&self.lr.version)
            < hbb_common::get_version_number("1.2.4")
        {
            if let Ok(q) = o.privacy_mode.enum_value() {
                if self.keyboard {
                    match q {
                        BoolOption::Yes => {
                            self.turn_on_privacy("".to_owned()).await;
                        }
                        BoolOption::No => {
                            self.turn_off_privacy("".to_owned()).await;
                        }
                        _ => {}
                    }
                }
            }
        }
        if let Ok(q) = o.block_input.enum_value() {
            if self.keyboard && self.block_input {
                match q {
                    BoolOption::Yes => {
                        self.tx_input.send(MessageInput::BlockOn).ok();
                    }
                    BoolOption::No => {
                        self.tx_input.send(MessageInput::BlockOff).ok();
                    }
                    _ => {}
                }
            } else {
                if q != BoolOption::NotSet {
                    let state = if q == BoolOption::Yes {
                        back_notification::BlockInputState::BlkOnFailed
                    } else {
                        back_notification::BlockInputState::BlkOffFailed
                    };
                    if let Some(tx) = &self.inner.tx {
                        Self::send_block_input_error(tx, state, "No permission".to_string());
                    }
                }
            }
        }
    }

    async fn turn_on_privacy(&mut self, impl_key: String) {
        let msg_out = if !privacy_mode::is_privacy_mode_supported() {
            crate::common::make_privacy_mode_msg_with_details(
                back_notification::PrivacyModeState::PrvNotSupported,
                "Unsupported. 1 Multi-screen is not supported. 2 Please confirm the license is activated.".to_string(),
                impl_key,
            )
        } else {
            let is_pre_privacy_on = privacy_mode::is_in_privacy_mode();
            let pre_impl_key = privacy_mode::get_cur_impl_key();

            if is_pre_privacy_on {
                if let Some(pre_impl_key) = pre_impl_key {
                    if !privacy_mode::is_current_privacy_mode_impl(&pre_impl_key) {
                        let off_msg = crate::common::make_privacy_mode_msg(
                            back_notification::PrivacyModeState::PrvOffSucceeded,
                            pre_impl_key,
                        );
                        self.send(off_msg).await;
                    }
                }
            }

            let turn_on_res = privacy_mode::turn_on_privacy(&impl_key, self.inner.id).await;
            match turn_on_res {
                Some(Ok(res)) => {
                    if res {
                        let err_msg = privacy_mode::check_privacy_mode_err(
                            self.inner.id,
                            self.display_idx,
                            5_000,
                        );
                        if err_msg.is_empty() {
                            crate::common::make_privacy_mode_msg(
                                back_notification::PrivacyModeState::PrvOnSucceeded,
                                impl_key,
                            )
                        } else {
                            log::error!(
                                "Check privacy mode failed: {}, turn off privacy mode.",
                                &err_msg
                            );
                            let _ = Self::turn_off_privacy_to_msg(self.inner.id);
                            crate::common::make_privacy_mode_msg_with_details(
                                back_notification::PrivacyModeState::PrvOnFailed,
                                err_msg,
                                impl_key,
                            )
                        }
                    } else {
                        crate::common::make_privacy_mode_msg(
                            back_notification::PrivacyModeState::PrvOnFailedPlugin,
                            impl_key,
                        )
                    }
                }
                Some(Err(e)) => {
                    log::error!("Failed to turn on privacy mode. {}", e);
                    if privacy_mode::is_in_privacy_mode() {
                        let _ = Self::turn_off_privacy_to_msg(
                            privacy_mode::INVALID_PRIVACY_MODE_CONN_ID,
                        );
                    }
                    crate::common::make_privacy_mode_msg_with_details(
                        back_notification::PrivacyModeState::PrvOnFailed,
                        e.to_string(),
                        impl_key,
                    )
                }
                None => crate::common::make_privacy_mode_msg_with_details(
                    back_notification::PrivacyModeState::PrvOffFailed,
                    "Not supported".to_string(),
                    impl_key,
                ),
            }
        };
        self.send(msg_out).await;
    }

    async fn turn_off_privacy(&mut self, impl_key: String) {
        let msg_out = if !privacy_mode::is_privacy_mode_supported() {
            crate::common::make_privacy_mode_msg_with_details(
                back_notification::PrivacyModeState::PrvNotSupported,
                // This error message is used for magnifier. It is ok to use it here.
                "Unsupported. 1 Multi-screen is not supported. 2 Please confirm the license is activated.".to_string(),
                impl_key,
            )
        } else {
            Self::turn_off_privacy_to_msg(self.inner.id)
        };
        self.send(msg_out).await;
    }

    pub fn turn_off_privacy_to_msg(_conn_id: i32) -> Message {
        let impl_key = "".to_owned();
        match privacy_mode::turn_off_privacy(_conn_id, None) {
            Some(Ok(_)) => crate::common::make_privacy_mode_msg(
                back_notification::PrivacyModeState::PrvOffSucceeded,
                impl_key,
            ),
            Some(Err(e)) => {
                log::error!("Failed to turn off privacy mode {}", e);
                crate::common::make_privacy_mode_msg_with_details(
                    back_notification::PrivacyModeState::PrvOffFailed,
                    e.to_string(),
                    impl_key,
                )
            }
            None => crate::common::make_privacy_mode_msg_with_details(
                back_notification::PrivacyModeState::PrvOffFailed,
                "Not supported".to_string(),
                impl_key,
            ),
        }
    }

    async fn on_close(&mut self, reason: &str, lock: bool) {
        if self.closed {
            return;
        }
        self.closed = true;
        log::info!("#{} Connection closed: {}", self.inner.id(), reason);
        if lock && self.lock_after_session_end && self.keyboard {
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            lock_screen().await;
        }
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        let data = if self.chat_unanswered || self.file_transferred && cfg!(feature = "flutter") {
            ipc::Data::Disconnected
        } else {
            ipc::Data::Close
        };
        #[cfg(any(target_os = "android", target_os = "ios"))]
        let data = ipc::Data::Close;
        self.tx_to_cm.send(data).ok();
        self.port_forward_socket.take();
    }

    // The `reason` should be consistent with `check_if_retry` if not empty
    async fn send_close_reason_no_retry(&mut self, reason: &str) {
        let mut misc = Misc::new();
        if reason.is_empty() {
            misc.set_close_reason("Closed manually by the peer".to_string());
        } else {
            misc.set_close_reason(reason.to_string());
        }
        let mut msg_out = Message::new();
        msg_out.set_misc(misc);
        self.send(msg_out).await;
        SESSIONS.lock().unwrap().remove(&self.lr.my_id);
    }

    fn read_dir(&mut self, dir: &str, include_hidden: bool) {
        let dir = dir.to_string();
        self.send_fs(ipc::FS::ReadDir {
            dir,
            include_hidden,
        });
    }

    #[inline]
    async fn send(&mut self, msg: Message) {
        allow_err!(self.stream.send(&msg).await);
    }

    pub fn alive_conns() -> Vec<i32> {
        ALIVE_CONNS.lock().unwrap().clone()
    }

    #[cfg(windows)]
    fn portable_check(&mut self) {
        if self.portable.is_installed
            || self.file_transfer.is_some()
            || self.port_forward_socket.is_some()
            || !self.keyboard
        {
            return;
        }
        let running = portable_client::running();
        let show_elevation = !running;
        self.send_to_cm(ipc::Data::DataPortableService(
            ipc::DataPortableService::CmShowElevation(show_elevation),
        ));
        if self.authorized {
            let p = &mut self.portable;
            if Some(running) != p.last_running {
                p.last_running = Some(running);
                let mut misc = Misc::new();
                misc.set_portable_service_running(running);
                let mut msg = Message::new();
                msg.set_misc(misc);
                self.inner.send(msg.into());
            }
            let uac = crate::video_service::IS_UAC_RUNNING.lock().unwrap().clone();
            if p.last_uac != uac {
                p.last_uac = uac;
                if !uac || !running {
                    let mut misc = Misc::new();
                    misc.set_uac(uac);
                    let mut msg = Message::new();
                    msg.set_misc(misc);
                    self.inner.send(msg.into());
                }
            }
            let foreground_window_elevated = crate::video_service::IS_FOREGROUND_WINDOW_ELEVATED
                .lock()
                .unwrap()
                .clone();
            if p.last_foreground_window_elevated != foreground_window_elevated {
                p.last_foreground_window_elevated = foreground_window_elevated;
                if !foreground_window_elevated || !running {
                    let mut misc = Misc::new();
                    misc.set_foreground_window_elevated(foreground_window_elevated);
                    let mut msg = Message::new();
                    msg.set_misc(misc);
                    self.inner.send(msg.into());
                }
            }
        }
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    fn release_pressed_modifiers(&mut self) {
        for modifier in self.pressed_modifiers.iter() {
            rdev::simulate(&rdev::EventType::KeyRelease(*modifier)).ok();
        }
        self.pressed_modifiers.clear();
    }

    fn get_auto_disconenct_timer() -> Option<(Instant, u64)> {
        if Config::get_option("allow-auto-disconnect") == "Y" {
            let mut minute: u64 = Config::get_option("auto-disconnect-timeout")
                .parse()
                .unwrap_or(10);
            if minute == 0 {
                minute = 10;
            }
            Some((Instant::now(), minute))
        } else {
            None
        }
    }

    fn update_auto_disconnect_timer(&mut self) {
        self.auto_disconnect_timer
            .as_mut()
            .map(|t| t.0 = Instant::now());
    }

    fn update_supported_encoding(&mut self) {
        let Some(last) = &self.last_supported_encoding else {
            return;
        };
        let usable = scrap::codec::Encoder::usable_encoding();
        let Some(usable) = usable else {
            return;
        };
        if usable.vp8 != last.vp8
            || usable.av1 != last.av1
            || usable.h264 != last.h264
            || usable.h265 != last.h265
        {
            let mut misc: Misc = Misc::new();
            let supported_encoding = SupportedEncoding {
                vp8: usable.vp8,
                av1: usable.av1,
                h264: usable.h264,
                h265: usable.h265,
                ..last.clone()
            };
            log::info!("update supported encoding: {:?}", supported_encoding);
            self.last_supported_encoding = Some(supported_encoding.clone());
            misc.set_supported_encoding(supported_encoding);
            let mut msg = Message::new();
            msg.set_misc(misc);
            self.inner.send(msg.into());
        };
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    async fn handle_cursor_switch_display(&mut self, pos: CursorPosition) {
        if self.multi_ui_session {
            return;
        }
        let displays = super::display_service::get_sync_displays();
        let d_index = displays.iter().position(|d| {
            let scale = d.scale;
            pos.x >= d.x
                && pos.y >= d.y
                && (pos.x - d.x) as f64 * scale < d.width as f64
                && (pos.y - d.y) as f64 * scale < d.height as f64
        });
        if let Some(d_index) = d_index {
            if self.display_idx != d_index {
                let mut misc = Misc::new();
                misc.set_follow_current_display(d_index as i32);
                let mut msg_out = Message::new();
                msg_out.set_misc(misc);
                self.send(msg_out).await;
            }
        }
    }
}

pub fn insert_switch_sides_uuid(id: String, uuid: uuid::Uuid) {
    SWITCH_SIDES_UUID
        .lock()
        .unwrap()
        .insert(id, (tokio::time::Instant::now(), uuid));
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
async fn start_ipc(
    mut rx_to_cm: mpsc::UnboundedReceiver<ipc::Data>,
    tx_from_cm: mpsc::UnboundedSender<ipc::Data>,
    mut _rx_desktop_ready: mpsc::Receiver<()>,
    tx_stream_ready: mpsc::Sender<()>,
) -> ResultType<()> {
    use hbb_common::anyhow::anyhow;

    loop {
        if !crate::platform::is_prelogin() {
            break;
        }
        sleep(1.).await;
    }
    let mut stream = None;
    if let Ok(s) = crate::ipc::connect(1000, "_cm").await {
        stream = Some(s);
    } else {
        #[allow(unused_mut)]
        #[allow(unused_assignments)]
        let mut args = vec!["--cm"];
        #[allow(unused_mut)]
        #[cfg(target_os = "linux")]
        let mut user = None;

        // Cm run as user, wait until desktop session is ready.
        #[cfg(target_os = "linux")]
        if crate::platform::is_headless_allowed() && linux_desktop_manager::is_headless() {
            let mut username = linux_desktop_manager::get_username();
            loop {
                if !username.is_empty() {
                    break;
                }
                let _res = timeout(1_000, _rx_desktop_ready.recv()).await;
                username = linux_desktop_manager::get_username();
            }
            let uid = {
                let output = run_cmds(&format!("id -u {}", &username))?;
                let output = output.trim();
                if output.is_empty() || !output.parse::<i32>().is_ok() {
                    bail!("Invalid username {}", &username);
                }
                output.to_string()
            };
            user = Some((uid, username));
            args = vec!["--cm-no-ui"];
        }
        let run_done;
        if crate::platform::is_root() {
            let mut res = Ok(None);
            for _ in 0..10 {
                #[cfg(not(any(target_os = "linux")))]
                {
                    log::debug!("Start cm");
                    res = crate::platform::run_as_user(args.clone());
                }
                #[cfg(target_os = "linux")]
                {
                    log::debug!("Start cm");
                    res = crate::platform::run_as_user(
                        args.clone(),
                        user.clone(),
                        None::<(&str, &str)>,
                    );
                }
                if res.is_ok() {
                    break;
                }
                log::error!("Failed to run cm: {res:?}");
                sleep(1.).await;
            }
            if let Some(task) = res? {
                super::CHILD_PROCESS.lock().unwrap().push(task);
            }
            run_done = true;
        } else {
            run_done = false;
        }
        if !run_done {
            log::debug!("Start cm");
            super::CHILD_PROCESS
                .lock()
                .unwrap()
                .push(crate::run_me(args)?);
        }
        for _ in 0..20 {
            sleep(0.3).await;
            if let Ok(s) = crate::ipc::connect(1000, "_cm").await {
                stream = Some(s);
                break;
            }
        }
        if stream.is_none() {
            bail!("Failed to connect to connection manager");
        }
    }

    let _res = tx_stream_ready.send(()).await;
    let mut stream = stream.ok_or(anyhow!("none stream"))?;
    loop {
        tokio::select! {
            res = stream.next() => {
                match res {
                    Err(err) => {
                        return Err(err.into());
                    }
                    Ok(Some(data)) => {
                        match data {
                            ipc::Data::ClickTime(_)=> {
                                let ct = CLICK_TIME.load(Ordering::SeqCst);
                                let data = ipc::Data::ClickTime(ct);
                                stream.send(&data).await?;
                            }
                            _ => {
                                tx_from_cm.send(data)?;
                            }
                        }
                    }
                    _ => {}
                }
            }
            res = rx_to_cm.recv() => {
                match res {
                    Some(data) => {
                        if let Data::FS(ipc::FS::WriteBlock{id,
                            file_num,
                            data,
                            compressed}) = data {
                                stream.send(&Data::FS(ipc::FS::WriteBlock{id, file_num, data: Bytes::new(), compressed})).await?;
                                stream.send_raw(data).await?;
                        } else {
                            stream.send(&data).await?;
                        }
                    }
                    None => {
                        bail!("expected");
                    }
                }
            }
        }
    }
}

// in case screen is sleep and blank, here to activate it
fn try_activate_screen() {
    #[cfg(windows)]
    std::thread::spawn(|| {
        mouse_move_relative(-6, -6);
        std::thread::sleep(std::time::Duration::from_millis(30));
        mouse_move_relative(6, 6);
    });
}

pub enum AlarmAuditType {
    IpWhitelist = 0,
    ExceedThirtyAttempts = 1,
    SixAttemptsWithinOneMinute = 2,
}

pub enum FileAuditType {
    RemoteSend = 0,
    RemoteReceive = 1,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FileActionLog {
    id: i32,
    conn_id: i32,
    path: String,
    dir: bool,
}

struct FileRemoveLogControl {
    conn_id: i32,
    instant: Instant,
    removed_files: Vec<FileRemoveFile>,
    removed_dirs: Vec<FileRemoveDir>,
}

impl FileRemoveLogControl {
    fn new(conn_id: i32) -> Self {
        FileRemoveLogControl {
            conn_id,
            instant: Instant::now(),
            removed_files: vec![],
            removed_dirs: vec![],
        }
    }

    fn on_remove_file(&mut self, f: FileRemoveFile) -> Option<ipc::Data> {
        self.instant = Instant::now();
        self.removed_files.push(f.clone());
        Some(ipc::Data::FileTransferLog((
            "remove".to_string(),
            serde_json::to_string(&FileActionLog {
                id: f.id,
                conn_id: self.conn_id,
                path: f.path,
                dir: false,
            })
            .unwrap_or_default(),
        )))
    }

    fn on_remove_dir(&mut self, d: FileRemoveDir) -> Option<ipc::Data> {
        self.instant = Instant::now();
        let direct_child = |parent: &str, child: &str| {
            PathBuf::from(child).parent().map(|x| x.to_path_buf()) == Some(PathBuf::from(parent))
        };
        self.removed_files
            .retain(|f| !direct_child(&f.path, &d.path));
        self.removed_dirs
            .retain(|x| !direct_child(&d.path, &x.path));
        if !self
            .removed_dirs
            .iter()
            .any(|x| direct_child(&x.path, &d.path))
        {
            self.removed_dirs.push(d.clone());
        }
        Some(ipc::Data::FileTransferLog((
            "remove".to_string(),
            serde_json::to_string(&FileActionLog {
                id: d.id,
                conn_id: self.conn_id,
                path: d.path,
                dir: true,
            })
            .unwrap_or_default(),
        )))
    }

    fn on_timer(&mut self) -> Vec<ipc::Data> {
        if self.instant.elapsed().as_secs() < 1 {
            return vec![];
        }
        let mut v: Vec<ipc::Data> = vec![];
        self.removed_files
            .drain(..)
            .map(|f| {
                v.push(ipc::Data::FileTransferLog((
                    "remove".to_string(),
                    serde_json::to_string(&FileActionLog {
                        id: f.id,
                        conn_id: self.conn_id,
                        path: f.path,
                        dir: false,
                    })
                    .unwrap_or_default(),
                )));
            })
            .count();
        self.removed_dirs
            .drain(..)
            .map(|d| {
                v.push(ipc::Data::FileTransferLog((
                    "remove".to_string(),
                    serde_json::to_string(&FileActionLog {
                        id: d.id,
                        conn_id: self.conn_id,
                        path: d.path,
                        dir: true,
                    })
                    .unwrap_or_default(),
                )));
            })
            .count();
        v
    }
}

fn start_wakelock_thread() -> std::sync::mpsc::Sender<(usize, usize)> {
    use crate::platform::{get_wakelock, WakeLock};
    let (tx, rx) = std::sync::mpsc::channel::<(usize, usize)>();
    std::thread::spawn(move || {
        let mut wakelock: Option<WakeLock> = None;
        let mut last_display = false;
        loop {
            match rx.recv() {
                Ok((conn_count, remote_count)) => {
                    if conn_count == 0 {
                        wakelock = None;
                        log::info!("drop wakelock");
                    } else {
                        let mut display = remote_count > 0;
                        if let Some(_w) = wakelock.as_mut() {
                            if display != last_display {
                                #[cfg(any(target_os = "windows", target_os = "macos"))]
                                {
                                    log::info!("set wakelock display to {display}");
                                    if let Err(e) = _w.set_display(display) {
                                        log::error!(
                                            "failed to set wakelock display to {display}: {e:?}"
                                        );
                                    }
                                }
                            }
                        } else {
                            if cfg!(target_os = "linux") {
                                display = true;
                            }
                            wakelock = Some(get_wakelock(display));
                        }
                        last_display = display;
                    }
                }
                Err(e) => {
                    log::error!("wakelock receive error: {e:?}");
                    break;
                }
            }
        }
    });
    tx
}

#[cfg(windows)]
pub struct PortableState {
    pub last_uac: bool,
    pub last_foreground_window_elevated: bool,
    pub last_running: Option<bool>,
    pub is_installed: bool,
}

#[cfg(windows)]
impl Default for PortableState {
    fn default() -> Self {
        Self {
            is_installed: crate::platform::is_installed(),
            last_uac: Default::default(),
            last_foreground_window_elevated: Default::default(),
            last_running: Default::default(),
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        self.release_pressed_modifiers();
    }
}

#[cfg(target_os = "linux")]
struct LinuxHeadlessHandle {
    pub is_headless_allowed: bool,
    pub is_headless: bool,
    pub wait_ipc_timeout: u64,
    pub rx_cm_stream_ready: mpsc::Receiver<()>,
    pub tx_desktop_ready: mpsc::Sender<()>,
}

#[cfg(target_os = "linux")]
impl LinuxHeadlessHandle {
    pub fn new(rx_cm_stream_ready: mpsc::Receiver<()>, tx_desktop_ready: mpsc::Sender<()>) -> Self {
        let is_headless_allowed = crate::is_server() && crate::platform::is_headless_allowed();
        let is_headless = is_headless_allowed && linux_desktop_manager::is_headless();
        Self {
            is_headless_allowed,
            is_headless,
            wait_ipc_timeout: 10_000,
            rx_cm_stream_ready,
            tx_desktop_ready,
        }
    }

    pub fn try_start_desktop(&mut self, os_login: Option<&OSLogin>) -> String {
        if self.is_headless_allowed {
            match os_login {
                Some(os_login) => {
                    linux_desktop_manager::try_start_desktop(&os_login.username, &os_login.password)
                }
                None => linux_desktop_manager::try_start_desktop("", ""),
            }
        } else {
            "".to_string()
        }
    }

    pub async fn wait_desktop_cm_ready(&mut self) {
        if self.is_headless {
            self.tx_desktop_ready.send(()).await.ok();
            let _res = timeout(self.wait_ipc_timeout, self.rx_cm_stream_ready.recv()).await;
        }
    }
}

extern "C" fn connection_shutdown_hook() {
    // https://stackoverflow.com/questions/35980148/why-does-an-atexit-handler-panic-when-it-accesses-stdout
    // Please make sure there is no print in the call stack
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    {
        *WALLPAPER_REMOVER.lock().unwrap() = None;
    }
}

#[cfg(target_os = "macos")]
#[derive(Debug, Default)]
struct Retina {
    displays: Vec<DisplayInfo>,
}

#[cfg(target_os = "macos")]
impl Retina {
    #[inline]
    fn set_displays(&mut self, displays: &Vec<DisplayInfo>) {
        self.displays = displays.clone();
    }

    #[inline]
    fn on_mouse_event(&mut self, e: &mut MouseEvent, current: usize) {
        let evt_type = e.mask & 0x7;
        if evt_type == crate::input::MOUSE_TYPE_WHEEL {
            // x and y are always 0, +1 or -1
            return;
        }
        let Some(d) = self.displays.get(current) else {
            return;
        };
        let s = d.scale;
        if s > 1.0 && e.x >= d.x && e.y >= d.y && e.x < d.x + d.width && e.y < d.y + d.height {
            e.x = d.x + ((e.x - d.x) as f64 / s) as i32;
            e.y = d.y + ((e.y - d.y) as f64 / s) as i32;
        }
    }

    #[inline]
    fn on_cursor_pos(&mut self, pos: &CursorPosition, current: usize) -> Option<Message> {
        let Some(d) = self.displays.get(current) else {
            return None;
        };
        let s = d.scale;
        if s > 1.0
            && pos.x >= d.x
            && pos.y >= d.y
            && (pos.x - d.x) as f64 * s < d.width as f64
            && (pos.y - d.y) as f64 * s < d.height as f64
        {
            let mut pos = pos.clone();
            pos.x = d.x + ((pos.x - d.x) as f64 * s) as i32;
            pos.y = d.y + ((pos.y - d.y) as f64 * s) as i32;
            let mut msg = Message::new();
            msg.set_cursor_position(pos);
            return Some(msg);
        }
        None
    }
}

mod raii {
    // CONN_COUNT: remote connection count in fact
    // ALIVE_CONNS: all connections, including unauthorized connections
    // AUTHED_CONNS: all authorized connections

    use super::*;
    pub struct ConnectionID(i32);

    impl ConnectionID {
        pub fn new(id: i32) -> Self {
            ALIVE_CONNS.lock().unwrap().push(id);
            Self(id)
        }
    }

    impl Drop for ConnectionID {
        fn drop(&mut self) {
            let mut active_conns_lock = ALIVE_CONNS.lock().unwrap();
            active_conns_lock.retain(|&c| c != self.0);
            video_service::VIDEO_QOS
                .lock()
                .unwrap()
                .on_connection_close(self.0);
        }
    }

    pub struct AuthedConnID(i32, AuthConnType);

    impl AuthedConnID {
        pub fn new(id: i32, conn_type: AuthConnType) -> Self {
            AUTHED_CONNS.lock().unwrap().push((id, conn_type));
            Self::check_wake_lock();
            use std::sync::Once;
            static _ONCE: Once = Once::new();
            _ONCE.call_once(|| {
                shutdown_hooks::add_shutdown_hook(connection_shutdown_hook);
            });
            Self(id, conn_type)
        }

        fn check_wake_lock() {
            let conn_count = AUTHED_CONNS.lock().unwrap().len();
            let remote_count = AUTHED_CONNS
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.1 == AuthConnType::Remote)
                .count();
            allow_err!(WAKELOCK_SENDER
                .lock()
                .unwrap()
                .send((conn_count, remote_count)));
        }

        pub fn remote_and_file_conn_count() -> usize {
            AUTHED_CONNS
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.1 == AuthConnType::Remote || c.1 == AuthConnType::FileTransfer)
                .count()
        }
    }

    impl Drop for AuthedConnID {
        fn drop(&mut self) {
            if self.1 == AuthConnType::Remote {
                scrap::codec::Encoder::update(scrap::codec::EncodingUpdate::Remove(self.0));
            }
            AUTHED_CONNS.lock().unwrap().retain(|&c| c.0 != self.0);
            let remote_count = AUTHED_CONNS
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.1 == AuthConnType::Remote)
                .count();
            if remote_count == 0 {
                #[cfg(any(target_os = "windows", target_os = "linux"))]
                {
                    *WALLPAPER_REMOVER.lock().unwrap() = None;
                }
                #[cfg(not(any(target_os = "android", target_os = "ios")))]
                display_service::reset_resolutions();
                #[cfg(windows)]
                let _ = virtual_display_manager::reset_all();
                #[cfg(target_os = "linux")]
                scrap::wayland::pipewire::try_close_session();
            }
            Self::check_wake_lock();
        }
    }
}

mod test {
    #[allow(unused)]
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn retina() {
        let mut retina = Retina {
            displays: vec![DisplayInfo {
                x: 10,
                y: 10,
                width: 1000,
                height: 1000,
                scale: 2.0,
                ..Default::default()
            }],
        };
        let mut mouse: MouseEvent = MouseEvent {
            x: 510,
            y: 510,
            ..Default::default()
        };
        retina.on_mouse_event(&mut mouse, 0);
        assert_eq!(mouse.x, 260);
        assert_eq!(mouse.y, 260);
        let pos = CursorPosition {
            x: 260,
            y: 260,
            ..Default::default()
        };
        let msg = retina.on_cursor_pos(&pos, 0).unwrap();
        let pos = msg.cursor_position();
        assert_eq!(pos.x, 510);
        assert_eq!(pos.y, 510);
    }
}
