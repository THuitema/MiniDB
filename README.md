# mini-db

A Rust project for a small database as I study Designing Data-Intensive Applications.

Each database engine will be implemented in `/src/storage`, each supporting the common operations `get`, `put` and `delete`.

Run the executable with:

```sh
cargo run
```

## Version 1: In-memory HashMap
File: `memory.rs`

Just a HashMap, nothing special going on here. Requires all data to fit within memory, and does not persist through crashes.

## Version 2: Append-only Log
File: `append_log.rs`

This database writes key-value entries to disk. As the name suggests, entries are not overwritten. Instead, we append new records to the end of the log to reflect updates. Meanwhile, we maintain an in-memory hash index that maps a key to the byte offset of its associated record on disk.

Record formats:
- Put: `<crc32: u32><op: u8 = 0><key_len: u32><value_len: u32><key bytes><value bytes>`
- Delete: `<crc32: u32><op: u8 = 1><key_len: u32><value_len: u32 = 0><key bytes>`

This storage strategy allows for crash recovery. If our process terminates, `open()` sequentially scans the log file on disk to rebuild the hash index.

We also ensure data integrity by storing the CRC32 checksum of new records, and verifying them whenever they are retrieved from the log.

Since we don't overwrite or delete log entries, it's useful to occasionally go through and "clean up" unnecessary data. For example, if we put a record, update its value three times, then delete it, the log will continue to have five records that are no longer needed. We call `compact()` to scan through the file and replace it with a new log that only contains the last-updated entries that haven't been deleted. The hash index is also updated to reflect the adjusted byte offsets of the values.

An extension of this storage strategy which was not implemented here is to split the log across multiple files (segments) when too much data is in one file. This way, we can compact across all files, and merge ones together that have shrunk sufficiently in size.

API:
- `create(path)` - Creates an empty log and hash index.
- `open(path)` - Reopens existing data file and attempts to rebuild index.
- `get(key)` - Returns the value associated with key, if it exists.
- `put(key, value)` - Inserts key and value into the database and hash index, overwriting an existing value if the key has already been written (doesn't actually overwrite the value on disk).
- `delete(key, value)` -  "Removes" the key-value pair by appending a delete record to the log and removing the entry from the hash index. Returns the value if it exists.
- `compact()` - Replaces the log file and hash index by removing all old, redundant put and delete records.
