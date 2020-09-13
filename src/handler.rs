use std::convert::Infallible;
use std::convert::TryFrom;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tomsg_rs::command::Command;
use tomsg_rs::id::Id;
use tomsg_rs::line::Line;

use crate::command::handle_command;
use crate::db::Database;
use crate::room::{Room, RoomUser};
use crate::state::State;
use crate::tomsg::{Channel, ConnectionShed};
use crate::user::ManagedUser;
use crate::{get_local_part, get_matrix_client, get_state, TOMSG_CONN_SHED};

use matrix_appservice_rs::{mxc_to_url, serve, Mappable, MappingId};

use ruma::events::room::member::MembershipState;
use ruma::events::room::message::{MessageEventContent, RelatesTo};
use ruma::events::{AnyEvent, AnyMessageEvent, AnyStateEvent};
use ruma::identifiers::EventId;
use ruma::identifiers::RoomId;
use ruma::identifiers::UserId;
use ruma::Raw;

struct TomsgSendInfo<'a> {
    db: &'a Arc<Mutex<Database>>,
    ch: &'a mut Channel,
    room: &'a mut Room,
    sender: &'a RoomUser,
    line: Line,
    reply_to: Option<Id>,
    matrix_event_id: EventId,
}

async fn send_message_tomsg(info: TomsgSendInfo<'_>) {
    let res = info
        .ch
        .send(Command::Send(
            info.room.as_external().to_owned(),
            info.reply_to,
            info.line.clone(),
        ))
        .await
        .unwrap();
    let tomsg_message_id = res.number().expect("expected number");
    let tomsg_message_id = Id::try_from(tomsg_message_id).expect("expected Id");

    eprintln!(
        "[{} ({})] {} -> {} '{}'",
        info.sender.as_matrix(),
        info.sender.as_external(),
        info.matrix_event_id,
        tomsg_message_id,
        info.line
    );

    {
        let db = info.db.lock().unwrap();
        info.room
            .handle_message(&db, &tomsg_message_id, info.matrix_event_id);
    }
}

/// invite the given user as another user.
async fn invite_user(
    state: &mut State,
    shed: &mut ConnectionShed,
    db: &Arc<Mutex<Database>>,
    room_id: &RoomId,
    invited_user: &ManagedUser,
) -> bool {
    let room = state
        .rooms
        .get(MappingId::Matrix(room_id))
        .expect("room not found");

    // get an user that is in the room and we manage as the inviter.
    let inviter = room.matrix_invited_or_joined.iter().find_map(|user_id| {
        let user = state.get_user(MappingId::Matrix(user_id))?;

        if user.0.is_puppet() {
            // we need to have ownership over the tomsg connection, which only happens with real
            // users
            return None;
        } else if !room.in_room(&user) {
            // just to make sure that we got a user that is doubly managed
            return None;
        }

        Some(user)
    });

    let inviter = match inviter {
        Some(user) => user,
        None => return false,
    };

    let mut conn = shed.ensure_connection(&inviter).await.unwrap();

    {
        let room = state.rooms.get_mut(MappingId::Matrix(room_id)).unwrap();
        // invite the tomsg user in the room, and store that
        room.ensure_tomsg_user_in_room(&db, &mut conn, invited_user.as_external().to_owned())
            .await;

        {
            let db = db.lock().unwrap();

            // make sure that the user is marked as managed in the room (actually only useful for the
            // matrix side of things), this is a precondition for running this function.
            room.insert_user(&db, &invited_user);
        }
    }

    true
}

fn get_reply_to(event_id: &EventId, room: &Room) -> Option<Id> {
    room.get_handled_message(MappingId::Matrix(event_id))
        .map(|msg| msg.as_external().to_owned())
}

