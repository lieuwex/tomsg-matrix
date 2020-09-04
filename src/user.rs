use crate::matrix::*;
use crate::room::RoomUser;

use ruma::api::OutgoingRequest;
use ruma::identifiers::{RoomId, UserId};
use ruma_client::Error;

use shrinkwraprs::Shrinkwrap;

use tomsg_rs::line::Line;
use tomsg_rs::word::Word;

use matrix_appservice_rs::Mappable;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TomsgCredentials {
    pub username: Word,
    pub password: Line,
    pub auto_generated: bool,
}

#[derive(Debug, Clone)]
pub enum User {
    Real {
        tomsg_credentials: TomsgCredentials,
        matrix_id: UserId,
    },
    Puppet {
        tomsg_name: Word,
        matrix_id: UserId,
    },
}

impl User {
    pub async fn request<R: OutgoingRequest + std::fmt::Debug>(
        &self,
        client: &MatrixClient,
        request: R,
    ) -> Result<R::IncomingResponse, Error<R::EndpointError>> {
        let user_id = match self {
            User::Real { .. } => panic!(),
            User::Puppet { matrix_id, .. } => matrix_id,
        };

        // REVIEW: None
        client.request(request, Some(user_id), None).await
    }

    pub fn is_puppet(&self) -> bool {
        match self {
            User::Real { .. } => false,
            User::Puppet { .. } => true,
        }
    }
}

impl Mappable for User {
    type MatrixType = UserId;
    type ExternalType = Word;

    fn get_matrix(&self) -> &UserId {
        match self {
            User::Real { matrix_id, .. } => matrix_id,
            User::Puppet { matrix_id, .. } => matrix_id,
        }
    }
    fn into_matrix(self) -> Self::MatrixType {
        match self {
            User::Real { matrix_id, .. } => matrix_id,
            User::Puppet { matrix_id, .. } => matrix_id,
        }
    }

    fn get_external(&self) -> &Word {
        match self {
            User::Real {
                tomsg_credentials, ..
            } => &tomsg_credentials.username,
            User::Puppet { tomsg_name, .. } => tomsg_name,
        }
    }
    fn into_external(self) -> Self::ExternalType {
        match self {
            User::Real {
                tomsg_credentials, ..
            } => tomsg_credentials.username,
            User::Puppet { tomsg_name, .. } => tomsg_name,
        }
    }
}

#[derive(Shrinkwrap, Debug, Clone)]
pub struct ManagedUser(pub User);

impl Mappable for ManagedUser {
    type MatrixType = UserId;
    type ExternalType = Word;

    fn get_matrix(&self) -> &UserId {
        self.0.get_matrix()
    }
    fn into_matrix(self) -> Self::MatrixType {
        self.0.into_matrix()
    }

    fn get_external(&self) -> &Word {
        self.0.get_external()
    }
    fn into_external(self) -> Self::ExternalType {
        self.0.into_external()
    }
}

#[derive(Debug, Clone)]
pub enum SendableUser {
    RoomUser(RoomUser),
    AppService(UserId),
}

impl SendableUser {
    pub fn get_matrix(&self) -> &UserId {
        match self {
            SendableUser::RoomUser(u) => u.get_matrix(),
            SendableUser::AppService(u) => u,
        }
    }
    pub fn into_matrix(self) -> UserId {
        match self {
            SendableUser::RoomUser(u) => u.0.into_matrix(),
            SendableUser::AppService(u) => u,
        }
    }

    pub fn can_send_to(&self, room_id: &RoomId) -> bool {
        match self {
            SendableUser::RoomUser(user) => user.check_room(room_id),
            SendableUser::AppService(_) => true,
        }
    }
}

impl From<RoomUser> for SendableUser {
    fn from(u: RoomUser) -> Self {
        SendableUser::RoomUser(u)
    }
}
