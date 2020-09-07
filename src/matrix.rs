use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::db::Database;
use crate::get_appservice_sendable_user;
use crate::room::*;
use crate::state::*;
use crate::tomsg;
use crate::user::*;

use matrix_appservice_rs::RequestBuilder;
use matrix_appservice_rs::{Mappable, MappingId};

use ruma::api::client::r0::account::register;
use ruma::api::client::r0::membership::invite_user;
use ruma::api::client::r0::membership::join_room_by_id;
use ruma::api::client::r0::membership::leave_room;
use ruma::api::client::r0::message::send_message_event;
use ruma::api::client::r0::profile::set_display_name;
use ruma::api::client::r0::room::{create_room, get_room_event};
use ruma::events::room::message::MessageEventContent;
use ruma::events::{AnyMessageEventContent, AnyRoomEvent};
use ruma::identifiers::{EventId, RoomId, ServerName, UserId};
use ruma::Raw;
use ruma_client::Error;
use ruma_client::{Client, HttpsClient, Session};

use tomsg_rs::command::Command;
use tomsg_rs::reply::*;
use tomsg_rs::word::Word;

pub struct MatrixClient {
    server_name: Box<ServerName>,
    homeserver_url: http::Uri,
    access_token: String,

    db: Arc<Mutex<Database>>,

    c: HttpsClient,
    txnid: AtomicUsize,
}

impl MatrixClient {
    pub async fn new(
        server_name: Box<ServerName>,
        homeserver_url: http::Uri,
        access_token: String,
        db: Arc<Mutex<Database>>,
    ) -> Self {
        let session = Session {
            access_token: access_token.to_string(),
            identification: None,
        };

        let client = match homeserver_url.scheme_str().unwrap() {
            "http" => todo!(),
            "https" => Client::https(homeserver_url.clone(), Some(session)),
            scheme => panic!("unknown scheme {}", scheme),
        };

        let txn_id = {
            let db = db.lock().unwrap();
            db.get_meta().unwrap().txn_id
        };

        Self {
            server_name,
            homeserver_url,
            access_token,

            db,

            c: client,
            txnid: AtomicUsize::new(txn_id),
        }
    }

    pub fn server_name(&self) -> &ServerName {
        &self.server_name
    }
    pub fn homserver_url(&self) -> &http::Uri {
        &self.homeserver_url
    }
    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    /// make a request, as the given user_name.
    pub async fn request<R: ruma::api::OutgoingRequest + std::fmt::Debug>(
        &self,
        request: R,
        user_id: Option<&UserId>,
        ts: Option<i64>, // milliseconds
    ) -> Result<R::IncomingResponse, Error<R::EndpointError>> {
        println!("[matrix] request: {:?}", request);

        let mut builder = RequestBuilder::new(&self.c, request);
        if let Some(user_id) = user_id {
            builder.user_id(user_id);
        }
        if let Some(ts) = ts {
            builder.timestamp(ts);
        }
        builder.request().await
    }

    pub async fn get_txin(&self) -> usize {
        let id = self.txnid.fetch_add(1, Ordering::Relaxed);

        {
            let db = self.db.lock().unwrap();
            db.set_txn_id(id + 1).unwrap();
        }

        id
    }

    pub async fn create_puppet(&self, tomsg_name: Word) -> User {
        let local_part = format!("tomsg_{}", tomsg_name);

        let mut request = register::Request::new();
        request.username = Some(&local_part);
        request.inhibit_login = true;

        println!("creating puppet for {}", tomsg_name);
        let res = self
            .c
            .request_with_url_params(request, {
                let mut params = BTreeMap::new();
                params.insert("access_token".to_string(), self.access_token.to_string());
                Some(params)
            })
            .await
            .unwrap();
        let matrix_id = res.user_id;

        self.request(
            set_display_name::Request::new(&matrix_id, Some(&format!("{} (tomsg)", tomsg_name))),
            Some(&matrix_id),
            None,
        )
        .await
        .unwrap();

        println!("made puppet for {}", tomsg_name);
        User::Puppet {
            tomsg_name,
            matrix_id,
        }
    }

    pub async fn invite_matrix_user(
        &self,
        room_id: &RoomId,
        inviter: &SendableUser,
        invited: &UserId,
    ) -> invite_user::Response {
        let good = match inviter {
            SendableUser::RoomUser(user) => user.check_room(room_id),
            SendableUser::AppService(_) => true,
        };
        if !good {
            panic!("inviter RoomUser has different room_id");
        }

        println!("inviting {} as {}", invited, inviter.get_matrix());
        self.request(
            invite_user::Request::new(
                room_id,
                invite_user::InvitationRecipient::UserId { user_id: invited },
            ),
            Some(&inviter.get_matrix()),
            None,
        )
        .await
        .unwrap()
    }