async fn get_room_user(
    state: &mut State,
    shed: &mut ConnectionShed,
    db: &Arc<Mutex<Database>>,
    user_id: &UserId,
    room_id: &RoomId,
) -> Option<RoomUser> {
    let room_id_cloned = room_id.clone();
    let room_mapping_id = MappingId::Matrix(&room_id_cloned);

    let user = state.ensure_real_user(user_id, None).await;

    let room = match state.rooms.get(room_mapping_id.clone()) {
        None => {
            println!("get_room_user: room not found");
            return None;
        }
        Some(r) => r,
    };

    shed.ensure_connection(&user).await.unwrap();
    if !room.in_room(&user) {
        // TODO: this should be enforced by typing system

        let found = invite_user(state, shed, &db, room_id, &user).await;
        if !found {
            println!(
                "no person in room which connection we can use for invite, ignoring message..."
            );
            return None;
        }
    }

    let room = state.rooms.get_mut(room_mapping_id).unwrap();
    room.to_room_user(user).ok()
}

fn is_puppet(info: &mut Info<'_>, user_id: &UserId) -> bool {
    info.state
        .get_user(MappingId::Matrix(user_id))
        .map(|u| u.0.is_puppet())
        .unwrap_or(false)
}

/// Retrieve the sender's information from the database.
/// If the user has been found, it _can_ be a puppet and we will extract that information.
/// If no information is found in the database, it can't be a puppet and we will create a new
/// real user for the sender.
async fn ensure_real_user(info: &mut Info<'_>, sender_id: &UserId) -> (bool, ManagedUser) {
    let sender_user = info.state.get_user(MappingId::Matrix(sender_id)).cloned();

    match sender_user {
        None => (false, info.state.ensure_real_user(sender_id, None).await),
        Some(u) => (u.is_puppet(), u),
    }
}

/*
async fn get_info<'a>(
    state: &mut State,
    shed: &mut ConnectionShed,
    db: &Arc<Mutex<Database>>,
    sender_id: UserId,
    room_id: RoomId,
) -> Option<(RoomUser, Channel, &'a mut Room)> {
    let room_mapping_id = MappingId::Matrix(room_id.clone());

    let sender = state.ensure_real_user(sender_id, None).await;
    let room = state.rooms.get(&room_mapping_id).expect("room not found");
    let mut conn = shed.ensure_connection(&sender).await.unwrap();
    if !room.in_room(&sender) {
        // TODO: this should be enforced by typing system

        let found = invite_user(&mut state, &mut shed, &db, room_id.clone(), &sender).await;
        if !found {
            println!(
                "no person in room which connection we can use for invite, ignoring message..."
            );
            return None;
        }
    }
    let room = state.rooms.get_mut(&room_mapping_id).unwrap();

    Some((sender, conn, room))
}
*/

struct Info<'a> {
    state: &'a mut State,
    db: &'a Arc<Mutex<Database>>,
}

