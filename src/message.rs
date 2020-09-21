use crate::room::Room;

use ruma::identifiers::{EventId, RoomId};

use tomsg_rs::{Id, Word};

use matrix_appservice_rs::Mappable;

#[derive(Clone)]
pub struct Message {
    matrix_id: EventId,
    tomsg_id: Id,

    matrix_room_id: RoomId,
    tomsg_room_name: Word,
}

impl Message {
    pub fn new(matrix_id: EventId, tomsg_id: Id, room: &Room) -> Self {
        Self {
            matrix_id,
            tomsg_id,

            matrix_room_id: room.as_matrix().to_owned(),
            tomsg_room_name: room.as_external().to_owned(),
        }
    }
}

impl Mappable for Message {
    type MatrixType = EventId;
    type ExternalType = Id;

    fn as_matrix(&self) -> &EventId {
        &self.matrix_id
    }
    fn into_matrix(self) -> Self::MatrixType {
        self.matrix_id
    }

    fn as_external(&self) -> &Id {
        &self.tomsg_id
    }
    fn into_external(self) -> Self::ExternalType {
        self.tomsg_id
    }

    fn split(self) -> (Self::MatrixType, Self::ExternalType) {
        (self.matrix_id, self.tomsg_id)
    }
}
