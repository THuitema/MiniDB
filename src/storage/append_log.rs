/*
Version 2: Append-only log 

Record format
Put record: <operation: u8 = 0><key_len: u32><key bytes><value_len: u32><value bytes>
Delete record: <operation: u8 = 1><key_len: u32><key bytes>
*/

use std::os::unix::fs::FileExt;
use std::{collections::HashMap, fs::File};
use std::io::{self, BufReader, ErrorKind, Read, Seek, Write};
use std::fs::OpenOptions;

const OP_BYTES: usize = 1;
const LEN_BYTES: usize = 4;
const MAX_BYTES: usize = 4096;

pub struct AppendLogDB {
    /* index maps key to byte offset of value length in log */
    index: HashMap<Vec<u8>, u64>,
    file: File
}

impl AppendLogDB {
    // Creates new data file
    pub fn create() -> io::Result<Self> {
        let file = match OpenOptions::new()
                                .read(true)
                                .append(true)
                                .create_new(true)
                                .open("data/0001.log") {
            Err(e) => return Err(e),
            Ok(file) => file
        };

        Ok(Self {
            index: HashMap::new(),
            file
        })
    }

    // Reopens existing data file and attempts to rebuild index
    pub fn open() -> io::Result<Self> {
        let mut file = match OpenOptions::new()
                                .read(true)
                                .append(true)
                                .open("data/0001.log") {
            Err(e) => return Err(e),
            Ok(file) => file
        };
        
        file.seek(std::io::SeekFrom::Start(0))?;

        // scan file to build index
        let mut index: HashMap<Vec<u8>, u64> = HashMap::new();

        // default buffer size of 8 KiB
        let mut reader = BufReader::new(&mut file);

        let mut op_buf = [0u8; OP_BYTES];
        loop {
            match reader.read_exact(&mut op_buf) {
                Ok(()) => {},
                Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e)
            }

            match op_buf[0] {
                // put record
                0 => {
                    let key = Self::read_field(&mut reader)?;
                    let value_offset = reader.stream_position()?;
                    // Read the value to validate the record and advance to the next one.
                    Self::read_field(&mut reader)?;
                    index.insert(key, value_offset);
                },
                // delete record
                1 => {
                    let key = Self::read_field(&mut reader)?;
                    index.remove(&key);
                },
                _ => return Err(io::Error::new(ErrorKind::InvalidData, "invalid operation code"))
            }
            
        }

        Ok(Self {
            index,
            file
        })
    }

    // Returns the value associated with key
    // If the key-value pair doesn't exist, this will return Ok(None)
    pub fn get(&self, key: impl AsRef<[u8]>) -> io::Result<Option<Vec<u8>>> {
        let key = key.as_ref();

        match self.index.get(key) {
            Some(offset) => self.read_value_at(*offset).map(Some),
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

        let record_start = self.file.seek(std::io::SeekFrom::End(0))?;
        let value_offset = record_start + OP_BYTES as u64 + LEN_BYTES as u64 + key.len() as u64;

        let mut record = Vec::with_capacity(OP_BYTES + 2 * LEN_BYTES + key.len() + value.len());
        record.push(0);
        record.extend_from_slice(&(key.len() as u32).to_le_bytes());
        record.extend_from_slice(&key);
        record.extend_from_slice(&(value.len() as u32).to_le_bytes());
        record.extend_from_slice(value);

        // WARNING: a failed write may still leave part of the record in the file
        self.file.write_all(&record)?;

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
                let value_buf = self.read_value_at(value_offset)?;
                
                // write delete record
                let mut record = Vec::with_capacity(OP_BYTES + LEN_BYTES + key.len());
                record.push(1);
                record.extend_from_slice(&(key.len() as u32).to_le_bytes());
                record.extend_from_slice(key);

                self.file.write_all(&record)?;

                // remove from index
                self.index.remove(key);

                Ok(Some(value_buf))
            },
            None => return Ok(None)
        }
    }

    // TODO: compact

    // converts a little-endian u32 to usize, while checking that it is less than the max length constraint
    fn decode_len(bytes: [u8; LEN_BYTES]) -> io::Result<usize> {
        let len = usize::try_from(u32::from_le_bytes(bytes))
            .map_err(|_| io::Error::new(ErrorKind::InvalidData, format!("Length size does not fit usize")))?;
        if len > MAX_BYTES {
            return Err(io::Error::new(ErrorKind::InvalidData, format!("Length size exceeds limit")));
        }
        Ok(len)
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
    // assume bytes are formatted
    fn read_value_at(&self, offset: u64) -> io::Result<Vec<u8>> {
        let mut len_buf = [0u8; LEN_BYTES];
        self.file.read_exact_at(&mut len_buf, offset)?;
        let mut value = vec![0u8; Self::decode_len(len_buf)?];
        self.file.read_exact_at(&mut value, offset + LEN_BYTES as u64)?;
        Ok(value)
    }
}