async fn handle_message_event(mut info: Info<'_>, event: AnyMessageEvent) {
    // extract IDs for clarity
    let event_id = event.event_id().to_owned();
    let room_id = event.room_id().to_owned();
    let sender_id = event.sender().to_owned();

    let room_id_cloned = room_id.clone();
    let room_mapping_id = MappingId::Matrix(&room_id_cloned);

    if sender_id.localpart() == get_local_part() {
        return;
    }

    // We don't need to handle events sent by a puppet of ours.
    if is_puppet(&mut info, &sender_id) {
        println!(
            "ignorning message with id {} because it's from puppet {}",
            event_id, sender_id
        );
        return;
    }

    // get information about the room, whether it's a management room, and if it isn't, the `Room`
    // object.
    // `room` will be `None` iff `is_management_room == true`.
    let is_management_room = info.state.is_management_room(&room_id).is_some();
    /*
    let room = if is_management_room {
        None
    } else {
        match info.state.rooms.get(&room_mapping_id) {
            Some(r) => Some(r),
            None => {
                eprintln!("unhandled room: {}, skipping message", room_id);
                return;
            }
        }
    };
    */

    /*
    // REVIEW: earlier we fail when the room has not been found.
    // If a room can be found, we upgrade the sender to a `RoomUser` to make sure we're handling
    // the correct user.
    let (sender_room_user, sender_user) = {
        let room = info.state.rooms.get(MappingId::Matrix(room_id.clone()));
        match room {
            None => (None, Some(sender_user)),
            Some(room) => match room.to_room_user(sender_user) {
                Ok(room_user) => (Some(room_user), None),
                Err(user) => (None, Some(user)),
            },
        }
    };
    */

    /*
    let (sender_room_user, sender_user) = if is_management_room {
        (None, sender_user)
    } else {
        let mut shed = TOMSG_CONN_SHED.lock().await;

        match get_room_user(info.state, &mut shed, info.db, sender_id, room_id).await {
            None => (None, sender_user),
            Some(u) => (Some(u), sender_user),
        }
    };
    */

    macro_rules! handle_text_event {
        ($is_emote:expr, $body:expr, $formatted:expr, $relates_to:expr) => {{
            let is_emote: bool = $is_emote;
            let body: String = $body;
            let _formatted = $formatted;
            let relates_to: Option<RelatesTo> = $relates_to;

            let (user, mut conn) = {
                let mut shed = TOMSG_CONN_SHED.lock().await;

                let user = get_room_user(info.state, &mut shed, info.db, &sender_id, &room_id)
                    .await
                    .unwrap();
                let conn = shed.ensure_connection(&user).await.unwrap();

                (user, conn)
            };

            let room = info
                .state
                .rooms
                .get_mut(room_mapping_id)
                .expect("room is not a management room, but is None");

            // TODO

            let mut reply_to = relates_to
                .and_then(|r: RelatesTo| r.in_reply_to)
                .map(|r| r.event_id)
                .and_then(|event_id| get_reply_to(&event_id, room));

            let mut trimming_reply = reply_to.is_some();
            for line in body.trim_end().lines() {
                // HACKy reply fixing
                if trimming_reply && line.starts_with("> ") {
                    continue;
                } else if trimming_reply && line.trim().is_empty() {
                    trimming_reply = false;
                    continue;
                }

                let line = if is_emote {
                    format!("/me {}", line)
                } else {
                    line.to_string()
                };
                let line = Line::try_from(line).unwrap();

                send_message_tomsg(TomsgSendInfo {
                    db: info.db,
                    ch: &mut conn,
                    room: room,
                    sender: &user,
                    line: line,
                    reply_to: reply_to,
                    matrix_event_id: event_id.to_owned(),
                })
                .await;

                // we only want the first message to reply
                reply_to = None;
            }
        }};
    }

    macro_rules! handle_file_event {
        ($url:expr) => {{
            let (user, mut conn) = {
                let mut shed = TOMSG_CONN_SHED.lock().await;

                let user = get_room_user(info.state, &mut shed, info.db, &sender_id, &room_id)
                    .await
                    .unwrap();
                let conn = shed.ensure_connection(&user).await.unwrap();

                (user, conn)
            };

            let room = info
                .state
                .rooms
                .get_mut(room_mapping_id)
                .expect("room is not a management room, but is None");

            let url = match $url {
                None => {
                    println!("file has no url, ignoring...");
                    return;
                }
                Some(url) => url,
            };

            let url: http::Uri = url.parse().unwrap();

            let line = mxc_to_url(get_matrix_client().homeserver_url(), &url)
                .unwrap()
                .to_string();
            let line = Line::try_from(line).unwrap();

            send_message_tomsg(TomsgSendInfo {
                db: info.db,
                ch: &mut conn,
                room: room,
                sender: &user,
                line: line,
                reply_to: None,
                matrix_event_id: event_id.to_owned(),
            })
            .await;
        };};
    }

    match event {
        AnyMessageEvent::RoomMessage(m) => match m.content {
            MessageEventContent::Text(e) => {
                let items: Option<Vec<String>> = if is_management_room {
                    Some(e.body.split(' ').map(|v| v.to_string()).collect())
                } else if e.body.starts_with("!tomsg") {
                    Some(e.body.split(' ').skip(1).map(|v| v.to_string()).collect())
                } else {
                    None
                };

                match items {
                    Some(items) => {
                        let mut shed = TOMSG_CONN_SHED.lock().await;

                        handle_command(
                            info.state,
                            &mut shed,
                            &get_matrix_client(),
                            sender_id,
                            m.room_id,
                            items,
                        )
                        .await;
                    }
                    None => handle_text_event!(false, e.body, e.formatted, e.relates_to),
                }
            }
            MessageEventContent::Notice(e) => {
                handle_text_event!(false, e.body, e.formatted, e.relates_to)
            }
            MessageEventContent::Emote(e) => handle_text_event!(true, e.body, e.formatted, None),

            MessageEventContent::File(f) => handle_file_event!(f.url),
            MessageEventContent::Image(f) => handle_file_event!(f.url),
            MessageEventContent::Video(f) => handle_file_event!(f.url),
            MessageEventContent::Audio(f) => handle_file_event!(f.url),

            typ => println!("unknown matrix type {:?}", typ),
        },

        typ => println!("unknown matrix type {:?}", typ),
    }
}