    pub async fn invite_tomsg_members(
        &self,
        state: &mut State,
        room_tomsg_name: Word,
        room_matrix_id: &RoomId,
        tomsg_conn: &mut tomsg::Channel,
    ) -> Vec<UserId> {
        let res = tomsg_conn
            .send(Command::ListMembers(room_tomsg_name.clone()))
            .await
            .unwrap();
        let members = match res {
            Reply::List(members) => members,
            _ => panic!(),
        };

        let mut res = vec![];
        for member in members {
            if let Some(u) = state.get_user(MappingId::External(&member)) {
                if !u.is_puppet() {
                    continue;
                }

                let room = state
                    .get_room(MappingId::External(&room_tomsg_name))
                    .unwrap();
                if room.matrix_invited_or_joined.contains(&u.get_matrix()) {
                    continue;
                }
            }

            let user = state.ensure_puppet(&self, &member).await.unwrap();

            self.puppet_join_room(
                &user.get_matrix(),
                room_matrix_id,
                Some(&get_appservice_sendable_user()),
            )
            .await;

            res.push(user.clone());
        }

        {
            let db = self.db.lock().unwrap();
            let room = state
                .get_room_mut(MappingId::External(&room_tomsg_name))
                .unwrap();
            for user in &res {
                room.insert_user(&db, user);
            }
        }
        res.into_iter().map(|v| v.into_matrix()).collect()
    }

    /// Join the room as the puppet with the given `invited` user id, in the room with the given
    /// `room_id`.
    /// If `inviter` is not `None`, the puppet will be invited by the `SendableUser` stored in the
    /// `Option`.
    pub async fn puppet_join_room(
        &self,
        invited: &UserId,
        room_id: &RoomId,
        inviter: Option<&SendableUser>,
    ) {
        if let Some(inviter) = inviter {
            self.invite_matrix_user(room_id, inviter, invited).await;
        }

        self.request(join_room_by_id::Request::new(room_id), Some(invited), None)
            .await
            .unwrap();
    }

    /// Create a new message with the given `data`, sending it as `sender` in the room with the
    /// given `room_id`.
    /// It is checked that the given `sender` is able to send in the room, this happens by calling
    /// `SendableUser::can_send_to` with the given `room_id`.
    pub async fn create_message(
        &self,
        room_id: &RoomId,
        sender: &SendableUser,
        data: MessageEventContent,
        ts: i64,
    ) -> send_message_event::Response {
        if !sender.can_send_to(&room_id) {
            panic!("RoomUser has different room_id");
        }

        let message = AnyMessageEventContent::RoomMessage(data);

        let txn_id = self.get_txin().await.to_string();
        let request = send_message_event::Request::new(room_id, &txn_id, &message);

        self.request(request, Some(&sender.get_matrix()), Some(ts))
            .await
            .unwrap()
    }

    /// Create a new Matrix room with the given `alias` and the given `friendly_name`.
    /// The alias should not include the server_name or the leading #, for example a correct
    /// `alias` would be 'coffee'.
    /// Returns the `RoomId` of the newly created room.
    pub async fn create_room(&self, alias: &str, friendly_name: &str) -> RoomId {
        let mut request = create_room::Request::new();
        request.name = Some(friendly_name);
        request.room_alias_name = Some(alias);

        self.request(request, None, None).await.unwrap().room_id
    }

    pub async fn leave_room(&self, user: &RoomUser, room: &Room) {
        self.request(
            leave_room::Request::new(room.get_matrix()),
            Some(&user.get_matrix()),
            None,
        )
        .await
        .unwrap();
    }

    pub async fn get_room_event(
        &self,
        room_id: RoomId,
        event_id: EventId,
    ) -> Option<Raw<AnyRoomEvent>> {
        self.request(get_room_event::Request { room_id, event_id }, None, None)
            .await
            .ok()
            .map(|res| res.event)
    }
}

pub enum MatrixToItem<'a> {
    Event(&'a RoomId, &'a EventId),
    User(&'a UserId),
}
pub fn matrix_to_url(item: MatrixToItem<'_>) -> String {
    let slug = match item {
        MatrixToItem::Event(room_id, event_id) => format!("{}/{}", room_id, event_id),
        MatrixToItem::User(user_id) => format!("{}", user_id),
    };

    format!("https://matrix.to/#/{}", slug)
}

pub fn mxc_to_url(client: &MatrixClient, url: &http::Uri) -> String {
    assert!(url.scheme_str().unwrap() == "mxc");

    let server_name = url.host().unwrap();
    let id = &url.path()[1..];

    format!(
        "{}_matrix/media/r0/download/{}/{}",
        client.homserver_url(),
        server_name,
        id
    )
}
