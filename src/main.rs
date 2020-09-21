mod command;
mod db;
mod handler;
mod matrix;
mod message;
mod migration;
mod room;
mod state;
mod tomsg;
mod user;
mod util;

use std::convert::TryInto;
use std::fs::File;
use std::io::{ErrorKind, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use tokio::net;
use tokio::sync::Mutex;
use tokio::task;

use futures::future;

use ruma::identifiers::UserId;

use once_cell::sync::{Lazy, OnceCell};

use self::db::*;
use self::handler::listen;
use self::matrix::*;
use self::migration::run_migrations;
use self::state::State;
use self::tomsg::ConnectionShed;
use self::user::*;

use matrix_appservice_rs::{ApplicationService, Namespaces, Registration};

static REGISTRATION: OnceCell<Registration> = OnceCell::new();
static APPSERVICE: OnceCell<ApplicationService> = OnceCell::new();

static MATRIX_CLIENT: OnceCell<MatrixClient> = OnceCell::new();

static STATE: OnceCell<Arc<Mutex<State>>> = OnceCell::new();

static TOMSG_CONN_SHED: Lazy<Arc<Mutex<ConnectionShed>>> =
    Lazy::new(|| Arc::new(Mutex::new(ConnectionShed::new())));

static TOMSG_IP: OnceCell<SocketAddr> = OnceCell::new();

static APPSERVICE_SENDABLE_USER: OnceCell<SendableUser> = OnceCell::new();

#[inline(always)]
pub fn get_local_part() -> &'static str {
    &REGISTRATION.get().unwrap().sender_localpart
}

/// Get a SendableUser that points to the appservice management user.
/// One should note that the appservice should be in every room managed by this bridge, so this
/// should be valid for every room that we know of.
#[inline(always)]
pub fn get_appservice_sendable_user() -> &'static SendableUser {
    APPSERVICE_SENDABLE_USER.get().unwrap()
}

#[inline(always)]
pub fn get_matrix_client() -> &'static MatrixClient {
    MATRIX_CLIENT.get().unwrap()
}

#[inline(always)]
pub fn get_state() -> &'static Arc<Mutex<State>> {
    STATE.get().unwrap()
}

#[tokio::main]
async fn main() {
    let service = {
        let config_path = Path::new("./config.yaml");

        let file = match File::open(config_path) {
            Ok(f) => Some(f),
            Err(e) => match e.kind() {
                ErrorKind::NotFound => None,
                e => panic!(e),
            },
        };

        match file {
            None => {
                let service = ApplicationService::new(
                    "lieuwe.xyz".try_into().unwrap(),
                    "https://matrix.lieuwe.xyz".parse().unwrap(),
                );

                {
                    let mut file = File::create(config_path).unwrap();
                    let yaml = serde_yaml::to_string(&service).unwrap();
                    file.write_all(yaml.as_bytes()).unwrap();
                }

                eprintln!("created config at {}", config_path.to_str().unwrap());
                service
            }
            Some(f) => {
                let value: serde_yaml::Value = serde_yaml::from_reader(f).unwrap();
                ApplicationService::from_yaml(value).unwrap()
            }
        }
    };

    let registration = {
        let registration_path = Path::new("./registration.yaml");

        let file = match File::open(registration_path) {
            Ok(f) => Some(f),
            Err(e) => match e.kind() {
                ErrorKind::NotFound => None,
                e => panic!(e),
            },
        };

        match file {
            None => {
                let registration = Registration::new(
                    "tomsg".to_string(),
                    Namespaces {
                        users: vec![],
                        aliases: vec![],
                    },
                    "tomsgbot".to_string(),
                    "http://todo.todo/".parse().unwrap(),
                    false,
                );

                {
                    let mut f = File::create(registration_path).unwrap();
                    let yaml = serde_yaml::to_string(&registration).unwrap();
                    f.write_all(yaml.as_bytes()).unwrap();
                }

                eprintln!(
                    "created registration file at {}",
                    registration_path.to_str().unwrap()
                );

                return;
            }
            Some(f) => {
                let value: serde_yaml::Value = serde_yaml::from_reader(f).unwrap();
                Registration::from_yaml(value).unwrap()
            }
        }
    };

    macro_rules! okky {
        ($expr:expr) => {
            match $expr {
                Ok(_) => {}
                Err(_) => panic!("oncecell contained stuff"),
            }
        };
    }

    let db = Arc::new(std::sync::Mutex::new(
        Database::new("./db.db".to_string()).unwrap(),
    ));

    okky!(MATRIX_CLIENT.set(
        MatrixClient::new(
            service.server_name().try_into().unwrap(),
            service.server_url().clone(),
            registration.as_token.clone(),
            db.clone(),
        )
        .await,
    ));

    run_migrations(db.clone()).await.unwrap();

    okky!(STATE.set(Arc::new(Mutex::new(State::from_db(db).await))));

    okky!(APPSERVICE_SENDABLE_USER.set({
        let sender = UserId::parse_with_server_name(
            registration.sender_localpart.clone(),
            service.server_name(),
        )
        .unwrap();

        SendableUser::AppService(sender)
    }));

    okky!(REGISTRATION.set(registration));
    okky!(APPSERVICE.set(service));

    let ip = net::lookup_host("127.0.0.1:5030")
        //let ip = net::lookup_host("127.0.0.1:29536")
        .await
        .unwrap()
        .next()
        .unwrap();
    okky!(TOMSG_IP.set(ip));

    {
        let state = get_state().lock().await;

        let futs: Vec<_> = state
            .users
            .iter()
            .filter(|u| !u.is_puppet())
            .map(|u| {
                let user = u.clone();
                task::spawn(async move {
                    TOMSG_CONN_SHED.lock().await.bind(user).await.unwrap();
                })
            })
            .collect();

        future::join_all(futs).await;
    }

    listen().await;
}
