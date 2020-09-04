use std::convert::TryFrom;
use std::time::Duration;
use std::time::SystemTime;

use crate::matrix::*;
use crate::state::State;
use crate::user::*;
use crate::*;

use ruma::events::room::message::{MessageEventContent, NoticeMessageEventContent};
use ruma::identifiers::{RoomId, UserId};

use tokio::select;
use tokio::time;

use tomsg_rs::command::Command;
use tomsg_rs::line::Line;
use tomsg_rs::reply::*;
use tomsg_rs::word::Word;

use matrix_appservice_rs::{Mappable, MappingId};

/// Remove any puppet that uses the given tomsg username
///
/// Returns whether or not there was a puppet to remove.
async fn take_over(state: &mut State, matrix: &MatrixClient, tomsg_username: Word) -> bool {
    let id = MappingId::External(tomsg_username);
    let user = match state.get_user(&id) {
        None => {
            eprintln!("user with tomsg username not found, everything should be fine");
            return false;
        }
        Some(u) => u,
    };
    if !user.0.is_puppet() {
        eprintln!("already a real user...");
        return false;
    }
    let user = user.to_owned();

    // remove puppet from db
    state.remove_user(&id).await;

    for room in state.rooms.iter_mut() {
        let user = match room.to_room_user(user.clone()) {
            Ok(u) => u,
            Err(_) => continue,
        };

        // leave room
        room.remove_user(&state.db, matrix, &user, true, false)
            .await;
    }

    true
}

