use std::collections::HashMap;
use std::io;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, RwLock,
};
use std::time::{Duration, UNIX_EPOCH};

use tokio::io::Result;
use tokio::time;

use futures::channel::mpsc;
use futures::channel::oneshot;
use futures::future::{select, AbortHandle, Abortable, Either};
use futures::stream::StreamExt;
use futures_util::sink::SinkExt;

use crate::room::*;
use crate::state::State;
use crate::user::*;
use crate::{get_appservice_sendable_user, get_matrix_client, get_state, TOMSG_IP};

use tomsg_rs::connection::*;
use tomsg_rs::{Command, Id, Line, Message, PushMessage, Reply, Word};

use ruma::events::room::message::{
    FormattedBody, InReplyTo, MessageEvent, MessageEventContent, MessageFormat, RelatesTo,
    TextMessageEventContent,
};
use ruma::events::{AnyMessageEvent, AnyRoomEvent};
use ruma::identifiers::{EventId, UserId};

use rand::{thread_rng, Rng};

use once_cell::sync::Lazy;

use regex::Regex;

use matrix_appservice_rs::convert::to_matrix;
use matrix_appservice_rs::{Mappable, MappingId, MatrixToItem};

static RE_MX_REPLY: Lazy<Regex> = Lazy::new(|| Regex::new(r"<mx-reply>.+?</mx-reply>").unwrap());

const NUM_HISTORY_PER_ITER: i64 = 20;
const CONNECT_MAX_TRIES: u32 = 10;

macro_rules! get_or_make_plumbed_room {
    ($state:expr, $tomsg_name:expr) => {{
        let state = $state;
        let name = $tomsg_name;
        match state.get_room_mut(MappingId::External(&name)) {
            Some(r) => r,
            None => state.create_plumbed_room(get_matrix_client(), name).await,
        }
    }};
}

/// Get the puppet for the given message.
/// This does not make a puppet if it does not exist.
///
/// If `Ok`, the contained value is the puppet as `RoomUser`.
/// If `Err(true)`, the user has been found, but is a real user.
/// If `Err(false)`, the user has not been found or is not in the room.
async fn get_puppet(msg: &Message, state: &mut State) -> std::result::Result<RoomUser, bool> {
    let user = match state.get_user(MappingId::External(&msg.username)) {
        None => {
            println!("no user found with tomsg username '{}'", msg.username);
            return Err(false);
        }
        Some(u) => u,
    };

    // REVIEW: do we actually want to use `ensure_puppet` here?

    let puppet = match &user.0 {
        User::Real { .. } => {
            // user is real user
            return Err(true);
        }
        User::Puppet { .. } => user.to_owned(),
    };

    let room = get_or_make_plumbed_room!(state, msg.roomname.clone());
    match room.to_room_user(puppet) {
        Ok(p) => Ok(p),
        Err(u) => {
            println!(
                "puppet for {} not in room {}.",
                u.as_external(),
                room.as_matrix()
            );
            Err(false)
        }
    }
}

/// retrieve history for the room, as much as is needed.
pub async fn get_unhandled_history(ch: &mut Channel, room: &mut Room) -> Vec<Message> {
    if room.fetched_tomsg_history {
        return vec![];
    }

    let mut res = vec![];

    let mut earliest_id: Option<Id> = None;
    let mut get_history = true;

    while get_history {
        let reply = match earliest_id {
            None => ch
                .send(Command::History {
                    roomname: room.as_external().to_owned(),
                    count: NUM_HISTORY_PER_ITER,
                })
                .await
                .unwrap(),
            Some(id) => ch
                .send(Command::HistoryBefore {
                    roomname: room.as_external().to_owned(),
                    count: NUM_HISTORY_PER_ITER,
                    message_id: id,
                })
                .await
                .unwrap(),
        };
        let messages = reply.history().unwrap();

        if let Some(msg) = messages.first() {
            earliest_id = Some(msg.id);
        }

        get_history = false;
        for msg in messages {
            if room.handled_message(MappingId::External(&msg.id)) {
                continue;
            }
            get_history = true;

            res.push(msg);
        }
    }

    eprintln!(
        "retrieved history for room {} ({}): {:?}",
        room.as_matrix(),
        room.as_external(),
        res
    );

    room.fetched_tomsg_history = true;

    res
}

