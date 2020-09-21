use crate::message::*;
use crate::room::*;
use crate::user::*;

use std::collections::HashMap;
use std::collections::HashSet;
use std::convert::TryFrom;
use std::iter::FromIterator;

use rusqlite::{params, Connection, Result};

use ruma::identifiers::{EventId, RoomId, UserId};

use tomsg_rs::{Id, Line, Word};

use matrix_appservice_rs::{Mappable, MappingDict};

pub struct Meta {
    pub schema_version: u64,
    pub txn_id: usize,
}

pub struct Database {
    pub conn: Connection,
}

impl Database {
    pub fn new(path: String) -> Result<Self> {
        let conn = Connection::open(path)?;
        Ok(Self { conn })
    }

    pub fn get_rooms(&self) -> Result<Vec<Room>> {
        let mut res = vec![];

        let mut stmt = self
            .conn
            .prepare("SELECT id, type, tomsg_name, matrix_id FROM rooms")
            .unwrap();

        let mut query = stmt.query(params![]).unwrap();
        while let Some(row) = query.next().unwrap() {
            let room_id: i64 = row.get(0).unwrap();

            let participants = {
                let mut stmt = self
                    .conn
                    .prepare(
                        "SELECT room_id, matrix_id, name FROM room_participants WHERE room_id = ?1",
                    )
                    .unwrap();

                let participants = stmt
                    .query_map(params![room_id], |row| {
                        let matrix_id: String = row.get(1).unwrap();
                        let matrix_id = UserId::try_from(matrix_id).unwrap();

                        let name: String = row.get(2).unwrap();
                        let name = Word::try_from(name).unwrap();

                        Ok((matrix_id, name))
                    })
                    .unwrap();

                HashSet::from_iter(participants.map(|v| v.unwrap()))
            };

            let tomsg_name: String = row.get(2).unwrap();
            let matrix_id: String = row.get(3).unwrap();

            let mut room = Room {
                room_type: match row.get(1).unwrap() {
                    0 => RoomType::Plumbed,
                    1 => RoomType::Bridged,
                    _ => panic!("unknown room type"),
                },

                tomsg_name: Word::try_from(tomsg_name).unwrap(),
                matrix_id: RoomId::try_from(matrix_id).unwrap(),

                handled_messages: MappingDict::new(),

                participants,

                fetched_tomsg_history: false,
            };

            let handled_messages = {
                let mut stmt = self
                    .conn
                    .prepare("SELECT tomsg_id, matrix_id FROM handled_messages WHERE room_id = ?1")
                    .unwrap();

                stmt.query_map(params![room_id], |row| {
                    let tomsg_id: i64 = row.get(0)?;
                    let tomsg_id: Id = Id::try_from(tomsg_id).unwrap();

                    let event_id: String = row.get(1)?;
                    let event_id = EventId::try_from(event_id).unwrap();

                    Ok(Message::new(event_id, tomsg_id, &room))
                })
                .unwrap()
                .map(|i| i.unwrap())
                .collect()
            };
            room.handled_messages = MappingDict::from_vec(handled_messages);

            res.push(room);
        }

        Ok(res)
    }

    pub fn get_users(&self) -> Result<Vec<ManagedUser>> {
        // tomsgname -> user
        let mut map: HashMap<Word, ManagedUser> = HashMap::new();

        let users = {
            let mut stmt = self
            .conn
            .prepare("SELECT matrix_id, tomsg_username, tomsg_password, tomsg_auto_generated FROM real_users")?;

            let users: Vec<ManagedUser> = stmt
                .query_map(params![], |row| {
                    let matrix_id: String = row.get(0)?;
                    let matrix_id = UserId::try_from(matrix_id).unwrap();

                    let tomsg_username: String = row.get(1)?;
                    let tomsg_username: Word = Word::try_from(tomsg_username).unwrap();

                    let tomsg_password: String = row.get(2)?;
                    let tomsg_password: Line = Line::try_from(tomsg_password).unwrap();

                    let tomsg_auto_generated: bool = row.get(3)?;

                    let u = User::Real {
                        tomsg_credentials: TomsgCredentials {
                            username: tomsg_username,
                            password: tomsg_password,
                            auto_generated: tomsg_auto_generated,
                        },
                        matrix_id,
                    };
                    Ok(ManagedUser(u))
                })?
                .collect::<Result<Vec<ManagedUser>>>()?;
            users
        };

        let puppets = {
            let mut stmt = self
                .conn
                .prepare("SELECT matrix_id, tomsg_username FROM puppet_users")?;

            let puppets: Vec<ManagedUser> = stmt
                .query_map(params![], |row| {
                    let matrix_id: String = row.get(0)?;
                    let matrix_id = UserId::try_from(matrix_id).unwrap();

                    let tomsg_username: String = row.get(1)?;
                    let tomsg_username: Word = Word::try_from(tomsg_username).unwrap();

                    let u = User::Puppet {
                        tomsg_name: tomsg_username,
                        matrix_id,
                    };
                    Ok(ManagedUser(u))
                })?
                .collect::<Result<Vec<ManagedUser>>>()?;
            puppets
        };

        for user in users {
            let tomsg = user.as_external();

            let auto_generated = match &user.0 {
                User::Puppet { .. } => unreachable!(),
                User::Real {
                    tomsg_credentials, ..
                } => tomsg_credentials.auto_generated,
            };

            if map.contains_key(tomsg) && auto_generated {
                continue;
            }

            map.insert(tomsg.to_owned(), user);
        }

        for user in puppets {
            let tomsg = user.as_external();
            map.entry(tomsg.to_owned()).or_insert(user);
        }

        Ok(map.into_iter().map(|(_, v)| v).collect())
    }

