CREATE TABLE rooms (
	id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
	type INTEGER NOT NULL,
	tomsg_name TEXT NOT NULL,
	matrix_id TEXT NOT NULL
);
CREATE TABLE tomsg_room_join (
	id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
	room_id INTEGER NOT NULL,
	name TEXT NOT NULL,

	FOREIGN KEY (room_id) REFERENCES rooms(id) ON DELETE CASCADE
);
CREATE TABLE matrix_room_join (
	id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
	room_id INTEGER NOT NULL,
	matrix_id TEXT NOT NULL,

	FOREIGN KEY (room_id) REFERENCES rooms(id) ON DELETE CASCADE
);

CREATE TABLE handled_messages (
	tomsg_id INTEGER NOT NULL PRIMARY KEY,
	matrix_id TEXT NOT NULL,
	room_id INTEGER NOT NULL,

	FOREIGN KEY (room_id) REFERENCES rooms(id) ON DELETE CASCADE
);

CREATE TABLE real_users (
	id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
	matrix_id TEXT NOT NULL,
	tomsg_username TEXT NOT NULL,
	tomsg_password TEXT NOT NULL,
	tomsg_auto_generated BOOLEAN NOT NULL
);

CREATE TABLE puppet_users (
	id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
	matrix_id TEXT NOT NULL,
	tomsg_username TEXT NOT NULL
);

CREATE TABLE management_rooms (
	id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
	user_id TEXT NOT NULL,
	room_id TEXT NOT NULL
);

CREATE TABLE meta (
	key TEXT NOT NULL PRIMARY KEY,
	value TEXT
);
INSERT INTO meta(key, value) values("schema_version", "1");
INSERT INTO meta(key, value) values("txn_id", "0");