async fn handle_tomsg_message(state: &mut State, msg: Message) {
    println!("msg {:?}", msg);
    let db = state.db.clone();

    let (is_appservice, puppet) = match get_puppet(&msg, state).await {
        Ok(p) => (false, p.into()),
        Err(_) => (true, (*get_appservice_sendable_user()).clone()),
        //Err(true) => return,
    };

    let room = get_or_make_plumbed_room!(state, msg.roomname.clone());
    if room.handled_message(MappingId::External(&msg.id)) {
        println!(
            "already handled tomsg message with id {}, ignoring...",
            msg.id
        );
        return;
    }

    let body = match is_appservice {
        false => msg.message.into_string(),
        true => format!("<{}> {}", msg.username, msg.message),
    };

    let reply_event_id = match msg.reply_on {
        None => None,
        Some(id) => {
            let event_id = match room.get_handled_message(MappingId::External(&id)) {
                // create a random (hopefuly invalid) ID.
                // TODO: actually fetch the message from the tomsg server.
                None => EventId::new(&get_matrix_client().server_name()),
                Some(msg) => msg.as_matrix().to_owned(),
            };
            Some(event_id)
        }
    };

    let event = match reply_event_id.clone() {
        None => None,
        Some(event_id) => {
            get_matrix_client()
                .get_room_event(room.as_matrix(), &event_id)
                .await
        }
    };

    let formatted_body = {
        let event: Option<MessageEvent> = match event {
            None => None,
            Some(event) => {
                let event: Option<AnyRoomEvent> = match event.deserialize() {
                    Err(_) => None,
                    Ok(event) => Some(event),
                };
                match event {
                    None => None,
                    Some(AnyRoomEvent::Message(AnyMessageEvent::RoomMessage(event))) => Some(event),
                    Some(unknown) => {
                        eprintln!("we got {:?} instead of MessageEvent, giving None", unknown);
                        None
                    }
                }
            }
        };

        let body = {
            let map: HashMap<_, _> = room
                .participants
                .iter()
                .map(|(user_id, name)| (name.to_string(), MatrixToItem::User(user_id)))
                .collect();

            let info = to_matrix::Info { map };

            let encoded = html_escape::encode_safe(&body);
            to_matrix::convert(encoded.to_string(), &info)
        };

        match event {
            None => body,
            Some(event) => {
                let formatted_body: String = match event.content {
                    MessageEventContent::Text(c) => c.formatted.map(|f| f.body).unwrap_or(c.body),
                    unknown => {
                        eprintln!("don't know how to handle {:?}", unknown);
                        "Sent something".to_string()
                    }
                };
                let sender_id: UserId = event.sender;

                format!(
                    "<mx-reply><blockquote><a href=\"{}\">In reply to</a> <a href=\"{}\">{}</a><br>{}</blockquote></mx-reply>{}",
                    MatrixToItem::Event(room.as_matrix(), &event.event_id).to_url_string(),
                    MatrixToItem::User(&sender_id).to_url_string(),
                    sender_id.to_string(),
                    RE_MX_REPLY.replace(&formatted_body, ""),
                    body,
                )
            }
        }
    };

    let message_data = MessageEventContent::Text(TextMessageEventContent {
        body,
        formatted: Some(FormattedBody {
            format: MessageFormat::Html,
            body: formatted_body,
        }),
        relates_to: reply_event_id.map(|event_id| RelatesTo {
            in_reply_to: Some(InReplyTo { event_id }),
        }),
    });

    let matrix_id = get_matrix_client()
        .create_message(
            room.as_matrix(),
            &puppet,
            message_data,
            msg.timestamp
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64,
        )
        .await
        .event_id;

    {
        let db = db.lock().unwrap();
        room.handle_message(&db, &msg.id, matrix_id);
    }
}