async fn handle_state_event(info: Info<'_>, event: AnyStateEvent) {
    let room_id = event.room_id().to_owned();
    let sender_id = event.sender().to_owned();

    match event {
        AnyStateEvent::RoomMember(e) => {
            let state_key = UserId::try_from(e.state_key).unwrap();

            match e.content.membership {
                MembershipState::Invite => {
                    if state_key.localpart() != get_local_part() {
                        return;
                    }

                    get_matrix_client()
                        .puppet_join_room(&state_key, &room_id, None)
                        .await;
                    if e.content.is_direct.unwrap_or(false) {
                        info.state.set_management_room(sender_id, room_id);
                    }
                }

                MembershipState::Join => {
                    // state_key is the person that joined

                    if !info.state.rooms.has(MappingId::Matrix(&room_id)) {
                        return;
                    }

                    let mut shed = TOMSG_CONN_SHED.lock().await;

                    // TODO: make room join fail
                    get_room_user(info.state, &mut shed, info.db, &state_key, &room_id)
                        .await
                        .unwrap();
                }

                MembershipState::Leave => {
                    // state_key is the person that left

                    if state_key.localpart() == "tomsgbot" {
                        // TODO: disabled handling a leave of tomsgbot, have to figure this out
                        // when we got kicked by irc appservice.
                        // If this is enabled we also need to actually make the puppets leave the
                        // room too.
                        //info.state.remove_room(MappingId::Matrix(&room_id)).await;
                        eprintln!("disabled handling a leave of tomsgbot");
                    } else {
                        println!("tomsg doesn't support leaving a room, lol");
                    }
                }

                typ => println!("unknown membership type {:?}", typ),
            }
        }

        typ => println!("unknown matrix type {:?}", typ),
    }
}

async fn handler(
    txn_id: String,
    events: Vec<Raw<AnyEvent>>,
) -> core::result::Result<String, Infallible> {
    println!("handling transaction {}", txn_id);

    for event in events {
        println!("{}", serde_json::to_string(&event).unwrap());

        let event: AnyEvent = match event.deserialize() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("error while deserializing: {}", e);
                continue;
            }
        };

        let mut state = get_state().lock().await;
        let db = state.db.clone();
        let info = Info {
            state: &mut state,
            db: &db,
        };

        match event {
            AnyEvent::Message(m) => handle_message_event(info, m).await,
            AnyEvent::State(m) => handle_state_event(info, m).await,
            e => {
                eprintln!("todo: {:?}", e);
                continue;
            }
        };
    }

    Ok("{}".to_string())
}

pub async fn listen() {
    let ip: [u8; 4] = [0, 0, 0, 0];
    let port: u16 = 5010;
    let addr = SocketAddr::from((ip, port));

    serve(addr, handler).await.unwrap();
}
