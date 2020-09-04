use std::collections::HashMap;
use std::convert::TryFrom;
use std::sync::{Arc, Mutex};

use crate::db::*;
use crate::matrix::*;
use crate::room::*;
use crate::tomsg::{connect, register, RegisterError};
use crate::user::*;
use crate::util::random_alphanumeric;

use ruma::identifiers::RoomId;
use ruma::identifiers::UserId;

use tomsg_rs::line::Line;
use tomsg_rs::word::Word;

use matrix_appservice_rs::{Mappable, MappingDict, MappingId};

/// Manages state in the database and memory
pub struct State {
    pub db: Arc<Mutex<Database>>,

    pub rooms: MappingDict<Room>,
    pub users: MappingDict<ManagedUser>,

    pub management_rooms: HashMap<UserId, RoomId>,
}

impl State {
    pub async fn from_db(db: Arc<Mutex<Database>>) -> Self {
        let mut rooms_map = MappingDict::new();
        let mut users_map = MappingDict::new();

        let (rooms, users, management_rooms) = {
            let db = db.lock().unwrap();

            let rooms = db.get_rooms().unwrap();
            let users = db.get_users().unwrap();
            let management_rooms = db.get_management_rooms().unwrap();

            (rooms, users, management_rooms)
        };

        for room in rooms {
            rooms_map.insert(room);
        }

        for user in users {
            users_map.insert(user);
        }

        State {
            db,

            rooms: rooms_map,
            users: users_map,

            management_rooms,
        }
    }

    /// Create a new plumbed room, this will actually create a new Matrix room.
    /// This function inserts the newly created room into the current state.
    /// Returns a reference to the newly created `Room` object.
    pub async fn create_plumbed_room(
        &mut self,
        client: &MatrixClient,
        tomsg_name: Word,
    ) -> &mut Room {
        let alias = format!("tomsg_{}", tomsg_name);
        let friendly_name = format!("{} (tomsg)", tomsg_name);

        let room_id = client.create_room(&alias, &friendly_name).await;

        let room = Room::new(RoomType::Plumbed, tomsg_name, room_id);
        self.add_room(room).unwrap()
    }
    /// Create a new bridged room, this will _not_ create a Matrix room, but will just return
    /// reference to the new `Room` object.
    /// This function inserts the newly created room into the current state.
    pub fn create_bridged_room(&mut self, room_id: RoomId, tomsg_name: Word) -> &mut Room {
        let room = Room::new(RoomType::Bridged, tomsg_name, room_id);
        self.add_room(room).unwrap()
    }

    fn add_room(&mut self, room: Room) -> Option<&mut Room> {
        if self
            .rooms
            .has(&MappingId::External(room.get_external().to_owned()))
        {
            return None;
        }

        self.db.lock().unwrap().insert_room(&room).unwrap();
        Some(self.rooms.insert(room))
    }
    pub async fn remove_room(&mut self, id: &MappingId<Word, RoomId>) -> bool {
        if let Some(room) = self.rooms.remove(id) {
            self.db
                .lock()
                .unwrap()
                .remove_room(room.get_external())
                .unwrap();
            true
        } else {
            false
        }
    }

    pub fn get_room(&self, id: &MappingId<Word, RoomId>) -> Option<&Room> {
        self.rooms.get(id)
    }
    pub fn get_room_mut(&mut self, id: &MappingId<Word, RoomId>) -> Option<&mut Room> {
        self.rooms.get_mut(id)
    }

    pub fn get_user(&self, id: &MappingId<Word, UserId>) -> Option<&ManagedUser> {
        self.users.get(id)
    }
    pub fn get_user_mut(&mut self, id: &MappingId<Word, UserId>) -> Option<&mut ManagedUser> {
        self.users.get_mut(id)
    }
    pub async fn remove_user(&mut self, id: &MappingId<Word, UserId>) {
        if let Some(user) = self.users.remove(id) {
            let db = self.db.lock().unwrap();
            db.remove_user(&user).unwrap();
        }
    }

    /// Returns the puppet for the given tomsg name, creating one if necessary.
    /// Returns `None` if the given tomsg username is bound to a real user.
    pub async fn ensure_puppet(
        &mut self,
        client: &MatrixClient,
        tomsg_name: Word,
    ) -> Option<ManagedUser> {
        if let Some(user) = self.get_user(&MappingId::External(tomsg_name.clone())) {
            return match &user.0 {
                User::Real { .. } => None,
                User::Puppet { .. } => Some(user.clone()),
            };
        }

        let user = ManagedUser(client.create_puppet(tomsg_name).await);
        self.db.lock().unwrap().insert_user(&user).unwrap();
        self.users.insert(user.clone());
        Some(user)
    }

    /// Mark the given Matrix room id as being the management room for the Matrix user with the
    /// given Matrix user id.
    pub fn set_management_room(&mut self, user_id: UserId, room_id: RoomId) {
        self.db
            .lock()
            .unwrap()
            .insert_management_room(&user_id, &room_id)
            .unwrap();

        self.management_rooms.insert(user_id, room_id);
    }
    /// Returns whether or not the given Matrix room id is a management room.
    pub fn is_management_room(&self, room_id: &RoomId) -> Option<UserId> {
        self.management_rooms
            .iter()
            .find(|(_, v)| *v == room_id)
            .map(|p| p.0.to_owned())
    }
    /// Gets the management room for the given Matrix user id.
    pub fn get_managment_room(&self, user_id: &UserId) -> Option<RoomId> {
        match self.management_rooms.get(user_id) {
            Some(id) => Some(id.to_owned()),
            None => None,
        }
    }

    /// Returns the real user for the given Matrix user id.
    /// Optionally you can give credentials to the tomsg server, which will be stored in the
    /// database.
    ///
    /// If no such user exists in the database, one will be made with the given credentials, or
    /// auto-generated ones if none are provided.  The auto-generated credentials will use an
    /// username based on the matrix user's username, combined with a fully random password.
    /// The auto-generated credentials will also be registered immediately on the tomsg server.
    ///
    /// This function panics if the given Matrix user id is a puppet.
    pub async fn ensure_real_user(
        &mut self,
        user_id: UserId,
        creds: Option<TomsgCredentials>,
    ) -> ManagedUser {
        if let Some(user) = self.get_user(&MappingId::Matrix(user_id.clone())) {
            if user.0.is_puppet() {
                panic!("user exists, but is not a real user");
            }

            return user.clone();
        }

        let creds = match creds {
            Some(c) => c,
            None => {
                let mut i = 0usize;

                loop {
                    let username = if i == 0 {
                        format!("{}[m]", user_id.localpart())
                    } else {
                        format!("{}[m]{}", user_id.localpart(), i)
                    };
                    let username = Word::try_from(username).unwrap();

                    let password: String = random_alphanumeric(15);
                    let password = Line::try_from(password).unwrap();

                    let conn = connect().await.unwrap();
                    match register(&conn.0, username, password, true).await {
                        Ok(c) => break c,
                        Err(RegisterError::UsernameTaken) => {
                            i += 1;
                            continue;
                        }
                        Err(RegisterError::IO(e)) => panic!(e),
                    };
                }
            }
        };

        let user = ManagedUser(User::Real {
            tomsg_credentials: creds,
            matrix_id: user_id,
        });
        self.db.lock().unwrap().insert_user(&user).unwrap();
        self.users.insert(user.clone());
        user
    }
}
