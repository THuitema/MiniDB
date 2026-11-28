/*
Version 2: Append-only log 
*/

use std::os::unix::fs::FileExt;
use std::{collections::HashMap, fs::File, path::{Path, PathBuf}};
use std::io::{self, BufReader, ErrorKind, Read, Seek, Write};
use std::fs::OpenOptions;
use crc32fast;

const CRC32_BYTES: usize = 4;
const OP_BYTES: usize = 1;
const LEN_BYTES: usize = 4;
const MAX_BYTES: usize = 4096;
const HEADER_LEN: usize = CRC32_BYTES + OP_BYTES + 2 * LEN_BYTES;

pub struct AppendLogDB {
    /* index maps key -> (record offset, record length) */
    index: HashMap<Vec<u8>, (u64, usize)>,
    file: File,
    path: PathBuf
}

/*
Record byte format
Put:    <crc32: u32><op: u8 = 0><key_len: u32><value_len: u32><key bytes><value bytes>
Delete: <crc32: u32><op: u8 = 1><key_len: u32><value_len: u32 = 0><key bytes>
*/
enum Record {
    Put { key: Vec<u8>, value: Vec<u8> },
    Delete { key: Vec<u8> }
}

impl AppendLogDB {
    // Creates new data file
    pub fn create(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let file = match OpenOptions::new()
                                .read(true)
                                .append(true)
                                .create_new(true)
                                .open(path) {
            Err(e) => return Err(e),
            Ok(file) => file
        };

        // Sync file to disk and parent directory 
        file.sync_all()?;

        let parent_dir = path.parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        File::open(parent_dir)?.sync_all()?;

        Ok(Self {
            index: HashMap::new(),
            file,
            path: path.to_path_buf()
        })
    }

    // Reopens existing data file and attempts to rebuild index
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let mut file = match OpenOptions::new()
                                .read(true)
                                .append(true)
                                .open(path) {
            Err(e) => return Err(e),
            Ok(file) => file
        };
        
        file.seek(std::io::SeekFrom::Start(0))?;

        // scan file to build index
        let mut index: HashMap<Vec<u8>, (u64, usize)> = HashMap::new();

        // default buffer size of 8 KiB
        let mut reader = BufReader::new(&mut file);

        loop {
            match Self::read_next_record(&mut reader) {
                Ok(Some((Record::Put { key, value }, record_offset))) => {
                    let record_len = HEADER_LEN + key.len() + value.len();
                    index.insert(key, (record_offset, record_len));
                },
                Ok(Some((Record::Delete { key }, _))) => {
                    index.remove(&key);
                }
                Ok(None) => break,
                Err(e) => return Err(e)
            }
        }