type Pair = (Command, oneshot::Sender<Result<Reply>>);
type Sender = mpsc::Sender<Pair>;
type Receiver = mpsc::Receiver<Pair>;

#[derive(Clone)]
pub struct Channel {
    // debug
    pub tag: Arc<RwLock<String>>,

    ch: Sender,
    // TODO: actually use this
    is_connecting: Arc<AtomicBool>,
}

impl Channel {
    pub async fn send(&mut self, msg: Command) -> Result<Reply> {
        {
            let tag = self.tag.read().unwrap();
            println!("[tomsg] {} command: {:?}", tag, msg);
        }

        let (sender, receiver) = oneshot::channel();

        if let Err(e) = self.ch.send((msg, sender)).await {
            if e.is_full() {
                return Err(io::Error::new(io::ErrorKind::Other, "channel full"));
            } else if e.is_disconnected() {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "channel receiver disconnected",
                ));
            }

            return Err(io::Error::new(io::ErrorKind::Other, "unknown send error"));
        }

        receiver.await.unwrap() // this should not be able fail
    }

    pub fn is_connecting(&self) -> bool {
        self.is_connecting.load(Ordering::SeqCst)
    }

    pub fn close(&mut self) {
        self.ch.close_channel()
    }
}

pub struct ConnectionShed {
    map: HashMap<UserId, (Channel, AbortHandle)>,
}

impl ConnectionShed {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    pub fn get_channel(&self, user_id: &UserId) -> Option<Channel> {
        self.map.get(user_id).map(|v| v.0.clone())
    }

    /// connects and stay connected
    pub async fn ensure_connection(&mut self, user: &ManagedUser) -> Result<Channel> {
        if let Some(ch) = self.get_channel(user.as_matrix()) {
            return Ok(ch);
        }

        self.bind(user.clone()).await
    }

    /// binds the given matrix_id with the given tomsg_credentials.
    pub async fn bind(&mut self, user: ManagedUser) -> Result<Channel> {
        let matrix_id = user.as_matrix().to_owned();
        self.drop_connection(&matrix_id).await;

        let (abort_handle, abort_registration) = AbortHandle::new_pair();
        let (sender, mut receiver): (Sender, Receiver) = mpsc::channel(0);

        let ch = Channel {
            tag: Arc::new(RwLock::new("".to_string())),
            ch: sender,
            is_connecting: Arc::new(AtomicBool::new(true)),
        };
        let ch_cloned = ch.clone();

        let t = async move {
            loop {
                let pair = match try_connect(CONNECT_MAX_TRIES).await {
                    Err(e) => {
                        eprintln!("giving up trying to connect... Err: {}", e);
                        return;
                    }
                    Ok(p) => p,
                };
                let conn = pair.0.clone();

                let mut handler = async {
                    loop {
                        let message = receiver.next().await;
                        match message {
                            None => {
                                // don't reconnect
                                return false;
                            }
                            Some((message, sender)) => {
                                let res = match conn.send_command(message).await {
                                    Err(e) => {
                                        eprintln!("sending failed with: {}", e);
                                        return true;
                                    }
                                    Ok(res) => res,
                                };
                                let res = match res {
                                    Err(e) => {
                                        eprintln!("sending failed with: {:?}", e);
                                        return true;
                                    }
                                    Ok(res) => res,
                                };
                                sender.send(Ok(res)).unwrap();
                            }
                        }
                    }
                };
                let handler = unsafe { Pin::new_unchecked(&mut handler) };

                let mut t = bind(pair.0, ch_cloned.clone(), pair.1, user.clone());
                let t = unsafe { Pin::new_unchecked(&mut t) };

                match select(t, handler).await {
                    Either::Left(_) | Either::Right((true, _)) => {
                        eprintln!("connection closed, trying again...");
                    }
                    Either::Right((false, _)) => {
                        eprintln!(
                            "the receive end of the send channel is closed, closing the bind task"
                        );
                        break;
                    }
                }
            }
        };

        let t = Abortable::new(t, abort_registration);
        tokio::spawn(async move {
            t.await.unwrap();
        });

        self.map.insert(matrix_id, (ch.clone(), abort_handle));
        Ok(ch)
    }

