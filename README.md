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