        Ok(Self {
            index,
            file,
            path: path.to_path_buf()
        })
    }

    // Returns the value associated with key
    // If the key-value pair doesn't exist, this will return Ok(None)
    pub fn get(&self, key: impl AsRef<[u8]>) -> io::Result<Option<Vec<u8>>> {
        let key = key.as_ref();

        match self.index.get(key).copied() {
            Some((record_offset, record_len)) => {
                let mut buffer = vec![0u8; record_len];
                self.file.read_exact_at(&mut buffer, record_offset)?;
                let record = Self::decode_record(&buffer)?;
                match record {
                    Record::Put { key: record_key, value } if record_key == key => Ok(Some(value)),
                    _ => Err(io::Error::new(ErrorKind::InvalidData, "index points to the wrong record"))
                }
            },
            None => Ok(None)
        }
    }

    // Inserts key and value into the database and hash index, 
    // overwriting an existing value if the key has already been written
    // Key and value must not exceed MAX_BYTES bytes
    pub fn put(&mut self, key: impl Into<Vec<u8>>, value: impl AsRef<[u8]>) -> io::Result<()> {
        let key = key.into();
        if key.len() > MAX_BYTES {
            return Err(io::Error::new(ErrorKind::InvalidInput, "key size exceeds limit"));
        }

        let value = value.as_ref();
        if value.len() > MAX_BYTES {
            return Err(io::Error::new(ErrorKind::InvalidInput, "value size exceeds limit"));
        }

        // write put record
        let record = Record::Put { key: key.clone(), value: value.to_vec() };
        let record_offset = Self::append_record(&mut self.file, &record)?;

        // update index after successfully writing the record
        let record_len = HEADER_LEN + key.len() + value.len();
        self.index.insert(key, (record_offset, record_len));

        Ok(())
    }

    // "Removes" the key-value pair by appending a delete record to the log 
    // and removing the entry from the hash index
    // If the entry exists, returns the value
    pub fn delete(&mut self, key: impl AsRef<[u8]>) -> io::Result<Option<Vec<u8>>> {
        let key = key.as_ref();

        match self.index.get(key).copied() {
            Some((record_offset, record_len)) => {
                let mut buffer = vec![0u8; record_len];
                self.file.read_exact_at(&mut buffer, record_offset)?;
                let record = Self::decode_record(&buffer)?;
                let value = match record {
                    Record::Put { key: record_key, value } if record_key == key => value,
                    _ => return Err(io::Error::new(ErrorKind::InvalidData, "index points to the wrong record"))
                };
                
                // write delete record
                let delete_record = Record::Delete { key: key.to_vec() };
                Self::append_record(&mut self.file, &delete_record)?;

                // remove from index
                self.index.remove(key);

                Ok(Some(value))
            },
            None => return Ok(None)
        }
    }

    // Replaces the log and hash index by removing all old redundant put records and delete records
    // The new log will only contain the most recent key-value entries that haven't been deleted
    // The new hash index will contain the same keys as before, but with updated record offsets
    // corresponding to the new log
    pub fn compact(&mut self) -> io::Result<()> {
        let mut temp_path = self.path.as_os_str().to_os_string();
        temp_path.push(".tmp");
        let temp_path = PathBuf::from(temp_path);

        let mut compacted_file = match OpenOptions::new()
                                .read(true)
                                .append(true)
                                .create_new(true)
                                .open(&temp_path) {
            Err(e) => return Err(e),
            Ok(file) => file
        };

        let mut compacted_index: HashMap<Vec<u8>, (u64, usize)> = HashMap::new();

        // NOTE: sequential scan is generally more efficient than random seeks using the hash index's record offsets

        self.file.seek(std::io::SeekFrom::Start(0))?;
        let mut reader = BufReader::new(&mut self.file);

        loop {
            match Self::read_next_record(&mut reader) {
                Ok(Some((Record::Put { key, value }, record_offset))) => {
                    // if this is the latest update for the key, add to the new log and index
                    if self.index.get(&key) == Some(&(record_offset, HEADER_LEN + key.len() + value.len())) {
                        let new_record = Record::Put { key: key.clone(), value: value.clone() };
                        let new_record_offset = Self::append_record(&mut compacted_file, &new_record)?;
                        let new_record_len = HEADER_LEN + key.len() + value.len();
                        compacted_index.insert(key, (new_record_offset, new_record_len));
                    }
                },
                Ok(Some((Record::Delete { key: _ }, _))) => continue,
                Ok(None) => break,
                Err(e) => return Err(e)
            }
        }

        compacted_file.sync_all()?; // persist all file contents and metadata to disk

        // Replace original file and index with the compacted ones
        std::fs::rename(&temp_path, &self.path)?;
        self.file = compacted_file;
        self.index = compacted_index;

        // Force filesystem to commit the updated directory entry to disk
        let parent_dir = self.path.parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        File::open(parent_dir)?.sync_all()?;
        Ok(())
    }

    /***** HELPER FUNCTIONS *****/

    // Writes the record to the end of file, returning the offset of the first byte
    fn append_record(file: &mut File, record: &Record) -> io::Result<u64> {
        let record_offset = file.seek(std::io::SeekFrom::End(0))?;
        let buffer = Self::encode_record(record);

        // WARNING: a failed partial-write may still leave part of the record in the file
        file.write_all(&buffer)?;
        file.sync_data()?;

        Ok(record_offset)
    }

    // Returns next record parsed into Record, and byte offset in file of record start
    // If EOF, None is returned
    fn read_next_record<R: Read + Seek>(reader: &mut R) -> io::Result<Option<(Record, u64)>> {
        let record_offset = reader.stream_position()?;

        // Parse header to get key and value lengths
        let mut bytes = vec![0u8; HEADER_LEN];
        match reader.read_exact(&mut bytes[..1]) {
            Ok(()) => {},
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        reader.read_exact(&mut bytes[1..])?;

        let (_, _, key_len, value_len) = Self::decode_header(&bytes)?;

        // Read key and value
        let mut payload = vec![0u8; key_len + value_len];
        reader.read_exact(&mut payload)?;
        bytes.extend_from_slice(&payload);

        let record = Self::decode_record(&bytes)?;
        Ok(Some((record, record_offset)))
    }

    // Encodes record into byte format specified at top of file
    fn encode_record(record: &Record) -> Vec<u8> {
        let (op, key, value): (u8, &[u8], &[u8]) = match record {
            Record::Put { key, value, .. } => (0, key, value),
            Record::Delete { key } => (1, key, &[]),
        };

        let mut bytes = Vec::with_capacity(CRC32_BYTES + OP_BYTES + 2 * LEN_BYTES + key.len() + value.len());
        bytes.extend_from_slice(&[0; CRC32_BYTES]); // compute checksum after putting values in buffer
        bytes.push(op);
        bytes.extend_from_slice(&(key.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(value.len() as u32).to_le_bytes());
        bytes.extend_from_slice(key);
        bytes.extend_from_slice(value);

        let checksum = crc32fast::hash(&bytes[CRC32_BYTES..]);
        bytes[..CRC32_BYTES].copy_from_slice(&checksum.to_le_bytes());
        bytes
    }

    // Parses bytes for checksum, op, key_len, value_len
    fn decode_header(bytes: &[u8]) -> io::Result<(u32, u8, usize, usize)> {
        if bytes.len() < HEADER_LEN {
            return Err(io::Error::new(ErrorKind::UnexpectedEof, "incomplete record header"));
        }

        let checksum = u32::from_le_bytes(bytes[..CRC32_BYTES].try_into().unwrap());

        let op = bytes[CRC32_BYTES];
        if op != 0 && op != 1 {
            return Err(io::Error::new(ErrorKind::InvalidData, "invalid operation code"));
        }

        let key_len_offset = CRC32_BYTES + OP_BYTES;
        let value_len_offset = key_len_offset + LEN_BYTES;
        let key_len = Self::decode_len(bytes[key_len_offset..value_len_offset].try_into().unwrap())?;
        let value_len = Self::decode_len(bytes[value_len_offset..HEADER_LEN].try_into().unwrap())?;
        if op == 1 && value_len != 0 {
            return Err(io::Error::new(ErrorKind::InvalidData, "delete record has a value"));
        }

        Ok((checksum, op, key_len, value_len))
    }

    // Parses bytes into a Record, while verifying checksum
    fn decode_record(bytes: &[u8]) -> io::Result<Record> {
        let (checksum_field, op, key_len, value_len) = Self::decode_header(bytes)?;

        let record_len = HEADER_LEN + key_len + value_len;

        if bytes.len() < record_len {
            return Err(io::Error::new(ErrorKind::UnexpectedEof, "incomplete record payload"));
        }
        if bytes.len() > record_len {
            return Err(io::Error::new(ErrorKind::InvalidData, "extra bytes after record"));
        }

        // Verify checksum
        let actual_checksum = crc32fast::hash(&bytes[CRC32_BYTES..]);
        if checksum_field != actual_checksum {
            return Err(io::Error::new(ErrorKind::InvalidData, "invalid CRC32 checksum"));
        }

        // Read key and value
        let value_offset = HEADER_LEN + key_len;
        let mut key: Vec<u8> = Vec::with_capacity(key_len);
        key.extend_from_slice(&bytes[HEADER_LEN..value_offset]);

        if op == 1 {
            return Ok(Record::Delete { key });
        } else {
            let mut value: Vec<u8> = Vec::with_capacity(value_len);
            value.extend_from_slice(&bytes[value_offset..]);
            return Ok(Record::Put { key, value });
        }
    }

    // converts a little-endian u32 to usize, while checking that it is less than the max length constraint
    fn decode_len(bytes: [u8; LEN_BYTES]) -> io::Result<usize> {
        let len = usize::try_from(u32::from_le_bytes(bytes))
            .map_err(|_| io::Error::new(ErrorKind::InvalidData, format!("Length size does not fit usize")))?;
        if len > MAX_BYTES {
            return Err(io::Error::new(ErrorKind::InvalidData, format!("Length size exceeds limit")));
        }
        Ok(len)
    }
}

#[cfg(test)]
mod tests {
    use super::{AppendLogDB, Record};
    use std::{
        io::{self, Cursor, Write},
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

    struct TestLog {
        path: PathBuf,
    }

    impl TestLog {
        fn new() -> Self {
            let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "mini-db-{}-{timestamp}-{id}.log",
                std::process::id()
            ));
            Self { path }
        }
    }

    impl Drop for TestLog {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn encode_put_record() {
        let record = Record::Put {
            key: b"key".to_vec(),
            value: b"value".to_vec()
        };
        let bytes = AppendLogDB::encode_record(&record);

        assert_eq!(&bytes[4..], b"\x00\x03\x00\x00\x00\x05\x00\x00\x00keyvalue");
        assert_eq!(&bytes[..4], &crc32fast::hash(&bytes[4..]).to_le_bytes());
    }

    #[test]
    fn encode_delete_record() {
        let record = Record::Delete { key: b"key".to_vec() };
        let bytes = AppendLogDB::encode_record(&record);

        assert_eq!(&bytes[4..], b"\x01\x03\x00\x00\x00\x00\x00\x00\x00key");
        assert_eq!(&bytes[..4], &crc32fast::hash(&bytes[4..]).to_le_bytes());
    }

    #[test]
    fn decode_record_rejects_bad_header_and_short_payload() {
        let record = Record::Delete { key: b"key".to_vec() };
        let bytes = AppendLogDB::encode_record(&record);

        assert_eq!(AppendLogDB::decode_record(&bytes[..12]).err().unwrap().kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(AppendLogDB::decode_record(&bytes[..bytes.len() - 1]).err().unwrap().kind(), io::ErrorKind::UnexpectedEof);

        let mut invalid_op = bytes.clone();
        invalid_op[4] = 2;
        assert_eq!(AppendLogDB::decode_record(&invalid_op).err().unwrap().kind(), io::ErrorKind::InvalidData);

        let mut invalid_len = bytes;
        invalid_len[5..9].copy_from_slice(&4097u32.to_le_bytes());
        assert_eq!(AppendLogDB::decode_record(&invalid_len).err().unwrap().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn decode_encode_roundtrip() -> io::Result<()> {
        let records = [
            Record::Put { key: b"key".to_vec(), value: b"value".to_vec() },
            Record::Put { key: b"empty".to_vec(), value: Vec::new() },
            Record::Delete { key: b"key".to_vec() },
        ];

        for record in records {
            let bytes = AppendLogDB::encode_record(&record);
            let decoded = AppendLogDB::decode_record(&bytes)?;
            match (record, decoded) {
                (Record::Put { key, value }, Record::Put { key: decoded_key, value: decoded_value }) => {
                    assert_eq!(decoded_key, key);
                    assert_eq!(decoded_value, value);
                },
                (Record::Delete { key }, Record::Delete { key: decoded_key }) => {
                    assert_eq!(decoded_key, key);
                },
                _ => panic!("decoded record has the wrong operation"),
            }
        }

        Ok(())
    }

    #[test]
    fn read_next_record_decodes_records_and_tracks_offsets() -> io::Result<()> {
        let put = AppendLogDB::encode_record(&Record::Put {
            key: b"key".to_vec(),
            value: b"value".to_vec(),
        });
        let delete = AppendLogDB::encode_record(&Record::Delete { key: b"key".to_vec() });
        let mut reader = Cursor::new([put.clone(), delete].concat());

        let (record, offset) = AppendLogDB::read_next_record(&mut reader)?.unwrap();
        assert_eq!(offset, 0);
        match record {
            Record::Put { key, value } => {
                assert_eq!(key, b"key");
                assert_eq!(value, b"value");
            },
            _ => panic!("expected Put record"),
        }

        let (record, offset) = AppendLogDB::read_next_record(&mut reader)?.unwrap();
        assert_eq!(offset, put.len() as u64);
        match record {
            Record::Delete { key } => assert_eq!(key, b"key"),
            _ => panic!("expected Delete record"),
        }
        assert!(AppendLogDB::read_next_record(&mut reader)?.is_none());
        Ok(())
    }

    #[test]
    fn read_next_record_rejects_partial_records_and_bad_checksum() {
        let bytes = AppendLogDB::encode_record(&Record::Put {
            key: b"key".to_vec(),
            value: b"value".to_vec(),
        });

        let mut partial_header = Cursor::new(bytes[..1].to_vec());
        assert_eq!(AppendLogDB::read_next_record(&mut partial_header).err().unwrap().kind(), io::ErrorKind::UnexpectedEof);

        let mut partial_payload = Cursor::new(bytes[..bytes.len() - 1].to_vec());
        assert_eq!(AppendLogDB::read_next_record(&mut partial_payload).err().unwrap().kind(), io::ErrorKind::UnexpectedEof);

        let mut damaged = bytes;
        let last_byte = damaged.len() - 1;
        damaged[last_byte] ^= 1;
        let mut bad_checksum = Cursor::new(damaged);
        assert_eq!(AppendLogDB::read_next_record(&mut bad_checksum).err().unwrap().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn missing_key_returns_none() -> io::Result<()> {
        let log = TestLog::new();
        let db = AppendLogDB::create(&log.path)?;

        assert_eq!(db.get(b"missing")?, None);
        Ok(())
    }

    #[test]
    fn put_then_get_key() -> io::Result<()> {
        let log = TestLog::new();
        let mut db = AppendLogDB::create(&log.path)?;

        db.put(b"name".to_vec(), b"Bob")?;

        assert_eq!(db.get(b"name")?, Some(b"Bob".to_vec()));
        Ok(())
    }

    #[test]
    fn get_and_delete_reject_an_index_entry_for_another_key() -> io::Result<()> {
        let log = TestLog::new();
        let mut db = AppendLogDB::create(&log.path)?;
        db.put(b"first", b"one")?;
        db.put(b"second", b"two")?;

        let second_location = db.index.get(b"second".as_slice()).copied().unwrap();
        db.index.insert(b"first".to_vec(), second_location);

        assert_eq!(db.get(b"first").err().unwrap().kind(), io::ErrorKind::InvalidData);
        assert_eq!(db.delete(b"first").err().unwrap().kind(), io::ErrorKind::InvalidData);
        assert_eq!(db.get(b"second")?, Some(b"two".to_vec()));
        Ok(())
    }

    #[test]
    fn get_overwritten_value() -> io::Result<()> {
        let log = TestLog::new();
        let mut db = AppendLogDB::create(&log.path)?;

        db.put(b"name".to_vec(), b"Bob")?;
        db.put(b"name".to_vec(), b"Tom")?;

        assert_eq!(db.get(b"name")?, Some(b"Tom".to_vec()));
        drop(db);
        Ok(())
    }

    #[test]
    fn overwrite_survives_reopening() -> io::Result<()> {
        let log = TestLog::new();
        let mut db = AppendLogDB::create(&log.path)?;
        db.put(b"name", b"Bob")?;
        db.put(b"name", b"Tom")?;

        assert_eq!(db.get(b"name")?, Some(b"Tom".to_vec()));
        drop(db);

        let db = AppendLogDB::open(&log.path)?;
        assert_eq!(db.get(b"name")?, Some(b"Tom".to_vec()));
        Ok(())
    }

    #[test]
    fn delete_key_returns_none() -> io::Result<()> {
        let log = TestLog::new();
        let mut db = AppendLogDB::create(&log.path)?;

        db.put(b"name", b"Bob")?;
        db.delete(b"name")?;

        assert_eq!(db.get(b"name")?, None);
        drop(db);
        Ok(())
    }

    #[test]
    fn delete_survives_reopening() -> io::Result<()> {
        let log = TestLog::new();
        let mut db = AppendLogDB::create(&log.path)?;
        db.put(b"name", b"Bob")?;
        db.delete(b"name")?;

        assert_eq!(db.get(b"name")?, None);
        drop(db);

        let db = AppendLogDB::open(&log.path)?;
        assert_eq!(db.get(b"name")?, None);
        Ok(())
    }

    #[test]
    fn complex_reopen() -> io::Result<()> {
        let log = TestLog::new();
        let mut db = AppendLogDB::create(&log.path)?;
        db.put(b"name", b"Bob")?;
        db.put(b"age", b"21")?;
        db.put(b"role", b"Engineer")?;
        db.delete(b"age")?;
        db.put(b"name", "Tom")?;

        drop(db);

        let db = AppendLogDB::open(&log.path)?;
        assert_eq!(db.get(b"name")?, Some(b"Tom".to_vec()));
        assert_eq!(db.get(b"age")?, None);
        assert_eq!(db.get(b"role")?, Some(b"Engineer".to_vec()));
        Ok(())
    }

    #[test]
    fn compact_keeps_only_latest_live_records() -> io::Result<()> {
        let log = TestLog::new();
        let mut db = AppendLogDB::create(&log.path)?;
        db.put(b"name", b"Bob")?;
        db.put(b"age", b"21")?;
        db.put(b"role", b"Engineer")?;
        db.delete(b"age")?;
        db.put(b"name", b"Tom")?;

        let size_before = std::fs::metadata(&log.path)?.len();
        db.compact()?;

        // Only two put records should remain: name=Tom (20 bytes) and role=Engineer (25 bytes)
        assert!(std::fs::metadata(&log.path)?.len() < size_before);
        assert_eq!(std::fs::metadata(&log.path)?.len(), 45);
        assert_eq!(db.get(b"name")?, Some(b"Tom".to_vec()));
        assert_eq!(db.get(b"age")?, None);
        assert_eq!(db.get(b"role")?, Some(b"Engineer".to_vec()));

        drop(db);
        Ok(())
    }

    #[test]
    fn reopen_and_write_after_compact() -> io::Result<()> {
        let log = TestLog::new();
        let mut db = AppendLogDB::create(&log.path)?;
        db.put(b"name", b"Bob")?;
        db.put(b"age", b"21")?;
        db.put(b"role", b"Engineer")?;
        db.delete(b"age")?;
        db.put(b"name", b"Tom")?;

        db.compact()?;

        // Only two put records should remain: name=Tom (20 bytes) and role=Engineer (25 bytes)
        db.put(b"name", b"John")?;
        db.put(b"role", b"Architect")?;

        drop(db);
        let db = AppendLogDB::open(&log.path)?;
        assert_eq!(db.get(b"name")?, Some(b"John".to_vec()));
        assert_eq!(db.get(b"age")?, None);
        assert_eq!(db.get(b"role")?, Some(b"Architect".to_vec()));
        Ok(())
    }

    #[test]
    fn open_rejects_partial_record() -> io::Result<()> {
        let log = TestLog::new();
        let mut db = AppendLogDB::create(&log.path)?;
        db.put(b"name", b"Bob")?;
        drop(db);

        // The record declares a 5-byte value, but the last 3 bytes are missing
        let mut file = std::fs::OpenOptions::new().append(true).open(&log.path)?;
        let record = AppendLogDB::encode_record(&Record::Put {
            key: b"lost".to_vec(),
            value: b"abcde".to_vec(),
        });
        file.write_all(&record[..record.len() - 3])?;
        drop(file);

        let error = match AppendLogDB::open(&log.path) {
            Ok(_) => panic!("open accepted a truncated record"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        Ok(())
    }
}
