/*
Version 2: Append-only log 
*/

use std::os::unix::fs::FileExt;
use std::{collections::HashMap, fs::File, path::{Path, PathBuf}};
use std::io::{self, BufReader, ErrorKind, Read, Seek, Write};
use std::fs::OpenOptions;

const OP_BYTES: usize = 1;
const LEN_BYTES: usize = 4;
const MAX_BYTES: usize = 4096;

pub struct AppendLogDB {
    /* index maps key to byte offset of value length in log */
    index: HashMap<Vec<u8>, u64>,
    file: File,
    path: PathBuf
}

/*
Record byte format
Put record: <operation: u8 = 0><key_len: u32><key bytes><value_len: u32><value bytes>
Delete record: <operation: u8 = 1><key_len: u32><key bytes>
*/
enum Record {
    Put { key: Vec<u8>, value: Vec<u8>, value_offset: u64},
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
        let mut index: HashMap<Vec<u8>, u64> = HashMap::new();

        // default buffer size of 8 KiB
        let mut reader = BufReader::new(&mut file);

        loop {
            match Self::read_next_record(&mut reader) {
                Ok(Some(Record::Put { key, value: _, value_offset })) => {
                    index.insert(key, value_offset);
                },
                Ok(Some(Record::Delete { key })) => {
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
            Some(value_offset) => self.read_field_at(value_offset).map(Some),
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
        let value_offset = Self::append_put_record(&mut self.file, &key, value)?;

        // update index after successfully writing the record
        self.index.insert(key, value_offset);

        Ok(())
    }

    // "Removes" the key-value pair by appending a delete record to the log 
    // and removing the entry from the hash index
    // If the entry exists, returns the value
    pub fn delete(&mut self, key: impl AsRef<[u8]>) -> io::Result<Option<Vec<u8>>> {
        let key = key.as_ref();

        match self.index.get(key).copied() {
            Some(value_offset) => {
                let value_buf = self.read_field_at(value_offset)?;
                
                // write delete record
                Self::append_delete_record(&mut self.file, key)?;

                // remove from index
                self.index.remove(key);

                Ok(Some(value_buf))
            },
            None => return Ok(None)
        }
    }

    // Creates a new log and hash index by removing all old redundant put records and delete records
    // The new log will only contain the most recent key-value entries that haven't been deleted
    // The new hash index will contain the same keys as before, but with updated value offsets
    // corresponding to the new log
    // note: sequential scan is generally more efficient than random seeks using the hash index's value offsets
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

        let mut compacted_index: HashMap<Vec<u8>, u64> = HashMap::new();

        self.file.seek(std::io::SeekFrom::Start(0))?;
        let mut reader = BufReader::new(&mut self.file);

        loop {
            match Self::read_next_record(&mut reader) {
                Ok(Some(Record::Put { key, value, value_offset })) => {
                    // if this is the latest update for the key, add to the new log and index
                    if self.index.get(&key) == Some(&value_offset) {
                        let new_value_offset = Self::append_put_record(&mut compacted_file, &key, &value)?;
                        compacted_index.insert(key, new_value_offset);
                    }
                },
                Ok(Some(Record::Delete { key: _ })) => continue,
                Ok(None) => break,
                Err(e) => return Err(e)
            }
        }

        // Replace original file and index with the compacted ones
        std::fs::rename(&temp_path, &self.path)?;
        self.file = compacted_file;
        self.index = compacted_index;

        Ok(())
    }

    /***** HELPER FUNCTIONS *****/

    // Appends a Put record to the end of file, returning the offset of the value
    fn append_put_record(file: &mut File, key: &[u8], value: &[u8]) -> io::Result<u64> {
        let record_start = file.seek(std::io::SeekFrom::End(0))?;
        let value_offset = record_start + OP_BYTES as u64 + LEN_BYTES as u64 + key.len() as u64;

        let mut record = Vec::with_capacity(OP_BYTES + 2 * LEN_BYTES + key.len() + value.len());
        record.push(0);
        record.extend_from_slice(&(key.len() as u32).to_le_bytes());
        record.extend_from_slice(key);
        record.extend_from_slice(&(value.len() as u32).to_le_bytes());
        record.extend_from_slice(value);

        // WARNING: a failed partial-write may still leave part of the record in the file
        file.write_all(&record)?;

        Ok(value_offset)
    }

    // Appends a Delete record to the end of file
    fn append_delete_record(file: &mut File, key: &[u8]) -> io::Result<()> {
        let mut record = Vec::with_capacity(OP_BYTES + LEN_BYTES + key.len());
        record.push(1);
        record.extend_from_slice(&(key.len() as u32).to_le_bytes());
        record.extend_from_slice(key);
        file.write_all(&record)?;
        Ok(())
    }

    // Returns next record parsed into the Record enum
    // If EOF, None is returned
    fn read_next_record<R: Read + Seek>(reader: &mut R) -> io::Result<Option<Record>> {
        let mut op_buf = [0u8; OP_BYTES];
         match reader.read_exact(&mut op_buf) {
            Ok(()) => {},
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None), // EOF
            Err(e) => return Err(e)
        }

        match op_buf[0] {
            // put record
            0 => {
                let key = Self::read_field(reader)?;
                let value_offset = reader.stream_position()?;
                let value = Self::read_field(reader)?;
                return Ok(Some(Record::Put { key, value, value_offset }))
                // index.insert(key, value_offset);
            },
            // delete record
            1 => {
                let key = Self::read_field(reader)?;
                return Ok(Some(Record::Delete { key }))
                // index.remove(&key);
            },
            _ => return Err(io::Error::new(ErrorKind::InvalidData, "invalid operation code"))
        }
    }

    // read field at current seek position
    // assume bytes are formatted <length: u32><field bytes>
    fn read_field(reader: &mut impl Read) -> io::Result<Vec<u8>> {
        let mut len_buf = [0u8; LEN_BYTES];
        reader.read_exact(&mut len_buf)?;
        let mut bytes = vec![0u8; Self::decode_len(len_buf)?];
        reader.read_exact(&mut bytes)?;
        Ok(bytes)
    }

    // read field at specific byte offset in file
    // assume bytes are formatted <length: u32><field bytes>
    fn read_field_at(&self, offset: u64) -> io::Result<Vec<u8>> {
        let mut len_buf = [0u8; LEN_BYTES];
        self.file.read_exact_at(&mut len_buf, offset)?;
        let mut value = vec![0u8; Self::decode_len(len_buf)?];
        self.file.read_exact_at(&mut value, offset + LEN_BYTES as u64)?;
        Ok(value)
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

// TODO: write tests
