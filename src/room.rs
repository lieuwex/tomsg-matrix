use crate::db::Database;
use crate::matrix::MatrixClient;
use crate::message::Message;
use crate::tomsg;
use crate::user::ManagedUser;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use ruma::identifiers::EventId;
use ruma::identifiers::RoomId;
use ruma::identifiers::UserId;

use tomsg_rs::{Command, Id, Word};

use shrinkwraprs::Shrinkwrap;

use matrix_appservice_rs::{Mappable, MappingDict, MappingId};

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum RoomType {
    Plumbed,
    Bridged,
}

#[derive(Shrinkwrap, Debug, Clone)]
pub struct RoomUser(#[shrinkwrap(main_field)] pub ManagedUser, RoomId);

impl RoomUser {
    pub fn check_room(&self, room_id: &RoomId) -> bool {
        &self.1 == room_id
    }
}

pub struct Room {
    pub room_type: RoomType,

    pub tomsg_name: Word,
    pub matrix_id: RoomId,

    pub handled_messages: MappingDict<Message>,

    pub participants: HashSet<(UserId, Word)>,
}

impl Room {
    pub fn new(room_type: RoomType, tomsg_name: Word, matrix_id: RoomId) -> Self {
        Self {
            room_type,
            tomsg_name,
            matrix_id,

            handled_messages: MappingDict::new(),

            participants: HashSet::new(),
        }
    }

    pub fn handled_message(&self, id: MappingId<Id, EventId>) -> bool {
        self.handled_messages.has(id)
    }

    pub fn get_handled_message(&self, id: MappingId<Id, EventId>) -> Option<&Message> {
        self.handled_messages.get(id)
    }

    pub fn handle_message(&mut self, db: &Database, message_id: &Id, matrix_id: EventId) -> bool {
        if self.handled_message(MappingId::External(message_id)) {
            return false;
        }

        db.insert_handled_message(message_id, &matrix_id, &self)
            .unwrap();

        let msg = Message::new(matrix_id, message_id.to_owned(), &self);
        self.handled_messages.insert(msg);

        true
    }

    /// Insert the given `ManagedUser` into this room.
    /// Callers should be sure the user is managed in both directions before calling this function.
    ///
    /// The return value indicates whether or not the user was inserted.
    pub fn insert_user(&mut self, db: &Database, user: &ManagedUser) -> bool {
        if !self.participants.insert(user.clone().split()) {
            return false;
        }

        db.insert_room_member(&self, user).unwrap();

        true
    }

    /// Makes sure that the given `username` is in the room.
    /// If they are not in the room, invite them using the `invited_conn` and insert it into the
    /// given `db`.
    ///
    /// The caller must make sure that the person is already in the room as a matrix user.
    ///
    /// The return value indicates whether or not the user was inserted.
    pub async fn ensure_tomsg_user_in_room(
        &mut self,
        db: &Arc<Mutex<Database>>,
        inviter_conn: &mut tomsg::Channel,
        user: &ManagedUser,
    ) -> bool {
        let inserted = {
            let db = db.lock().unwrap();
            self.insert_user(&db, user)
        };

        if inserted {
            inviter_conn
                .send(Command::Invite {
                    roomname: self.tomsg_name.clone(),
                    username: user.as_external().to_owned(),
                })
                .await
                .unwrap();
            true
        } else {
            false
        }
    }

    /// Returns whether or not the given user is doubly managed in the current room.
    /// This means that the user is both in the tomsg user list and the matrix user list.
    pub fn in_room(&self, user: &ManagedUser) -> bool {
        self.participants
            .iter()
            .any(|(user_id, name)| user_id == user.as_matrix() && name == user.as_external())
    }

    /// Upgrades the given `ManagedUser` into a `RoomUser` iff the user is in the current room, in that
    /// case this function returns `Ok(RoomUser)`.
    /// Iff the user is not in the the room this function returns `Err(ManagedUser)` (just passing
    /// back the given `user` parameter).
    ///
    /// This operation is cheap.
    pub fn to_room_user(&self, user: ManagedUser) -> Result<RoomUser, ManagedUser> {
        if self.in_room(&user) {
            Ok(RoomUser(user, self.as_matrix().clone()))
        } else {
            Err(user)
        }
    }

    pub async fn insert_and_invite(
        &mut self,
        db: &Arc<Mutex<Database>>,
        client: &MatrixClient,
        conn: &mut tomsg::Channel,
        user: &ManagedUser,
    ) -> bool {
        let inserted = {
            let db = db.lock().unwrap();
            self.insert_user(&db, user)
        };
        if !inserted {
            return false;
        }

        if user.0.is_puppet() {
            // REVIEW: is it correct that this is None?
            client
                .puppet_join_room(user.as_matrix(), &self.matrix_id, None)
                .await;
        }

        conn.send(Command::Invite {
            roomname: self.tomsg_name.clone(),
            username: user.as_external().clone(),
        })
        .await
        .unwrap();

        true
    }

    pub async fn remove_user(
        &mut self,
        db: &Arc<Mutex<Database>>,
        client: &MatrixClient,
        user: &RoomUser,
        leave_matrix: bool,
        leave_tomsg: bool,
    ) {
        if !user.check_room(&self.matrix_id) {
            eprintln!(
                "[remove_user] user {} not in room {}",
                user.as_matrix(),
                self.matrix_id
            );
            return;
        }

        let removed = {
            let db = db.lock().unwrap();

            let joined = self
                .participants
                .remove(&(user.as_matrix().to_owned(), user.as_external().to_owned()));

            if joined {
                db.remove_room_member(&self, &user.0).unwrap();
                true
            } else {
                false
            }
        };

        if !removed {
            return;
        }

        if leave_tomsg {
            eprintln!("leaving is not (yet) supported in tomsg, ignoring...");
        }

        if leave_matrix {
            client.leave_room(user, &self).await;
        }
    }
}

impl Mappable for Room {
    type MatrixType = RoomId;
    type ExternalType = Word;

    fn as_matrix(&self) -> &RoomId {
        &self.matrix_id
    }
    fn into_matrix(self) -> RoomId {
        self.matrix_id
    }

    fn as_external(&self) -> &Word {
        &self.tomsg_name
    }
    fn into_external(self) -> Word {
        self.tomsg_name
    }

    fn split(self) -> (Self::MatrixType, Self::ExternalType) {
        (self.matrix_id, self.tomsg_name)
    }
}
