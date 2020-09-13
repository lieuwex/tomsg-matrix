use std::collections::HashMap;
use std::future::Future;

use crate::db::*;
use crate::get_matrix_client;

use rusqlite::Result;

use matrix_appservice_rs::Mappable;

//type Closure = fn(&Database) -> dyn Future<Output = Result<String>>;

async fn one(db: &Database) -> Result<String> {
    use ruma::api::client::r0::profile::set_display_name;

    let client = get_matrix_client();
    for user in db.get_users()? {
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
}

async fn run(db: &Database, ver: u64) -> Result<String> {
    match ver {
        1 => one(db).await,
        _ => unreachable!(),
    }
}

pub async fn run_migrations(db: &Database) -> Result<()> {
    let meta = db.get_meta()?;
    let curr = meta.schema_version;

    let max = 1u64;

    for id in curr..max {
        let id = id + 1;
        eprintln!("running DB migration {} -> {}", id - 1, id);

        let res = match run(db, id).await {
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

    // fuck performance
    /*
    let migrations: HashMap<u64, Box<Closure>> = vec![(1, one)].into_iter().collect();

    for (id, f) in migrations {
        if id <= curr {
            continue;
        }

        eprintln!("running DB migration {} -> {}", id - 1, id);

        let res = match f(db).await {
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
    */

    Ok(())
}