pub async fn handle_command(
    state: &mut State,
    shed: &mut ConnectionShed,
    matrix: &MatrixClient,
    sender_id: UserId,
    room_id: RoomId,
    words: Vec<String>,
) {
    macro_rules! reply {
        ($text:expr) => {{
            let s = String::from($text);

            let room_id = state
                .get_room(&MappingId::Matrix(room_id.clone()))
                .map(|r| r.get_matrix().to_owned())
                .or_else(|| state.get_managment_room(&sender_id))
                .expect(&format!("room {} not found in state", room_id));

            matrix
                .create_message(
                    &room_id,
                    &get_appservice_sendable_user(),
                    MessageEventContent::Notice(NoticeMessageEventContent {
                        body: s,
                        formatted: None,
                        relates_to: None,
                    }),
                    SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as i64,
                )
                .await;
        }};
    }

    match words[0].as_str() {
        "login" => {
            let creds = if words.len() >= 3 {
                let username = Word::try_from(words[1].clone()).unwrap();
                let password = Line::try_from(words[2..].join(" ")).unwrap();
                Some(TomsgCredentials {
                    username,
                    password,
                    auto_generated: false,
                })
            } else {
                None
            };

            let id = MappingId::Matrix(sender_id.clone());

            let creds_differ = {
                let user = state.users.get(&id);
                match user {
                    None => true,
                    Some(user) => match &user.0 {
                        User::Real {
                            tomsg_credentials, ..
                        } => creds
                            .as_ref()
                            .map(|creds| tomsg_credentials != creds)
                            .unwrap_or(false),
                        User::Puppet { .. } => true,
                    },
                }
            };

            dbg!(creds_differ);

            if creds_differ {
                // If creds is Some(_), we could have the situation that the tomsg user was already
                // managed as a puppet by the bridge (the tomsg account already existed).
                // Kick out the puppet from all rooms and stop managing it, "forget" the puppet, making
                // room for the real user.
                if let Some(creds) = &creds {
                    take_over(state, matrix, creds.username.to_owned()).await;
                }

                let creds = match creds {
                    Some(creds) => Some(creds),
                    None => state.users.get(&id).cloned().and_then(|u| match u.0 {
                        User::Real {
                            tomsg_credentials, ..
                        } => Some(tomsg_credentials),
                        User::Puppet { .. } => None,
                    }),
                };

                // remove any user that is associated with the given id
                state.users.remove(&id);

                // create the real user in the database, register if creds is None.
                let user = state.ensure_real_user(sender_id.clone(), creds).await;
                // bind, this will also create and join rooms
                shed.bind(user).await.unwrap();
            } else {
                // since creds_differ is false, the user must exist and is not a puppet.
                let user = state.get_user(&id).unwrap().to_owned();
                shed.bind(user).await.unwrap();
            }

            reply!("you're now logged in on tomsg");
        }
        // TODO: logout
        "bridge" => {
            if let Some(user_id) = state.is_management_room(&room_id) {
                reply!(format!("current room is management room for {}", user_id));
                return;
            } else if state.rooms.has(&MappingId::Matrix(room_id.clone())) {
                reply!("given room is already managed");
                return;
            }

            let sender = state.ensure_real_user(sender_id.clone(), None).await;
            let mut tomsg_conn = shed.ensure_connection(&sender).await.unwrap();

            let tomsg_name = match words.get(1) {
                Some(n) => match Word::try_from(n.to_owned()) {
                    Ok(w) => w,
                    Err(_) => {
                        reply!(format!("invalid tomsg room name: {}", n));
                        return;
                    }
                },
                None => {
                    let res = tomsg_conn.send(Command::CreateRoom).await.unwrap();
                    match res {
                        Reply::Name(n) => n,
                        _ => panic!("expected name"),
                    }
                }
            };

            // TODO: if the tomsg room is already known, we need to do some stuff.

            // TODO: it is nice to check if the given tomsg_name is actually (if given by the user)
            // is actually joined by the current user.  Otherwise it's just a giant possible
            // privacy breach.
            //
            // If not fixed, one could bridge a room that they're not in.  If any actual room
            // member is managed by the bridge, the room will be bridged into the current matrix
            // room.  Without necessarily knowledge of the room members.
            //
            // This also leads to the idea of notifying the users in the room, in some way. (TODO)
            // (Maybe it's actually a good idea to have a tomsgbot user on the tomsg side?)

            let db = state.db.clone();
            let room = state.create_bridged_room(room_id.clone(), tomsg_name.clone());
            {
                let db = db.lock().unwrap();
                room.insert_user(&db, &sender);
            }

            matrix
                .invite_tomsg_members(state, tomsg_name.clone(), &room_id, &mut tomsg_conn)
                .await;

            reply!(format!(
                "{} is now bridged to the tomsg room {}",
                room_id, tomsg_name
            ));
        }

        "ping" => {
            let start = SystemTime::now();

            let tomsg_conn = shed.get_channel(&sender_id);
            let mut tomsg_conn = match tomsg_conn {
                None => {
                    reply!("not connected to tomsg");
                    return;
                }
                Some(c) => c,
            };

            // TODO
            //if tomsg_conn.is_connecting() {
            //    reply!("not connected to tomsg, currently connecting");
            //    return;
            //}

            let a = tomsg_conn.send(Command::Ping);
            let b = time::delay_for(Duration::from_secs(5));
            select! {
                left = a => match left {
                    Ok(_) => match start.elapsed() {
                        Ok(e) => reply!(format!(
                            "connection is good, roundtrip took {} milliseconds",
                            e.as_millis()
                        )),
                        Err(_) => reply!("connection is good"),
                    },
                    Err(e) => {
                        reply!(format!("connection is not good: {}", e));
                    },
                },

                _ = b => {
                    reply!("sending ping timed out");
                }
            }
        }

        "help" => {
            let s = r#"Welcome to the management room for the tomsg matrix bridge.
The following commands are avaiable:
- `login [<username> <password>]`: login with the given credentials, or generate some.
- `bridge [tomsg room name]`: bridge the current room to tomsg.  briding with the given tomsg room, or creating one if no arguments are provided.
- `ping`: checks if the connection is still up with the tomsg server and providedes the roundtrip duration between the bridge and the tomsg server.
- `help`: show this help message
"#;

            reply!(s);
        }

        cmd => {
            reply!(format!(
                "unknown command {}.  use the `help` command for a listing of possible commands",
                cmd
            ));
        }
    }
}
