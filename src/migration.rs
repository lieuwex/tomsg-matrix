use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use crate::db::*;
use crate::get_matrix_client;

use rusqlite::{params, Result};

use matrix_appservice_rs::Mappable;

type Res = Pin<Box<dyn Future<Output = Result<String>>>>;
type Closure = fn(Arc<Mutex<Database>>) -> Res;

fn one(db: Arc<Mutex<Database>>) -> Res {
    use ruma::api::client::r0::profile::set_display_name;

    Box::pin(async move {
        let client = get_matrix_client();
        for user in db.lock().unwrap().get_users()? {
            if !user.is_puppet() {
                continue;
            }

            let mxid = user.as_matrix();
            let tomsg_name = user.as_external();

            client
                .request(
                    set_display_name::Request::new(mxid, Some(&format!("{} (tomsg)", tomsg_name))),
                    Some(mxid),
                    None,
                )
                .await
                .unwrap();
        }

        Ok(String::new())
    })
}

fn two(db: Arc<Mutex<Database>>) -> Res {
    Box::pin(async move {
        let mut db = db.lock().unwrap();

        let txn = db.conn.transaction()?;

        txn.execute("CREATE TABLE room_participants ( id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT, room_id INTEGER NOT NULL, name TEXT NOT NULL, matrix_id TEXT NOT NULL, FOREIGN KEY (room_id) REFERENCES rooms(id) ON DELETE CASCADE);", params![])?;
        txn.execute("INSERT INTO room_participants(room_id, name, matrix_id) SELECT tomsg_room_join.room_id,tomsg_username,matrix_id FROM puppet_users INNER JOIN tomsg_room_join on tomsg_room_join.name = puppet_users.tomsg_username;", params![])?;
        txn.execute("INSERT INTO room_participants(room_id, name, matrix_id) SELECT tomsg_room_join.room_id,tomsg_username,matrix_id FROM real_users INNER JOIN tomsg_room_join on tomsg_room_join.name = real_users.tomsg_username;", params![])?;
        txn.execute("DROP TABLE tomsg_room_join", params![])?;
        txn.execute("DROP TABLE matrix_room_join", params![])?;

        txn.commit()?;

        Ok(String::new())
    })
}

/*
/// Convert the `handled_messages` to hold `nth` (paired with matrix_id), to support multiline
/// Matrix messages.
fn two(db: Arc<Mutex<Database>>) -> Res {
    Box::pin(async move {
        db.lock().unwrap().get_raw().execute(
            "ALTER TABLE handled_messages ADD nth INTEGER NOT NULL DEFAULT 0",
            params![],
        )?;

        Ok(String::new())
    })
}
*/

pub async fn run_migrations(db: Arc<Mutex<Database>>) -> Result<()> {
    let meta = db.lock().unwrap().get_meta()?;
    let curr = meta.schema_version;

    let migrations: HashMap<u64, Closure> = vec![(1u64, one as Closure), (2u64, two as Closure)]
        .into_iter()
        .collect();

    for (id, f) in migrations {
        if id <= curr {
            continue;
        }

        eprintln!("running DB migration {} -> {}", id - 1, id);

        let res = match f(db.clone()).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "error while running transaction {} -> {}: {}",
                    id - 1,
                    id,
                    e
                );
                return Err(e);
            }
        };
        if res.is_empty() {
            eprintln!("succesful");
        } else {
            eprintln!("succesful: {}", res);
        }
    }

    Ok(())
}