    pub fn insert_room(&self, room: &Room) -> Result<()> {
        self.conn.execute(
            "INSERT INTO rooms(type, tomsg_name, matrix_id) VALUES(?1, ?2, ?3)",
            params![
                match room.room_type {
                    RoomType::Plumbed => 0,
                    RoomType::Bridged => 1,
                },
                room.tomsg_name.as_str(),
                room.matrix_id.as_str()
            ],
        )?;
        Ok(())
    }
    pub fn insert_room_member(&self, room: &Room, user: &ManagedUser) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, tomsg_name FROM rooms WHERE tomsg_name = ?1")?;
        let room_id = stmt.query_row(params![room.as_external().as_str()], |row| {
            let id: i64 = row.get(0)?;
            Ok(id)
        })?;

        self.conn.execute(
            "INSERT INTO room_participants(room_id, matrix_id, name) VALUES(?1, ?2)",
            params![
                room_id,
                user.as_matrix().as_str(),
                user.as_external().as_str()
            ],
        )?;

        Ok(())
    }
    pub fn remove_room(&self, tomsg_name: &Word) -> Result<()> {
        self.conn.execute(
            "DELETE FROM rooms WHERE tomsg_name = ?1",
            params![tomsg_name.as_str()],
        )?;
        Ok(())
    }
    pub fn remove_room_member(&self, room: &Room, user: &ManagedUser) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, tomsg_name FROM rooms WHERE tomsg_name = ?1")?;
        let room_id = stmt.query_row(params![room.as_external().as_str()], |row| {
            let id: i64 = row.get(0)?;
            Ok(id)
        })?;

        self.conn.execute(
            "DELETE FROM room_participants WHERE room_id = ?1 AND name = ?2",
            params![room_id, user.as_matrix().as_str()],
        )?;
        Ok(())
    }

    pub fn insert_user(&self, user: &ManagedUser) -> Result<()> {
        match &user.0 {
            User::Real {
                tomsg_credentials: creds,
                matrix_id,
            } => {
                self.conn.execute(
                    "INSERT INTO real_users(matrix_id, tomsg_username, tomsg_password, tomsg_auto_generated) VALUES(?1, ?2, ?3, ?4)",
                    params![matrix_id.as_str(), creds.username.as_str(), creds.password.as_str(), creds.auto_generated]
                )?
            },
            User::Puppet { tomsg_name,matrix_id } => {
                self.conn.execute(
                    "INSERT INTO puppet_users(matrix_id, tomsg_username) VALUES(?1, ?2)",
                    params![matrix_id.as_str(), tomsg_name.as_str()]
                )?
            },
        };
        Ok(())
    }
    pub fn remove_user(&self, user: &User) -> Result<()> {
        match user {
            User::Real { matrix_id, .. } => self.conn.execute(
                "DELETE FROM real_users WHERE matrix_id = ?1",
                params![matrix_id.as_str()],
            )?,
            User::Puppet { tomsg_name, .. } => self.conn.execute(
                "DELETE FROM puppet_users WHERE tomsg_username = ?1",
                params![tomsg_name.as_str()],
            )?,
        };
        Ok(())
    }

    pub fn get_management_rooms(&self) -> Result<HashMap<UserId, RoomId>> {
        self.conn
            .prepare("SELECT user_id, room_id FROM management_rooms")?
            .query_map(params![], |row| {
                let user_id: String = row.get(0)?;
                let room_id: String = row.get(1)?;
                Ok((
                    UserId::try_from(user_id).unwrap(),
                    RoomId::try_from(room_id).unwrap(),
                ))
            })?
            .collect()
    }
    pub fn insert_management_room(&self, user_id: &UserId, room_id: &RoomId) -> Result<()> {
        self.conn.execute(
            "INSERT INTO management_rooms(user_id, room_id) VALUES(?, ?)",
            params![user_id.to_string(), room_id.to_string()],
        )?;
        Ok(())
    }

    pub fn insert_handled_message(
        &self,
        tomsg_id: &Id,
        matrix_id: &EventId,
        room: &Room,
    ) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, tomsg_name FROM rooms WHERE tomsg_name = ?1")?;
        let room_id = stmt.query_row(params![room.as_external().as_str()], |row| {
            let id: i64 = row.get(0)?;
            Ok(id)
        })?;

        self.conn.execute(
            "INSERT INTO handled_messages(tomsg_id, matrix_id, room_id) VALUES(?1, ?2, ?3)",
            params![tomsg_id.as_i64(), matrix_id.to_string(), room_id],
        )?;

        Ok(())
    }

    pub fn get_meta(&self) -> Result<Meta> {
        let mut stmt = self.conn.prepare("SELECT key, value FROM meta")?;

        // TODO: improve error handling
        let map: HashMap<String, String> = stmt
            .query_map(params![], |row| Ok((row.get(0)?, row.get(1)?)))?
            .map(|v| v.unwrap())
            .collect();

        Ok(Meta {
            schema_version: map.get("schema_version").unwrap().parse().unwrap(),
            txn_id: map.get("txn_id").unwrap().parse().unwrap(),
        })
    }
    pub fn set_txn_id(&self, txn_id: usize) -> Result<()> {
        self.conn.execute(
            "UPDATE meta SET value = ?1 WHERE key = 'txn_id'",
            params![txn_id.to_string()],
        )?;
        Ok(())
    }
}