    pub async fn drop_connection(&mut self, matrix_id: &UserId) -> bool {
        if let Some((mut ch, handle)) = self.map.remove(matrix_id) {
            ch.close();
            handle.abort();
            true
        } else {
            false
        }
    }
}

pub type ConnPair = (Arc<Connection>, mpsc::Receiver<PushMessage>);

pub async fn connect() -> Result<ConnPair> {
    let ip = TOMSG_IP.get().unwrap();
    println!("using ip {}", ip);
    let (conn, push_channel) = Connection::connect(Type::Plain, *ip).await?;

    Ok((Arc::new(conn), push_channel))
}

async fn try_connect(max_tries: u32) -> Result<ConnPair> {
    let mut ntry: u32 = 0;
    loop {
        let mut duration = Duration::from_millis(thread_rng().gen_range(0, 500));
        if ntry > 0 {
            duration += Duration::from_secs(2_u64.pow(ntry));
            eprintln!(
                "connecting try {}, backing off for {} seconds",
                ntry,
                duration.as_secs()
            );
        }
        time::delay_for(duration).await;

        let res = connect().await;
        match res {
            Ok(p) => break Ok(p),
            Err(e) => {
                ntry += 1;
                if ntry >= max_tries {
                    break Err(e);
                }
            }
        }
    }
}

#[derive(Debug)]
pub enum RegisterError {
    IO(io::Error),
    UsernameTaken,
}

impl From<io::Error> for RegisterError {
    fn from(e: io::Error) -> Self {
        RegisterError::IO(e)
    }
}

pub async fn register(
    conn: &Connection,
    username: Word,
    password: Line,
    auto_generated: bool,
) -> std::result::Result<TomsgCredentials, RegisterError> {
    let res = conn
        .send_command(Command::Register {
            username: username.clone(),
            password: password.clone(),
        })
        .await?
        .map_err(|_| io::Error::new(io::ErrorKind::ConnectionAborted, "connection reset"))?;

    match res {
        Reply::Ok => Ok(TomsgCredentials {
            username,
            password,
            auto_generated,
        }),
        Reply::Error(_) => Err(RegisterError::UsernameTaken),
        _ => panic!("expected ok or error"),
    }
}

async fn bind(
    conn: Arc<Connection>,
    mut ch: Channel,
    mut push_channel: mpsc::Receiver<PushMessage>,
    user: ManagedUser,
) -> Result<()> {
    assert!(
        !user.is_puppet()
            && get_state()
                .lock()
                .await
                .users
                .has(MappingId::External(user.as_external()))
    );

    let (creds, matrix_id) = match &user.0 {
        User::Puppet { .. } => unreachable!(),
        User::Real {
            tomsg_credentials,
            matrix_id,
        } => (tomsg_credentials.clone(), matrix_id.clone()),
    };

    match ch
        .send(Command::Login {
            username: creds.username.clone(),
            password: creds.password,
        })
        .await?
    {
        Reply::Ok => *ch.tag.write().unwrap() = creds.username.to_string(),
        Reply::Error(err) => {
            // TODO: actually recover from this in some way. We should at least send an error in
            // the management room, or create one and send it there if it doesn't exist.
            panic!(format!(
                "got error while logging in ({}): {}.  Throwing a panic.",
                creds.username, err
            ));
        }
        _ => panic!("expected Ok or Err"),
    }

    // make sure the puppet is invited to all the rooms, and the rooms exist.
    let rooms = ch.send(Command::ListRooms).await?.list().unwrap();
    {
        let mut state = get_state().lock().await;
        let db = state.db.clone();

        for tomsg_name in rooms {
            let room = get_or_make_plumbed_room!(&mut state, tomsg_name.clone());
            let should_invite = {
                let db = db.lock().unwrap();
                room.insert_user(&db, &user)
            };

            let room_tomsg_name = room.as_external().to_owned();
            let room_matrix_id = room.as_matrix().to_owned();

            get_matrix_client()
                .invite_tomsg_members(&mut state, room_tomsg_name, &room_matrix_id, &mut ch)
                .await;

            if should_invite {
                get_matrix_client()
                    .invite_matrix_user(
                        &room_matrix_id,
                        &get_appservice_sendable_user(),
                        &matrix_id,
                    )
                    .await;
            }

            // fetch history of room
            let room = get_or_make_plumbed_room!(&mut state, tomsg_name.clone());
            let history = get_unhandled_history(&mut ch, room).await;
            for msg in history {
                handle_tomsg_message(&mut state, msg).await;
            }
        }
    }

    let mut ch_cloned = ch.clone();
    let conn_cloned = conn.clone();

    // handle tomsg messages
    let t = tokio::spawn(async move {
        loop {
            let push_message = push_channel.next().await;
            match push_message {
                None => {
                    eprintln!(
                        "tomsg connection for {} ({}) has been closed, reason: {:?}. We're returning from the bind() function",
                        user.as_external(),
                        user.as_matrix(),
                        conn_cloned.close_reason().await,
                    );
                    return;
                }
                Some(message) => handle_push(&user, &mut ch_cloned, message).await.unwrap(),
            };
        }
    });

    // ping loop
    tokio::spawn(async move {
        loop {
            if conn.is_closed().await {
                eprintln!("DEBUG conn closed in ping loop");
                return;
            }

            ch.send(Command::Ping).await.unwrap();
            time::delay_for(Duration::from_secs(60)).await;
        }
    });

    t.await?;
    eprintln!("t in bind() has finished.");
    Ok(())
}

async fn handle_push(user: &ManagedUser, conn: &mut Channel, message: PushMessage) -> Result<()> {
    match message {
        PushMessage::Message(msg) => {
            let mut state = get_state().lock().await;
            handle_tomsg_message(&mut state, msg).await;
        }

        PushMessage::Invite { roomname, inviter } => {
            println!("a {} {}", roomname, inviter);
            let mut state = get_state().lock().await;
            println!("b {} {}", roomname, inviter);

            let db = state.db.clone();

            let inviter = state.users.get(MappingId::External(&inviter)).cloned();

            let room = get_or_make_plumbed_room!(&mut state, roomname.clone());
            room.insert_and_invite(&db, &get_matrix_client(), conn, &user)
                .await;

            let inviter = inviter.and_then(|inviter| room.to_room_user(inviter).ok());
            let inviter = match inviter {
                Some(u) => u.into(),
                None => (*get_appservice_sendable_user()).to_owned(),
            };

            let tomsg_name = room.as_external().to_owned();
            let matrix_id = room.as_matrix().to_owned();

            get_matrix_client()
                .invite_tomsg_members(&mut state, tomsg_name, &matrix_id, conn)
                .await;

            let invited = user.as_matrix();

            get_matrix_client()
                .invite_matrix_user(&matrix_id, &inviter, invited)
                .await;
        }

        PushMessage::Join { roomname, username } => {
            let mut state = get_state().lock().await;
            let db = state.db.clone();

            let user = match state.ensure_puppet(&get_matrix_client(), &username).await {
                None => {
                    eprintln!(
                        "we already got a real user for tomsg user {}. Ignoring Join.",
                        username
                    );
                    return Ok(());
                }
                Some(u) => u,
            };
            let room = get_or_make_plumbed_room!(&mut state, roomname.clone());

            room.insert_and_invite(&db, &get_matrix_client(), conn, &user)
                .await;
        }

        PushMessage::Online { sessions, username } => {
            println!("todo pushmessage online ({} {})", sessions, username)
        }
    }

    Ok(())
}
